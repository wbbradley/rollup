use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub repos: Vec<RepoRef>,
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
    Ok(Config { repos })
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
}
