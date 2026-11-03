//! The brain bridge: one voice turn or play-resolution against the Aria
//! Mosaic app (the `ask` / `resolve-play` workflows over REST, loopback).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;

use crate::config::Config;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("cannot reach the brain: {0}")]
    Connect(String),
    #[error("brain error: {0}")]
    Other(String),
}

/// One spoken answer for the device.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub text: Option<String>,
}

/// The brain's decision for a play request. `action: "none"` = no match.
#[derive(Debug, Clone, Default)]
pub struct ResolveOutcome {
    pub action: String,
    pub slug: String,
    pub reason: String,
}

impl ResolveOutcome {
    pub fn is_none(&self) -> bool {
        self.action == "none" || self.slug.is_empty()
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the Alexa router can ask the brain to do.
pub trait AgentBackend: Send + Sync {
    /// One conversational turn; `timeout` is the voice response budget.
    fn ask(
        &self,
        device: &str,
        text: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<TurnOutcome, AgentError>>;
    /// Resolve a play request against the media collection.
    fn resolve_play(
        &self,
        query: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<ResolveOutcome, AgentError>>;
}

/// Real backend: the Aria Mosaic app over its REST API (loopback in-pod).
pub struct BrainAgent {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl BrainAgent {
    pub fn new(cfg: &Config) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(cfg.brain.timeout())
                .build()
                .expect("brain http client"),
            base: cfg.brain.base_url.trim_end_matches('/').to_string(),
            token: cfg.brain.token.clone().unwrap_or_default(),
        }
    }
}

impl AgentBackend for BrainAgent {
    fn ask(
        &self,
        device: &str,
        text: &str,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<TurnOutcome, AgentError>> {
        let device = device.to_string();
        let text = text.to_string();
        let http = self.http.clone();
        let base = self.base.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let mut req = http
                .post(format!("{base}/api/workflows/ask/run"))
                .json(&serde_json::json!({ "utterance": text, "device": device }));
            if !token.is_empty() {
                req = req.bearer_auth(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| AgentError::Connect(e.to_string()))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(AgentError::Other(format!("{status}: {body}")));
            }
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| AgentError::Other(e.to_string()))?;
            let text = v
                .pointer("/answer/output")
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .or_else(|| v.get("answer").and_then(|a| a.as_str()).map(str::to_string));
            Ok(TurnOutcome { text })
        })
    }

    fn resolve_play(
        &self,
        query: &str,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<ResolveOutcome, AgentError>> {
        let query = query.to_string();
        let http = self.http.clone();
        let base = self.base.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let mut req = http
                .post(format!("{base}/api/workflows/resolve-play/run"))
                .json(&serde_json::json!({ "query": query }));
            if !token.is_empty() {
                req = req.bearer_auth(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| AgentError::Connect(e.to_string()))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(AgentError::Other(format!("{status}: {body}")));
            }
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| AgentError::Other(e.to_string()))?;
            Ok(ResolveOutcome {
                action: v
                    .get("action")
                    .and_then(|x| x.as_str())
                    .unwrap_or("none")
                    .to_string(),
                slug: v
                    .get("slug")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                reason: v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
    }
}

/// Development backend: no brain needed; canned answers.
pub struct MockAgent;

impl MockAgent {
    pub fn new() -> Self {
        Self
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
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(TurnOutcome {
                text: Some(format!(
                    "Mock brain here. You said: {text} (set brain.mock = false to use the real brain)"
                )),
            })
        })
    }

    fn resolve_play(
        &self,
        _query: &str,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<ResolveOutcome, AgentError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(ResolveOutcome {
                action: "none".into(),
                slug: String::new(),
                reason: "mock brain does not resolve".into(),
            })
        })
    }
}
