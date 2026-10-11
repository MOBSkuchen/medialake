#!/usr/bin/env python3
"""Sets the MediaLake version everywhere it is written down.

    scripts/set-version.py                 print the current version
    scripts/set-version.py 0.2.0           set an explicit version
    scripts/set-version.py patch           bump (major | minor | patch)
    scripts/set-version.py 0.2.0 --dry-run show what would change

The source of truth is [workspace.package] in Cargo.toml; every crate inherits it.
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?")
BUMPS = ("major", "minor", "patch")


def windows_version(version: str) -> str:
    """The four-part numeric form Windows manifests want; a pre-release suffix has no place in it."""
    return ".".join(SEMVER.fullmatch(version).groups()[:3]) + ".0"


# (file, pattern, formatter, optional). Group 1 and 2 of the pattern surround the version. A file that is
# not optional must exist and every pattern must match, so a moved version string fails instead of being
# skipped silently.
TARGETS = [
    ("Cargo.toml", r'(\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]+(")', str, False),
    # ignored by git, but keep a local one in step so the next build does not rewrite it
    ("Cargo.lock", r'(^name = "medialake-[^"]+"\r?\nversion = ")[^"]+(")', str, True),
    ("installer/medialake.iss", r'(#define AppVersion ")[^"]+(")', str, False),
    ("scripts/windows/build-release.ps1", r"(build-release\.ps1 \[-Version )[^\]\s]+(\])", str, False),
    ("scripts/windows/build-release.ps1", r'(\[string\]\$Version = ")[^"]+(")', str, False),
    ("scripts/macos/build-release.sh", r"(build-release\.sh \[--version )[^\]\s]+(\])", str, False),
    ("scripts/macos/build-release.sh", r'(^version=")[^"]+(")', str, False),
    ("ui/MediaLake.App/app.manifest", r'(<assemblyIdentity\s+version=")[^"]+(")', windows_version, False),
]


def current_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        sys.exit("error: no [workspace.package] version in Cargo.toml")
    return m.group(1)


def resolve(arg: str, current: str) -> str:
    if arg in BUMPS:
        m = SEMVER.fullmatch(current)
        if not m:
            sys.exit(f"error: cannot bump {current!r}, it is not MAJOR.MINOR.PATCH")
        parts = [int(p) for p in m.groups()[:3]]
        i = BUMPS.index(arg)
        parts[i] += 1
        parts[i + 1:] = [0] * (2 - i)
        return ".".join(map(str, parts))
    version = arg[1:] if arg.startswith("v") else arg
    if not SEMVER.fullmatch(version):
        sys.exit(f"error: {arg!r} is not MAJOR.MINOR.PATCH[-prerelease] or one of {', '.join(BUMPS)}")
    return version


def main() -> None:
    parser = argparse.ArgumentParser(description="Set the MediaLake version across the project.")
    parser.add_argument("version", nargs="?", help="new version (e.g. 0.2.0) or major | minor | patch")
    parser.add_argument("-n", "--dry-run", action="store_true", help="show the changes without writing them")
    args = parser.parse_args()

    current = current_version()
    if args.version is None:
        print(current)
        return
    version = resolve(args.version, current)

    # Apply everything in memory first: nothing is written unless every target matched.
    texts: dict[Path, str] = {}
    originals: dict[Path, str] = {}
    errors = []
    for rel, pattern, fmt, optional in TARGETS:
        path = ROOT / rel
        if path not in texts:
            if not path.exists():
                if not optional:
                    errors.append(f"{rel}: file not found")
                continue
            # newline="" keeps CRLF files CRLF
            with open(path, encoding="utf-8", newline="") as f:
                texts[path] = originals[path] = f.read()
        new = fmt(version)
        texts[path], n = re.subn(pattern, lambda m: m.group(1) + new + m.group(2), texts[path], flags=re.M)
        if n == 0:
            errors.append(f"{rel}: nothing matched {pattern!r}")
    if errors:
        sys.exit("error: version not found where expected, nothing was changed\n  " + "\n  ".join(errors))

    changed = [p for p in texts if texts[p] != originals[p]]
    for path in changed:
        print(path.relative_to(ROOT).as_posix())
        for old, new in zip(originals[path].splitlines(), texts[path].splitlines()):
            if old != new:
                print(f"  - {old.strip()}\n  + {new.strip()}")
        if not args.dry_run:
            with open(path, "w", encoding="utf-8", newline="") as f:
                f.write(texts[path])

    if not changed:
        print(f"already at {version}")
    elif args.dry_run:
        print(f"dry run: {current} -> {version}, {len(changed)} file(s) would change")
    else:
        print(f"{current} -> {version}, {len(changed)} file(s) updated")


if __name__ == "__main__":
    main()
