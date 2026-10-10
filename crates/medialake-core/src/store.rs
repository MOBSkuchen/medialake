//! SQLite metadata store. The database is the source of truth for vectors (stored as f16 blobs);
//! the HNSW index file is a cache that is rebuilt from here when missing or stale.

use std::collections::HashMap;
use std::path::Path;

use half::f16;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub name: String,
    /// Last seen size / mtime (updated on every scan, even when nothing is re-embedded).
    pub size: u64,
    pub mtime: i64,
    pub modality: String,
    /// File size at the last embed (for the accumulating-change rule).
    pub embedded_size: u64,
    /// Hash of the raw file at the last embed.
    pub content_hash: Option<Vec<u8>>,
    /// Hash of the decoded media content (pixels / samples) at the last embed.
    pub media_hash: Option<Vec<u8>>,
    /// `ok`, `name_only` (too large / unsupported), `error`
    pub status: String,
    pub indexed_at: i64,
}

#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub id: i64,
    pub kind: String,
    pub seq: i64,
    pub chunk_hash: Vec<u8>,
    pub byte_len: u64,
    /// Per-line signature (hash, length pairs) of text chunks, used to measure how much changed.
    pub sig: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct NewChunk {
    pub kind: String,
    pub seq: i64,
    pub chunk_hash: Vec<u8>,
    pub byte_len: u64,
    pub snippet: String,
    pub sig: Vec<u8>,
    pub vector: Vec<f32>,
}

/// Everything search needs to know about a hit.
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub chunk_id: i64,
    pub file_id: i64,
    pub path: String,
    pub name: String,
    pub modality: String,
    pub kind: String,
    pub seq: i64,
    pub snippet: String,
    pub mtime: i64,
    pub size: u64,
}

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    modality TEXT NOT NULL,
    embedded_size INTEGER NOT NULL DEFAULT 0,
    content_hash BLOB,
    media_hash BLOB,
    status TEXT NOT NULL DEFAULT 'ok',
    indexed_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    seq INTEGER NOT NULL,
    chunk_hash BLOB NOT NULL,
    byte_len INTEGER NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    sig BLOB,
    vec BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
";

fn f16_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for x in v {
        out.extend_from_slice(&f16::from_f32(*x).to_le_bytes());
    }
    out
}

fn blob_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(2).map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32()).collect()
}

fn file_from_row(r: &rusqlite::Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        path: r.get(1)?,
        name: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        mtime: r.get(4)?,
        modality: r.get(5)?,
        embedded_size: r.get::<_, i64>(6)? as u64,
        content_hash: r.get(7)?,
        media_hash: r.get(8)?,
        status: r.get(9)?,
        indexed_at: r.get(10)?,
    })
}

