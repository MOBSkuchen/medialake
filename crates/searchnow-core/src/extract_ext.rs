//! Extractors for documents (PDF, Office/OpenDocument) and audio.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

// ---------------- documents ----------------

/// Plain text of a PDF / docx / pptx / xlsx / odt file, or `None` when nothing could be extracted.
pub fn document_text(path: &Path, max_bytes: u64) -> Option<String> {
    let ext = crate::extract::ext_of(path);
    let text = match ext.as_str() {
        "pdf" => pdf_text(path, max_bytes),
        "docx" | "pptx" | "xlsx" | "odt" => office_text(path, &ext),
        _ => None,
    }?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn pdf_text(path: &Path, max_bytes: u64) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(max_bytes).read_to_end(&mut buf).ok()?;
    // pdf-extract panics on some malformed documents; a bad PDF must not take a worker down
    std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&buf).ok()).ok().flatten()
}

fn office_text(path: &Path, ext: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut parts: Vec<String> = zip
        .file_names()
        .filter(|n| match ext {
            "docx" => *n == "word/document.xml" || n.starts_with("word/header") || n.starts_with("word/footer") || *n == "word/footnotes.xml",
            "pptx" => n.starts_with("ppt/slides/slide") && n.ends_with(".xml") || n.starts_with("ppt/notesSlides/notesSlide"),
            "xlsx" => *n == "xl/sharedStrings.xml",
            "odt" => *n == "content.xml",
            _ => false,
        })
        .map(|s| s.to_string())
        .collect();
    parts.sort_by_key(|n| natural_key(n));
    let mut out = String::new();
    for name in parts {
        let mut f = zip.by_name(&name).ok()?;
        let mut xml = String::new();
        // cap each part so a pathological archive cannot exhaust memory
        f.by_ref().take(64 * 1024 * 1024).read_to_string(&mut xml).ok()?;
        out.push_str(&xml_to_text(&xml));
        out.push('\n');
    }
    Some(out)
}

fn natural_key(s: &str) -> (String, u32) {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    (s.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_alphabetic()).to_string(), digits.parse().unwrap_or(0))
}

/// Strips XML tags; paragraph / line / row ends become newlines, entities are decoded.
pub fn xml_to_text(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 3);
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let end = xml[i..].find('>').map(|e| i + e).unwrap_or(bytes.len() - 1);
            let tag = &xml[i + 1..end];
            let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
            let closing = tag.starts_with('/');
            let breaks = matches!(name, "w:p" | "a:p" | "text:p" | "text:h" | "si" | "row" | "w:tr")
                && (closing || name == "si");
            let space = matches!(name, "w:br" | "w:tab" | "text:tab" | "text:line-break" | "text:s" | "w:t");
            if breaks && closing {
                out.push('\n');
            } else if space && !closing {
                out.push(' ');
            }
            i = end + 1;
        } else if bytes[i] == b'&' {
            let end = xml[i..].find(';').map(|e| i + e).unwrap_or(i);
            let ent = &xml[i + 1..end.max(i + 1)];
            match ent {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                e if e.starts_with('#') => {
                    let n = if let Some(h) = e.strip_prefix("#x") { u32::from_str_radix(h, 16).ok() } else { e[1..].parse().ok() };
                    if let Some(c) = n.and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                _ => {}
            }
            i = end + 1;
        } else {
            let next = xml[i..].find(['<', '&']).map(|e| i + e).unwrap_or(bytes.len());
            out.push_str(&xml[i..next]);
            i = next;
        }
    }
    // collapse blank-line runs
    let mut cleaned = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        cleaned.push_str(l);
        cleaned.push('\n');
    }
    cleaned
}

// ---------------- audio ----------------

pub const SEGMENT_SECONDS: usize = 60;
pub const MAX_AUDIO_SEGMENTS: usize = 20;

pub struct PreparedAudio {
    /// Mono f32 segments of at most [`SEGMENT_SECONDS`] each.
    pub segments: Vec<Vec<f32>>,
    /// Hash of the decoded samples (independent of tags / container).
    pub hash: Vec<u8>,
    pub seconds: f32,
}

/// Decodes any supported audio file to mono and resamples to `target_rate`.
pub fn prepare_audio(path: &Path, target_rate: u32) -> Result<PreparedAudio, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    let ext = crate::extract::ext_of(path);
    if !ext.is_empty() {
        hint.with_extension(&ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported audio: {e}"))?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("unsupported codec: {e}"))?;

    let seg_len = SEGMENT_SECONDS * target_rate as usize;
    let max_samples = seg_len * MAX_AUDIO_SEGMENTS;
    let mut samples: Vec<f32> = Vec::new(); // resampled mono
    let mut hasher = Sha256::new();
    let mut src_rate = 0u32;
    // box-filter resampler state
    let (mut acc, mut cnt, mut phase) = (0.0f32, 0u32, 0.0f64);

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) => continue, // skip a corrupt frame
            Err(e) => return Err(e.to_string()),
        };
        let spec = *decoded.spec();
        src_rate = spec.rate;
        let ch = spec.channels.count().max(1);
        let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sb.copy_interleaved_ref(decoded);
        let step = src_rate as f64 / target_rate as f64; // source samples per output sample
        for frame in sb.samples().chunks_exact(ch) {
            let s = frame.iter().sum::<f32>() / ch as f32;
            acc += s;
            cnt += 1;
            phase += 1.0;
            if phase >= step {
                phase -= step;
                let v = acc / cnt as f32;
                acc = 0.0;
                cnt = 0;
                hasher.update(((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                samples.push(v);
            }
        }
        if samples.len() >= max_samples {
            break;
        }
    }
    if samples.is_empty() || src_rate == 0 {
        return Err("no audio samples".into());
    }
    samples.truncate(max_samples);
    let seconds = samples.len() as f32 / target_rate as f32;
    let mut segments: Vec<Vec<f32>> = samples.chunks(seg_len).map(|c| c.to_vec()).collect();
    // drop a trailing sliver (< 1 s) unless it is all there is
    if segments.len() > 1 && segments.last().is_some_and(|s| s.len() < target_rate as usize) {
        segments.pop();
    }
    Ok(PreparedAudio { segments, hash: hasher.finalize().to_vec(), seconds })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_text() {
        let x = "<w:p><w:r><w:t>Hello &amp; welcome</w:t></w:r></w:p><w:p><w:r><w:t>Second</w:t></w:r></w:p>";
        assert_eq!(xml_to_text(x).trim(), "Hello & welcome\nSecond");
    }
}
