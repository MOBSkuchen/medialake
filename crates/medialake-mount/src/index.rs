//! The mount's own bookkeeping, one SQLite file per sync next to the cache tree: inode numbers for the
//! NFS server, which blocks of which file are present, what is dirty, and what is pinned.
//!
//! The *tree* (real sparse files) is what the sync engine looks at; this index only adds what the file
//! system cannot say: "the bytes here are zeros because they were never fetched".

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Hydration granularity: a read fetches the blocks it touches.
pub const BLOCK: u64 = 1 << 20;

pub fn blocks_of(size: u64) -> u32 {
    size.div_ceil(BLOCK) as u32
}

/// A set of block numbers as sorted, non-touching half-open ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges(pub Vec<(u32, u32)>);

impl Ranges {
    pub fn all(blocks: u32) -> Ranges {
        if blocks == 0 { Ranges(vec![]) } else { Ranges(vec![(0, blocks)]) }
    }

    pub fn from_blob(b: &[u8]) -> Ranges {
        let mut v = Vec::with_capacity(b.len() / 8);
        for c in b.chunks_exact(8) {
            let s = u32::from_le_bytes(c[0..4].try_into().unwrap());
            let e = u32::from_le_bytes(c[4..8].try_into().unwrap());
            if e > s {
                v.push((s, e));
            }
        }
        Ranges(v)
    }

    pub fn to_blob(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.0.len() * 8);
        for (s, e) in &self.0 {
            b.extend_from_slice(&s.to_le_bytes());
            b.extend_from_slice(&e.to_le_bytes());
        }
        b
    }

    pub fn insert(&mut self, start: u32, end: u32) {
        if end <= start {
            return;
        }
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.0.len() + 1);
        let (mut s, mut e) = (start, end);
        let mut placed = false;
        for &(a, b) in &self.0 {
            if b < s {
                out.push((a, b));
            } else if a > e {
                if !placed {
                    out.push((s, e));
                    placed = true;
                }
                out.push((a, b));
            } else {
                s = s.min(a);
                e = e.max(b);
            }
        }
        if !placed {
            out.push((s, e));
        }
        self.0 = out;
    }

    /// The parts of `[start, end)` that are not in the set.
    pub fn missing(&self, start: u32, end: u32) -> Vec<(u32, u32)> {
        let mut out = vec![];
        let mut at = start;
        for &(a, b) in &self.0 {
            if b <= at {
                continue;
            }
            if a >= end {
                break;
            }
            if a > at {
                out.push((at, a.min(end)));
            }
            at = at.max(b);
            if at >= end {
                break;
            }
        }
        if at < end {
            out.push((at, end));
        }
        out
    }

    pub fn count(&self) -> u64 {
        self.0.iter().map(|(a, b)| (*b - *a) as u64).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What the index knows about one file of the tree. Files without a row are plain local files
/// (created through the mount and not synced yet): fully present.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRow {
    pub size: u64,
    /// The mtime the placeholder carries (restored after hydration writes).
    pub mtime: i64,
    /// Content hash recorded when the content was last known to equal the host's / pushed.
    pub hash: String,
    pub rev: u64,
    /// Written through the mount since `hash` was recorded.
    pub dirty: bool,
    pub full: bool,
    pub blocks: Ranges,
    pub atime: i64,
}

impl FileRow {
    pub fn cached_bytes(&self) -> u64 {
        if self.full { self.size } else { (self.blocks.count() * BLOCK).min(self.size) }
    }
}

pub struct Index {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inodes (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    hash TEXT NOT NULL,
    rev INTEGER NOT NULL,
    dirty INTEGER NOT NULL DEFAULT 0,
    full INTEGER NOT NULL DEFAULT 0,
    blocks BLOB NOT NULL DEFAULT x'',
    cached INTEGER NOT NULL DEFAULT 0,
    atime INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS pins (prefix TEXT PRIMARY KEY);
INSERT OR IGNORE INTO inodes(id, path) VALUES (1, '');
";

const ROW: &str = "size, mtime, hash, rev, dirty, full, blocks, atime";

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        size: r.get::<_, i64>(0)? as u64,
        mtime: r.get(1)?,
        hash: r.get(2)?,
        rev: r.get::<_, i64>(3)? as u64,
        dirty: r.get::<_, i64>(4)? != 0,
        full: r.get::<_, i64>(5)? != 0,
        blocks: Ranges::from_blob(&r.get::<_, Vec<u8>>(6)?),
        atime: r.get(7)?,
    })
}

