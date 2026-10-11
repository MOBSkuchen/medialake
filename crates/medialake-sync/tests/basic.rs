mod common;

use common::*;
use medialake_proto::sync::*;
use medialake_sync::host::config::BackupPolicy;
use medialake_sync::host::hosted::ChangesResult;
use medialake_sync::member::engine::{Choice, ConflictPolicy};

fn none() -> BackupPolicy {
    BackupPolicy::None
}

fn file_policy() -> BackupPolicy {
    BackupPolicy::File { keep: 100, min_interval_secs: 0, deleted_days: 30 }
}

#[test]
fn host_scan_numbers_changes() {
    let h = host(none());
    write(&h.root, "a.txt", "one");
    write(&h.root, "sub/b.txt", "two");
    assert_eq!(h.hs.scan().unwrap(), 3); // a.txt, sub, sub/b.txt
    let list = h.hs.list();
    assert_eq!(list.len(), 3);
    assert!(list.iter().all(|e| e.rev > 0 && !e.deleted && e.author == "host"));
    // nothing changed: no new revisions
    let seq = h.hs.seq();
    assert_eq!(h.hs.scan().unwrap(), 0);
    assert_eq!(h.hs.seq(), seq);
    // ignored names never appear
    write(&h.root, ".DS_Store", "x");
    write(&h.root, "tmp.part", "x");
    assert_eq!(h.hs.scan().unwrap(), 0);
    // an edit becomes one new revision
    write(&h.root, "a.txt", "one more");
    assert_eq!(h.hs.scan().unwrap(), 1);
    let ChangesResult::Page { entries, .. } = h.hs.changes("", seq, 100).unwrap() else { panic!() };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "a.txt");
    // delete -> tombstone, listed for cursors > 0 but not for a fresh member
    std::fs::remove_file(h.root.join("a.txt")).unwrap();
    assert_eq!(h.hs.scan().unwrap(), 1);
    let ChangesResult::Page { entries, .. } = h.hs.changes("", seq + 1, 100).unwrap() else { panic!() };
    assert!(entries.iter().any(|e| e.path == "a.txt" && e.deleted));
    let ChangesResult::Page { entries, .. } = h.hs.changes("", 0, 100).unwrap() else { panic!() };
    assert!(entries.iter().all(|e| !e.deleted));
}

#[test]
fn host_detects_rename_as_move() {
    let h = host(none());
    write(&h.root, "old.txt", "content that is long enough");
    h.hs.scan().unwrap();
    std::fs::rename(h.root.join("old.txt"), h.root.join("new.txt")).unwrap();
    assert_eq!(h.hs.scan().unwrap(), 1, "one move, not a delete and a create");
    assert!(h.hs.entry("old.txt").unwrap().deleted);
    assert!(!h.hs.entry("new.txt").unwrap().deleted);
}

#[test]
fn two_way_sync() {
    let h = host(none());
    write(&h.root, "from-host.txt", "hello from host");
    write(&h.root, "dir/inner.txt", "inner");
    h.hs.scan().unwrap();

    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    let r = m.sync();
    assert_eq!(m.read("from-host.txt").unwrap(), "hello from host");
    assert_eq!(m.read("dir/inner.txt").unwrap(), "inner");
    assert!(r.downloaded >= 2);

    // member -> host
    m.write("from-member.txt", "hello from member");
    m.write("deep/er/file.bin", "deep");
    let r = m.sync();
    assert!(r.pushed >= 2);
    h.hs.scan().unwrap();
    assert_eq!(std::fs::read_to_string(h.root.join("from-member.txt")).unwrap(), "hello from member");
    assert_eq!(std::fs::read_to_string(h.root.join("deep/er/file.bin")).unwrap(), "deep");
    assert_eq!(h.hs.entry("from-member.txt").unwrap().author, "Mac");

    // a second round is quiet
    let r = m.sync();
    assert_eq!(r.pushed + r.downloaded + r.conflicts, 0);

    // host edit -> member
    write(&h.root, "from-host.txt", "edited on host");
    h.hs.scan().unwrap();
    m.sync();
    assert_eq!(m.read("from-host.txt").unwrap(), "edited on host");

    // deletes both ways
    std::fs::remove_file(h.root.join("dir/inner.txt")).unwrap();
    h.hs.scan().unwrap();
    m.remove("from-member.txt");
    m.sync();
    assert!(!m.exists("dir/inner.txt"));
    assert!(h.hs.entry("from-member.txt").unwrap().deleted);
    assert!(!h.root.join("from-member.txt").exists());
    assert_eq!(tree(&h.root), m.tree());
}

