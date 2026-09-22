//! Configuration: TOML file + environment overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub library: LibraryConfig,
    pub zeroclaw: ZeroclawConfig,
    pub radio: RadioConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// e.g. "0.0.0.0:8080".
    pub bind: String,
    /// Public base URL (https) that Amazon and Echo devices reach. Used to
    /// build /stream and /art URLs.
    pub public_base_url: String,
    /// Alexa skill client id; when set, requests from other skills are
    /// rejected.
    pub client_id: Option<String>,
    /// Native TLS (otherwise run behind Caddy/nginx).
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Where state.json lives.
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryConfig {
    pub paths: Vec<PathBuf>,
    /// Rescan the library every N seconds (0 = never).
    pub rescan_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZeroclawConfig {
    /// Gateway base URL, e.g. ws://127.0.0.1:3000
    pub gateway: String,
    /// Bearer token; defaults to $EUGEIS_ZC_TOKEN.
    pub token: Option<String>,
    /// ZeroClaw agent alias used for voice turns.
    pub agent_alias: String,
    /// Wall-clock budget for one voice turn before we speak the partial
    /// (Alexa requires a response within ~8s).
    pub turn_timeout_ms: u64,
    /// Prepended to every utterance sent to the agent.
    pub voice_note: String,
    /// Development mode: don't talk to zeroclaw, echo back.
    pub mock: bool,
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
            bind: "0.0.0.0:8080".into(),
            public_base_url: String::new(),
            client_id: None,
            tls_cert: None,
            tls_key: None,
            data_dir: None,
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            paths: vec![PathBuf::from("/srv/music")],
            rescan_secs: 3600,
        }
    }
}

impl Default for ZeroclawConfig {
    fn default() -> Self {
        Self {
            gateway: "ws://127.0.0.1:3000".into(),
            token: None,
            agent_alias: "voice".into(),
            turn_timeout_ms: 7500,
            voice_note: DEFAULT_VOICE_NOTE.into(),
            mock: false,
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

pub const DEFAULT_VOICE_NOTE: &str = "You are talking to the user through an Alexa device. \
Keep answers short (one to three sentences), conversational, and free of markdown, \
lists, and code — they will be spoken aloud by a text-to-speech engine.";

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

    pub fn turn_timeout(&self) -> Duration {
        Duration::from_millis(self.zeroclaw.turn_timeout_ms.clamp(1000, 7900))
    }

    /// Public base URL with trailing slash stripped.
    pub fn base_url(&self) -> &str {
        self.server.public_base_url.trim_end_matches('/')
    }

    pub fn voice_text(&self, user_text: &str) -> String {
        format!("{}\n\nUser: {user_text}", self.zeroclaw.voice_note)
    }
}

pub fn default_data_dir() -> PathBuf {
    std::env::var_os("EUGEIS_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".eugeis")))
        .unwrap_or_else(|| PathBuf::from(".eugeis"))
}

/// Load config from an explicit path, or the default location, or built-in
/// defaults when no file exists. Environment overrides:
///   EUGEIS_CONFIG, EUGEIS_ZC_TOKEN, EUGEIS_GATEWAY, EUGEIS_PUBLIC_URL
pub fn load(explicit: Option<&Path>) -> std::result::Result<Config, ConfigError> {
    let path = explicit
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("EUGEIS_CONFIG").map(PathBuf::from))
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
    if let Ok(t) = std::env::var("EUGEIS_ZC_TOKEN") {
        if !t.is_empty() {
            cfg.zeroclaw.token = Some(t);
        }
    }
    if let Ok(g) = std::env::var("EUGEIS_GATEWAY") {
        if !g.is_empty() {
            cfg.zeroclaw.gateway = g;
        }
    }
    if let Ok(u) = std::env::var("EUGEIS_PUBLIC_URL") {
        if !u.is_empty() {
            cfg.server.public_base_url = u;
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
        assert_eq!(c.zeroclaw.agent_alias, "voice");
        assert_eq!(c.turn_timeout().as_millis(), 7500);
        assert!(c.zeroclaw.voice_note.contains("spoken aloud"));
    }

    #[test]
    fn turn_timeout_clamps() {
        let mut c = Config::default();
        c.zeroclaw.turn_timeout_ms = 50;
        assert_eq!(c.turn_timeout().as_millis(), 1000);
        c.zeroclaw.turn_timeout_ms = 99_999;
        assert_eq!(c.turn_timeout().as_millis(), 7900);
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:9000"
            public_base_url = "https://eugeis.example/"
            client_id = "amzn1.ask.skill.x"
            [library]
            paths = ["/music"]
            rescan_secs = 60
            [zeroclaw]
            gateway = "ws://127.0.0.1:3000"
            agent_alias = "home"
            turn_timeout_ms = 6000
            [radio.stations]
            "rock fm" = "https://r.example/rock.mp3"
            [logging]
            level = "debug"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.server.bind, "127.0.0.1:9000");
        assert_eq!(c.base_url(), "https://eugeis.example");
        assert_eq!(c.library.paths, vec![PathBuf::from("/music")]);
        assert_eq!(c.zeroclaw.agent_alias, "home");
        assert_eq!(
            c.radio.stations.get("rock fm").unwrap(),
            "https://r.example/rock.mp3"
        );
        assert_eq!(c.logging.level, "debug");
    }

    #[test]
    fn rejects_unknown_keys() {
        let r = toml::from_str::<Config>("bogus = 1");
        assert!(r.is_err());
    }
}
