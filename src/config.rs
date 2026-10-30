//! `%LOCALAPPDATA%\Wolf2 Tray\config.toml` — local settings, never credentials.
//! Every key is optional; a missing file is the common case.

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// The site. Default https://wolf2.net (or `AVSTREAM_BASE_URL`).
    pub base_url: Option<String>,
    /// Tenant id for `/api/recordings?tenant=`. Default: the first tenant the
    /// membership lists.
    pub tenant: Option<String>,
    /// The Tacview exporter's real-time telemetry port. Default 42674.
    pub port: Option<u16>,
    /// Pick the ownship by `Pilot=` label instead of by flight data.
    pub pilot_name: Option<String>,
}

pub const DEFAULT_PORT: u16 = 42674;

impl Config {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(crate::auth::base_url)
    }

    pub fn pilot_name(&self) -> Option<String> {
        self.pilot_name.clone().filter(|s| !s.trim().is_empty())
    }
}

/// `%LOCALAPPDATA%\Wolf2 Tray` (or `~/.wolf2-tray` elsewhere), created on demand.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("Wolf2 Tray"))
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".wolf2-tray"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&base);
    base
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    data_dir().join("tray.log")
}

pub fn spool_dir() -> PathBuf {
    data_dir().join("spool")
}

pub fn load() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config.toml ignored: {e}");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults() {
        let c: Config = toml::from_str("port = 42676\npilot_name = \"Airboss\"\n").unwrap();
        assert_eq!(c.port(), 42676);
        assert_eq!(c.pilot_name().as_deref(), Some("Airboss"));
        assert_eq!(c.tenant, None);
        let d: Config = toml::from_str("").unwrap();
        assert_eq!(d.port(), DEFAULT_PORT);
        assert!(d.base_url().starts_with("http"));
        let e: Config =
            toml::from_str("base_url = \"https://example.test/\"\npilot_name = \" \"").unwrap();
        assert_eq!(e.base_url(), "https://example.test");
        assert_eq!(e.pilot_name(), None);
    }
}
