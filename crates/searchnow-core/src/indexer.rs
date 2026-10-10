//! The indexing pipeline: a deduplicating work queue, worker tasks that read/decode files and ask
//! the server for embeddings, and the re-embed rules (max size, min change, accumulating edits).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use searchnow_proto::{Embedding, TextItem, TextKind};
use tokio::sync::{Notify, broadcast};

use crate::client::{ClientError, EmbedClient, Lane};
use crate::config::Config;
use crate::events::Event;
use crate::extract::{self, Modality};
use crate::store::{FileRow, NewChunk, Store};
use crate::vindex::VectorIndex;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    /// Nothing to do (unchanged, ignored, or change below the threshold).
    Skipped,
    Indexed,
    Removed,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<PathBuf>,
    queued: HashSet<String>,
    /// Files being processed right now: a file is never processed by two workers at once.
    active: HashSet<String>,
}

pub struct Indexer {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) vindex: Arc<VectorIndex>,
    client: Arc<EmbedClient>,
    cfg: Arc<RwLock<Config>>,
    queue: Mutex<Queue>,
    notify: Notify,
    total: AtomicU64,
    done: AtomicU64,
    inflight: AtomicU64,
    dirty: AtomicBool,
    paused: AtomicBool,
    events: broadcast::Sender<Event>,
    current: Mutex<Option<String>>,
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn name_item(path: &Path) -> TextItem {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    TextItem { title: name, text: dir }
}

fn new_chunk(kind: &str, seq: i64, hash: Vec<u8>, byte_len: u64, snippet: String, sig: Vec<u8>, vector: Vec<f32>) -> NewChunk {
    NewChunk { kind: kind.into(), seq, chunk_hash: hash, byte_len, snippet, sig, vector }
}