impl Index {
    pub fn open(path: &Path) -> Result<Index> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Index { conn: Mutex::new(conn) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- inodes ----

    pub fn lookup_ino(&self, path: &str) -> Option<u64> {
        self.c().query_row("SELECT id FROM inodes WHERE path = ?1", [path], |r| r.get::<_, i64>(0)).optional().ok().flatten().map(|v| v as u64)
    }

    pub fn ino(&self, path: &str) -> Result<u64> {
        let c = self.c();
        if let Some(id) = c.query_row("SELECT id FROM inodes WHERE path = ?1", [path], |r| r.get::<_, i64>(0)).optional()? {
            return Ok(id as u64);
        }
        c.execute("INSERT INTO inodes(path) VALUES (?1)", [path])?;
        Ok(c.last_insert_rowid() as u64)
    }

    pub fn path_of(&self, id: u64) -> Option<String> {
        self.c().query_row("SELECT path FROM inodes WHERE id = ?1", [id as i64], |r| r.get(0)).optional().ok().flatten()
    }

    // ---- file rows ----

    pub fn row(&self, path: &str) -> Option<FileRow> {
        self.c().query_row(&format!("SELECT {ROW} FROM files WHERE path = ?1"), [path], row_of).optional().ok().flatten()
    }

    pub fn put(&self, path: &str, r: &FileRow) -> Result<()> {
        let cached = r.cached_bytes() as i64;
        self.c().execute(
            "INSERT INTO files(path, size, mtime, hash, rev, dirty, full, blocks, cached, atime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3, hash = ?4, rev = ?5, dirty = ?6, full = ?7,
                 blocks = ?8, cached = ?9, atime = ?10",
            params![path, r.size as i64, r.mtime, r.hash, r.rev as i64, r.dirty as i64, r.full as i64, r.blocks.to_blob(), cached, r.atime],
        )?;
        Ok(())
    }

    pub fn set_blocks(&self, path: &str, blocks: &Ranges, full: bool, cached: u64) -> Result<()> {
        self.c().execute(
            "UPDATE files SET blocks = ?2, full = ?3, cached = ?4 WHERE path = ?1",
            params![path, blocks.to_blob(), full as i64, cached as i64],
        )?;
        Ok(())
    }

    pub fn set_dirty(&self, path: &str, dirty: bool) -> Result<()> {
        self.c().execute("UPDATE files SET dirty = ?2 WHERE path = ?1", params![path, dirty as i64])?;
        Ok(())
    }

    pub fn touch(&self, path: &str, atime: i64) {
        let _ = self.c().execute("UPDATE files SET atime = ?2 WHERE path = ?1", params![path, atime]);
    }

    pub fn set_mtime(&self, path: &str, mtime: i64) {
        let _ = self.c().execute("UPDATE files SET mtime = ?2 WHERE path = ?1", params![path, mtime]);
    }

    pub fn cached_bytes(&self) -> u64 {
        self.c().query_row("SELECT COALESCE(SUM(cached), 0) FROM files", [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64
    }

    /// Clean files with cached bytes, least recently used first.
    pub fn eviction_candidates(&self) -> Vec<(String, FileRow)> {
        let c = self.c();
        let Ok(mut st) = c.prepare(&format!("SELECT path, {ROW} FROM files WHERE dirty = 0 AND cached > 0 ORDER BY atime ASC")) else {
            return vec![];
        };
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                FileRow {
                    size: r.get::<_, i64>(1)? as u64,
                    mtime: r.get(2)?,
                    hash: r.get(3)?,
                    rev: r.get::<_, i64>(4)? as u64,
                    dirty: r.get::<_, i64>(5)? != 0,
                    full: r.get::<_, i64>(6)? != 0,
                    blocks: Ranges::from_blob(&r.get::<_, Vec<u8>>(7)?),
                    atime: r.get(8)?,
                },
            ))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => vec![],
        }
    }

