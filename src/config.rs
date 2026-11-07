//! Settings, read from environment variables at start-up.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 3000;
/// Where progress and settings are saved. Relative to the folder the tool is
/// started from; run.sh starts it from the project folder, so everything
/// stays inside the project.
pub const DEFAULT_STATE_DIR: &str = ".state";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("IMMICH_URL is not set (example: http://100.x.x.x:2283, Immich's Tailscale address)")]
    MissingUrl,
    #[error("IMMICH_URL must start with http:// or https:// (got {0:?})")]
    BadUrl(String),
    #[error(
        "IMMICH_URL is plain http:// to {0}, which is not a Tailscale address, so the API key would \
         travel unencrypted. Use Immich's Tailscale address (http://100.x.x.x:2283) or an https:// address"
    )]
    Unencrypted(String),
    #[error("IMMICH_API_KEY is not set or empty (run.sh reads it from 1Password, using IMMICH_API_KEY_REF in .env)")]
    MissingKey,
    #[error("PHOTO_CLEAROUT_PORT must be a number between 1024 and 65535 (got {0:?})")]
    BadPort(String),
}

pub struct Config {
    pub immich_url: String,
    pub api_key: String,
    pub port: u16,
    pub state_dir: PathBuf,
    pub open_browser: bool,
}

// Hand-written so the API key can never end up in a log or error message.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("immich_url", &self.immich_url)
            .field("api_key", &"<redacted>")
            .field("port", &self.port)
            .field("state_dir", &self.state_dir)
            .field("open_browser", &self.open_browser)
            .finish()
    }
}

impl Config {
    /// `get` looks up one variable. Passing it in (instead of reading the
    /// real environment directly) lets the tests supply fake values.
    ///
    /// # Errors
    /// If a setting is missing or invalid, including plain `http://` to an
    /// address that isn't on Tailscale.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let immich_url = get("IMMICH_URL")
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or(ConfigError::MissingUrl)?;
        let parsed = reqwest::Url::parse(&immich_url).map_err(|_| ConfigError::BadUrl(immich_url.clone()))?;
        match parsed.scheme() {
            "https" => {}
            "http" => {
                let host = parsed.host_str().unwrap_or_default().to_string();
                if !travels_encrypted_over_http(&parsed) {
                    return Err(ConfigError::Unencrypted(host));
                }
            }
            _ => return Err(ConfigError::BadUrl(immich_url)),
        }

        let api_key = get("IMMICH_API_KEY")
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .ok_or(ConfigError::MissingKey)?;

        let port = match get("PHOTO_CLEAROUT_PORT") {
            None => DEFAULT_PORT,
            Some(p) => p
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p >= 1024)
                .ok_or(ConfigError::BadPort(p))?,
        };

        let state_dir = match get("PHOTO_CLEAROUT_STATE_DIR") {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
            _ => PathBuf::from(DEFAULT_STATE_DIR),
        };

        // Any value turns the browser off, except an empty one, 0, false or no.
        let open_browser = get("PHOTO_CLEAROUT_NO_BROWSER")
            .map(|v| v.trim().to_ascii_lowercase())
            .is_none_or(|v| matches!(v.as_str(), "" | "0" | "false" | "no"));

        Ok(Config {
            immich_url,
            api_key,
            port,
            state_dir,
            open_browser,
        })
    }
}

/// Plain `http://` is only allowed where something else encrypts the
/// connection: a Tailscale address (the `WireGuard` tunnel encrypts it), or
/// this Mac itself (e.g. an SSH tunnel, which is encrypted).
fn travels_encrypted_over_http(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    // IPv6 addresses come in brackets: [fd7a:115c:a1e0::1]
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        is_tailscale_ip(ip) || ip.is_loopback()
    } else {
        let name = host.to_ascii_lowercase();
        name == "localhost" || name.ends_with(".ts.net")
    }
}

/// Plain `http://` to anything but this Mac itself must be checked to really
/// go through Tailscale's tunnel (see `tunnel`). `https://` is encrypted anyway.
#[must_use]
pub fn needs_tunnel(url: &reqwest::Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    let host = url.host_str().unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) || host.eq_ignore_ascii_case("localhost");
    !loopback
}

