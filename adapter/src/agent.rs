//! The brain bridge: one voice turn or play-resolution against the Aria
//! Mosaic app (the `ask` / `resolve-play` workflows over REST, loopback).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;
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
    /// Reorg LLM assist: normalized catalog suggestions for the given
    /// ambiguous tracks (the brain's `reorg-suggest` workflow, OpenRouter).
    /// `tracks_json` is a JSON array; the result is the workflow's
    /// `suggestions` output serialized as JSON.
    fn reorg_suggest(
        &self,
        tracks_json: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<String, AgentError>>;
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

/// Cheap bible-detection for the voice router: does the utterance look like
/// a Bible question? (book names in DE + EN, "psalm", "passage", "verse",
/// "kapitel", …). The adapter routes these to the capped `ask-bible` agent
/// workflow (the LLM must choose the passage parameters); everything else
/// goes to the chat-mode `ask` workflow. A false positive just gets a
/// Bible-flavored "no match" answer — a false negative loses the verse text.
pub fn bible_question(utterance: &str) -> bool {
    // Umlaut-fold so the (umlaut-free) term list matches German speech
    // ("römerbrief" -> "romerbrief").
    let u = utterance
        .to_lowercase()
        .replace('ö', "o")
        .replace('ä', "a")
        .replace('ü', "u")
        .replace('ß', "ss");
    const TERMS: &[&str] = &[
        // keywords
        "bibel",
        "bible",
        "psalm",
        "psalter",
        "kapitel",
        "verse",
        "vers ",
        "passage",
        "abschnitt",
        "heiliger geist",
        "gott",
        "christus",
        "jesus",
        "heiliger",
        "bucht",
        "testament",
        // OT book names (DE)
        "genesis",
        "1. mose",
        "2. mose",
        "3. mose",
        "4. mose",
        "5. mose",
        "exodus",
        "levitikus",
        "deuteronomium",
        "josua",
        "richter",
        "ruth",
        "1. samuel",
        "2. samuel",
        "1. könige",
        "2. könige",
        "1. chronik",
        "2. chronik",
        "esra",
        "nehemia",
        "esther",
        "hiob",
        "sprüche",
        "sprueche",
        "prediger",
        "hohelied",
        "jesaja",
        "jeremia",
        "klage",
        "hesekiel",
        "daniel",
        "hosea",
        "joel",
        "amos",
        "obadja",
        "jonas",
        "mika",
        "nahum",
        "habakuk",
        "zefanja",
        "haggai",
        "sacharja",
        "maleachi",
        // NT book names (DE)
        "matthäus",
        "matthaeus",
        "markus",
        "lukas",
        "johannes",
        "apostelgeschichte",
        "romer",
        "1. korinther",
        "2. korinther",
        "galater",
        "epheser",
        "philipper",
        "kolosser",
        "1. thessalonicher",
        "2. thessalonicher",
        "1. timotheus",
        "2. timotheus",
        "titus",
        "philemon",
        "hebräer",
        "hebraeer",
        "jakobus",
        "1. petrus",
        "2. petrus",
        "1. johannes",
        "2. johannes",
        "3. johannes",
        "juda",
        "offenbarung",
        // EN book names (the ones that differ from the DE)
        "genesis",
        "exodus",
        "leviticus",
        "deuteronomy",
        "joshua",
        "judges",
        "ruth",
        "samaritan",
        "samuel",
        "kings",
        "chronicles",
        "ezra",
        "nehemiah",
        "esther",
        "job",
        "proverbs",
        "ecclesiastes",
        "song of songs",
        "isaiah",
        "jeremiah",
        "lamentations",
        "ezekiel",
        "daniel",
        "hosea",
        "joel",
        "amos",
        "obadiah",
        "jonah",
        "micah",
        "nahum",
        "habakkuk",
        "zephaniah",
        "haggai",
        "zechariah",
        "malachi",
        "mattthew",
        "mark",
        "luke",
        "john",
        "acts",
        "romans",
        "corinthians",
        "galatians",
        "ephesians",
        "philippians",
        "colossians",
        "thessalonians",
        "timothy",
        "titus",
        "philemon",
        "hebrews",
        "james",
        "peter",
        "revelation",
    ];
    TERMS.iter().any(|t| u.contains(t))
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
        // Mode routing: Bible questions -> the capped agent workflow (the
        // model chooses the passage params); everything else -> chat mode
        // (deterministic searches + one LLM call, ~3 s).
        let workflow = if bible_question(&text) {
            "ask-bible"
        } else {
            "ask"
        };
        let http = self.http.clone();
        let base = self.base.clone();
        let token = self.token.clone();
        let workflow = workflow.to_string();
        Box::pin(async move {
            let mut req = http
                .post(format!("{base}/api/workflows/{workflow}/run"))
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

    fn reorg_suggest(
        &self,
        tracks_json: &str,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<String, AgentError>> {
        let tracks_json = tracks_json.to_string();
        let base = self.base.clone();
        let token = self.token.clone();
        // The shared client carries the voice-turn budget (~7.5 s) — an LLM
        // batch takes longer, so this call gets its own timeout-bound client
        // (built inside the future: the `?` cannot escape a BoxFuture body).
        Box::pin(async move {
            let Ok(http) = reqwest::Client::builder().timeout(timeout).build() else {
                return Err(AgentError::Connect("http client".into()));
            };
            let mut req = http
                .post(format!("{base}/api/workflows/reorg-suggest/run"))
                .json(&serde_json::json!({ "tracks": tracks_json }));
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
            // The end output (an array, or a JSON string of one).
            let s = v
                .get("suggestions")
                .cloned()
                .unwrap_or(Value::Array(vec![]));
            Ok(match s {
                Value::String(inner) => inner,
                other => other.to_string(),
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

    fn reorg_suggest(
        &self,
        _tracks_json: &str,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<String, AgentError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok("[]".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bible_terms_route_to_bible() {
        for u in [
            "was sagt psalm 23",
            "lies mir aus 1. Mose Kapitel 1 vor",
            "read john 3 verse 16",
            "was steht im Römerbrief",
            "erzähl von der Offenbarung",
            "Psalm 23, Vers 1 bitte",
            "was heißt in Jesaja 40",
            "the book of Proverbs chapter 3",
        ] {
            assert!(bible_question(u), "should be a bible question: {u}");
        }
    }

    #[test]
    fn non_bible_terms_route_to_ask() {
        for u in [
            "welche alben habe ich",
            "spiel bohemian rhapsody ab",
            "was sagte die predigt über gnade",
            "welche predigten habe ich",
            "mach das lautstärker",
        ] {
            assert!(!bible_question(u), "should NOT be a bible question: {u}");
        }
    }

    #[test]
    fn detection_is_case_and_space_insensitive() {
        assert!(bible_question("PSALM 23"));
        assert!(bible_question("Was sagt Psalm 23?"));
    }
}
