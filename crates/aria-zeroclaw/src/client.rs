//! Persistent WebSocket sessions against the ZeroClaw gateway.
//!
//! One logical session per Alexa device (its `session_id`), created lazily and
//! kept alive; the agent's memory therefore persists across Alexa sessions.
//! Turns are serialized per session; an `approval_request` returned mid-turn
//! completes the turn early so the caller can voice the prompt to the user.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::protocol::{self, InFrame};

/// Hard lifetime for a stuck turn slot (gateway hang safety valve).
const STALE_TURN_AFTER: Duration = Duration::from_secs(300);
const CONNECT_RETRY_MAX: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("gateway connection failed: {0}")]
    Connect(String),
    #[error("session busy (previous turn still in progress)")]
    Busy,
    #[error("gateway disconnected: {0}")]
    Disconnected(String),
    #[error("gateway configuration error: {0}")]
    Config(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Approve,
    Deny,
    Always,
}

impl Decision {
    pub fn as_wire(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
            Decision::Always => "always",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalInfo {
    pub request_id: String,
    pub tool: String,
    pub summary: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// Full agent reply (present when the turn completed).
    pub full: Option<String>,
    /// Text streamed so far (used on timeout or early return).
    pub partial: String,
    pub timed_out: bool,
    /// Turn paused waiting for a tool approval; the gateway turn is still live.
    pub awaiting_approval: Option<ApprovalInfo>,
    pub cost_usd: Option<f64>,
}

/// Out-of-band events for the surrounding app.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    ApprovalRequested {
        device: String,
        info: ApprovalInfo,
    },
    /// Reply that completed a turn whose caller had already left (approval
    /// decision or timeout); surface it on the next user turn.
    LateReply {
        device: String,
        text: String,
    },
    SessionError {
        device: String,
        message: String,
    },
}

#[derive(Clone)]
pub struct GatewayConfig {
    /// Gateway base URL, e.g. `ws://127.0.0.1:3000`.
    pub base: String,
    /// Bearer token for the gateway (optional on trusted LAN setups).
    pub token: Option<String>,
    /// ZeroClaw agent alias to converse with.
    pub agent_alias: String,
}

impl GatewayConfig {
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.base.trim().is_empty() {
            return Err(AgentError::Config("zeroclaw.gateway is empty".into()));
        }
        if self.agent_alias.trim().is_empty() {
            return Err(AgentError::Config("zeroclaw.agent_alias is empty".into()));
        }
        Ok(())
    }
}

struct Inner {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    events: broadcast::Sender<AgentEvent>,
}

/// Handle to the ZeroClaw gateway. Cheap to clone.
#[derive(Clone)]
pub struct ZeroClaw {
    cfg: GatewayConfig,
    inner: Arc<Inner>,
}

struct Session {
    device: String,
    /// Sender for the current connection generation; replaced on reconnect.
    out: Mutex<mpsc::UnboundedSender<String>>,
    turn: Mutex<Option<TurnSlot>>,
    alive: Arc<AtomicBool>,
}

struct TurnSlot {
    partial: Mutex<String>,
    done_tx: Option<oneshot::Sender<TurnOutcome>>,
    /// Set when the turn left early for an approval; a later `done` becomes a
    /// LateReply instead of a normal outcome.
    awaiting: bool,
    started: Instant,
}