/// Tailscale gives devices addresses in `100.64.0.0/10` and `fd7a:115c:a1e0::/48`.
#[must_use]
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..=127).contains(&b)
        }
        IpAddr::V6(v6) => {
            let tailnet = Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0).segments();
            v6.segments()[..3] == tailnet[..3]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg = Config::from_env(env(&[
            ("IMMICH_URL", "http://100.64.0.10:2283/"),
            ("IMMICH_API_KEY", " secret "),
        ]))
        .unwrap();
        assert_eq!(cfg.immich_url, "http://100.64.0.10:2283");
        assert_eq!(cfg.api_key, "secret");
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(cfg.open_browser);
        assert_eq!(cfg.state_dir, PathBuf::from(".state"));
    }

    #[test]
    fn missing_url_is_an_error() {
        let err = Config::from_env(env(&[("IMMICH_API_KEY", "k")])).unwrap_err();
        assert_eq!(err, ConfigError::MissingUrl);
    }

    #[test]
    fn url_without_scheme_is_rejected() {
        let err = Config::from_env(env(&[
            ("IMMICH_URL", "100.64.0.10:2283"),
            ("IMMICH_API_KEY", "k"),
        ]))
        .unwrap_err();
        assert!(matches!(err, ConfigError::BadUrl(_)));
    }

    #[test]
    fn empty_key_is_an_error() {
        let err = Config::from_env(env(&[
            ("IMMICH_URL", "http://100.64.0.1:2283"),
            ("IMMICH_API_KEY", "   "),
        ]))
        .unwrap_err();
        assert_eq!(err, ConfigError::MissingKey);
    }

    #[test]
    fn low_or_invalid_port_is_rejected() {
        for bad in ["80", "abc", "70000"] {
            let err = Config::from_env(env(&[
                ("IMMICH_URL", "http://100.64.0.1:2283"),
                ("IMMICH_API_KEY", "k"),
                ("PHOTO_CLEAROUT_PORT", bad),
            ]))
            .unwrap_err();
            assert!(matches!(err, ConfigError::BadPort(_)), "port {bad}");
        }
    }

    #[test]
    fn overrides_are_used() {
        let cfg = Config::from_env(env(&[
            ("IMMICH_URL", "https://photos.example"),
            ("IMMICH_API_KEY", "k"),
            ("PHOTO_CLEAROUT_PORT", "4000"),
            ("PHOTO_CLEAROUT_STATE_DIR", "/tmp/state"),
            ("PHOTO_CLEAROUT_NO_BROWSER", "1"),
        ]))
        .unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.state_dir, PathBuf::from("/tmp/state"));
        assert!(!cfg.open_browser);
    }

    fn url_check(url: &str) -> Result<Config, ConfigError> {
        Config::from_env(env(&[("IMMICH_URL", url), ("IMMICH_API_KEY", "k")]))
    }

    #[test]
    fn plain_http_is_only_allowed_through_tailscale_or_this_mac() {
        for ok in [
            "http://100.100.10.20:2283",
            "http://100.64.0.1:2283",
            "http://100.127.255.254:2283",
            "http://immich.tail1234.ts.net:2283",
            "http://[fd7a:115c:a1e0::1]:2283",
            "http://localhost:2283",
            "http://127.0.0.1:2283",
            "https://192.168.1.10:2283",
            "https://photos.example",
        ] {
            assert!(url_check(ok).is_ok(), "{ok} should be allowed");
        }
        for bad in [
            "http://192.168.1.10:2283",
            "http://100.63.255.255:2283",
            "http://100.128.0.1:2283",
            "http://photos.example",
            "http://immich.ts.net.example.com",
            "http://[fd00::1]:2283",
        ] {
            assert!(matches!(url_check(bad), Err(ConfigError::Unencrypted(_))), "{bad} should be refused");
        }
    }

    #[test]
    fn only_plain_http_to_another_machine_needs_the_tunnel() {
        let needs = |u: &str| needs_tunnel(&reqwest::Url::parse(u).unwrap());
        assert!(needs("http://100.100.10.20:2283"));
        assert!(needs("http://immich.tail1234.ts.net:2283"));
        assert!(!needs("https://100.100.10.20:2283"));
        assert!(!needs("http://localhost:2283"));
        assert!(!needs("http://127.0.0.1:2283"));
    }

    #[test]
    fn no_browser_setting_reads_naturally() {
        let open = |v: &str| {
            Config::from_env(env(&[
                ("IMMICH_URL", "http://100.64.0.1:2283"),
                ("IMMICH_API_KEY", "k"),
                ("PHOTO_CLEAROUT_NO_BROWSER", v),
            ]))
            .unwrap()
            .open_browser
        };
        assert!(!open("1"));
        assert!(!open("yes"));
        assert!(open("0"));
        assert!(open("false"));
        assert!(open(""));
    }

    #[test]
    fn debug_output_never_shows_the_key() {
        let cfg = Config::from_env(env(&[
            ("IMMICH_URL", "http://100.64.0.1:2283"),
            ("IMMICH_API_KEY", "super-secret-key"),
        ]))
        .unwrap();
        let printed = format!("{cfg:?}");
        assert!(!printed.contains("super-secret-key"));
        assert!(printed.contains("<redacted>"));
    }
}
