#!/usr/bin/env bash
#
# Populate a running holofs cluster with a variety of test data.
# Invoked by `make dev-seed`; the Makefile sets the required env
# vars (BASE, FETCH, SAMPLE_PNG).
#
# Structure after seeding:
#
#   /                            loose files at catalog root
#     ├── README.md              text
#     ├── notes.txt              text
#     ├── config.toml            text
#     ├── pattern.png            image
#     └── random-4k.bin          binary (opaque)
#   /photos/
#     ├── simple/                synthetic-ish (assets/sample.png variants)
#     └── real/                  live 512×512 JPEGs from picsum.photos
#   /audio/
#     ├── short/                 3–6 s WAV + MP3
#     └── long/                  15 s MP3 + 15 s WAV
#   /binaries/                   opaque blobs (random + tar.gz)
#
# All PUTs run sequentially with a 300 ms pause to keep the
# per-node connection pool healthy (see the 2026-07-03 findings
# in project_deferred.md).

set -e

BASE="${BASE:-http://127.0.0.1:8787}"
FETCH="${FETCH:-/tmp/holofs-fetch}"
SAMPLE_PNG="${SAMPLE_PNG:-$PWD/assets/sample.png}"

mkdir -p "$FETCH"

# --- coloured status helpers -------------------------------------------
BOLD="\033[1m"; DIM="\033[2m"; GRN="\033[32m"; RED="\033[31m"
CYA="\033[36m"; YLW="\033[33m"; NC="\033[0m"

say()  { printf "${CYA}[seed]${NC} %s\n" "$*"; }
ok()   { printf "  ${GRN}✓${NC} %-42s (%s B)\n" "$1" "$2"; }
bad()  { printf "  ${RED}✗${NC} %-42s → %s\n" "$1" "$2"; }
skip() { printf "  ${YLW}·${NC} %-42s %s\n" "$1" "$2"; }

# --- primitives --------------------------------------------------------

mkdir_p() {
    curl -sS -X POST "$BASE/api/mkdir" \
        -H 'content-type: application/x-www-form-urlencoded' \
        --data "name=$1" > /dev/null || true
}

# put_file <catalog_path> <local_file>
put_file() {
    local remote="$1"
    local local_file="$2"
    if [ ! -s "$local_file" ]; then
        bad "$remote" "local file empty/missing"
        return
    fi
    local size
    size=$(/usr/bin/stat -f%z "$local_file" 2>/dev/null || /usr/bin/stat -c%s "$local_file")
    local code
    code=$(curl -sS -o /dev/null -w "%{http_code}" \
        -X PUT --data-binary "@$local_file" "$BASE/$remote")
    if [ "$code" = "201" ]; then
        ok "$remote" "$size"
    else
        bad "$remote" "$code"
    fi
    sleep 0.3
}

# fetch <url> <local_out> [--max-time N] — returns non-zero if
# download fails or is empty.
fetch() {
    local url="$1"; local out="$2"; shift 2
    if ! curl -sSfL -o "$out" --max-time 30 "$@" "$url"; then
        return 1
    fi
    [ -s "$out" ]
}

# --- directory tree ----------------------------------------------------

say "creating directory tree"
for d in photos photos/simple photos/real audio audio/short audio/long binaries; do
    mkdir_p "$d"
done

# --- /photos/simple/ --------------------------------------------------
# Cheap synthetic-ish content — 5 copies of the built-in sample under
# different names. Exercises the image-kind path without needing the
# network.
say "photos/simple/ — 5 built-in-sample variants"
for i in 01 02 03 04 05; do
    put_file "photos/simple/tile-$i.png" "$SAMPLE_PNG"
done

# --- /photos/real/ ----------------------------------------------------
# 20 real 512×512 JPEGs from picsum.photos with deterministic seeds
# so a re-seed lands on the same images (dedup path exercised).
say "photos/real/ — 20 real photos from picsum.photos (may take ~90 s)"
for i in $(seq -w 1 20); do
    seed="holofs-dev-$i"
    out="$FETCH/real-$i.jpg"
    if fetch "https://picsum.photos/seed/$seed/512/512.jpg" "$out"; then
        put_file "photos/real/landscape-$i.jpg" "$out"
    else
        bad "photos/real/landscape-$i.jpg" "download failed"
    fi