impl ZeroClaw {
    pub fn new(cfg: GatewayConfig) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            cfg,
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                events,
            }),
        }
    }

    /// Subscribe to out-of-band gateway events.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.inner.events.subscribe()
    }

    /// Run one conversational turn. Returns when the agent finished, the turn
    /// paused on a tool approval, or `timeout` elapsed (partial reply then).
    pub async fn ask(
        &self,
        device: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<TurnOutcome, AgentError> {
        let session = self.session(device).await?;
        self.wait_ready(&session, Duration::from_secs(10)).await?;

        let (done_tx, done_rx) = oneshot::channel();
        {
            let mut turn = session.turn.lock().unwrap();
            if let Some(existing) = turn.as_ref() {
                if existing.started.elapsed() < STALE_TURN_AFTER {
                    return Err(AgentError::Busy);
                }
                debug!(device = %session.device, "replacing stale turn slot");
            }
            *turn = Some(TurnSlot {
                partial: Mutex::new(String::new()),
                done_tx: Some(done_tx),
                awaiting: false,
                started: Instant::now(),
            });
        }

        session
            .out
            .lock()
            .unwrap()
            .send(protocol::message_frame(text))
            .map_err(|_| AgentError::Disconnected("send channel closed".into()))?;

        match tokio::time::timeout(timeout, done_rx).await {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(_)) => Err(AgentError::Disconnected("turn channel closed".into())),
            Err(_) => {
                // Timeout: hand back what streamed so far. The slot stays so a
                // late `done` can turn into a LateReply.
                let partial = session
                    .turn
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.partial.lock().unwrap().clone())
                    .unwrap_or_default();
                Ok(TurnOutcome {
                    full: None,
                    partial,
                    timed_out: true,
                    awaiting_approval: None,
                    cost_usd: None,
                })
            }
        }
    }

    /// Answer a pending tool-approval prompt on the gateway.
    pub fn respond_approval(
        &self,
        device: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<(), AgentError> {
        let sessions = self.inner.sessions.lock().unwrap();
        let session = sessions
            .get(&sanitize_key(device))
            .ok_or_else(|| AgentError::Disconnected("no session".into()))?;
        session
            .out
            .lock()
            .unwrap()
            .send(protocol::approval_frame(request_id, decision.as_wire()))
            .map_err(|_| AgentError::Disconnected("send channel closed".into()))
    }

    async fn session(&self, device: &str) -> Result<Arc<Session>, AgentError> {
        self.cfg.validate()?;
        let key = sanitize_key(device);
        let (existing, needs_spawn) = {
            let sessions = self.inner.sessions.lock().unwrap();
            match sessions.get(&key).cloned() {
                Some(s) if s.alive.load(std::sync::atomic::Ordering::SeqCst) => (Some(s), false),
                Some(s) => (Some(s), true),
                None => (None, true),
            }
        };
        let session = match existing {
            Some(s) => s,
            None => {
                let (out, _rx) = mpsc::unbounded_channel();
                let s = Arc::new(Session {
                    device: key.clone(),
                    out: Mutex::new(out),
                    turn: Mutex::new(None),
                    alive: Arc::new(AtomicBool::new(false)),
                });
                self.inner
                    .sessions
                    .lock()
                    .unwrap()
                    .insert(key, Arc::clone(&s));
                s
            }
        };
        if needs_spawn {
            spawn_conn(&self.inner, &self.cfg, &session);
        }
        Ok(session)
    }

    async fn wait_ready(
        &self,
        session: &Arc<Session>,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let deadline = Instant::now() + timeout;
        loop {
            if session.alive.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AgentError::Connect(format!(
                    "gateway not reachable at {} (agent {})",
                    self.cfg.base, self.cfg.agent_alias
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn sanitize_key(device: &str) -> String {
    device
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Connection task ────────────────────────────────────────────────────────

/// Spawn (or respawn) the read/write loop for a device session. A fresh
/// sender channel is installed per generation; replacing it closes the
/// previous generation's channel, terminating its task.
fn spawn_conn(client: &Arc<Inner>, cfg: &GatewayConfig, session: &Arc<Session>) {
    let events = client.events.clone();
    let session = Arc::clone(session);
    let base = cfg.base.clone();
    let alias = cfg.agent_alias.clone();
    let token = cfg.token.clone();
    tokio::spawn(async move {
        let url = protocol::chat_url(&base, &alias, &session.device, token.as_deref())
            .unwrap_or_else(|| panic!("invalid gateway base url: {base} (alias {alias})"));
        let mut backoff = Duration::from_secs(1);
        loop {
            let (out, rx) = mpsc::unbounded_channel();
            *session.out.lock().unwrap() = out;
            session
                .alive
                .store(false, std::sync::atomic::Ordering::SeqCst);
            debug!(device = %session.device, "connecting to zeroclaw gateway");
            let connected = match tokio_tungstenite::connect_async(&url).await {
                Ok((ws, _resp)) => ws,
                Err(e) => {
                    warn!(device = %session.device, %e, "zeroclaw gateway connect failed");
                    let _ = rx;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(CONNECT_RETRY_MAX);
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
            session
                .alive
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(device = %session.device, "zeroclaw gateway connected");
            run_loop(&session, connected, rx, &events).await;
            session
                .alive
                .store(false, std::sync::atomic::Ordering::SeqCst);
            warn!(device = %session.device, "zeroclaw gateway connection lost");
            finalize_turn(&session, "gateway connection lost mid-turn");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(CONNECT_RETRY_MAX);
        }
    });
}

async fn run_loop<S>(
    session: &Arc<Session>,
    ws: WebSocketStream<S>,
    mut rx: mpsc::UnboundedReceiver<String>,
    events: &broadcast::Sender<AgentEvent>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            biased;
            out = rx.recv() => {
                match out {
                    Some(frame) => {
                        if sink.send(Message::text(frame)).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => handle_frame(session, events, &t),
                    Some(Ok(Message::Ping(p))) => {
                        if sink.send(Message::Pong(p)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!(device = %session.device, %e, "ws read error");
                        return;
                    }
                    None => return,
                }
            }
        }
    }
}

/// Complete an in-flight turn when the connection dies so callers do not wait
/// their full timeout.
fn finalize_turn(session: &Arc<Session>, reason: &str) {
    let taken = session.turn.lock().unwrap().take();
    if let Some(slot) = taken {
        let partial = slot.partial.lock().unwrap().clone();
        if let Some(tx) = slot.done_tx {
            let _ = tx.send(TurnOutcome {
                full: None,
                partial,
                timed_out: true,
                awaiting_approval: None,
                cost_usd: None,
            });
        }
        warn!(device = %session.device, %reason, "turn interrupted");
    }
}

fn handle_frame(session: &Arc<Session>, events: &broadcast::Sender<AgentEvent>, text: &str) {
    let frame: InFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(_) => return,
    };
    match frame {
        InFrame::Chunk { content } => {
            if let Some(slot) = session.turn.lock().unwrap().as_ref() {
                slot.partial.lock().unwrap().push_str(&content);
            }
        }
        InFrame::Done {
            full_response,
            cost_usd,
        } => {
            let mut turn = session.turn.lock().unwrap();
            let Some(slot) = turn.as_mut() else {
                return;
            };
            if slot.awaiting {
                // Caller already left (approval decision / timeout): surface as
                // a late reply.
                let text = if full_response.is_empty() {
                    slot.partial.lock().unwrap().clone()
                } else {
                    full_response.clone()
                };
                let _ = events.send(AgentEvent::LateReply {
                    device: session.device.clone(),
                    text,
                });
                *turn = None;
            } else if let Some(tx) = slot.done_tx.take() {
                let partial = slot.partial.lock().unwrap().clone();
                let _ = tx.send(TurnOutcome {
                    full: Some(full_response.clone()),
                    partial,
                    timed_out: false,
                    awaiting_approval: None,
                    cost_usd,
                });
                *turn = None;
            }
        }
        InFrame::ApprovalRequest {
            request_id,
            tool,
            arguments_summary,
            timeout_secs,
        } => {
            let info = ApprovalInfo {
                request_id,
                tool,
                summary: arguments_summary,
                timeout_secs,
            };
            let _ = events.send(AgentEvent::ApprovalRequested {
                device: session.device.clone(),
                info: info.clone(),
            });
            // If a turn is active, let the caller out early so it can voice
            // the prompt; the slot stays (awaiting) for the late done.
            let mut turn = session.turn.lock().unwrap();
            if let Some(slot) = turn.as_mut() {
                if !slot.awaiting {
                    slot.awaiting = true;
                    if let Some(tx) = slot.done_tx.take() {
                        let partial = slot.partial.lock().unwrap().clone();
                        let _ = tx.send(TurnOutcome {
                            full: None,
                            partial,
                            timed_out: false,
                            awaiting_approval: Some(info),
                            cost_usd: None,
                        });
                    }
                }
            }
        }
        InFrame::Other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_device_key() {
        assert_eq!(sanitize_key("Echo-A123.abc"), "Echo-A123.abc");
        assert_eq!(sanitize_key("weird/dev id!"), "weird_dev_id_");
    }

    #[test]
    fn decision_wire_values() {
        assert_eq!(Decision::Approve.as_wire(), "approve");
        assert_eq!(Decision::Deny.as_wire(), "deny");
        assert_eq!(Decision::Always.as_wire(), "always");
    }
}