/// What the file contributes besides its name vector.
enum Content {
    /// Content is not (or cannot be) embedded.
    NameOnly(&'static str),
    Text(Vec<extract::TextChunk>),
    Image(extract::PreparedImage),
    Audio(crate::extract_ext::PreparedAudio),
    Video(crate::video::PreparedVideo),
}

impl Indexer {
    pub fn new(
        store: Arc<Mutex<Store>>,
        vindex: Arc<VectorIndex>,
        client: Arc<EmbedClient>,
        cfg: Arc<RwLock<Config>>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Indexer> {
        Arc::new(Indexer {
            store,
            vindex,
            client,
            cfg,
            queue: Mutex::new(Queue::default()),
            notify: Notify::new(),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            events,
            current: Mutex::new(None),
        })
    }

    pub fn start_workers(self: &Arc<Self>, n: usize) {
        for i in 0..n {
            let me = self.clone();
            tokio::spawn(async move { me.worker(i).await });
        }
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(45));
            loop {
                tick.tick().await;
                me.save_if_dirty();
            }
        });
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
        if !p {
            self.notify.notify_waiters();
        }
    }

    pub fn enqueue(&self, path: impl Into<PathBuf>) {
        let p: PathBuf = path.into();
        let k = key(&p);
        {
            let mut q = self.queue.lock().unwrap();
            if !q.queued.insert(k) {
                return;
            }
            q.pending.push_back(p);
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_one();
    }

    pub fn queue_len(&self) -> usize {
        self.queue.lock().unwrap().pending.len()
    }

    pub fn is_busy(&self) -> bool {
        self.queue_len() > 0 || self.inflight.load(Ordering::Relaxed) > 0
    }

    pub fn save_if_dirty(&self) {
        if self.dirty.swap(false, Ordering::SeqCst) {
            let st = self.store.lock().unwrap();
            self.vindex.save(&st);
        }
    }

    fn progress(&self) {
        let queued = self.queue_len() as u64;
        let _ = self.events.send(Event::Progress {
            queued,
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            current: self.current.lock().unwrap().clone(),
        });
    }

    async fn worker(self: Arc<Self>, _id: usize) {
        loop {
            let next = {
                if self.paused.load(Ordering::Relaxed) {
                    None
                } else {
                    let mut q = self.queue.lock().unwrap();
                    let idx = q.pending.iter().position(|p| !q.active.contains(&key(p)));
                    let p = idx.and_then(|i| q.pending.remove(i));
                    if let Some(p) = &p {
                        q.queued.remove(&key(p));
                        q.active.insert(key(p));
                        // counted while still holding the lock so is_busy() never sees a gap
                        self.inflight.fetch_add(1, Ordering::SeqCst);
                    }
                    p
                }
            };
            let Some(path) = next else {
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                continue;
            };
            *self.current.lock().unwrap() = Some(path.to_string_lossy().into_owned());
            self.progress();
            let res = self.process(&path).await;
            self.queue.lock().unwrap().active.remove(&key(&path));
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            match res {
                Ok(_) => {
                    self.done.fetch_add(1, Ordering::Relaxed);
                }
                Err(ClientError::Unavailable(why)) => {
                    tracing::warn!("embedding server unavailable ({why}); will retry {}", path.display());
                    let _ = self.events.send(Event::Error { message: format!("Embedding server unavailable: {why}") });
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    self.total.fetch_sub(1, Ordering::Relaxed); // re-added by enqueue
                    self.enqueue(path);
                }
                Err(ClientError::Rejected(why)) => {
                    tracing::error!("embedding rejected for {}: {why}", path.display());
                    self.done.fetch_add(1, Ordering::Relaxed);
                }
            }
            if !self.is_busy() {
                *self.current.lock().unwrap() = None;
                self.save_if_dirty();
                self.progress();
                let (files, chunks) = self.store.lock().unwrap().counts().unwrap_or((0, 0));
                let _ = self.events.send(Event::Idle { files, chunks });
                self.total.store(0, Ordering::Relaxed);
                self.done.store(0, Ordering::Relaxed);
            } else if self.done.load(Ordering::Relaxed) % 5 == 0 {
                self.progress();
            }
        }
    }

    fn mark_dirty(&self) {
        if !self.dirty.swap(true, Ordering::SeqCst) {
            let st = self.store.lock().unwrap();
            VectorIndex::mark_dirty(&st);
        }
    }

    /// Removes a file (or everything below a directory) from the store and index.
    pub fn remove_path(&self, path: &Path) -> usize {
        let p = path.to_string_lossy().into_owned();
        let ids = {
            let st = self.store.lock().unwrap();
            let mut ids = st.delete_file(&p).unwrap_or_default();
            ids.extend(st.delete_under(&format!("{}\\", p.trim_end_matches('\\'))).unwrap_or_default());
            ids
        };
        if !ids.is_empty() {
            self.mark_dirty();
            for id in &ids {
                self.vindex.remove(*id);
            }
        }
        ids.len()
    }

    /// A file moved/renamed: update the path and refresh the (cheap) name vector only.
    pub fn rename_path(&self, from: &Path, to: &Path) {
        let (f, t) = (from.to_string_lossy().into_owned(), to.to_string_lossy().into_owned());
        let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let moved = {
            let st = self.store.lock().unwrap();
            let mut moved: Vec<String> = vec![];
            if st.rename_file(&f, &t, &name).unwrap_or(false) {
                moved.push(t.clone());
            }
            moved.extend(st.rename_prefix(f.trim_end_matches('\\'), t.trim_end_matches('\\')).unwrap_or_default());
            moved
        };
        if moved.is_empty() {
            self.enqueue(to); // unknown to us: treat as new
        }
        for m in moved {
            self.enqueue(m);
        }
    }

    pub async fn process(&self, path: &Path) -> Result<Outcome, ClientError> {
        let md = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(_) => {
                return Ok(if self.remove_path(path) > 0 { Outcome::Removed } else { Outcome::Skipped });
            }
        };
        if md.is_dir() {
            return Ok(Outcome::Skipped);
        }
        if extract::is_ignored(path) || extract::is_hidden_or_system(&md) {
            return Ok(if self.remove_path(path) > 0 { Outcome::Removed } else { Outcome::Skipped });
        }
        let cfg = self.cfg.read().unwrap().clone();
        let (size, mtime) = (md.len(), mtime_secs(&md));
        let path_str = path.to_string_lossy().into_owned();
        let existing = self.store.lock().unwrap().file_by_path(&path_str).ok().flatten();

        let mut modality = extract::classify(path);
        let too_large = size > cfg.max_file_bytes;
        if modality == Modality::Other && size > 0 && size < 4 * 1024 * 1024 {
            let p = path.to_path_buf();
            if tokio::task::spawn_blocking(move || extract::sniff_is_text(&p)).await.unwrap_or(false) {
                modality = Modality::Text;
            }
        }

        let name_hash = extract::hash_bytes(path_str.to_lowercase().as_bytes());
        let existing_chunks = match &existing {
            Some(f) => self.store.lock().unwrap().chunks_of(f.id).unwrap_or_default(),
            None => vec![],
        };
        let old_name = existing_chunks.iter().find(|c| c.kind == "name");
        let need_name = old_name.map(|c| c.chunk_hash != name_hash).unwrap_or(true);

        if let Some(f) = &existing {
            if f.size == size && f.mtime == mtime && f.modality == modality.as_str() && f.status != "error" && !need_name {
                return Ok(Outcome::Skipped);
            }
        }

        // ---- decide what to read/embed for the content ----
        let content_wanted = !too_large
            && match modality {
                Modality::Text | Modality::Image | Modality::Audio | Modality::Document => true,
                Modality::Video => cfg.video_enabled,
                Modality::Other => false,
            };
        let had_content = existing_chunks.iter().any(|c| c.kind != "name");

        let (content, err_note): (Content, Option<String>) = if !content_wanted {
            let why = if too_large { "too large" } else { "unsupported" };
            (Content::NameOnly(why), None)
        } else {
            let p = path.to_path_buf();
            let max = cfg.max_file_bytes;
            let audio_rate = self.client.server_info().map(|i| i.audio_sample_rate).filter(|r| *r > 0).unwrap_or(16_000);
            let prepared = tokio::task::spawn_blocking(move || -> Result<Content, String> {
                lower_priority();
                match modality {
                    Modality::Text => match extract::read_text(&p, max) {
                        Some(t) => {
                            let mut chunks = extract::chunk_text(&t);
                            chunks.truncate(extract::MAX_CHUNKS_PER_FILE);
                            Ok(Content::Text(chunks))
                        }
                        None => Ok(Content::NameOnly("binary")),
                    },
                    Modality::Image => {
                        let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
                        extract::prepare_image(&bytes).map(Content::Image)
                    }
                    Modality::Document => match crate::extract_ext::document_text(&p, max) {
                        Some(t) => {
                            let mut chunks = extract::chunk_text(&t);
                            chunks.truncate(extract::MAX_CHUNKS_PER_FILE);
                            Ok(Content::Text(chunks))
                        }
                        None => Ok(Content::NameOnly("no text")),
                    },
                    Modality::Audio => crate::extract_ext::prepare_audio(&p, audio_rate).map(Content::Audio),
                    Modality::Video => crate::video::prepare_video(&p).map(Content::Video),
                    _ => Ok(Content::NameOnly("unsupported")),
                }
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match prepared {
                Ok(c) => (c, None),
                Err(e) => (Content::NameOnly("error"), Some(e)),
            }
        };

        // ---- plan: which chunks to keep, drop, embed ----
        let mut keep_ids: Vec<i64> = vec![];
        let mut drop_ids: Vec<i64> = vec![];
        let mut to_embed_text: Vec<(TextItem, NewChunk)> = vec![];
        let mut to_embed_image: Option<(Vec<u8>, NewChunk)> = None;
        let mut to_embed_audio: Vec<(Vec<f32>, NewChunk)> = vec![];
        let mut to_embed_video: Option<(Vec<Vec<u8>>, NewChunk)> = None;
        let mut embedded_size = existing.as_ref().map(|f| f.embedded_size).unwrap_or(0);
        let mut content_hash = existing.as_ref().and_then(|f| f.content_hash.clone());
        let mut media_hash = existing.as_ref().and_then(|f| f.media_hash.clone());
        let mut status = "ok".to_string();
        let mut content_embedded = false;
        let min_change = cfg.min_change_bytes;

        match &content {
            Content::NameOnly(why) => {
                // content that cannot be (or is no longer) embedded: drop stale content vectors
                drop_ids.extend(existing_chunks.iter().filter(|c| c.kind != "name").map(|c| c.id));
                status = if *why == "error" { "error".into() } else { "name_only".into() };
                if had_content {
                    embedded_size = 0;
                    content_hash = None;
                    media_hash = None;
                }
            }
            Content::Text(chunks) => {
                let old: Vec<_> = existing_chunks.iter().filter(|c| c.kind == "text").collect();
                let old_hashes: HashSet<&[u8]> = old.iter().map(|c| c.chunk_hash.as_slice()).collect();
                let new_hashes: HashSet<&[u8]> = chunks.iter().map(|c| c.hash.as_slice()).collect();
                // measure the change against the last embed, at line granularity
                let mut old_sig: Vec<u8> = vec![];
                for c in old.iter().filter(|c| !new_hashes.contains(c.chunk_hash.as_slice())) {
                    old_sig.extend_from_slice(&c.sig);
                }
                let mut new_sig: Vec<u8> = vec![];
                for c in chunks.iter().filter(|c| !old_hashes.contains(c.hash.as_slice())) {
                    new_sig.extend_from_slice(&c.sig);
                }
                let changed = extract::sig_distance(&old_sig, &new_sig);
                let first = old.is_empty();
                tracing::debug!("{}: changed {changed} B (min {min_change}), old chunks {}, new chunks {}", path.display(), old.len(), chunks.len());
                if first || changed >= min_change {
                    for c in &old {
                        if new_hashes.contains(c.chunk_hash.as_slice()) {
                            keep_ids.push(c.id);
                        } else {
                            drop_ids.push(c.id);
                        }
                    }
                    for (i, c) in chunks.iter().enumerate() {
                        if !old_hashes.contains(c.hash.as_slice()) {
                            let item = TextItem { title: name_item(path).title, text: c.text.clone() };
                            let snippet = extract::snippet_of(&c.text);
                            to_embed_text.push((item, new_chunk("text", i as i64, c.hash.clone(), c.byte_len, snippet, c.sig.clone(), vec![])));
                        }
                    }
                    embedded_size = size;
                    content_embedded = true;
                } else {
                    // below the threshold: keep the old vectors; the change accumulates
                    keep_ids.extend(old.iter().map(|c| c.id));
                    tracing::debug!("{}: {changed} B changed < {min_change} B, not re-embedding", path.display());
                }
            }
            Content::Image(img) => {
                let same = existing.as_ref().and_then(|f| f.media_hash.as_deref()) == Some(img.pixel_hash.as_slice());
                let old: Vec<_> = existing_chunks.iter().filter(|c| c.kind == "image").collect();
                if same && !old.is_empty() {
                    keep_ids.extend(old.iter().map(|c| c.id)); // only metadata changed
                } else {
                    drop_ids.extend(old.iter().map(|c| c.id));
                    to_embed_image = Some((
                        img.jpeg.clone(),
                        new_chunk("image", 0, img.pixel_hash.clone(), size, String::new(), vec![], vec![]),
                    ));
                    media_hash = Some(img.pixel_hash.clone());
                    embedded_size = size;
                    content_embedded = true;
                }
            }
            Content::Audio(a) => {
                let same = existing.as_ref().and_then(|f| f.media_hash.as_deref()) == Some(a.hash.as_slice());
                let old: Vec<_> = existing_chunks.iter().filter(|c| c.kind == "audio").collect();
                if same && !old.is_empty() {
                    keep_ids.extend(old.iter().map(|c| c.id));
                } else {
                    drop_ids.extend(old.iter().map(|c| c.id));
                    for (i, seg) in a.segments.iter().enumerate() {
                        let h = extract::hash_bytes(&a.hash.iter().copied().chain((i as u32).to_le_bytes()).collect::<Vec<u8>>());
                        to_embed_audio.push((seg.clone(), new_chunk("audio", i as i64, h, size, String::new(), vec![], vec![])));
                    }
                    media_hash = Some(a.hash.clone());
                    embedded_size = size;
                    content_embedded = true;
                }
            }
            Content::Video(v) => {
                let same = existing.as_ref().and_then(|f| f.media_hash.as_deref()) == Some(v.hash.as_slice());
                let old: Vec<_> = existing_chunks.iter().filter(|c| c.kind == "video").collect();
                if same && !old.is_empty() {
                    keep_ids.extend(old.iter().map(|c| c.id));
                } else {
                    drop_ids.extend(old.iter().map(|c| c.id));
                    to_embed_video = Some((v.frames.clone(), new_chunk("video", 0, v.hash.clone(), size, String::new(), vec![], vec![])));
                    media_hash = Some(v.hash.clone());
                    embedded_size = size;
                    content_embedded = true;
                }
            }
        }
        let _ = &mut content_hash;
        if let Some(e) = err_note {
            tracing::debug!("{}: {e}", path.display());
        }

        // ---- embed (all network I/O happens before any store change) ----
        let mut text_items: Vec<TextItem> = vec![];
        let mut name_slot = None;
        if need_name {
            name_slot = Some(text_items.len());
            text_items.push(name_item(path));
        }
        let content_text_start = text_items.len();
        text_items.extend(to_embed_text.iter().map(|(i, _)| i.clone()));

        let mut vectors: Vec<Option<Vec<f32>>> = vec![];
        for batch in text_items.chunks(32) {
            let res = self.client.embed_text(Lane::Bulk, TextKind::Document, batch.to_vec()).await?;
            vectors.extend(res.into_iter().map(|e| e.into_result().ok()));
        }
        let mut image_vec: Option<Vec<f32>> = None;
        if let Some((jpeg, _)) = &to_embed_image {
            let res = self.client.embed_images(vec![jpeg.clone()]).await?;
            if let Some(Embedding::Vector(v)) = res.into_iter().next() {
                image_vec = Some(v);
            } else {
                status = "error".into();
            }
        }

        let mut audio_vecs: Vec<Option<Vec<f32>>> = vec![];
        if !to_embed_audio.is_empty() {
            let segs: Vec<Vec<f32>> = to_embed_audio.iter().map(|(s, _)| s.clone()).collect();
            let res = self.client.embed_audio(segs).await?;
            for e in &res {
                if let Embedding::Failed(m) = e {
                    tracing::warn!("{}: audio embedding failed: {m}", path.display());
                }
            }
            audio_vecs = res.into_iter().map(|e| e.into_result().ok()).collect();
            if audio_vecs.iter().all(|v| v.is_none()) {
                status = "error".into();
            }
        }
        let mut video_vec: Option<Vec<f32>> = None;
        if let Some((frames, _)) = &to_embed_video {
            let res = self.client.embed_video(vec![frames.clone()]).await?;
            match res.into_iter().next() {
                Some(Embedding::Vector(v)) => video_vec = Some(v),
                other => {
                    tracing::warn!("{}: video embedding failed: {:?}", path.display(), other);
                    status = "error".into();
                }
            }
        }

        // ---- commit ----
        let mut added: Vec<(i64, Vec<f32>)> = vec![];
        let mut removed: Vec<i64> = drop_ids.clone();
        {
            self.mark_dirty();
            let st = self.store.lock().unwrap();
            let r = (|| -> rusqlite::Result<()> {
                st.begin()?;
                let row = FileRow {
                    id: existing.as_ref().map(|f| f.id).unwrap_or(0),
                    path: path_str.clone(),
                    name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    size,
                    mtime,
                    modality: modality.as_str().into(),
                    embedded_size: if content_embedded || existing.is_some() { embedded_size } else { 0 },
                    content_hash: content_hash.clone(),
                    media_hash: media_hash.clone(),
                    status: status.clone(),
                    indexed_at: now_secs(),
                };
                let file_id = st.upsert_file(&row)?;
                // name vector: replace in place
                if let Some(slot) = name_slot {
                    if let Some(v) = vectors[slot].clone() {
                        let nc = new_chunk("name", 0, name_hash.clone(), 0, String::new(), vec![], v.clone());
                        if let Some(old) = old_name {
                            st.replace_chunk(old.id, &nc)?;
                            removed.push(old.id);
                            added.push((old.id, v));
                        } else {
                            let id = st.insert_chunk(file_id, &nc)?;
                            added.push((id, v));
                        }
                    }
                }
                st.delete_chunks(&drop_ids)?;
                for (i, (_, mut nc)) in to_embed_text.into_iter().enumerate() {
                    if let Some(Some(v)) = vectors.get(content_text_start + i) {
                        nc.vector = v.clone();
                        let id = st.insert_chunk(file_id, &nc)?;
                        added.push((id, v.clone()));
                    }
                }
                if let (Some((_, mut nc)), Some(v)) = (to_embed_image, image_vec) {
                    nc.vector = v.clone();
                    let id = st.insert_chunk(file_id, &nc)?;
                    added.push((id, v));
                }
                for (i, (_, mut nc)) in to_embed_audio.into_iter().enumerate() {
                    if let Some(Some(v)) = audio_vecs.get(i) {
                        nc.vector = v.clone();
                        let id = st.insert_chunk(file_id, &nc)?;
                        added.push((id, v.clone()));
                    }
                }
                if let (Some((_, mut nc)), Some(v)) = (to_embed_video, video_vec) {
                    nc.vector = v.clone();
                    let id = st.insert_chunk(file_id, &nc)?;
                    added.push((id, v));
                }
                st.commit()
            })();
            if let Err(e) = r {
                st.rollback();
                tracing::error!("store update failed for {}: {e}", path.display());
                return Ok(Outcome::Skipped);
            }
        }
        for id in removed {
            self.vindex.remove(id);
        }
        for (id, v) in added {
            if let Err(e) = self.vindex.add(id, &v) {
                tracing::error!("index add failed: {e}");
            }
        }
        let _ = keep_ids;
        Ok(Outcome::Indexed)
    }

    /// Walks all roots; queues new/changed files and removes entries for vanished ones.
    pub async fn reconcile(self: &Arc<Self>) -> (u64, u64) {
        let cfg = self.cfg.read().unwrap().clone();
        let me = self.clone();
        let (queued, removed) = tokio::task::spawn_blocking(move || {
            lower_priority();
            let seen_db = me.store.lock().unwrap().all_seen().unwrap_or_default();
            let seen_lower: std::collections::HashMap<String, (i64, u64, i64, String)> =
                seen_db.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
            let mut on_disk: HashSet<String> = HashSet::new();
            let mut queued = 0u64;
            for d in &cfg.dirs {
                let mut w = walkdir::WalkDir::new(&d.path).follow_links(false);
                if !d.recursive {
                    w = w.max_depth(1);
                }
                let it = w.into_iter().filter_entry(|e| !(e.file_type().is_dir() && extract::is_ignored(e.path())));
                for e in it.flatten() {
                    if !e.file_type().is_file() || extract::is_ignored(e.path()) {
                        continue;
                    }
                    let Ok(md) = e.metadata() else { continue };
                    if extract::is_hidden_or_system(&md) {
                        continue;
                    }
                    let p = e.path().to_string_lossy().into_owned();
                    let lower = p.to_lowercase();
                    on_disk.insert(lower.clone());
                    let changed = match seen_lower.get(&lower) {
                        Some((_, size, mtime, status)) => *size != md.len() || *mtime != mtime_secs(&md) || status == "error",
                        None => true,
                    };
                    if changed {
                        me.enqueue(e.path().to_path_buf());
                        queued += 1;
                    }
                }
            }
            // anything in the store that is no longer reachable
            let mut removed = 0u64;
            for p in seen_db.keys() {
                if !on_disk.contains(&p.to_lowercase()) {
                    me.remove_path(Path::new(p));
                    removed += 1;
                }
            }
            (queued, removed)
        })
        .await
        .unwrap_or((0, 0));
        tracing::info!("reconcile: {queued} files queued, {removed} removed");
        (queued, removed)
    }
}

#[cfg(windows)]
fn lower_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(not(windows))]
fn lower_priority() {}
