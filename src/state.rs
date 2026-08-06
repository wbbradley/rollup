use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Pr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: String,
    pub pr: u64,
}

pub type BackburnerSet = BTreeSet<PrRef>;

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    backburner: BackburnerSet,
}

pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("rollup.yaml")
}

pub fn load() -> Result<BackburnerSet> {
    load_from(&state_path())
}

fn load_from(path: &Path) -> Result<BackburnerSet> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let state: StateFile =
        serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(state.backburner)
}

pub fn save(backburner: &BackburnerSet) -> Result<()> {
    save_to(&state_path(), backburner)
}

fn save_to(path: &Path, backburner: &BackburnerSet) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_yaml_ng::to_string(&StateFile {
        backburner: backburner.clone(),
    })?;
    let temp = path.with_extension("yaml.tmp");
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Drop entries that were not present in the latest authored-PR fetch.
pub fn scrub(backburner: &mut BackburnerSet, authored: &[Pr]) -> bool {
    let before = backburner.len();
    backburner.retain(|entry| {
        authored
            .iter()
            .any(|pr| pr.repo == entry.repo && pr.number == entry.pr)
    });
    backburner.len() != before
}

pub fn contains(backburner: &BackburnerSet, pr: &Pr) -> bool {
    backburner.contains(&PrRef {
        repo: pr.repo.clone(),
        pr: pr.number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_scrub() {
        let dir = std::env::temp_dir().join(format!("rollup-state-test-{}", std::process::id()));
        let path = dir.join("state.yaml");
        let mut set = BackburnerSet::from([
            PrRef {
                repo: "o/r".into(),
                pr: 1,
            },
            PrRef {
                repo: "o/r".into(),
                pr: 2,
            },
        ]);
        save_to(&path, &set).unwrap();
        assert_eq!(load_from(&path).unwrap(), set);

        assert!(scrub(&mut set, &[]));
        assert!(set.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