#[test]
fn two_members_exchange_files() {
    let h = host(none());
    let mut a = member(&h, "A", ConflictPolicy::ServerWins, Access::ReadWrite);
    let mut b = member(&h, "B", ConflictPolicy::ServerWins, Access::ReadWrite);
    a.write("shared/x.txt", "from a");
    a.sync();
    b.sync();
    assert_eq!(b.read("shared/x.txt").unwrap(), "from a");
    b.write("shared/y.txt", "from b");
    b.sync();
    a.sync();
    assert_eq!(a.read("shared/y.txt").unwrap(), "from b");
    assert_eq!(a.tree(), b.tree());
}

#[test]
fn moves_keep_history() {
    let h = host(file_policy());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("a.txt", "some content for a");
    m.sync();
    let v1 = h.hs.versions("a.txt");
    assert_eq!(v1.len(), 1);
    std::fs::rename(m.root.join("a.txt"), m.root.join("b.txt")).unwrap();
    let r = m.sync();
    assert_eq!(r.uploaded, 0, "a move must not re-send content");
    assert!(h.hs.entry("a.txt").unwrap().deleted);
    assert!(!h.hs.entry("b.txt").unwrap().deleted);
    assert_eq!(h.hs.versions("b.txt").len(), 1, "history follows the file");
    assert_eq!(std::fs::read_to_string(h.root.join("b.txt")).unwrap(), "some content for a");
}

#[test]
fn identical_content_needs_no_upload() {
    let h = host(none());
    write(&h.root, "orig.txt", "a fairly long piece of content, shared");
    h.hs.scan().unwrap();
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.sync();
    m.write("copy.txt", "a fairly long piece of content, shared");
    let r = m.sync();
    assert_eq!(r.uploaded, 0);
    assert_eq!(std::fs::read_to_string(h.root.join("copy.txt")).unwrap(), "a fairly long piece of content, shared");
}

#[test]
fn conflict_server_wins_keeps_both() {
    let h = host(file_policy());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("doc.txt", "v1");
    m.sync();
    // concurrent edits
    m.write("doc.txt", "mine");
    write(&h.root, "doc.txt", "theirs");
    h.hs.scan().unwrap();
    m.sync();
    assert_eq!(m.read("doc.txt").unwrap(), "theirs");
    let copies: Vec<_> = m.tree().into_iter().filter(|(p, _)| p.contains("(conflict, Mac,")).collect();
    assert_eq!(copies.len(), 1, "{:?}", m.tree());
    assert_eq!(copies[0].1, "mine");
    // the copy reached the host and everybody else
    m.sync();
    assert_eq!(std::fs::read_to_string(h.root.join(&copies[0].0)).unwrap(), "mine");
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "theirs");
}

#[test]
fn conflict_ask_waits_and_resolves() {
    for (choice, doc, copies) in [(Choice::Mine, "mine", 0usize), (Choice::Server, "theirs", 0), (Choice::Both, "theirs", 1)] {
        let h = host(file_policy());
        let mut m = member(&h, "Mac", ConflictPolicy::Ask, Access::ReadWrite);
        m.write("doc.txt", "v1");
        m.write("other.txt", "o1");
        m.sync();
        m.write("doc.txt", "mine");
        write(&h.root, "doc.txt", "theirs");
        h.hs.scan().unwrap();
        let r = m.sync();
        assert_eq!(r.conflicts, 1);
        // the local content stays, the host's is not applied, other files keep syncing
        assert_eq!(m.read("doc.txt").unwrap(), "mine");
        assert_eq!(m.engine.conflicts().len(), 1);
        m.write("other.txt", "o2");
        m.sync();
        assert_eq!(std::fs::read_to_string(h.root.join("other.txt")).unwrap(), "o2");
        assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "theirs");

        m.engine.resolve("doc.txt", choice).unwrap();
        m.sync();
        m.sync();
        h.hs.scan().unwrap();
        assert!(m.engine.conflicts().is_empty());
        assert_eq!(m.read("doc.txt").unwrap(), doc, "{choice:?}");
        assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), doc, "{choice:?}");
        let n = m.tree().keys().filter(|p| p.contains("(conflict,")).count();
        assert_eq!(n, copies, "{choice:?}");
        assert_eq!(tree(&h.root), m.tree(), "{choice:?}");
    }
}

