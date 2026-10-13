// A0: the save patterns of AppKit applications against the mounted spike.   swift saves.swift <mount point>
import Foundation

let mnt = CommandLine.arguments[1]
let fm = FileManager.default
func report(_ what: String, _ ok: Bool, _ detail: String = "") { print("\(ok ? "ok  " : "FAIL") \(what) \(detail)") }

// 1. NSData atomic write (temp file in the same directory, rename over the target)
let a = URL(fileURLWithPath: mnt + "/atomic.txt")
do {
    try Data("one".utf8).write(to: a)
    try Data("two".utf8).write(to: a, options: .atomic)
    report("Data.write(.atomic) over an existing file", try String(contentsOf: a) == "two")
} catch { report("Data.write(.atomic)", false, "\(error)") }

// 2. NSFileManager replaceItemAt (what NSDocument uses: temp item in .TemporaryItems / same dir, then swap)
let b = URL(fileURLWithPath: mnt + "/replace.txt")
do {
    try Data("old".utf8).write(to: b)
    let tmp = URL(fileURLWithPath: mnt + "/replace.tmp")
    try Data("new".utf8).write(to: tmp)
    let r = try fm.replaceItemAt(b, withItemAt: tmp)
    report("replaceItemAt", try String(contentsOf: r ?? b) == "new", "-> \(r?.lastPathComponent ?? "nil")")
} catch { report("replaceItemAt", false, "\(error)") }

// 3. same, with the temporary item in a different directory on the same volume (the TextEdit pattern)
do {
    let dir = URL(fileURLWithPath: mnt + "/.TemporaryItems-test")
    try fm.createDirectory(at: dir, withIntermediateDirectories: true)
    let tmp = dir.appendingPathComponent("doc.txt")
    try Data("third".utf8).write(to: tmp)
    let r = try fm.replaceItemAt(b, withItemAt: tmp, backupItemName: "backup~", options: [])
    report("replaceItemAt from another directory", try String(contentsOf: r ?? b) == "third")
    try? fm.removeItem(at: dir)
} catch { report("replaceItemAt from another directory", false, "\(error)") }

// 4. delete + recreate
do {
    try fm.removeItem(at: a)
    try Data("again".utf8).write(to: a)
    report("delete and recreate", try String(contentsOf: a) == "again")
} catch { report("delete and recreate", false, "\(error)") }

// 5. extended attribute, and the file attributes Finder sets
do {
    try Data("x".utf8).write(to: URL(fileURLWithPath: mnt + "/xattr.txt"))
    let rc = setxattr(mnt + "/xattr.txt", "com.apple.metadata:test", "v", 1, 0, 0)
    report("setxattr", rc == 0, rc == 0 ? "" : "errno \(errno)")
    let t = Date(timeIntervalSince1970: 1_500_000_000)
    try fm.setAttributes([.modificationDate: t], ofItemAtPath: mnt + "/xattr.txt")
    let m = try fm.attributesOfItem(atPath: mnt + "/xattr.txt")[.modificationDate] as? Date
    report("set modification date", m == t, "\(String(describing: m))")
} catch { report("attributes", false, "\(error)") }

// 6. directory move with contents, then rename over an empty directory
do {
    try fm.createDirectory(atPath: mnt + "/d1/sub", withIntermediateDirectories: true)
    try Data("f".utf8).write(to: URL(fileURLWithPath: mnt + "/d1/sub/f.txt"))
    try fm.moveItem(atPath: mnt + "/d1", toPath: mnt + "/d2")
    report("move directory tree", fm.fileExists(atPath: mnt + "/d2/sub/f.txt"))
    try fm.removeItem(atPath: mnt + "/d2")
} catch { report("move directory tree", false, "\(error)") }

// 7. preallocate + truncate + sparse write (databases, Office)
do {
    let fd = open(mnt + "/sparse.bin", O_RDWR | O_CREAT, 0o644)
    ftruncate(fd, 10_000_000)
    pwrite(fd, "end", 3, 9_999_997)
    close(fd)
    let size = try fm.attributesOfItem(atPath: mnt + "/sparse.bin")[.size] as? Int
    report("truncate + pwrite", size == 10_000_000, "size \(size ?? -1)")
} catch { report("sparse", false, "\(error)") }

// 8. flock / fcntl locks (Office and Preview take them; the mount is mounted nolocks)
do {
    let fd = open(mnt + "/atomic.txt", O_RDWR)
    let rc = flock(fd, LOCK_EX | LOCK_NB)
    report("flock", rc == 0, rc == 0 ? "" : "errno \(errno)")
    close(fd)
}
