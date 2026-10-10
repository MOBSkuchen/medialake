//! File classification and content extraction (text chunks, image preparation).

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality {
    Text,
    Document,
    Image,
    Audio,
    Video,
    Other,
}

impl Modality {
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Document => "document",
            Modality::Image => "image",
            Modality::Audio => "audio",
            Modality::Video => "video",
            Modality::Other => "other",
        }
    }
}

const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "rst", "adoc", "org", "rs", "py", "js", "mjs", "ts", "tsx", "jsx", "c", "h", "cc", "cpp", "hpp", "cs", "java",
    "kt", "go", "rb", "php", "swift", "lua", "pl", "r", "scala", "dart", "json", "jsonl", "yaml", "yml", "toml", "ini", "cfg", "conf",
    "xml", "html", "htm", "css", "scss", "csv", "tsv", "log", "sql", "sh", "bat", "cmd", "ps1", "tex", "srt", "vtt", "rtf", "svg",
    "gradle", "cmake", "mk", "vue", "svelte", "proto", "tf",
];
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];
const AUDIO_EXT: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "avi", "mov", "webm", "wmv", "m4v"];
const DOC_EXT: &[&str] = &["pdf", "docx", "xlsx", "pptx", "odt"];

pub fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Classifies by extension only; unknown extensions come back as `Other` (the indexer sniffs those).
pub fn classify(path: &Path) -> Modality {
    let e = ext_of(path);
    let e = e.as_str();
    if TEXT_EXT.contains(&e) {
        Modality::Text
    } else if IMAGE_EXT.contains(&e) {
        Modality::Image
    } else if AUDIO_EXT.contains(&e) {
        Modality::Audio
    } else if VIDEO_EXT.contains(&e) {
        Modality::Video
    } else if DOC_EXT.contains(&e) {
        Modality::Document
    } else {
        Modality::Other
    }
}

/// Directories and files that are never indexed.
pub fn is_ignored(path: &Path) -> bool {
    for c in path.components() {
        if let std::path::Component::Normal(n) = c {
            let n = n.to_string_lossy();
            if matches!(n.as_ref(), ".git" | ".svn" | ".hg" | "node_modules" | "target" | "__pycache__" | ".idea" | ".vs" | "$RECYCLE.BIN" | "System Volume Information") {
                return true;
            }
        }
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    name.starts_with("~$")
        || name.starts_with('.')
        || lower.ends_with(".tmp")
        || lower.ends_with(".part")
        || lower.ends_with(".crdownload")
        || lower.ends_with(".swp")
        || lower == "thumbs.db"
        || lower == "desktop.ini"
}

/// Hidden / system attribute (Windows).
pub fn is_hidden_or_system(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        meta.file_attributes() & (HIDDEN | SYSTEM) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

pub fn hash_bytes(b: &[u8]) -> Vec<u8> {
    Sha256::digest(b).to_vec()
}

/// Streams a file through SHA-256.
pub fn hash_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_vec())
}

// ---------------- text ----------------

/// Upper bound on content chunks per file; text beyond it stays findable by name only.
pub const MAX_CHUNKS_PER_FILE: usize = 500;
const MIN_CHUNK_CHARS: usize = 1_500;
const MAX_CHUNK_CHARS: usize = 6_000;

#[derive(Debug, Clone)]
pub struct TextChunk {
    pub text: String,
    pub hash: Vec<u8>,
    pub byte_len: u64,
    /// Per-line (hash, length) pairs, little-endian u32 each.
    pub sig: Vec<u8>,
}

/// Reads a text file if it looks like text (UTF-8, or UTF-16 with BOM). Returns `None` for binaries.
pub fn read_text(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.by_ref().take(max_bytes).read_to_end(&mut buf).ok()?;
    decode_text(&buf)
}