#[test]
fn edit_here_delete_there_keeps_the_edit() {
    let h = host(file_policy());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("doc.txt", "v1");
    m.sync();
    m.write("doc.txt", "edited here");
    std::fs::remove_file(h.root.join("doc.txt")).unwrap();
    h.hs.scan().unwrap();
    m.sync();
    m.sync();
    assert!(!m.exists("doc.txt"));
    let copy = m.tree().into_iter().find(|(p, _)| p.contains("(conflict,")).expect("conflict copy");
    assert_eq!(copy.1, "edited here");
    assert_eq!(std::fs::read_to_string(h.root.join(&copy.0)).unwrap(), "edited here");
}

#[test]
fn delete_here_edit_there_brings_it_back() {
    let h = host(file_policy());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("doc.txt", "v1");
    m.sync();
    m.remove("doc.txt");
    write(&h.root, "doc.txt", "edited there");
    h.hs.scan().unwrap();
    m.sync();
    m.sync();
    assert_eq!(m.read("doc.txt").unwrap(), "edited there");
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "edited there");
    assert!(!m.tree().keys().any(|p| p.contains("(conflict,")), "nothing was lost, so there is no copy");
}

#[test]
fn deleted_folder_survives_with_new_files() {
    let h = host(none());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("dir/old.txt", "old");
    m.sync();
    m.remove("dir");
    write(&h.root, "dir/new.txt", "new on host");
    h.hs.scan().unwrap();
    m.sync();
    m.sync();
    assert_eq!(m.read("dir/new.txt").unwrap(), "new on host");
    assert!(h.root.join("dir/new.txt").exists());
    assert!(!m.exists("dir/old.txt"));
    assert_eq!(tree(&h.root), m.tree());
}

#[test]
fn both_created_the_same_path() {
    let h = host(none());
    let mut a = member(&h, "A", ConflictPolicy::ServerWins, Access::ReadWrite);
    let mut b = member(&h, "B", ConflictPolicy::ServerWins, Access::ReadWrite);
    a.write("new.txt", "from a");
    b.write("new.txt", "from b");
    a.sync();
    b.sync();
    a.sync();
    b.sync();
    assert_eq!(a.read("new.txt").unwrap(), "from a");
    assert_eq!(b.read("new.txt").unwrap(), "from a");
    assert_eq!(a.tree(), b.tree());
    assert!(b.tree().iter().any(|(p, c)| p.contains("(conflict, B,") && c == "from b"));
}

#[test]
fn equal_content_is_not_a_conflict() {
    let h = host(none());
    let mut a = member(&h, "A", ConflictPolicy::Ask, Access::ReadWrite);
    let mut b = member(&h, "B", ConflictPolicy::Ask, Access::ReadWrite);
    a.write("same.txt", "identical");
    b.write("same.txt", "identical");
    a.sync();
    let r = b.sync();
    assert_eq!(r.conflicts, 0);
    assert!(b.engine.conflicts().is_empty());
}

#[test]
fn case_collisions_are_rejected() {
    let h = host(none());
    write(&h.root, "Report.txt", "upper");
    write(&h.root, "Dir/x.txt", "x");
    h.hs.scan().unwrap();
    // neither Windows nor a default macOS volume could hold both names
    let r = h
        .hs
        .commit(
            Access::ReadWrite,
            "B",
            &[
                Change::Mkdir { path: "report.txt".into() },
                Change::Mkdir { path: "dir/y".into() },
                Change::Mkdir { path: "Dir/sub".into() },
            ],
        )
        .unwrap();
    assert!(matches!(r[0], ChangeResult::Conflict { reason: ConflictReason::CaseCollision, .. }), "{r:?}");
    assert!(matches!(r[1], ChangeResult::Conflict { reason: ConflictReason::CaseCollision, .. }), "{r:?}");
    assert!(matches!(r[2], ChangeResult::Ok { .. }), "{r:?}");
    let live: Vec<_> = h.hs.list().into_iter().map(|e| e.path).collect();
    assert!(live.contains(&"Report.txt".to_string()));
    assert!(!live.contains(&"report.txt".to_string()), "{live:?}");
}

