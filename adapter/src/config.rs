//! Configuration: TOML file + environment overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub collections: CollectionsConfig,
    pub brain: BrainConfig,
    pub scan: ScanConfig,
    pub radio: RadioConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// e.g. "0.0.0.0:8090".
    pub bind: String,
    /// Public base URL (https) that Amazon and Echo devices reach. Used to
    /// build /stream and /art URLs.
    pub public_base_url: String,
    /// Alexa skill client id; when set, requests from other skills are
    /// rejected.
    pub client_id: Option<String>,
    /// Native TLS (otherwise run behind the ingress).
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Where state.json lives.
    pub data_dir: Option<PathBuf>,
    /// Web-app login token for the `/dl/*` API (env ARIA_WEB_TOKEN).
    /// None = open access (dev only).
    pub web_token: Option<String>,
}

/// The user's media collections: one or more folders, each with a kind.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollectionsConfig {
    /// Re-scan the collections every N seconds (0 = never). A changed scan
    /// republishes the metadata JSONL and asks the brain to reindex.
    pub rescan_secs: u64,
    pub items: Vec<CollectionEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionEntry {
    /// Collection name (folder-level; appears in the metadata records).
    pub name: String,
    /// `music` | `audiobook` | `sermon`.
    pub kind: String,
    pub path: PathBuf,
}

impl CollectionEntry {
    pub fn collection_kind(&self) -> Result<aria_audio::CollectionKind, String> {
        match self.kind.as_str() {
            "music" => Ok(aria_audio::CollectionKind::Music),
            "audiobook" => Ok(aria_audio::CollectionKind::Audiobook),
            "sermon" => Ok(aria_audio::CollectionKind::Sermon),
            other => Err(format!(
                "collection `{}`: kind must be music | audiobook | sermon (got `{other}`)",
                self.name
            )),
        }
    }

    pub fn item(&self) -> Result<aria_audio::CollectionItem, String> {
        Ok(aria_audio::CollectionItem {
            name: self.name.clone(),
            kind: self.collection_kind()?,
            path: self.path.clone(),
        })
    }
}

/// The Aria Mosaic app (the brain) — loopback REST in the same pod.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrainConfig {
    /// e.g. http://127.0.0.1:3000 (the app container in the same pod).
    pub base_url: String,
    /// Bearer token of the `adapter` service account; defaults to
    /// $ARIA_BRAIN_TOKEN.
    pub token: Option<String>,
    /// Wall-clock budget for one voice turn before we give up and speak the
    /// fallback (Alexa requires a response within ~8 s).
    pub turn_timeout_ms: u64,
    /// Development mode: no brain, canned answers.
    pub mock: bool,
}

impl BrainConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.turn_timeout_ms.clamp(1000, 7900))
    }
}

/// The library scanner -> metadata JSONL -> brain reindex pipeline.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Where media.jsonl.gz is written (the brain ingests
    /// `knowledge/media` under its knowledge base). Default:
    /// {data_dir}/knowledge/media/media.jsonl.gz.
    pub out_path: Option<PathBuf>,
    /// POST the brain's /api/ops/reindex after a changed scan.
    pub reindex_after_scan: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RadioConfig {
    /// Station name (lowercase) -> stream URL (MP3/AAC).
    pub stations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8090".into(),
            public_base_url: String::new(),
            client_id: None,
            tls_cert: None,
            tls_key: None,
            data_dir: None,
            web_token: None,
        }
    }
}

impl Default for CollectionsConfig {
    fn default() -> Self {
        Self {
            rescan_secs: 3600,
            items: vec![CollectionEntry {
                name: "music".into(),
                kind: "music".into(),
                path: PathBuf::from("/media"),
            }],
        }
    }
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:3000".into(),
            token: None,
            turn_timeout_ms: 7500,
            mock: false,
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            out_path: None,
            reindex_after_scan: true,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

impl Config {
    pub fn data_dir(&self) -> PathBuf {
        self.server
            .data_dir
            .clone()
            .unwrap_or_else(default_data_dir)
    }

    pub fn state_path(&self) -> PathBuf {
        self.data_dir().join("state.json")
    }

    /// Where the scan publishes media.jsonl.gz.
    pub fn scan_out_path(&self) -> PathBuf {
        self.scan
            .out_path
            .clone()
            .unwrap_or_else(|| self.data_dir().join("knowledge/media/media.jsonl.gz"))
    }

    pub fn turn_timeout(&self) -> Duration {
        self.brain.timeout()
    }

    /// Public base URL with trailing slash stripped.
    pub fn base_url(&self) -> &str {
        self.server.public_base_url.trim_end_matches('/')
    }

