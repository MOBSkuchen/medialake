//! HNSW vector index (usearch, cosine, f16 storage). The file on disk is only a cache of the
//! vectors in SQLite: if it is missing or inconsistent it is rebuilt from the database.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::store::Store;

pub struct VectorIndex {
    index: Mutex<Index>,
    path: PathBuf,
    /// Vector length of the active model.
    dims: AtomicUsize,
}

fn new_index(dims: usize) -> Result<Index, String> {
    let options = IndexOptions {
        dimensions: dims,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F16,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    Index::new(&options).map_err(|e| e.to_string())
}

impl VectorIndex {
    /// Loads the index file if it matches the database, otherwise rebuilds it from SQLite.
    pub fn open(path: &Path, store: &Store, dims: usize) -> Result<VectorIndex, String> {
        let (_, chunks) = store.counts().map_err(|e| e.to_string())?;
        let clean = store.meta("index_clean").as_deref() == Some("1");
        if clean && path.exists() {
            if let Ok(idx) = new_index(dims) {
                if idx.load(path.to_str().unwrap()).is_ok() && idx.dimensions() == dims && idx.size() as u64 == chunks {
                    tracing::info!("loaded vector index ({} vectors)", idx.size());
                    return Ok(VectorIndex { index: Mutex::new(idx), path: path.to_path_buf(), dims: AtomicUsize::new(dims) });
                }
            }
            tracing::warn!("vector index file inconsistent, rebuilding");
        }
        let v = VectorIndex { index: Mutex::new(new_index(dims)?), path: path.to_path_buf(), dims: AtomicUsize::new(dims) };
        v.rebuild(store)?;
        Ok(v)
    }

    /// Vector length used from the next [`VectorIndex::rebuild`] on (the model changed).
    pub fn set_dims(&self, dims: usize) {
        self.dims.store(dims, Ordering::Relaxed);
    }

    pub fn rebuild(&self, store: &Store) -> Result<(), String> {
        let dims = self.dims.load(Ordering::Relaxed);
        let fresh = new_index(dims)?;
        let (_, n) = store.counts().map_err(|e| e.to_string())?;
        fresh.reserve((n as usize).max(1024) * 5 / 4).map_err(|e| e.to_string())?;
        let mut err = None;
        store
            .for_each_vector(|id, v| {
                if v.len() == dims {
                    if let Err(e) = fresh.add(id as u64, &v) {
                        err = Some(e.to_string());
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        if let Some(e) = err {
            return Err(e);
        }
        tracing::info!("rebuilt vector index ({} vectors)", fresh.size());
        *self.index.lock().unwrap() = fresh;
        Ok(())
    }

    pub fn add(&self, id: i64, v: &[f32]) -> Result<(), String> {
        let idx = self.index.lock().unwrap();
        if v.len() != idx.dimensions() {
            return Err(format!("vector has {} dimensions, the index {}", v.len(), idx.dimensions()));
        }
        if idx.size() + 1 > idx.capacity() {
            idx.reserve((idx.capacity() * 2).max(4096)).map_err(|e| e.to_string())?;
        }
        idx.add(id as u64, v).map_err(|e| e.to_string())
    }

    pub fn remove(&self, id: i64) {
        let _ = self.index.lock().unwrap().remove(id as u64);
    }

    /// Returns `(chunk id, cosine similarity)`, best first.
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(i64, f32)> {
        let idx = self.index.lock().unwrap();
        if idx.size() == 0 || q.len() != idx.dimensions() {
            return vec![];
        }
        match idx.search(q, k) {
            Ok(m) => m.keys.iter().zip(m.distances.iter()).map(|(k, d)| (*k as i64, 1.0 - d)).collect(),
            Err(e) => {
                tracing::error!("vector search failed: {e}");
                vec![]
            }
        }
    }

    pub fn len(&self) -> usize {
        self.index.lock().unwrap().size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Persists the index and records in the database that the file is consistent.
    pub fn save(&self, store: &Store) {
        let idx = self.index.lock().unwrap();
        let tmp = self.path.with_extension("usearch.tmp");
        if idx.save(tmp.to_str().unwrap()).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
            let _ = store.set_meta("index_clean", "1");
        }
    }

    /// Call before the first modification after a save.
    pub fn mark_dirty(store: &Store) {
        let _ = store.set_meta("index_clean", "0");
    }
}
