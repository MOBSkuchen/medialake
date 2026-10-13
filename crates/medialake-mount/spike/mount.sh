#!/usr/bin/env bash
# mount.sh <port> <mount point> [extra mount_nfs options]    unprivileged loopback NFSv3 mount (A0)
set -eu
port="$1"; dir="$2"; extra="${3:-}"
mkdir -p "$dir"
opts="vers=3,tcp,locallocks,noresvport,port=$port,mountport=$port,rsize=1048576,wsize=1048576,nobrowse"
[ -n "$extra" ] && opts="$opts,$extra"
exec mount_nfs -o "$opts" localhost:/ "$dir"
