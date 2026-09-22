//! Agent backend trait + ZeroClaw / mock implementations.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aria_zeroclaw::{AgentError, AgentEvent, Decision, GatewayConfig, TurnOutcome, ZeroClaw};
use tokio::sync::broadcast;

use crate::config::Config;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the Alexa router can ask an agent brain to do.
pub trait AgentBackend: Send + Sync {
    /// One conversational turn; `timeout` is the voice response budget.
    fn ask(
        &self,
        device: &str,
        text: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<TurnOutcome, AgentError>>;
    /// Answer a pending tool-approval prompt.
    fn respond_approval(
        &self,
        device: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<(), AgentError>;
    /// Out-of-band events (approvals, late replies, errors).
    fn events(&self) -> broadcast::Receiver<AgentEvent>;
}

/// Real backend: ZeroClaw gateway over WebSocket.
pub struct ZeroClawAgent {
    client: ZeroClaw,
    cfg: Config,
}

impl ZeroClawAgent {
    pub fn new(cfg: &Config) -> Self {
        let gw = GatewayConfig {
            base: cfg.zeroclaw.gateway.clone(),
            token: cfg.zeroclaw.token.clone(),
            agent_alias: cfg.zeroclaw.agent_alias.clone(),
        };
        Self {
            client: ZeroClaw::new(gw),
            cfg: cfg.clone(),
        }
    }
}

impl AgentBackend for ZeroClawAgent {
    fn ask(
        &self,
        device: &str,
        text: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<TurnOutcome, AgentError>> {
        let client = self.client.clone();
        let device = device.to_string();
        let text = self.cfg.voice_text(text);
        Box::pin(async move { client.ask(&device, &text, timeout).await })
    }

    fn respond_approval(
        &self,
        device: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<(), AgentError> {
        self.client.respond_approval(device, request_id, decision)
    }

    fn events(&self) -> broadcast::Receiver<AgentEvent> {
        self.client.subscribe()
    }
}

/// Development backend: no zeroclaw needed; echoes the utterance.
pub struct MockAgent {
    events: broadcast::Sender<AgentEvent>,
}

impl MockAgent {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(16);
        Self { events }
    }
}

impl Default for MockAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBackend for MockAgent {
    fn ask(
        &self,
        _device: &str,
        text: &str,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<TurnOutcome, AgentError>> {
        let text = text.to_string();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(TurnOutcome {
                full: Some(format!(
                    "Mock agent here. You said: {text} (set zeroclaw.mock = false to use the real agent)"
                )),
                partial: String::new(),
                timed_out: false,
                awaiting_approval: None,
                cost_usd: None,
            })
        })
    }

    fn respond_approval(
        &self,
        _device: &str,
        _request_id: &str,
        _decision: Decision,
    ) -> Result<(), AgentError> {
        Ok(())
    }

    fn events(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }
}

/// Background task that folds gateway events into app state (pending
/// approvals, cached late replies).
pub fn spawn_event_collector(
    agent: &dyn AgentBackend,
    on_event: impl Fn(AgentEvent) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let mut rx = agent.events();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => on_event(ev),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "agent event lag");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