pub fn decode_text(buf: &[u8]) -> Option<String> {
    if buf.starts_with(&[0xFF, 0xFE]) || buf.starts_with(&[0xFE, 0xFF]) {
        let le = buf[0] == 0xFF;
        let units: Vec<u16> =
            buf[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return String::from_utf16(&units).ok();
    }
    let b = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(buf);
    if b.iter().take(8192).any(|&x| x == 0) {
        return None;
    }
    match std::str::from_utf8(b) {
        Ok(s) => Some(s.to_string()),
        // a multi-byte char cut by the size limit is fine; anything else is a binary
        Err(e) if e.error_len().is_none() => Some(String::from_utf8_lossy(&b[..e.valid_up_to()]).into_owned()),
        Err(_) => None,
    }
}

/// Cheap check used for files without a known extension.
pub fn sniff_is_text(path: &Path) -> bool {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = vec![0u8; 8192];
    let n = f.read(&mut buf).unwrap_or(0);
    n > 0 && decode_text(&buf[..n]).is_some()
}

fn line_hash(l: &str) -> u32 {
    // FNV-1a
    let mut h = 0x811c9dc5u32;
    for b in l.trim().bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    h
}

pub fn line_sig(text: &str) -> Vec<u8> {
    let mut sig = Vec::new();
    for l in text.split_inclusive('\n') {
        sig.extend_from_slice(&line_hash(l).to_le_bytes());
        sig.extend_from_slice(&(l.len() as u32).to_le_bytes());
    }
    sig
}

/// Bytes that differ between two signature sets (lines added + lines removed).
pub fn sig_distance(old: &[u8], new: &[u8]) -> u64 {
    use std::collections::HashMap;
    let mut m: HashMap<u32, (i64, u64)> = HashMap::new(); // hash -> (count delta, line len)
    for p in old.chunks_exact(8) {
        let h = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let l = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as u64;
        let e = m.entry(h).or_insert((0, l));
        e.0 -= 1;
    }
    for p in new.chunks_exact(8) {
        let h = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let l = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as u64;
        let e = m.entry(h).or_insert((0, l));
        e.0 += 1;
    }
    m.values().map(|(c, l)| c.unsigned_abs() * l).sum()
}

/// Content-defined chunking on line boundaries: an edit only changes the chunks around it, so
/// unchanged chunks keep their hashes (and their vectors) across modifications.
pub fn chunk_text(s: &str) -> Vec<TextChunk> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut flush = |cur: &mut String, chunks: &mut Vec<TextChunk>| {
        if cur.trim().is_empty() {
            cur.clear();
            return;
        }
        let text = std::mem::take(cur);
        chunks.push(TextChunk { hash: hash_bytes(text.as_bytes()), byte_len: text.len() as u64, sig: line_sig(&text), text });
    };
    for line in s.split_inclusive('\n') {
        // a single enormous line (minified files) is cut at char boundaries
        if line.len() > MAX_CHUNK_CHARS {
            if !cur.is_empty() {
                flush(&mut cur, &mut chunks);
            }
            let mut start = 0;
            while start < line.len() {
                let mut end = (start + MAX_CHUNK_CHARS).min(line.len());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                cur.push_str(&line[start..end]);
                flush(&mut cur, &mut chunks);
                start = end;
            }
            continue;
        }
        cur.push_str(line);
        let boundary = cur.len() >= MIN_CHUNK_CHARS && line_hash(line) & 0x7 == 0;
        if boundary || cur.len() >= MAX_CHUNK_CHARS {
            flush(&mut cur, &mut chunks);
        }
    }
    flush(&mut cur, &mut chunks);
    chunks
}

pub fn snippet_of(text: &str) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    t.chars().take(240).collect()
}

// ---------------- images ----------------

pub const MAX_IMAGE_SIDE: u32 = 768;

pub struct PreparedImage {
    /// JPEG, downscaled so that neither side exceeds [`MAX_IMAGE_SIDE`].
    pub jpeg: Vec<u8>,
    /// Hash of the decoded (downscaled) pixels: independent of EXIF and other metadata.
    pub pixel_hash: Vec<u8>,
}

pub fn prepare_image(bytes: &[u8]) -> Result<PreparedImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    prepare_decoded(img)
}

pub fn prepare_decoded(img: image::DynamicImage) -> Result<PreparedImage, String> {
    let img = if img.width() > MAX_IMAGE_SIDE || img.height() > MAX_IMAGE_SIDE {
        img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let pixel_hash = hash_bytes(rgb.as_raw());
    let mut jpeg = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88);
    rgb.write_with_encoder(enc).map_err(|e| e.to_string())?;
    Ok(PreparedImage { jpeg, pixel_hash })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> String {
        (0..n).map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog number {}\n", i * 7919 % 1013)).collect()
    }

    #[test]
    fn chunks_are_stable_under_local_edits() {
        let a = sample(600);
        let ca = chunk_text(&a);
        assert!(ca.len() > 3);
        let mut b = a.clone();
        b.insert_str(0, "an inserted first line\n");
        let cb = chunk_text(&b);
        let ha: std::collections::HashSet<_> = ca.iter().map(|c| c.hash.clone()).collect();
        let shared = cb.iter().filter(|c| ha.contains(&c.hash)).count();
        assert!(shared >= ca.len() - 2, "only {shared} of {} chunks survived an edit at the top", ca.len());
    }

    #[test]
    fn change_measure() {
        let a = sample(50);
        let mut b = a.clone();
        b.push_str("one more line\n");
        assert_eq!(sig_distance(&line_sig(&a), &line_sig(&a)), 0);
        assert_eq!(sig_distance(&line_sig(&a), &line_sig(&b)), 14);
    }

    #[test]
    fn long_lines_and_binaries() {
        let long = "x".repeat(20_000);
        assert!(chunk_text(&long).len() >= 3);
        assert!(decode_text(&[0, 1, 2, 3]).is_none());
        assert_eq!(decode_text("häl".as_bytes()).unwrap(), "häl");
    }

    #[test]
    fn classify_and_ignore() {
        assert_eq!(classify(Path::new("a/b/C.JPG")), Modality::Image);
        assert_eq!(classify(Path::new("x.rs")), Modality::Text);
        assert!(is_ignored(Path::new("C:/p/node_modules/x/y.js")));
        assert!(is_ignored(Path::new("C:/p/~$doc.docx")));
        assert!(!is_ignored(Path::new("C:/p/report.docx")));
    }
}