    /// The collection items, validated.
    pub fn collection_items(&self) -> Result<Vec<aria_audio::CollectionItem>, String> {
        self.collections
            .items
            .iter()
            .map(|e| e.item())
            .collect::<Result<Vec<_>, _>>()
    }

    /// The folder of the named collection (case-insensitive) — downloads
    /// land inside it.
    pub fn collection_path(&self, name: &str) -> Option<PathBuf> {
        self.collections
            .items
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(name))
            .map(|i| i.path.clone())
    }
}

pub fn default_data_dir() -> PathBuf {
    std::env::var_os("ARIA_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aria")))
        .unwrap_or_else(|| PathBuf::from(".aria"))
}

/// Load config from an explicit path, or the default location, or built-in
/// defaults when no file exists. Environment overrides:
///   ARIA_CONFIG, ARIA_DATA_DIR, ARIA_BRAIN_TOKEN, ARIA_BRAIN_URL,
///   ARIA_PUBLIC_URL
pub fn load(explicit: Option<&Path>) -> std::result::Result<Config, ConfigError> {
    let path = explicit
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ARIA_CONFIG").map(PathBuf::from))
        .unwrap_or_else(default_config_path);
    let cfg = if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::Read {
            path: path.clone(),
            source: e,
        })?;
        toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?
    } else {
        tracing::info!(path = %path.display(), "no config file found; using built-in defaults");
        Config::default()
    };
    let cfg = apply_env(cfg);
    Ok(cfg)
}

pub fn default_config_path() -> PathBuf {
    default_data_dir().join("config.toml")
}

fn apply_env(mut cfg: Config) -> Config {
    if let Ok(t) = std::env::var("ARIA_BRAIN_TOKEN") {
        if !t.is_empty() {
            cfg.brain.token = Some(t);
        }
    }
    if let Ok(u) = std::env::var("ARIA_BRAIN_URL") {
        if !u.is_empty() {
            cfg.brain.base_url = u;
        }
    }
    if let Ok(u) = std::env::var("ARIA_PUBLIC_URL") {
        if !u.is_empty() {
            cfg.server.public_base_url = u;
        }
    }
    if let Ok(c) = std::env::var("ARIA_CLIENT_ID") {
        if !c.is_empty() {
            cfg.server.client_id = Some(c);
        }
    }
    if let Ok(t) = std::env::var("ARIA_WEB_TOKEN") {
        if !t.is_empty() {
            cfg.server.web_token = Some(t);
        }
    }
    cfg
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

impl ConfigError {
    pub fn to_exit_message(&self) -> String {
        format!("{self}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.brain.timeout().as_millis(), 7500);
        assert!(c.collections.items[0].name == "music");
        assert!(c.scan.reindex_after_scan);
    }

    #[test]
    fn turn_timeout_clamps() {
        let mut c = Config::default();
        c.brain.turn_timeout_ms = 50;
        assert_eq!(c.turn_timeout().as_millis(), 1000);
        c.brain.turn_timeout_ms = 99_999;
        assert_eq!(c.turn_timeout().as_millis(), 7900);
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:9000"
            public_base_url = "https://aria.eisler-systems.de/"
            client_id = "amzn1.ask.skill.x"
            [collections]
            rescan_secs = 600
            [[collections.items]]
            name = "rock"
            kind = "music"
            path = "/media/rock"
            [[collections.items]]
            name = "bible-audio"
            kind = "audiobook"
            path = "/media/bible-audio"
            [[collections.items]]
            name = "sermons"
            kind = "sermon"
            path = "/media/sermons-audio"
            [brain]
            base_url = "http://127.0.0.1:3000"
            turn_timeout_ms = 6000
            [scan]
            out_path = "/data/knowledge/media/media.jsonl.gz"
            [radio.stations]
            "rock fm" = "https://r.example/rock.mp3"
            [logging]
            level = "debug"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.server.bind, "127.0.0.1:9000");
        assert_eq!(c.base_url(), "https://aria.eisler-systems.de");
        assert_eq!(c.collections.items.len(), 3);
        let items = c.collection_items().unwrap();
        assert_eq!(items[1].kind, aria_audio::CollectionKind::Audiobook);
        assert_eq!(c.brain.turn_timeout_ms, 6000);
        assert_eq!(
            c.scan_out_path(),
            PathBuf::from("/data/knowledge/media/media.jsonl.gz")
        );
        assert_eq!(
            c.radio.stations.get("rock fm").unwrap(),
            "https://r.example/rock.mp3"
        );
        assert_eq!(c.logging.level, "debug");
    }

    #[test]
    fn rejects_bad_collection_kind() {
        let e = CollectionEntry {
            name: "x".into(),
            kind: "video".into(),
            path: PathBuf::from("/x"),
        };
        assert!(e.item().is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        let r = toml::from_str::<Config>("bogus = 1");
        assert!(r.is_err());
    }
}