#[test]
fn read_only_members_cannot_change_the_host() {
    let h = host(none());
    write(&h.root, "doc.txt", "host version");
    h.hs.scan().unwrap();
    let mut m = member(&h, "Viewer", ConflictPolicy::ServerWins, Access::ReadOnly);
    m.sync();
    assert_eq!(m.read("doc.txt").unwrap(), "host version");
    // an edit is reverted from the host; the edited file is kept locally
    m.write("doc.txt", "vandalised");
    m.write("new.txt", "never uploaded");
    m.sync();
    assert_eq!(m.read("doc.txt").unwrap(), "host version");
    assert!(m.tree().iter().any(|(p, c)| p.contains("(conflict,") && c == "vandalised"));
    assert!(!h.root.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "host version");
    // and the host refuses a direct commit
    let r = h
        .hs
        .commit(Access::ReadOnly, "x", &[Change::Mkdir { path: "evil".into() }])
        .unwrap();
    assert_eq!(r, vec![ChangeResult::Denied]);
}

#[test]
fn mass_delete_is_held_until_confirmed() {
    let h = host(none());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    for i in 0..20 {
        m.write(&format!("f{i}.txt"), &format!("content {i}"));
    }
    m.write("keep.txt", "keep");
    m.sync();
    for i in 0..20 {
        m.remove(&format!("f{i}.txt"));
    }
    let r = m.sync();
    assert_eq!(r.held_deletes, Some(20));
    assert!(!h.hs.entry("f0.txt").unwrap().deleted);
    m.engine.confirm_deletes().unwrap();
    let r = m.sync();
    assert_eq!(r.held_deletes, None);
    assert!(h.hs.entry("f0.txt").unwrap().deleted);
    assert!(!h.hs.entry("keep.txt").unwrap().deleted);
}

#[test]
fn vanished_root_pauses_instead_of_deleting() {
    let h = host(none());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("a.txt", "a");
    m.write("b.txt", "b");
    m.sync();
    // folder gone entirely
    std::fs::remove_dir_all(&m.root).unwrap();
    assert!(m.engine.sync_once().is_err());
    // folder back but empty (failed mount)
    std::fs::create_dir_all(&m.root).unwrap();
    assert!(m.engine.sync_once().is_err());
    assert!(!h.hs.entry("a.txt").unwrap().deleted);
}

/// A transport whose host can be swapped, to simulate a host that lost its database.
struct Switch(std::sync::Mutex<medialake_sync::member::transport::LocalTransport>);

macro_rules! delegate {
    ($self:ident, $m:ident($($a:expr),*)) => { $self.0.lock().unwrap().$m($($a),*) };
}

impl medialake_sync::member::transport::Transport for Switch {
    fn changes(&self, e: &str, c: u64, l: u32) -> anyhow::Result<medialake_sync::member::transport::ChangesReply> {
        delegate!(self, changes(e, c, l))
    }
    fn propose(&self, c: &[Change]) -> anyhow::Result<Verdict> {
        delegate!(self, propose(c))
    }
    fn commit(&self, c: &[Change]) -> anyhow::Result<Vec<ChangeResult>> {
        delegate!(self, commit(c))
    }
    fn download(&self, p: &str, r: u64, part: &std::path::Path) -> anyhow::Result<medialake_sync::member::transport::Fetch> {
        delegate!(self, download(p, r, part))
    }
    fn upload(&self, h: &str, s: &std::path::Path, n: u64) -> anyhow::Result<medialake_sync::member::transport::Upload> {
        delegate!(self, upload(h, s, n))
    }
    fn versions(&self, p: &str) -> anyhow::Result<Vec<VersionInfo>> {
        delegate!(self, versions(p))
    }
    fn snapshots(&self) -> anyhow::Result<Vec<SnapshotInfo>> {
        delegate!(self, snapshots())
    }
    fn restore(&self, w: RestoreWhat, c: bool) -> anyhow::Result<Vec<ChangeResult>> {
        delegate!(self, restore(w, c))
    }
}

