//! Pure helpers for what the platform asks of a provider.

/// The platform's transfer granularity: offsets and lengths of TRANSFER_DATA must be multiples of this,
/// except for the piece that ends at the end of the file.
pub const ALIGN: u64 = 4096;

/// The `(offset, length)` pieces that answer a request for `[offset, offset + length)` of a file of
/// `size` bytes. The start is rounded down and the end up to [`ALIGN`] (the end is clamped to `size`, so
/// the last piece may be shorter than the alignment); no piece is longer than `max_chunk` bytes
/// (rounded down to a multiple of [`ALIGN`], at least one block).
pub fn plan_chunks(offset: u64, length: u64, size: u64, max_chunk: u64) -> Vec<(u64, u64)> {
    if offset >= size || length == 0 {
        return vec![];
    }
    let start = offset & !(ALIGN - 1);
    let wanted_end = offset.saturating_add(length);
    let end = wanted_end.div_ceil(ALIGN).saturating_mul(ALIGN).min(size);
    let step = ((max_chunk / ALIGN).max(1)) * ALIGN;
    let mut out = vec![];
    let mut at = start;
    while at < end {
        let to = (at + step).min(end);
        out.push((at, to - at));
        at = to;
    }
    out
}

/// The sync-relative path (`/`-separated, no leading slash) of `normalized`, a callback's
/// `NormalizedPath` (volume-relative, `\Users\me\MediaLake\Photos\a\b.jpg`), given the sync root as it was
/// registered (`C:\Users\me\MediaLake\Photos`). `None` if the file is not under the root.
/// The comparison ignores case, as NTFS does.
pub fn rel_from_normalized(root: &str, normalized: &str) -> Option<String> {
    let root = root.strip_prefix(r"\\?\").unwrap_or(root);
    // drop the drive: the platform's path is relative to the volume
    let root = match root.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => &root[2..],
        _ => root,
    };
    let root = root.trim_end_matches('\\');
    let norm = normalized.trim_end_matches('\\');
    let (r, n): (Vec<char>, Vec<char>) = (root.chars().collect(), norm.chars().collect());
    if n.len() < r.len() {
        return None;
    }
    for (a, b) in r.iter().zip(n.iter()) {
        if !a.to_lowercase().eq(b.to_lowercase()) {
            return None;
        }
    }
    match n.get(r.len()) {
        None => Some(String::new()),
        Some('\\') => Some(n[r.len() + 1..].iter().map(|&c| if c == '\\' { '/' } else { c }).collect()),
        Some(_) => None, // `Photos2` is not below `Photos`
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn a_request_inside_the_file_is_rounded_to_blocks() {
        assert_eq!(plan_chunks(5000, 100, 10 * MIB, MIB), vec![(4096, 4096)]);
        assert_eq!(plan_chunks(0, 4096, 10 * MIB, MIB), vec![(0, 4096)]);
        assert_eq!(plan_chunks(4095, 2, 10 * MIB, MIB), vec![(0, 8192)]);
    }

    #[test]
    fn the_last_piece_ends_at_the_end_of_the_file_unaligned() {
        assert_eq!(plan_chunks(0, 10_000, 10_000, MIB), vec![(0, 10_000)]);
        // a request past the end is clamped
        assert_eq!(plan_chunks(8192, 1 << 30, 10_000, MIB), vec![(8192, 10_000 - 8192)]);
        assert_eq!(plan_chunks(10_000, 5, 10_000, MIB), vec![]);
    }

    #[test]
    fn long_requests_are_cut_into_aligned_chunks() {
        let p = plan_chunks(0, 5 * MIB + 7, 100 * MIB, MIB);
        assert_eq!(p.len(), 6);
        assert!(p.iter().all(|(o, l)| o % ALIGN == 0 && *l <= MIB));
        assert_eq!(p.iter().map(|(_, l)| l).sum::<u64>(), 5 * MIB + ALIGN); // rounded up to the block
        // contiguous, no gaps and no overlaps
        for w in p.windows(2) {
            assert_eq!(w[0].0 + w[0].1, w[1].0);
        }
        // a chunk limit that is not a multiple is rounded down
        assert_eq!(plan_chunks(0, 20_000, 1 << 30, 10_000), vec![(0, 8192), (8192, 8192), (16384, 4096)]);
        // and never below one block
        assert_eq!(plan_chunks(0, 8192, 1 << 30, 1), vec![(0, 4096), (4096, 4096)]);
    }

    #[test]
    fn callback_paths_map_to_sync_paths() {
        let root = r"C:\Users\me\MediaLake\Photos";
        assert_eq!(rel_from_normalized(root, r"\Users\me\MediaLake\Photos\a\b.jpg").as_deref(), Some("a/b.jpg"));
        assert_eq!(rel_from_normalized(root, r"\users\ME\medialake\photos\Z.txt").as_deref(), Some("Z.txt"));
        assert_eq!(rel_from_normalized(root, r"\Users\me\MediaLake\Photos").as_deref(), Some(""));
        assert_eq!(rel_from_normalized(root, r"\Users\me\MediaLake\Photos2\a.jpg"), None);
        assert_eq!(rel_from_normalized(root, r"\Users\me\Other\a.jpg"), None);
        assert_eq!(rel_from_normalized(r"\\?\C:\Users\me\MediaLake\Photos\", r"\Users\me\MediaLake\Photos\x").as_deref(), Some("x"));
        // names with non-ASCII letters keep their case folding sane
        assert_eq!(rel_from_normalized(r"D:\Ärger", r"\ärger\ü.txt").as_deref(), Some("ü.txt"));
    }
}