done

# --- /audio/short/ ----------------------------------------------------
# Real 3–6 s samples from samplelib.com. WAV + MP3 both go through the
# symphonia decoder so we prove both codec paths at once.
say "audio/short/ — 3 – 6 second WAV + MP3"
for pair in \
    "https://download.samplelib.com/wav/sample-3s.wav|beep-3s.wav" \
    "https://download.samplelib.com/mp3/sample-6s.mp3|tone-6s.mp3" \
; do
    IFS='|' read -r url name <<< "$pair"
    out="$FETCH/$name"
    if fetch "$url" "$out"; then
        put_file "audio/short/$name" "$out"
    else
        skip "audio/short/$name" "(url unreachable)"
    fi
done

# --- /audio/long/ -----------------------------------------------------
# 15-second samples.
say "audio/long/ — 15 second WAV + MP3"
for pair in \
    "https://download.samplelib.com/mp3/sample-15s.mp3|ambient-15s.mp3" \
    "https://download.samplelib.com/wav/sample-15s.wav|ambient-15s.wav" \
; do
    IFS='|' read -r url name <<< "$pair"
    out="$FETCH/$name"
    if fetch "$url" "$out"; then
        put_file "audio/long/$name" "$out"
    else
        skip "audio/long/$name" "(url unreachable)"
    fi
done

# --- /binaries/ -------------------------------------------------------
# Opaque blobs generated locally so we don't need a fixed external
# host. Sizes chosen to exercise the disk-backed store: 4 KB is one
# k-shard's worth, 64 KB is comfortable, 256 KB stresses the codec
# without being unreasonable for a dev cluster.
say "binaries/ — synthetic opaque blobs"
dd if=/dev/urandom of="$FETCH/random-4k.bin"   bs=1024 count=4   2>/dev/null
dd if=/dev/urandom of="$FETCH/random-64k.bin"  bs=1024 count=64  2>/dev/null
dd if=/dev/urandom of="$FETCH/random-256k.bin" bs=1024 count=256 2>/dev/null
put_file "binaries/random-4k.bin"   "$FETCH/random-4k.bin"
put_file "binaries/random-64k.bin"  "$FETCH/random-64k.bin"
put_file "binaries/random-256k.bin" "$FETCH/random-256k.bin"

# Tiny archive to exercise the tar.gz content sniff.
cat > "$FETCH/archive-note.txt" <<'EOT'
This archive holds two tiny placeholder blobs the seeder generated.
Extract with `tar -xzf archive.tar.gz`.
EOT
tar -czf "$FETCH/archive.tar.gz" -C "$FETCH" archive-note.txt random-4k.bin
put_file "binaries/archive.tar.gz" "$FETCH/archive.tar.gz"

# --- Loose root files -------------------------------------------------
say "root — loose mixed-kind files"

cat > "$FETCH/README.md" <<'EOT'
# holofs dev cluster

Seeded by `make dev` — see the top-level `Makefile` and
`deploy/dev-seed.sh` for the layout and sources.

- `/photos/simple` — synthetic images
- `/photos/real`   — live 512×512 photos from picsum.photos
- `/audio/short`   — 3–6 s WAV + MP3
- `/audio/long`    — 15 s WAV + MP3
- `/binaries`      — opaque blobs + tar.gz
EOT

cat > "$FETCH/notes.txt" <<'EOT'
Loose plain-text file at the catalog root. Exercises the
ObjectKind::Text ingest path (K-chunk RLNC over the raw bytes,
minhash + language tags stashed on the manifest).
EOT

cat > "$FETCH/config.toml" <<'EOT'
# Loose toml — checked as text since content-type sniff picks
# text/plain for it.
[dev]
seeded = true
version = "0.7.0"
EOT

put_file "README.md"      "$FETCH/README.md"
put_file "notes.txt"      "$FETCH/notes.txt"
put_file "config.toml"    "$FETCH/config.toml"
put_file "pattern.png"    "$SAMPLE_PNG"
dd if=/dev/urandom of="$FETCH/random-4k.bin" bs=1024 count=4 2>/dev/null
put_file "random-4k.bin"  "$FETCH/random-4k.bin"

# --- Final report -----------------------------------------------------
echo ""
say "final /api/stats:"
curl -sS "$BASE/api/stats"
echo ""
