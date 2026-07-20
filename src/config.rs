use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// GitHub reload cadence used when the config omits `refresh_interval_secs`.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Lower bound the resolved interval is floored to, so a tiny (or zero)
/// configured value cannot hammer the GitHub API.
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Config {
    pub repos: Vec<RepoRef>,
    /// How often the background timer reloads from GitHub. Defaults to
    /// [`DEFAULT_REFRESH_INTERVAL`] and is floored at [`MIN_REFRESH_INTERVAL`].
    pub refresh_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default)]
    refresh_interval_secs: Option<u64>,
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("rollup").join("config.yaml")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading {}", path.display())));
        }
    };
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = serde_yaml_ng::from_str(text)?;
    let mut repos = Vec::with_capacity(raw.repos.len());
    for entry in raw.repos {
        let trimmed = entry.trim();
        let (owner, name) = trimmed
            .split_once('/')
            .ok_or_else(|| anyhow!("repo '{trimmed}' missing owner/name form"))?;
        if owner.is_empty() || name.is_empty() {
            return Err(anyhow!("repo '{trimmed}' has empty owner or name"));
        }
        repos.push(RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        });
    }
    Ok(Config {
        repos,
        refresh_interval: resolve_refresh_interval(raw.refresh_interval_secs),
    })
}

/// Resolve the configured refresh interval: unset falls back to the default,
/// and any explicit value is floored at [`MIN_REFRESH_INTERVAL`] so a tiny or
/// zero setting can't hammer the GitHub API.
fn resolve_refresh_interval(secs: Option<u64>) -> Duration {
    match secs {
        None => DEFAULT_REFRESH_INTERVAL,
        Some(secs) => Duration::from_secs(secs).max(MIN_REFRESH_INTERVAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_yaml() {
        let cfg = parse("").unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn parse_missing_repos_key() {
        let cfg = parse("other: []\n").unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn parse_valid_list() {
        let cfg = parse("repos:\n  - MystenLabs/walrus\n  - MystenLabs/sui\n").unwrap();
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].owner, "MystenLabs");
        assert_eq!(cfg.repos[0].name, "walrus");
        assert_eq!(cfg.repos[0].full(), "MystenLabs/walrus");
        assert_eq!(cfg.repos[1].full(), "MystenLabs/sui");
    }

    #[test]
    fn parse_rejects_missing_slash() {
        let err = parse("repos:\n  - justname\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("justname"), "msg={msg}");
    }

    #[test]
    fn parse_rejects_empty_owner() {
        let err = parse("repos:\n  - /name\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty owner or name"), "msg={msg}");
    }

    #[test]
    fn parse_rejects_empty_name() {
        let err = parse("repos:\n  - owner/\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty owner or name"), "msg={msg}");
    }

    #[test]
    fn default_config_uses_default_refresh_interval() {
        assert_eq!(Config::default().refresh_interval, DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn parse_omitted_refresh_interval_uses_default() {
        let cfg = parse("repos:\n  - o/r\n").unwrap();
        assert_eq!(cfg.refresh_interval, DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn parse_explicit_refresh_interval() {
        let cfg = parse("refresh_interval_secs: 600\n").unwrap();
        assert_eq!(cfg.refresh_interval, Duration::from_secs(600));
    }

    #[test]
    fn parse_floors_tiny_refresh_interval() {
        let cfg = parse("refresh_interval_secs: 1\n").unwrap();
        assert_eq!(cfg.refresh_interval, MIN_REFRESH_INTERVAL);
    }

    #[test]
    fn parse_floors_zero_refresh_interval() {
        let cfg = parse("refresh_interval_secs: 0\n").unwrap();
        assert_eq!(cfg.refresh_interval, MIN_REFRESH_INTERVAL);
    }
}
