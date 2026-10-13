#!/usr/bin/env bash
# A0 scoring run against a mounted `mount-spike`:   MNT=<mount point> LOG=<spike log> score.sh
# Prints what each operation did to the server (the log is the oracle: a READ line is a hydration).
set -u
: "${MNT:?}" "${LOG:?}"

mark() { wc -l < "$LOG" | tr -d ' '; }
since() { tail -n +"$(($1 + 1))" "$LOG" | sed 's/^ *[0-9]* ms  //'; }
step() { echo; echo "### $*"; }
timed() { local s=$SECONDS; "$@"; echo "(took $((SECONDS - s)) s)"; }

step "browse: ls -la (metadata only, must not READ)"
m=$(mark); ls -la "$MNT" "$MNT/docs" > /dev/null; since "$m" | sort | uniq -c | sort -rn | head -8

step "stat + file(1) on photo.jpg (file reads the first bytes: expected one small READ)"
m=$(mark); stat "$MNT/photo.jpg" > /dev/null; file "$MNT/photo.jpg"; since "$m" | grep -c READ

step "open + read 4 KiB (head -c)"
m=$(mark); head -c 4096 "$MNT/photo.jpg" | wc -c; since "$m" | grep READ | head -3

step "read hello.txt"
cat "$MNT/hello.txt"; echo

step "copy a 6 MB file out (cp): READ pattern and time"
m=$(mark); timed cp "$MNT/photo.jpg" /tmp/a0-photo.jpg; since "$m" | grep -c READ; cmp /tmp/a0-photo.jpg <(head -c 6000000 /dev/null) 2>/dev/null; ls -l /tmp/a0-photo.jpg | awk '{print $5}'
rm -f /tmp/a0-photo.jpg

step "copy a file in (cp) and read it back"
head -c 3000000 /dev/urandom > /tmp/a0-in.bin
m=$(mark); timed cp /tmp/a0-in.bin "$MNT/docs/in.bin"; since "$m" | sed 's/off=[0-9]* //' | sort | uniq -c | sort -rn | head -6
cmp /tmp/a0-in.bin "$MNT/docs/in.bin" && echo "round trip identical"
rm -f /tmp/a0-in.bin

step "rename + mkdir + rmdir + delete"
m=$(mark)
mkdir "$MNT/newdir" && mv "$MNT/docs/in.bin" "$MNT/newdir/moved.bin" && rm "$MNT/newdir/moved.bin" && rmdir "$MNT/newdir" && echo ok
since "$m" | grep -v lookup

step "atomic save: write temp, rename over the original (the common editor pattern)"
echo "version 1" > "$MNT/doc.txt"
m=$(mark)
echo "version 2" > "$MNT/.doc.txt.tmp" && mv "$MNT/.doc.txt.tmp" "$MNT/doc.txt"
cat "$MNT/doc.txt"; since "$m" | grep -v lookup

step "safe save: write backup copy, write new, delete backup (TextEdit / Office style)"
m=$(mark)
cp "$MNT/doc.txt" "$MNT/doc.txt~" && echo "version 3" > "$MNT/doc.txt" && rm "$MNT/doc.txt~"
cat "$MNT/doc.txt"; since "$m" | grep -v lookup

step "extended attributes and AppleDouble noise"
touch "$MNT/x.txt"
xattr -w com.apple.test 1 "$MNT/x.txt" 2>&1 | head -2
ls -la "$MNT" | grep -c '\._' | sed 's/^/._ files visible: /'
rm -f "$MNT/x.txt"

step "Quick Look thumbnail of photo.jpg (qlmanage -t reads the file itself)"
m=$(mark); perl -e 'alarm 25; exec @ARGV' qlmanage -t -s 128 -o /tmp "$MNT/photo.jpg" > /dev/null 2>&1; since "$m" | grep -c READ | sed 's/^/READ requests: /'

step "mdls (Spotlight metadata) on photo.jpg"
m=$(mark); mdls -name kMDItemKind "$MNT/photo.jpg" 2>&1 | head -2; since "$m" | grep -c READ | sed 's/^/READ requests: /'

step "large sequential read, 1 GiB from movie.mov (dd 1M blocks); run the server with --delay-ms 0 for throughput"
timed dd if="$MNT/movie.mov" of=/dev/null bs=1m count=1024 2>&1 | tail -2

step "large sequential write, 512 MiB (dd) and read back size"
timed dd if=/dev/zero of="$MNT/big.bin" bs=1m count=512 2>&1 | tail -2
ls -l "$MNT/big.bin" | awk '{print $5}'; rm -f "$MNT/big.bin"