const FILE_COLS: &str = "id, path, name, size, mtime, modality, embedded_size, content_hash, media_hash, status, indexed_at";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Store> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    // ---- meta ----

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
        Ok(())
    }

    // ---- files ----

    pub fn file_by_path(&self, path: &str) -> rusqlite::Result<Option<FileRow>> {
        self.conn
            .query_row(&format!("SELECT {FILE_COLS} FROM files WHERE path=?1"), [path], file_from_row)
            .optional()
    }

    /// Inserts or updates by path; returns the file id.
    pub fn upsert_file(&self, f: &FileRow) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO files(path,name,size,mtime,modality,embedded_size,content_hash,media_hash,status,indexed_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(path) DO UPDATE SET name=excluded.name,size=excluded.size,mtime=excluded.mtime,modality=excluded.modality,
               embedded_size=excluded.embedded_size,content_hash=excluded.content_hash,media_hash=excluded.media_hash,
               status=excluded.status,indexed_at=excluded.indexed_at",
            params![f.path, f.name, f.size as i64, f.mtime, f.modality, f.embedded_size as i64, f.content_hash, f.media_hash, f.status, f.indexed_at],
        )?;
        self.conn.query_row("SELECT id FROM files WHERE path=?1", [&f.path], |r| r.get(0))
    }

    /// Records the latest seen size/mtime without touching embedded state.
    pub fn touch_seen(&self, file_id: i64, size: u64, mtime: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE files SET size=?2, mtime=?3 WHERE id=?1", params![file_id, size as i64, mtime])?;
        Ok(())
    }

    /// Deletes a file; returns the chunk ids whose vectors must be removed from the index.
    pub fn delete_file(&self, path: &str) -> rusqlite::Result<Vec<i64>> {
        let Some(f) = self.file_by_path(path)? else { return Ok(vec![]) };
        let ids = self.chunk_ids(f.id)?;
        self.conn.execute("DELETE FROM files WHERE id=?1", [f.id])?;
        Ok(ids)
    }

    /// Deletes all files whose path starts with `prefix` (directory removal); returns chunk ids.
    pub fn delete_under(&self, prefix: &str) -> rusqlite::Result<Vec<i64>> {
        let like = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        let mut st = self.conn.prepare("SELECT c.id FROM chunks c JOIN files f ON f.id=c.file_id WHERE f.path LIKE ?1 ESCAPE '\\'")?;
        let ids: Vec<i64> = st.query_map([&like], |r| r.get(0))?.collect::<Result<_, _>>()?;
        self.conn.execute("DELETE FROM files WHERE path LIKE ?1 ESCAPE '\\'", [&like])?;
        Ok(ids)
    }

    pub fn rename_file(&self, old: &str, new: &str, new_name: &str) -> rusqlite::Result<bool> {
        // a stale row at the destination is replaced by the moved file
        self.conn.execute("DELETE FROM files WHERE path=?1", [new])?;
        let n = self.conn.execute("UPDATE files SET path=?2, name=?3 WHERE path=?1", [old, new, new_name])?;
        Ok(n > 0)
    }


    /// Moves all files below directory `old` to `new` (a directory rename). Returns the new paths.
    pub fn rename_prefix(&self, old: &str, new: &str) -> rusqlite::Result<Vec<String>> {
        let like = format!("{}\\%", old.replace('%', "\\%").replace('_', "\\_"));
        let mut st = self.conn.prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let paths: Vec<String> = st.query_map([&like], |r| r.get(0))?.collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for p in paths {
            let np = format!("{}{}", new, &p[old.len()..]);
            let name = np.rsplit('\\').next().unwrap_or(&np).to_string();
            self.conn.execute("DELETE FROM files WHERE path=?1", [&np])?;
            self.conn.execute("UPDATE files SET path=?2, name=?3 WHERE path=?1", [&p, &np, &name])?;
            out.push(np);
        }
        Ok(out)
    }
    /// `path -> (id, size, mtime, status)` for reconciliation.
    pub fn all_seen(&self) -> rusqlite::Result<HashMap<String, (i64, u64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT path, id, size, mtime, status FROM files")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?))))?;
        rows.collect()
    }

    // ---- chunks ----

    pub fn chunk_ids(&self, file_id: i64) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.conn.prepare("SELECT id FROM chunks WHERE file_id=?1")?;
        st.query_map([file_id], |r| r.get(0))?.collect()
    }

    pub fn chunks_of(&self, file_id: i64) -> rusqlite::Result<Vec<ChunkRow>> {
        let mut st = self.conn.prepare("SELECT id, kind, seq, chunk_hash, byte_len, sig FROM chunks WHERE file_id=?1 ORDER BY seq")?;
        let rows = st.query_map([file_id], |r| {
            Ok(ChunkRow { id: r.get(0)?, kind: r.get(1)?, seq: r.get(2)?, chunk_hash: r.get(3)?, byte_len: r.get::<_, i64>(4)? as u64, sig: r.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default() })
        })?;
        rows.collect()
    }

    pub fn insert_chunk(&self, file_id: i64, c: &NewChunk) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO chunks(file_id,kind,seq,chunk_hash,byte_len,snippet,sig,vec) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![file_id, c.kind, c.seq, c.chunk_hash, c.byte_len as i64, c.snippet, c.sig, f16_blob(&c.vector)],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_chunks(&self, ids: &[i64]) -> rusqlite::Result<()> {
        let mut st = self.conn.prepare("DELETE FROM chunks WHERE id=?1")?;
        for id in ids {
            st.execute([id])?;
        }
        Ok(())
    }

    pub fn set_chunk_seq(&self, id: i64, seq: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE chunks SET seq=?2 WHERE id=?1", params![id, seq])?;
        Ok(())
    }

    /// Replaces the vector (and bookkeeping) of one chunk in place, keeping its id.
    pub fn replace_chunk(&self, id: i64, c: &NewChunk) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE chunks SET kind=?2, seq=?3, chunk_hash=?4, byte_len=?5, snippet=?6, sig=?7, vec=?8 WHERE id=?1",
            params![id, c.kind, c.seq, c.chunk_hash, c.byte_len as i64, c.snippet, c.sig, f16_blob(&c.vector)],
        )?;
        Ok(())
    }

    pub fn chunk_vector(&self, id: i64) -> rusqlite::Result<Option<Vec<f32>>> {
        let b: Option<Vec<u8>> = self.conn.query_row("SELECT vec FROM chunks WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(b.map(|b| blob_f32(&b)))
    }

    /// Streams every vector (for rebuilding the HNSW index).
    pub fn for_each_vector(&self, mut f: impl FnMut(i64, Vec<f32>)) -> rusqlite::Result<()> {
        let mut st = self.conn.prepare("SELECT id, vec FROM chunks")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let b: Vec<u8> = r.get(1)?;
            f(id, blob_f32(&b));
        }
        Ok(())
    }

    pub fn chunk_info(&self, ids: &[i64]) -> rusqlite::Result<HashMap<i64, ChunkInfo>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, f.id, f.path, f.name, f.modality, c.kind, c.seq, c.snippet, f.mtime, f.size
             FROM chunks c JOIN files f ON f.id=c.file_id WHERE c.id=?1",
        )?;
        let mut out = HashMap::new();
        for id in ids {
            let row = st
                .query_row([id], |r| {
                    Ok(ChunkInfo {
                        chunk_id: r.get(0)?,
                        file_id: r.get(1)?,
                        path: r.get(2)?,
                        name: r.get(3)?,
                        modality: r.get(4)?,
                        kind: r.get(5)?,
                        seq: r.get(6)?,
                        snippet: r.get(7)?,
                        mtime: r.get(8)?,
                        size: r.get::<_, i64>(9)? as u64,
                    })
                })
                .optional()?;
            if let Some(i) = row {
                out.insert(*id, i);
            }
        }
        Ok(out)
    }

    // ---- stats / maintenance ----

    pub fn counts(&self) -> rusqlite::Result<(u64, u64)> {
        let f: i64 = self.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let c: i64 = self.conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok((f as u64, c as u64))
    }

    pub fn count_by_status(&self) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut st = self.conn.prepare("SELECT status, COUNT(*) FROM files GROUP BY status")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect()
    }

    pub fn clear_all(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM chunks; DELETE FROM files;")
    }

    /// Delete the content (non-name) vectors of one modality, e.g. when video embedding is turned off.
    pub fn delete_content_chunks_of_modality(&self, modality: &str) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.conn.prepare("SELECT c.id FROM chunks c JOIN files f ON f.id=c.file_id WHERE f.modality=?1 AND c.kind<>'name'")?;
        let ids: Vec<i64> = st.query_map([modality], |r| r.get(0))?.collect::<Result<_, _>>()?;
        self.conn.execute(
            "DELETE FROM chunks WHERE kind<>'name' AND file_id IN (SELECT id FROM files WHERE modality=?1)",
            [modality],
        )?;
        self.conn.execute(
            "UPDATE files SET content_hash=NULL, media_hash=NULL, embedded_size=0, size=0, mtime=0 WHERE modality=?1",
            [modality],
        )?;
        Ok(ids)
    }

    pub fn paths_of_modality(&self, modality: &str) -> rusqlite::Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM files WHERE modality=?1")?;
        st.query_map([modality], |r| r.get(0))?.collect()
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN")
    }

    pub fn commit(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("COMMIT")
    }

    pub fn rollback(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }
}
