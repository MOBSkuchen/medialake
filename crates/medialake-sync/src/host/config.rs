//! `syncs.json`: the syncs a host serves. Edited by `medialake-server sync add|set|remove` (or the app)
//! and re-read by a running host when the file changes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn d_every() -> u64 {
    24
}
fn d_keep_time() -> u32 {
    7
}
fn d_keep_file() -> u32 {
    10
}
fn d_min_interval() -> u64 {
    600
}
fn d_deleted_days() -> u32 {
    30
}

/// One strategy per sync (§6).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum BackupPolicy {
    /// Only the current state exists. Deletes and overwrites are final.
    None,
    /// A snapshot (a list of path, hash, size, mtime) every `every_hours`, the last `keep` are kept.
    Time {
        #[serde(default = "d_every")]
        every_hours: u64,
        #[serde(default = "d_keep_time")]
        keep: u32,
    },
    /// The last `keep` versions of every file. A new version by the same device within
    /// `min_interval_secs` replaces the previous one. Deleted files keep their versions `deleted_days`.
    File {
        #[serde(default = "d_keep_file")]
        keep: u32,
        #[serde(default = "d_min_interval")]
        min_interval_secs: u64,
        #[serde(default = "d_deleted_days")]
        deleted_days: u32,
    },
}

impl Default for BackupPolicy {
    fn default() -> Self {
        BackupPolicy::File { keep: d_keep_file(), min_interval_secs: d_min_interval(), deleted_days: d_deleted_days() }
    }
}

impl BackupPolicy {
    /// `none`, `time[:<hours>h[:<keep>]]`, `file[:<keep>]`
    pub fn parse(s: &str) -> Result<BackupPolicy, String> {
        let parts: Vec<&str> = s.split(':').collect();
        let num = |t: &str| t.trim_end_matches('h').parse::<u64>().map_err(|_| format!("not a number: {t:?}"));
        match parts[0] {
            "none" if parts.len() == 1 => Ok(BackupPolicy::None),
            "time" if parts.len() <= 3 => Ok(BackupPolicy::Time {
                every_hours: parts.get(1).map(|t| num(t)).transpose()?.unwrap_or(d_every()).max(1),
                keep: parts.get(2).map(|t| num(t)).transpose()?.map(|v| v as u32).unwrap_or(d_keep_time()).max(1),
            }),
            "file" if parts.len() <= 2 => Ok(BackupPolicy::File {
                keep: parts.get(1).map(|t| num(t)).transpose()?.map(|v| v as u32).unwrap_or(d_keep_file()).max(1),
                min_interval_secs: d_min_interval(),
                deleted_days: d_deleted_days(),
            }),
            _ => Err(format!("expected none, time[:24h[:7]] or file[:10], got {s:?}")),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            BackupPolicy::None => "none".into(),
            BackupPolicy::Time { every_hours, keep } => format!("time:{every_hours}h:{keep}"),
            BackupPolicy::File { keep, .. } => format!("file:{keep}"),
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, BackupPolicy::None)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostedConfig {
    /// 16 random bytes, hex.
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    /// Where `.medialake-sync` lives instead of `<root>/.medialake-sync` (another disk, for example).
    #[serde(default)]
    pub store: Option<PathBuf>,
    #[serde(default)]
    pub backup: BackupPolicy,
    /// Extra glob patterns to ignore, sent to members.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl HostedConfig {
    pub fn store_dir(&self) -> PathBuf {
        self.store.clone().unwrap_or_else(|| self.root.join(".medialake-sync"))
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SyncsFile {
    #[serde(default)]
    pub syncs: Vec<HostedConfig>,
}

impl SyncsFile {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("syncs.json")
    }

    pub fn load(state_dir: &Path) -> SyncsFile {
        std::fs::read_to_string(Self::path(state_dir)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        let p = Self::path(state_dir);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn find(&self, name_or_id: &str) -> Option<&HostedConfig> {
        self.syncs.iter().find(|s| s.id == name_or_id).or_else(|| self.syncs.iter().find(|s| s.name == name_or_id))
    }

    /// Adds a sync; names are unique.
    pub fn add(&mut self, name: &str, root: &Path, store: Option<PathBuf>, backup: BackupPolicy) -> anyhow::Result<HostedConfig> {
        if name.is_empty() || name.contains(['/', '\\']) {
            anyhow::bail!("invalid sync name {name:?}");
        }
        if self.syncs.iter().any(|s| s.name == name) {
            anyhow::bail!("a sync named {name:?} already exists");
        }
        let root = crate::util::plain_path(&root.canonicalize().map_err(|e| anyhow::anyhow!("{}: {e}", root.display()))?);
        if !root.is_dir() {
            anyhow::bail!("{} is not a folder", root.display());
        }
        if self.syncs.iter().any(|s| s.root == root) {
            anyhow::bail!("{} is already shared", root.display());
        }
        let cfg = HostedConfig { id: crate::util::random_id(), name: name.to_string(), root, store, backup, ignore: vec![] };
        self.syncs.push(cfg.clone());
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_policies() {
        assert_eq!(BackupPolicy::parse("none").unwrap(), BackupPolicy::None);
        assert_eq!(BackupPolicy::parse("time").unwrap(), BackupPolicy::Time { every_hours: 24, keep: 7 });
        assert_eq!(BackupPolicy::parse("time:12h:3").unwrap(), BackupPolicy::Time { every_hours: 12, keep: 3 });
        assert!(matches!(BackupPolicy::parse("file:5").unwrap(), BackupPolicy::File { keep: 5, .. }));
        assert!(BackupPolicy::parse("weekly").is_err());
    }

    #[test]
    fn config_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut f = SyncsFile::default();
        f.add("Photos", root.path(), None, BackupPolicy::Time { every_hours: 6, keep: 2 }).unwrap();
        assert!(f.add("Photos", root.path(), None, BackupPolicy::None).is_err());
        f.save(d.path()).unwrap();
        let g = SyncsFile::load(d.path());
        assert_eq!(g.syncs.len(), 1);
        assert_eq!(g.syncs[0].backup, BackupPolicy::Time { every_hours: 6, keep: 2 });
        assert!(g.find("Photos").is_some());
    }
}
