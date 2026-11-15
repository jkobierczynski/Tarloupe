#!/usr/bin/env bash
# Benchmarks reading one file out of a large .tar.gz: Tarloupe (indexed)
# against GNU tar and 7-Zip, which must decompress up to the file.
#
# Usage: scripts/bench-gzip.sh SIZE WORKDIR   (e.g. 4G /tmp/bench)
# Needs: a release build (cargo build --release), gzip, GNU tar, 7z.
set -euo pipefail
size=${1:-4G}
work=${2:-/tmp/tarloupe-bench}
bin=$(cd "$(dirname "$0")/.." && pwd)/target/release
mkdir -p "$work"
export TARLOUPE_INDEX_DIR="$work/idx"
gz="$work/bench-$size.tar.gz"
TIMEFORMAT=%R

t() { # prints wall seconds of a command
  { time "$@" >/dev/null 2>&1; } 2>&1 | tail -1
}

if [ ! -f "$gz" ]; then
  echo "generating $size tar and compressing with gzip -6 ..." >&2
  "$bin/tarloupe-corpus" bulk --size "$size" -o "$work/bench.tar" >&2
  gzip -6 -c "$work/bench.tar" >"$gz"
  rm "$work/bench.tar"
fi
rm -rf "$TARLOUPE_INDEX_DIR"

echo "| Step | Time (s) |"
echo "|---|---|"
echo "| gzip -dc (decompress everything, baseline) | $(t gzip -dc "$gz") |"
echo "| tarloupe index (first open, one pass) | $(t "$bin/tarloupe" index "$gz") |"
echo "| tarloupe ls (reopen with index) | $(t "$bin/tarloupe" ls "$gz") |"

# The last member is the worst case for tools that read sequentially, and
# a ~10 MB member in the middle is the plan's random-read target.
listing=$("$bin/tarloupe" list -v "$gz")
last=$(printf '%s\n' "$listing" | awk '$1 ~ /^-/ {print $6}' | tail -1)
mids=$(printf '%s\n' "$listing" | awk '$1 ~ /^-/ && $3 > 5000000 && $3 < 20000000 {print $6}')
mid=$(printf '%s\n' "$mids" | sed -n "$(( ($(printf '%s\n' "$mids" | wc -l) + 1) / 2 ))p")
for member in "$last" "$mid"; do
  bytes=$("$bin/tarloupe" cat "$gz" "$member" | wc -c)
  echo "| **$member** ($bytes bytes): tarloupe cat | $(t "$bin/tarloupe" cat "$gz" "$member") |"
  echo "| same file: GNU tar -xzO --occurrence | $(t tar -xzOf "$gz" --occurrence=1 "$member") |"
  echo "| same file: 7-Zip (7z x gz, then tar) | $(t sh -c "7z x -so '$gz' | 7z x -si -ttar -so '$member'") |"
done
du -h "$TARLOUPE_INDEX_DIR"/*.tli | awk '{print "| index size | " $1 " |"}'
ls -lh "$gz" | awk '{print "| archive size | " $5 " |"}'