#[test]
fn rebuilt_host_database_is_reconciled() {
    use medialake_sync::host::hosted::HostedSync;
    use medialake_sync::member::engine::{Engine, EngineConfig};
    use medialake_sync::member::frontend::Mirror;
    use medialake_sync::member::transport::LocalTransport;
    use std::sync::Arc;

    let h = host(none());
    let sw = Arc::new(Switch(std::sync::Mutex::new(LocalTransport { host: h.hs.clone(), access: Access::ReadWrite, author: "Mac".into() })));
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = EngineConfig {
        device: "Mac".into(),
        policy: ConflictPolicy::ServerWins,
        access: Access::ReadWrite,
        state_dir: base.join("state"),
        settle_ms: 0,
        ignore: vec![],
    };
    let mut engine = Engine::new(cfg, Arc::new(Mirror::new(&root)), sw.clone()).unwrap();
    write(&root, "a.txt", "a");
    write(&root, "b.txt", "b");
    write(&root, "gone.txt", "gone soon");
    engine.sync_once().unwrap();

    // the member edits b.txt offline; the host loses its database and the file gone.txt
    write(&root, "b.txt", "b edited here");
    let store = h.hs.store_dir().to_path_buf();
    let cfg = h.hs.config();
    let old_epoch = h.hs.epoch();
    drop(h.hs);
    std::fs::remove_file(store.join("state.db")).unwrap();
    let _ = std::fs::remove_file(store.join("state.db-wal"));
    let _ = std::fs::remove_file(store.join("state.db-shm"));
    std::fs::remove_file(h.root.join("gone.txt")).unwrap();
    write(&h.root, "c.txt", "c, added on the host");
    let hs2 = HostedSync::open(cfg, None).unwrap();
    hs2.scan().unwrap();
    assert_ne!(hs2.epoch(), old_epoch);
    *sw.0.lock().unwrap() = LocalTransport { host: hs2.clone(), access: Access::ReadWrite, author: "Mac".into() };

    let r = engine.sync_once().unwrap();
    assert_eq!(r.conflicts, 0, "{:?}", r.errors);
    engine.sync_once().unwrap();
    assert!(!root.join("gone.txt").exists(), "deleted on the host while its database was gone");
    assert_eq!(std::fs::read_to_string(root.join("c.txt")).unwrap(), "c, added on the host");
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b edited here");
    hs2.scan().unwrap();
    assert_eq!(std::fs::read_to_string(h.root.join("b.txt")).unwrap(), "b edited here");
    assert_eq!(tree(&h.root), tree(&root));
}

#[test]
fn file_backup_keeps_versions_and_restores() {
    let h = host(BackupPolicy::File { keep: 3, min_interval_secs: 0, deleted_days: 30 });
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    for v in 1..=5 {
        m.write("doc.txt", &format!("version {v}"));
        m.sync();
    }
    let versions = h.hs.versions("doc.txt");
    assert_eq!(versions.len(), 3, "keeps the last N");
    // newest first; the oldest kept is version 3
    let oldest = versions.last().unwrap();
    let results = h.hs.restore("admin", &RestoreWhat::File { path: "doc.txt".into(), rev: oldest.rev }, false).unwrap();
    assert!(matches!(results[0], ChangeResult::Ok { .. }), "{results:?}");
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "version 3");
    // a restore is a normal commit: it reaches the member
    m.sync();
    assert_eq!(m.read("doc.txt").unwrap(), "version 3");
    // restore as copy
    let r = h.hs.restore("admin", &RestoreWhat::File { path: "doc.txt".into(), rev: versions[0].rev }, true).unwrap();
    assert!(matches!(r[0], ChangeResult::Ok { .. }));
    let copy = h.hs.list().into_iter().find(|e| e.path.contains("(restored")).expect("copy");
    assert_eq!(std::fs::read_to_string(h.root.join(&copy.path)).unwrap(), "version 5");
}

#[test]
fn min_interval_replaces_rapid_saves() {
    let h = host(BackupPolicy::File { keep: 10, min_interval_secs: 3600, deleted_days: 30 });
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    for v in 1..=5 {
        m.write("doc.txt", &format!("save {v}"));
        m.sync();
    }
    assert_eq!(h.hs.versions("doc.txt").len(), 1);
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "save 5");
}

