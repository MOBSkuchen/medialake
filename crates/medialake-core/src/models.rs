//! Model files: presence checks and resumable, SHA-256-verified downloads.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use medialake_proto::models::{Model, ModelFile, ModelSet, Quality, model_set};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::events::Event;

#[derive(Serialize, Clone, Debug)]
pub struct QualityStatus {
    pub quality: String,
    pub downloaded: bool,
    pub size: u64,
    pub files: Vec<String>,
}

pub fn file_present(dir: &Path, f: &ModelFile) -> bool {
    std::fs::metadata(dir.join(f.file_name)).map(|m| m.len() == f.size).unwrap_or(false)
}

pub fn is_downloaded(dir: &Path, m: Model, q: Quality) -> bool {
    let s = model_set(m, q);
    file_present(dir, &s.model) && file_present(dir, &s.mmproj)
}

/// The quality tiers of one model.
pub fn status(dir: &Path, m: Model) -> Vec<QualityStatus> {
    Quality::ALL
        .iter()
        .map(|q| {
            let s = model_set(m, *q);
            QualityStatus {
                quality: q.as_str().into(),
                downloaded: is_downloaded(dir, m, *q),
                size: s.total_size(),
                files: vec![s.model.file_name.into(), s.mmproj.file_name.into()],
            }
        })
        .collect()
}

/// Downloads the missing files of `set` into `dir` (blocking; call from `spawn_blocking`).
pub fn download_blocking(dir: &Path, set: ModelSet, cancel: Arc<AtomicBool>, events: broadcast::Sender<Event>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let files = [set.model, set.mmproj];
    let grand_total: u64 = files.iter().filter(|f| !file_present(dir, f)).map(|f| f.size).sum();
    let mut finished_before = 0u64;
    for f in files {
        if file_present(dir, &f) {
            continue;
        }
        download_one(dir, &f, &cancel, &events, finished_before, grand_total)?;
        finished_before += f.size;
    }
    Ok(())
}

fn download_one(
    dir: &Path,
    f: &ModelFile,
    cancel: &AtomicBool,
    events: &broadcast::Sender<Event>,
    base: u64,
    grand_total: u64,
) -> Result<(), String> {
    let part: PathBuf = dir.join(format!("{}.part", f.file_name));
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > f.size {
        let _ = std::fs::remove_file(&part);
        have = 0;
    }
    let mut req = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).build().get(f.url);
    if have > 0 {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let resp = req.call().map_err(|e| format!("download {}: {e}", f.file_name))?;
    let resumed = resp.status() == 206;
    if !resumed {
        have = 0;
    }
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)
        .map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = have;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("download cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("download {}: {e}", f.file_name))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if last.elapsed() > Duration::from_millis(250) {
            last = Instant::now();
            let _ = events.send(Event::Download { file: f.file_name.into(), done: base + done, total: grand_total.max(1) });
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    if done != f.size {
        return Err(format!("{}: incomplete download ({done} of {} bytes)", f.file_name, f.size));
    }
    // verify
    let mut h = Sha256::new();
    let mut file = std::fs::File::open(&part).map_err(|e| e.to_string())?;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if hex != f.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{}: checksum mismatch", f.file_name));
    }
    std::fs::rename(&part, dir.join(f.file_name)).map_err(|e| e.to_string())?;
    let _ = events.send(Event::Download { file: f.file_name.into(), done: base + f.size, total: grand_total.max(1) });
    Ok(())
}