    /// Rows of files at or below `prefix` (empty = all) that are not fully present.
    pub fn incomplete_under(&self, prefix: &str) -> Vec<String> {
        let c = self.c();
        let Ok(mut st) = c.prepare("SELECT path FROM files WHERE full = 0 AND (?1 = '' OR path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')") else {
            return vec![];
        };
        match st.query_map([prefix], |r| r.get::<_, String>(0)) {
            Ok(it) => it.flatten().collect(),
            Err(_) => vec![],
        }
    }

    pub fn dirty_count(&self) -> u64 {
        self.c().query_row("SELECT COUNT(*) FROM files WHERE dirty = 1", [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64
    }

    // ---- tree changes ----

    /// Forgets `path` and everything below it (rows and inode numbers).
    pub fn remove_tree(&self, path: &str) -> Result<()> {
        let c = self.c();
        let pat = "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')";
        c.execute(&format!("DELETE FROM files WHERE {pat}"), [path])?;
        c.execute(&format!("DELETE FROM inodes WHERE id != 1 AND {pat}"), [path])?;
        Ok(())
    }

    /// Moves `from` (and everything below) to `to`, keeping inode numbers. Whatever is at `to` goes.
    pub fn rename_tree(&self, from: &str, to: &str) -> Result<()> {
        let c = self.c();
        let pat = "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')";
        c.execute(&format!("DELETE FROM files WHERE {pat}"), [to])?;
        c.execute(&format!("DELETE FROM inodes WHERE id != 1 AND {pat}"), [to])?;
        let n = from.len() as i64;
        for table in ["files", "inodes"] {
            c.execute(
                &format!("UPDATE {table} SET path = ?2 || substr(path, ?3 + 1) WHERE {pat}"),
                params![from, to, n],
            )?;
        }
        Ok(())
    }

    // ---- pins ----

    pub fn pins(&self) -> Vec<String> {
        let c = self.c();
        let Ok(mut st) = c.prepare("SELECT prefix FROM pins") else { return vec![] };
        match st.query_map([], |r| r.get::<_, String>(0)) {
            Ok(it) => it.flatten().collect(),
            Err(_) => vec![],
        }
    }

    pub fn add_pin(&self, prefix: &str) -> Result<()> {
        self.c().execute("INSERT OR IGNORE INTO pins(prefix) VALUES (?1)", [prefix])?;
        Ok(())
    }

    pub fn remove_pin(&self, prefix: &str) -> Result<()> {
        let c = self.c();
        c.execute("DELETE FROM pins WHERE prefix = ?1", [prefix])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_and_report_missing() {
        let mut r = Ranges::default();
        r.insert(2, 4);
        r.insert(6, 8);
        r.insert(4, 6);
        assert_eq!(r.0, vec![(2, 8)]);
        r.insert(0, 1);
        assert_eq!(r.0, vec![(0, 1), (2, 8)]);
        assert_eq!(r.missing(0, 10), vec![(1, 2), (8, 10)]);
        assert_eq!(r.missing(3, 5), vec![]);
        assert_eq!(r.missing(8, 8), vec![]);
        assert_eq!(Ranges::from_blob(&r.to_blob()), r);
        assert_eq!(r.count(), 7);
        let mut t = Ranges::default();
        t.insert(5, 6);
        t.insert(1, 2);
        t.insert(3, 4);
        assert_eq!(t.0, vec![(1, 2), (3, 4), (5, 6)]);
        t.insert(0, 10);
        assert_eq!(t.0, vec![(0, 10)]);
    }

    #[test]
    fn tree_rename_keeps_inodes() {
        let d = tempfile::tempdir().unwrap();
        let ix = Index::open(&d.path().join("i.db")).unwrap();
        let a = ix.ino("docs").unwrap();
        let b = ix.ino("docs/a.txt").unwrap();
        let c = ix.ino("docs-other").unwrap();
        ix.rename_tree("docs", "papers").unwrap();
        assert_eq!(ix.path_of(a).as_deref(), Some("papers"));
        assert_eq!(ix.path_of(b).as_deref(), Some("papers/a.txt"));
        assert_eq!(ix.path_of(c).as_deref(), Some("docs-other"));
        ix.remove_tree("papers").unwrap();
        assert_eq!(ix.path_of(b), None);
        assert_eq!(ix.path_of(c).as_deref(), Some("docs-other"));
    }
}
