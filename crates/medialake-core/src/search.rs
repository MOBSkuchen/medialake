//! Query-time ranking: ANN hits -> per-file scores -> calibrated confidence.

use std::collections::HashMap;

use serde::Serialize;

use crate::store::ChunkInfo;

#[derive(Serialize, Clone, Debug)]
pub struct SearchHit {
    pub path: String,
    pub name: String,
    pub modality: String,
    /// Calibrated confidence in 0..1.
    pub score: f32,
    /// Best raw cosine similarity of any chunk of the file.
    pub raw: f32,
    /// Best matching text snippet (text/document files).
    pub snippet: String,
    /// Which part matched: "name", "text", "image", ...
    pub matched: String,
    /// Chunk/segment index of the best content match.
    pub segment: i64,
    pub mtime: i64,
    pub size: u64,
    /// The file is on this computer. False for a placeholder (OneDrive, a synced folder): showing a thumbnail
    /// of it would download it, so the UI shows the file's icon instead.
    pub available: bool,
}

/// Raw cosines are model specific (EmbeddingGemma: unrelated ~0.50-0.66, relevant ~0.65-0.80;
/// Qwen3-VL-Embedding: ~0.32-0.52 / ~0.59-0.78): map `[floor, ceiling]` linearly to `[0, 1]`.
pub fn confidence(raw: f32, floor: f32, ceiling: f32) -> f32 {
    ((raw - floor) / (ceiling - floor).max(1e-3)).clamp(0.0, 1.0)
}

pub struct Ranking {
    pub floor: f32,
    pub ceiling: f32,
    pub threshold: f32,
    /// Raw-cosine distance below the best hit after which results are dropped.
    pub relative_cut: f32,
    pub max_results: usize,
}

/// `hits`: `(chunk id, cosine)`; `info`: metadata for those chunks.
pub fn rank(hits: &[(i64, f32)], info: &HashMap<i64, ChunkInfo>, r: &Ranking) -> Vec<SearchHit> {
    struct Acc {
        best: f32,
        best_chunk: i64,
        content_best: f32,
        content_chunk: Option<i64>,
    }
    let mut by_file: HashMap<i64, Acc> = HashMap::new();
    for (cid, cos) in hits {
        let Some(ci) = info.get(cid) else { continue };
        let a = by_file.entry(ci.file_id).or_insert(Acc { best: f32::MIN, best_chunk: *cid, content_best: f32::MIN, content_chunk: None });
        if *cos > a.best {
            a.best = *cos;
            a.best_chunk = *cid;
        }
        if ci.kind != "name" && *cos > a.content_best {
            a.content_best = *cos;
            a.content_chunk = Some(*cid);
        }
    }
    let mut out: Vec<SearchHit> = by_file
        .into_values()
        .filter_map(|a| {
            let best = info.get(&a.best_chunk)?;
            let content = a.content_chunk.and_then(|c| info.get(&c));
            let score = confidence(a.best, r.floor, r.ceiling);
            (score >= r.threshold).then(|| SearchHit {
                path: best.path.clone(),
                name: best.name.clone(),
                modality: best.modality.clone(),
                score,
                raw: a.best,
                snippet: content.map(|c| c.snippet.clone()).unwrap_or_default(),
                matched: best.kind.clone(),
                segment: content.map(|c| c.seq).unwrap_or(0),
                mtime: best.mtime,
                size: best.size,
                available: true,
            })
        })
        .collect();
    out.sort_by(|a, b| b.raw.partial_cmp(&a.raw).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(best) = out.first().map(|h| h.raw) {
        out.retain(|h| h.raw >= best - r.relative_cut);
    }
    out.truncate(r.max_results);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_mapping() {
        assert_eq!(confidence(0.5, 0.6, 0.8), 0.0);
        assert_eq!(confidence(0.9, 0.6, 0.8), 1.0);
        assert!((confidence(0.7, 0.6, 0.8) - 0.5).abs() < 1e-6);
    }
}
