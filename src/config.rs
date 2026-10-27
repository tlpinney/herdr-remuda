//! remuda's user config: `config.toml` in the plugin config directory.
//!
//! ```toml
//! # How this machine appears on panes it mirrors (default: the host name).
//! name = "travis-air"
//! # The herd board's seat grid, ROWSxCOLS (default: what fits the screen).
//! grid = "2x4"
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use crate::herdr;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub grid: Option<String>,
}

impl Config {
    /// Loads the config; a missing file means defaults, a broken one is an error.
    pub fn load() -> Result<Config, String> {
        let path = config_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    /// The name other machines see this one as: the configured `name`, or the
    /// short host name.
    pub fn machine_name(&self) -> String {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(short_hostname)
            .unwrap_or_else(|| "remuda".into())
    }
}

/// `$HERDR_PLUGIN_CONFIG_DIR`, or the same directory found the way herdr finds
/// it: `$XDG_CONFIG_HOME/herdr/plugins/config/<id>` or
/// `~/.config/herdr/plugins/config/<id>`.
fn config_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            config.join("herdr/plugins/config").join(herdr::plugin_id())
        })
}

/// The machine's node name without its domain, read the way herdr reads it
/// (`gethostname`), so it does not depend on which `hostname` is on PATH.
#[cfg(unix)]
fn short_hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]);
    let short = name.split('.').next().unwrap_or_default().trim();
    (!short.is_empty()).then(|| short.to_string())
}

#[cfg(not(unix))]
fn short_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_name_wins_and_blank_falls_back() {
        let named: Config = toml::from_str(r#"name = " travis-air ""#).unwrap();
        assert_eq!(named.machine_name(), "travis-air");
        let blank: Config = toml::from_str(r#"name = "  ""#).unwrap();
        assert!(!blank.machine_name().is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("nmae = \"typo\"").is_err());
    }
}
