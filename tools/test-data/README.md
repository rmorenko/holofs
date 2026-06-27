# holofs test-data toolkit

Three scripts + one Python generator that give you a clean, reproducible
end-to-end environment for testing every Stage 12.6–15.0 feature.

```
tools/test-data/
├── generate-samples.py        # writes ~38 deterministic samples under samples/
├── fetch-real-landscapes.sh   # downloads 6 real 2560×1440 landscapes (picsum)
├── clean-cluster.sh           # wipes catalog + shards + embeddings + versions
├── upload-samples.sh          # PUTs the sample tree, preserving hierarchy
├── run-tests.sh               # end-to-end smoke: 30+ checks across the new features
└── README.md                  # you are here
```

## Usage — one-time setup

The generator is dependency-free (only stdlib Python 3.10+). Generate the
sample tree once, then fetch the real landscape photos:

```bash
python3 tools/test-data/generate-samples.py
# wrote 38 samples (1,862,535 bytes) under <repo>/samples
# manifest at samples/MANIFEST.txt

tools/test-data/fetch-real-landscapes.sh
# fetches 6 real 2560×1440 JPEGs from picsum.photos into
# samples/photos/landscapes-xl/ — IDs are pinned for reproducibility.
```

Re-running `generate-samples.py` after the fetch is safe: it preserves
anything already sitting under `samples/photos/landscapes-xl/` and folds
those files into the MANIFEST automatically.

What lands under `samples/`:

```
photos/
  landscapes/    mountain.png, ocean.png, forest.png, desert.png, tundra.png  (256×256, ~17 KB)
  landscapes-xl/ norway-fjord.jpg, highland-pass.jpg, stormy-shore.jpg, himalaya-camp.jpg,
                 yosemite-sunset.jpg, yosemite-valley.jpg  (2560×1440 real JPEGs from
                 picsum.photos / Unsplash, ~280–900 KB each — populated by
                 `fetch-real-landscapes.sh`, not by the generator)
  abstract/      mandala-{a,b}.png, gradient-{warm,cool}.png, noise-rgb.png, pixel-blocks.png
  brand-pairs/   logo-N.png + logo-N-wm.png (3 pairs — robust-copy targets)
audio/
  music/         bass.wav, mid.wav, treble.wav, mixed.wav, chord-{major,minor}.wav
  effects/       beep.wav, double-beep.wav
  silence/       silence-short.wav, silence-long.wav
docs/
  notes/         note.txt, todo.md, journal.txt
  spec/          api-draft.txt, schema.json
  legal/         licence.txt
binaries/
  archives/      docs.zip, docs.tar
  blobs/         fixture-{small,medium,large}.bin   (1 / 16 / 128 KiB)
```

All bytes are deterministic — re-running the generator with the same args
produces byte-identical output.

## Reset + reseed cycle

```bash
# 1. Stop any running gateway and wipe persistent state.
tools/test-data/clean-cluster.sh
# (set FORCE=1 to skip the confirmation prompt)

# 2. Start the gateway with embed + versions on.
./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &

# 3. Push the sample tree.  Folders are mkdir'd before files are PUT.
tools/test-data/upload-samples.sh
```

After step 3 `/api/stats` should report ~44 objects (38 synthetic + 6 real
landscape JPEGs) plus ~15 directory markers. The total comes from
`find samples -type f -not -name MANIFEST.txt` + the folders the upload
script auto-creates.

## End-to-end smoke

Once the upload completes:

```bash
tools/test-data/run-tests.sh
```

What the script walks through, by stage:

| Stage  | Check                                              |
| ------ | -------------------------------------------------- |
| 9      | `/` and `/?p=<folder>` resolve for every subdir    |
| 12.6   | `/mix?a=<image>` renders the wavelet-mix composer  |
| 12.7   | `/health/<name>` per-file metrics page             |
| 12.7   | `/about` marketing page                            |
| 12.8/9 | `/search` UI + `/api/search?band=<any|coarse|mid|full>` |
| 13.0   | `/similar/<brand-pair logo>` includes robust-copy column |
| 13.1   | `/holo/<name>` + `/preview/stream/<name>` multipart |
| 13.2   | `/api/spotlight.png?mode=spatial`                  |
| 14.1   | `/api/spotlight.png?mode=coeff`                    |
| 13.4   | PUT twice → `/versions/<name>` shows archived row  |
| 14.0/3 | `POST /api/gc` returns a `GcReport` JSON           |

Each check prints `✓` / `✗`; final line reports `N / M passed`.  Exit
code is 0 only if every check passed.

## Custom storage / URL

Every script accepts overrides via either positional argument or env var:

```bash
tools/test-data/clean-cluster.sh /tmp/holofs-data
SAMPLES=/tmp/my-samples tools/test-data/upload-samples.sh http://localhost:9000
tools/test-data/run-tests.sh http://localhost:9000
```

## Notes / known limitations

* Sample PNGs are 256 × 256 — small enough that an `--enable-embed` PUT
  pipeline (decode + CLIP encode + index append) stays under a second per
  image on Apple-silicon CPU.  The gateway up-scales them to its
  configured cluster dimensions (typically 512 × 512) at PUT time, so
  the on-disk shard layout matches the rest of the catalog.
* The first `/api/search` after a clean restart with `--enable-embed`
  downloads ~155 MiB of CLIP weights from HuggingFace into
  `~/.cache/huggingface/hub/`.  Subsequent runs reuse the cache.
* The `upload-samples.sh` script throttles 600 ms between PUTs by default
  to give the OS time to recycle ephemeral ports (the gateway's audit +
  monitor backgrounds also open one TCP connection per RPC, and macOS's
  default TIME_WAIT keeps ports for ~60 s). Override with
  `THROTTLE_MS=0` on a quiet cluster, or bump to 1000+ if uploads
  still fail with "Couldn't connect to server".
* No cleanup is needed after `run-tests.sh` — the only test-side PUT
  it does (the `test-versioned-<timestamp>.png`) is DELETE'd at the end.
