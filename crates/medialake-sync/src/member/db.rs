//! Member state (`<data dir>/sync/<sync id>/state.db`): the **base** (what the host last told us),
//! a cache of the local fingerprint, and pending conflicts.
//!
//! The base is what makes conflict detection exact: a file is "changed here" if its local hash differs
//! from the base hash, and "changed there" if the host's `rev` is newer than the base `rev`.

use std::path::Path;

use anyhow::Result;
use medialake_proto::sync::{Entry, EntryKind};
use rusqlite::{Connection, OptionalExtension, Row, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entries(
    path TEXT PRIMARY KEY, kind INTEGER NOT NULL,
    has_base INTEGER NOT NULL DEFAULT 0, b_rev INTEGER NOT NULL DEFAULT 0, b_hash TEXT NOT NULL DEFAULT '',
    b_size INTEGER NOT NULL DEFAULT 0, b_mtime INTEGER NOT NULL DEFAULT 0, b_exec INTEGER NOT NULL DEFAULT 0,
    l_size INTEGER NOT NULL DEFAULT -1, l_mtime INTEGER NOT NULL DEFAULT 0, l_hash TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS conflicts(path TEXT PRIMARY KEY, host_entry TEXT NOT NULL, since INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS local_only(path TEXT PRIMARY KEY);
";

/// What the host last told us about a path.
#[derive(Debug, Clone, PartialEq)]
pub struct Base {
    pub path: String,
    pub kind: EntryKind,
    /// 0 = unknown (after the host's database was rebuilt); the hash is still the last known content.
    pub rev: u64,
    pub hash: String,
    pub size: u64,
    pub mtime: i64,
    pub exec: bool,
}

impl Base {
    pub fn from_entry(e: &Entry) -> Base {
        Base { path: e.path.clone(), kind: e.kind, rev: e.rev, hash: e.hash.clone(), size: e.size, mtime: e.mtime, exec: e.exec }
    }
}

/// Last known fingerprint of the local file, so that unchanged files are not hashed again.
#[derive(Debug, Clone)]
pub struct LocalCache {
    pub size: u64,
    pub mtime: i64,
    pub hash: String,
}

pub struct MemberDb {
    c: Connection,
}

fn kind_i(k: EntryKind) -> i64 {
    if k == EntryKind::File { 0 } else { 1 }
}

fn row_base(r: &Row) -> rusqlite::Result<Option<Base>> {
    if r.get::<_, i64>(2)? == 0 {
        return Ok(None);
    }
    Ok(Some(Base {
        path: r.get(0)?,
        kind: if r.get::<_, i64>(1)? == 0 { EntryKind::File } else { EntryKind::Dir },
        rev: r.get::<_, i64>(3)? as u64,
        hash: r.get(4)?,
        size: r.get::<_, i64>(5)? as u64,
        mtime: r.get(6)?,
        exec: r.get::<_, i64>(7)? != 0,
    }))
}

const BASE_COLS: &str = "path,kind,has_base,b_rev,b_hash,b_size,b_mtime,b_exec";

impl MemberDb {
    pub fn open(path: &Path) -> Result<MemberDb> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let c = Connection::open(path)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        c.execute_batch(SCHEMA)?;
        Ok(MemberDb { c })
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.c.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.c.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![key, value])?;
        Ok(())
    }

    pub fn cursor(&self) -> u64 {
        self.meta("cursor").and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn set_cursor(&self, c: u64) -> Result<()> {
        self.set_meta("cursor", &c.to_string())
    }

    pub fn epoch(&self) -> String {
        self.meta("epoch").unwrap_or_default()
    }

    // ---- base ----

    pub fn base(&self, path: &str) -> Option<Base> {
        self.c
            .query_row(&format!("SELECT {BASE_COLS} FROM entries WHERE path=?1"), [path], row_base)
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    pub fn set_base(&self, b: &Base) -> Result<()> {
        self.c.execute(
            "INSERT INTO entries(path,kind,has_base,b_rev,b_hash,b_size,b_mtime,b_exec) VALUES(?1,?2,1,?3,?4,?5,?6,?7)
             ON CONFLICT(path) DO UPDATE SET kind=?2,has_base=1,b_rev=?3,b_hash=?4,b_size=?5,b_mtime=?6,b_exec=?7",
            params![b.path, kind_i(b.kind), b.rev as i64, b.hash, b.size as i64, b.mtime, b.exec as i64],
        )?;
        Ok(())
    }

    /// Forgets the base of a path (and the local cache with it).
    pub fn drop_base(&self, path: &str) -> Result<()> {
        self.c.execute("DELETE FROM entries WHERE path=?1", [path])?;
        Ok(())
    }

    /// Every base at or below `prefix` (empty = all).
    pub fn bases_under(&self, prefix: &str) -> Vec<Base> {
        let Ok(mut st) = self.c.prepare(&format!(
            "SELECT {BASE_COLS} FROM entries WHERE has_base=1 AND (?1='' OR path=?1 OR substr(path,1,length(?1)+1)=?1||'/') ORDER BY path"
        )) else {
            return vec![];
        };
        st.query_map([prefix], row_base).map(|rows| rows.flatten().flatten().collect()).unwrap_or_default()
    }

    /// After the host's database was rebuilt: revisions mean nothing any more, hashes still do.
    pub fn forget_revs(&self) -> Result<()> {
        self.c.execute("UPDATE entries SET b_rev=0 WHERE has_base=1", [])?;
        Ok(())
    }

    /// Re-keys the bases below `from` after a local folder move acknowledged by the host.
    pub fn rename_prefix(&self, from: &str, to: &str) -> Result<()> {
        for b in self.bases_under(from) {
            let np = format!("{to}{}", &b.path[from.len()..]);
            self.drop_base(&b.path)?;
            self.set_base(&Base { path: np, ..b })?;
        }
        Ok(())
    }

    // ---- local cache ----

    pub fn local_cache(&self, path: &str) -> Option<LocalCache> {
        self.c
            .query_row("SELECT l_size,l_mtime,l_hash FROM entries WHERE path=?1", [path], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            })
            .optional()
            .ok()
            .flatten()
            .filter(|(s, _, h)| *s >= 0 && !h.is_empty())
            .map(|(s, m, h)| LocalCache { size: s as u64, mtime: m, hash: h })
    }

    pub fn set_local_cache(&self, path: &str, size: u64, mtime: i64, hash: &str) -> Result<()> {
        self.c.execute(
            "INSERT INTO entries(path,kind,l_size,l_mtime,l_hash) VALUES(?1,0,?2,?3,?4)
             ON CONFLICT(path) DO UPDATE SET l_size=?2,l_mtime=?3,l_hash=?4",
            params![path, size as i64, mtime, hash],
        )?;
        Ok(())
    }

    /// Deletes rows that carry neither a base nor a usable cache (keeps the table small).
    pub fn vacuum_rows(&self) -> Result<()> {
        self.c.execute("DELETE FROM entries WHERE has_base=0 AND l_size<0", [])?;
        Ok(())
    }

    pub fn file_count(&self) -> u64 {
        self.c
            .query_row("SELECT COUNT(*) FROM entries WHERE has_base=1 AND kind=0", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .unwrap_or(0)
    }

    // ---- conflicts (policy "ask") ----

    pub fn conflict(&self, path: &str) -> Option<Entry> {
        let j: Option<String> = self.c.query_row("SELECT host_entry FROM conflicts WHERE path=?1", [path], |r| r.get(0)).optional().ok().flatten();
        j.and_then(|s| serde_json_entry::decode(&s))
    }

    pub fn set_conflict(&self, host: &Entry, since: i64) -> Result<()> {
        self.c.execute(
            "INSERT INTO conflicts(path,host_entry,since) VALUES(?1,?2,?3)
             ON CONFLICT(path) DO UPDATE SET host_entry=?2",
            params![host.path, serde_json_entry::encode(host), since],
        )?;
        Ok(())
    }

    pub fn clear_conflict(&self, path: &str) -> Result<()> {
        self.c.execute("DELETE FROM conflicts WHERE path=?1", [path])?;
        Ok(())
    }

    pub fn conflicts(&self) -> Vec<(Entry, i64)> {
        let Ok(mut st) = self.c.prepare("SELECT host_entry,since FROM conflicts ORDER BY path") else { return vec![] };
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map(|rows| rows.flatten().filter_map(|(j, s)| serde_json_entry::decode(&j).map(|e| (e, s))).collect())
            .unwrap_or_default()
    }

    // ---- local-only paths (conflict copies on read-only members) ----

    pub fn add_local_only(&self, path: &str) -> Result<()> {
        self.c.execute("INSERT OR IGNORE INTO local_only(path) VALUES(?1)", [path])?;
        Ok(())
    }

    pub fn is_local_only(&self, path: &str) -> bool {
        self.c.query_row("SELECT 1 FROM local_only WHERE path=?1", [path], |_| Ok(())).optional().ok().flatten().is_some()
    }
}

/// `Entry` is a bierpc type, not a serde one: keep the stored form trivial and explicit.
mod serde_json_entry {
    use medialake_proto::sync::{Entry, EntryKind};

    pub fn encode(e: &Entry) -> String {
        // fields separated by \u{1f}; the path may contain anything but control characters
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            e.path,
            if e.kind == EntryKind::File { 0 } else { 1 },
            e.size,
            e.mtime,
            e.hash,
            e.exec as u8,
            e.rev,
            e.deleted as u8,
            e.author
        )
    }

    pub fn decode(s: &str) -> Option<Entry> {
        let p: Vec<&str> = s.split('\u{1f}').collect();
        if p.len() != 9 {
            return None;
        }
        Some(Entry {
            path: p[0].to_string(),
            kind: if p[1] == "0" { EntryKind::File } else { EntryKind::Dir },
            size: p[2].parse().ok()?,
            mtime: p[3].parse().ok()?,
            hash: p[4].to_string(),
            exec: p[5] == "1",
            rev: p[6].parse().ok()?,
            deleted: p[7] == "1",
            author: p[8].to_string(),
        })
    }
}