#[test]
fn direct_host_edits_are_preserved() {
    let h = host(file_policy());
    write(&h.root, "doc.txt", "first");
    h.hs.scan().unwrap();
    write(&h.root, "doc.txt", "second");
    h.hs.scan().unwrap();
    write(&h.root, "doc.txt", "third");
    h.hs.scan().unwrap();
    let v = h.hs.versions("doc.txt");
    assert_eq!(v.len(), 3);
    // the first content is still readable from the blob store
    let first = &v[2];
    h.hs.restore("admin", &RestoreWhat::File { path: "doc.txt".into(), rev: first.rev }, false).unwrap();
    assert_eq!(std::fs::read_to_string(h.root.join("doc.txt")).unwrap(), "first");
}

#[test]
fn time_snapshots_restore_folder_and_whole() {
    let h = host(BackupPolicy::Time { every_hours: 24, keep: 2 });
    write(&h.root, "a.txt", "a1");
    write(&h.root, "dir/b.txt", "b1");
    write(&h.root, "dir/c.txt", "c1");
    h.hs.scan().unwrap();
    let s1 = h.hs.snapshot_now().unwrap();
    // later: edits and a delete
    write(&h.root, "a.txt", "a2");
    write(&h.root, "dir/b.txt", "b2");
    std::fs::remove_file(h.root.join("dir/c.txt")).unwrap();
    h.hs.scan().unwrap();
    let s2 = h.hs.snapshot_now().unwrap();
    assert_ne!(s1, s2);
    assert_eq!(h.hs.snapshots().len(), 2);

    // a folder at a snapshot
    let r = h.hs.restore("admin", &RestoreWhat::Snapshot { id: s1, path: "dir".into() }, false).unwrap();
    assert!(r.iter().all(|r| matches!(r, ChangeResult::Ok { .. })), "{r:?}");
    assert_eq!(std::fs::read_to_string(h.root.join("dir/b.txt")).unwrap(), "b1");
    assert_eq!(std::fs::read_to_string(h.root.join("dir/c.txt")).unwrap(), "c1");
    assert_eq!(std::fs::read_to_string(h.root.join("a.txt")).unwrap(), "a2");
    // the whole snapshot
    h.hs.restore("admin", &RestoreWhat::Snapshot { id: s1, path: String::new() }, false).unwrap();
    assert_eq!(std::fs::read_to_string(h.root.join("a.txt")).unwrap(), "a1");

    // pruning: only `keep` snapshots remain, and blobs only they refer to are collected
    write(&h.root, "a.txt", "a3");
    h.hs.scan().unwrap();
    let s3 = h.hs.snapshot_now().unwrap();
    let ids: Vec<u64> = h.hs.snapshots().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![s2, s3]);
    let r = h.hs.restore("admin", &RestoreWhat::Snapshot { id: s1, path: String::new() }, false);
    assert!(r.is_err(), "snapshot s1 is gone");
}

#[test]
fn no_backup_means_final() {
    let h = host(BackupPolicy::None);
    write(&h.root, "a.txt", "a1");
    h.hs.scan().unwrap();
    assert!(h.hs.snapshot_now().is_err());
    assert!(h.hs.versions("a.txt").is_empty());
    // snapshots belong to the time strategy only
    let f = host(file_policy());
    assert!(f.hs.snapshot_now().is_err());
    let blobs = walkdir::WalkDir::new(h.hs.store_dir().join("blobs")).into_iter().flatten().filter(|e| e.file_type().is_file()).count();
    assert_eq!(blobs, 0);
}

#[test]
fn host_notifies_on_commit() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let h = host(none());
    let seen = std::sync::Arc::new(AtomicU64::new(0));
    let s2 = seen.clone();
    h.hs.set_notifier(Some(std::sync::Arc::new(move |_id: &str, seq: u64| s2.store(seq, Ordering::SeqCst))));
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    m.write("a.txt", "a");
    m.sync();
    assert_eq!(seen.load(Ordering::SeqCst), h.hs.seq());
    assert!(h.hs.seq() > 0);
}

#[test]
fn bad_paths_and_ignored_names_are_invalid() {
    let h = host(none());
    let r = h
        .hs
        .commit(
            Access::ReadWrite,
            "x",
            &[
                Change::Mkdir { path: "../escape".into() },
                Change::Mkdir { path: "a/../b".into() },
                Change::Mkdir { path: ".medialake-sync".into() },
                Change::Mkdir { path: "ok".into() },
            ],
        )
        .unwrap();
    assert!(matches!(r[0], ChangeResult::Invalid { .. }));
    assert!(matches!(r[1], ChangeResult::Invalid { .. }));
    assert!(matches!(r[2], ChangeResult::Invalid { .. }));
    assert!(matches!(r[3], ChangeResult::Ok { .. }));
}

