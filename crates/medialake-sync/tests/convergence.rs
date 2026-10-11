//! S3: one host and three mirror members make random concurrent edits; everything converges to
//! identical trees and no version that reached a sync point is lost: every content hash ever offered to
//! the sync is either current, a conflict copy, or in the host's history.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use common::*;
use medialake_proto::sync::Access;
use medialake_sync::host::config::BackupPolicy;
use medialake_sync::member::engine::ConflictPolicy;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const PATHS: &[&str] = &["a.txt", "b.txt", "c.txt", "d/x.txt", "d/y.txt", "d/e/z.txt", "f/g.txt", "h.bin"];

fn hash(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

fn hashes_of(root: &Path) -> HashMap<String, String> {
    tree(root).into_iter().filter(|(_, c)| c != "<dir>").map(|(p, c)| (hash(&c), format!("{p}: {c}"))).collect()
}

fn random_op(rng: &mut Rng, root: &Path, label: &str, step: usize, blocked: &HashSet<String>, offered: &mut HashMap<String, String>) {
    let p = PATHS[rng.below(PATHS.len())];
    // a file with a pending "ask" conflict is waiting for a person: the person does not edit it
    if blocked.contains(p) {
        return;
    }
    match rng.below(10) {
        0..=4 => {
            let content = format!("{label} step {step} for {p} #{}", rng.next() % 1000);
            write(root, p, &content);
        }
        5 | 6 => {
            let abs = root.join(p);
            if abs.is_file() {
                std::fs::remove_file(abs).unwrap();
            }
        }
        7 | 8 => {
            let to = PATHS[rng.below(PATHS.len())];
            if blocked.contains(to) {
                return;
            }
            let (a, b) = (root.join(p), root.join(to));
            if a.is_file() && !b.is_dir() && a != b {
                std::fs::create_dir_all(b.parent().unwrap()).unwrap();
                std::fs::rename(a, b).unwrap();
            }
        }
        _ => {
            // an edit that is a copy of another file's content
            let q = PATHS[rng.below(PATHS.len())];
            if blocked.contains(q) {
                return;
            }
            if let Ok(c) = std::fs::read_to_string(root.join(q)) {
                write(root, p, &c);
            }
        }
    }
    let _ = offered;
}

fn run(seed: u64, policy: ConflictPolicy, rounds: usize) {
    let h = host(BackupPolicy::File { keep: 10_000, min_interval_secs: 0, deleted_days: 365 });
    let mut a = member(&h, "A", policy, Access::ReadWrite);
    let mut b = member(&h, "B", policy, Access::ReadWrite);
    let mut c = member(&h, "C", policy, Access::ReadWrite);
    let mut rng = Rng(seed);
    let mut offered: HashMap<String, String> = HashMap::new();

    for round in 0..rounds {
        // concurrent edits everywhere, host included
        let blocked = |m: &Member| -> HashSet<String> { m.engine.conflicts().into_iter().map(|c| c.path).collect() };
        let (ba, bb, bc) = (blocked(&a), blocked(&b), blocked(&c));
        let none = HashSet::new();
        for step in 0..4 {
            random_op(&mut rng, &a.root, "A", round * 10 + step, &ba, &mut offered);
            random_op(&mut rng, &b.root, "B", round * 10 + step, &bb, &mut offered);
            random_op(&mut rng, &c.root, "C", round * 10 + step, &bc, &mut offered);
            if rng.below(3) == 0 {
                random_op(&mut rng, &h.root, "H", round * 10 + step, &none, &mut offered);
            }
        }
        // whatever is about to be pushed or overwritten has to survive
        offered.extend(hashes_of(&h.root));
        // the host's watcher notices its own edits at some point of the round (within a round, members
        // may commit first); before the next round's edits it always has
        let scan_after = rng.below(4);
        if scan_after == 0 {
            h.hs.scan().unwrap();
        }
        let mut order = [0usize, 1, 2];
        for i in (1..3).rev() {
            order.swap(i, rng.below(i + 1));
        }
        for (n, i) in order.into_iter().enumerate() {
            if n > 0 && n == scan_after {
                h.hs.scan().unwrap();
            }
            let m = match i {
                0 => &mut a,
                1 => &mut b,
                _ => &mut c,
            };
            offered.extend(hashes_of(&m.root));
            m.sync();
            let who = m.name.clone();
            let roots = [a.root.clone(), b.root.clone(), c.root.clone()];
            if std::env::var("CONV_DEBUG").is_ok() {
                let cur: HashSet<String> = hashes_of(&h.root).into_keys().collect();
                let mut hist: HashSet<String> = HashSet::new();
                for e in walkdir::WalkDir::new(h.hs.store_dir().join("blobs")).into_iter().flatten() {
                    if e.file_type().is_file() {
                        hist.insert(e.file_name().to_str().unwrap().to_string());
                    }
                }
                for (hh, d) in &offered {
                    if !cur.contains(hh) && !hist.contains(hh) {
                        // members may still hold it locally
                        let held = roots.iter().any(|r| hashes_of(r).contains_key(hh));
                        if !held {
                            println!("round {round}: after sync of {who}: unreachable {d}");
                        }
                    }
                }
            }
        }
        h.hs.scan().unwrap();
    }

    h.hs.scan().unwrap();
    // with *ask*, conflicts wait for a person: resolve them with "both" (keeps everything)
    for _ in 0..4 {
        for m in [&mut a, &mut b, &mut c] {
            for ci in m.engine.conflicts() {
                m.engine.resolve(&ci.path, medialake_sync::member::engine::Choice::Both).unwrap();
            }
        }
        offered.extend(hashes_of(&h.root));
        settle(&h, &mut [&mut a, &mut b, &mut c]);
        if [&a, &b, &c].iter().all(|m| m.engine.conflicts().is_empty()) {
            break;
        }
    }
    settle(&h, &mut [&mut a, &mut b, &mut c]);

    let ht = tree(&h.root);
    assert_eq!(a.tree(), ht, "seed {seed}: A differs from the host");
    assert_eq!(b.tree(), ht, "seed {seed}: B differs from the host");
    assert_eq!(c.tree(), ht, "seed {seed}: C differs from the host");

    // nothing lost: every offered content is current (maybe as a conflict copy) or in history
    let current: HashSet<String> = ht.iter().filter(|(_, v)| *v != "<dir>").map(|(_, v)| hash(v)).collect();
    let mut history: HashSet<String> = HashSet::new();
    for entry in walkdir::WalkDir::new(h.hs.store_dir().join("blobs")).into_iter().flatten() {
        if entry.file_type().is_file() {
            history.insert(entry.file_name().to_str().unwrap().to_string());
        }
    }
    let lost: Vec<&String> = offered.iter().filter(|(h, _)| !current.contains(*h) && !history.contains(*h)).map(|(_, d)| d).collect();
    assert!(lost.is_empty(), "seed {seed}: {} version(s) lost: {lost:?}", lost.len());
}

/// `CONV_SEEDS=300 cargo test --test convergence` for a longer soak.
fn seeds(default: u64) -> u64 {
    std::env::var("CONV_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[test]
fn converges_server_wins() {
    for seed in 1..=seeds(12) {
        run(seed, ConflictPolicy::ServerWins, 10);
    }
}

#[test]
fn converges_ask() {
    for seed in 100..=100 + seeds(8) {
        run(seed, ConflictPolicy::Ask, 8);
    }
}

#[test]
fn history_is_not_pruned_away_by_conflicts() {
    // a smoke run with more rounds and a single seed
    run(4242, ConflictPolicy::ServerWins, 25);
    let _: BTreeMap<String, String> = BTreeMap::new();
}