#[test]
fn nfc_names_are_normalised_on_the_host() {
    let h = host(none());
    let decomposed = "Cafe\u{301}.txt";
    write(&h.root, decomposed, "x");
    h.hs.scan().unwrap();
    let live: Vec<_> = h.hs.list().into_iter().map(|e| e.path).collect();
    assert_eq!(live, vec!["Caf\u{e9}.txt".to_string()]);
}

#[test]
fn uploads_resume_where_they_stopped() {
    use medialake_sync::member::transport::{LocalTransport, Transport, Upload};
    let h = host(none());
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let src = h.dir.path().join("src.bin");
    std::fs::write(&src, &content).unwrap();
    let (hash, size) = medialake_sync::util::hash_file(&src).unwrap();
    // an interrupted upload left half of it behind
    std::fs::write(h.hs.upload_part_path(&hash), &content[..123_456]).unwrap();
    assert_eq!(h.hs.upload_have(&hash, size), 123_456);
    let tr = LocalTransport { host: h.hs.clone(), access: Access::ReadWrite, author: "x".into() };
    assert_eq!(tr.upload(&hash, &src, size).unwrap(), Upload::Done);
    assert_eq!(h.hs.upload_have(&hash, size), size, "complete");
    // a corrupt partial is detected by the hash and thrown away
    let other: Vec<u8> = content.iter().map(|b| b.wrapping_add(1)).collect();
    let src2 = h.dir.path().join("src2.bin");
    std::fs::write(&src2, &other).unwrap();
    let (hash2, size2) = medialake_sync::util::hash_file(&src2).unwrap();
    std::fs::write(h.hs.upload_part_path(&hash2), &content[..1000]).unwrap(); // wrong prefix
    assert_eq!(tr.upload(&hash2, &src2, size2).unwrap(), Upload::HashMismatch);
    assert_eq!(h.hs.upload_have(&hash2, size2), 0);
}

#[test]
fn downloads_resume_where_they_stopped() {
    use medialake_sync::member::transport::{Fetch, LocalTransport, Transport};
    let h = host(none());
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(h.root.join("big.bin"), &content).unwrap();
    h.hs.scan().unwrap();
    let e = h.hs.entry("big.bin").unwrap();
    let part = h.dir.path().join("big.part");
    std::fs::write(&part, &content[..77_777]).unwrap();
    let tr = LocalTransport { host: h.hs.clone(), access: Access::ReadOnly, author: "x".into() };
    assert_eq!(tr.download("big.bin", e.rev, &part).unwrap(), Fetch::Done);
    assert_eq!(std::fs::read(&part).unwrap(), content);
    // a stale revision is reported, and a missing file too
    write(&h.root, "big.bin", "changed");
    let p2 = h.dir.path().join("p2");
    assert_eq!(tr.download("big.bin", e.rev, &p2).unwrap(), Fetch::Stale);
    assert_eq!(tr.download("nothing.bin", 0, &p2).unwrap(), Fetch::NotFound);
}

#[test]
fn old_cursor_after_tombstone_purge_reconciles_in_full() {
    let h = host(none());
    let mut m = member(&h, "Mac", ConflictPolicy::ServerWins, Access::ReadWrite);
    write(&h.root, "keep.txt", "keep");
    write(&h.root, "gone.txt", "gone");
    h.hs.scan().unwrap();
    m.sync();
    assert!(m.exists("gone.txt"));
    // the member is away while the host deletes, adds and forgets the tombstone
    std::fs::remove_file(h.root.join("gone.txt")).unwrap();
    write(&h.root, "new.txt", "new");
    h.hs.scan().unwrap();
    assert_eq!(h.hs.purge_tombstones(-1).unwrap(), 1);
    let r = m.sync();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(!m.exists("gone.txt"), "a delete that was purged is still learned, by absence");
    assert_eq!(m.read("new.txt").unwrap(), "new");
    assert_eq!(m.tree(), tree(&h.root));
}
