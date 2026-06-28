# holofs-e2e — browser-driven end-to-end tests

[`thirtyfour`](https://crates.io/crates/thirtyfour)-based test suite that
drives a real Chrome / Chromium against a fresh gateway child process
per scenario. Catches the kinds of regressions a pure HTTP smoke
can't — hydration races, SPA-router intercepts, multipart streams,
form-driven flows.

* **One process per test.** Each `#[tokio::test]` spawns its own
  `holofs-web` on an ephemeral port with a temp `--storage` dir, runs
  the scenario, and tears it all down on `Drop`. No shared state.
* **Serial.** Tests run with `--test-threads=1`. Chromedriver is a
  single-session-per-driver bottleneck and per-test cluster reboots
  serialise the suite anyway.
* **Hybrid asserts.** DOM `find(By::Css(...))` / `.text()` for the
  bulk of checks; screenshot-to-screenshot comparison for the few
  scenarios where visual progression is the test (streaming
  hologram, spotlight ROI).
* **No baseline images.** Screenshot diffs only ever compare two
  screenshots from the same harness run, so there's nothing to
  re-baseline on a font / Chrome update.

## Prerequisites

| | macOS | Linux | Windows |
|---|---|---|---|
| Browser | Google Chrome ≥ 110 | Chrome / Chromium ≥ 110 | Google Chrome ≥ 110 |
| Driver  | `brew install --cask chromedriver` | `apt install chromium-chromedriver` | `choco install chromedriver` or `scoop install chromedriver` |
| Gateway binary | `cargo build --release --features ssr --bin holofs-web` | (same) | (same) |

The runner scripts (`scripts/run-tests.sh`, `scripts/run-tests.ps1`)
will build the gateway and start chromedriver automatically if they
aren't already running. The driver listens on `localhost:9515` by
default.

### Docker alternative

For CI or a clean local setup, use the bundled compose file instead
of installing chromedriver natively:

```sh
docker compose -f crates/holofs-e2e/docker-compose.yml up -d
HOLOFS_E2E_WEBDRIVER=http://localhost:4444 \
    crates/holofs-e2e/scripts/run-tests.sh
docker compose -f crates/holofs-e2e/docker-compose.yml down
```

The compose service exposes noVNC on `:7900` (no password) so you can
literally watch the test run from a browser at
`http://localhost:7900/?autoconnect=1&resize=scale`.

## Running

```sh
# macOS / Linux
crates/holofs-e2e/scripts/run-tests.sh

# Windows (PowerShell 7+)
.\crates\holofs-e2e\scripts\run-tests.ps1

# Or directly via cargo (assumes you've started chromedriver yourself):
cargo test -p holofs-e2e -- --test-threads=1
```

Filter to a single test:

```sh
crates/holofs-e2e/scripts/run-tests.sh ui_catalog::tree_loads_root
```

Watch the browser instead of running headless:

```sh
HOLOFS_E2E_HEADED=1 crates/holofs-e2e/scripts/run-tests.sh
```

## Env knobs

| Var | Default | Effect |
|---|---|---|
| `HOLOFS_E2E_WEBDRIVER` | `http://localhost:9515` | WebDriver endpoint. Set to `http://localhost:4444` for the Selenium docker image. |
| `HOLOFS_E2E_HEADED`    | unset | If set, run Chrome with a visible window. |
| `HOLOFS_E2E_BINARY`    | (auto-locate) | Absolute path to a pre-built `holofs-web` binary. |
| `HOLOFS_E2E_VERBOSE_GATEWAY` | unset | Forward the gateway's stdout/stderr to the test runner. Useful when a test's failure looks like a gateway-side bug. |

## Layout

```
crates/holofs-e2e/
├── Cargo.toml                       ← test-only workspace member
├── README.md                        ← you are here
├── docker-compose.yml               ← optional Selenium standalone chrome
├── scripts/
│   ├── run-tests.sh                 ← bash runner (mac / linux)
│   └── run-tests.ps1                ← PowerShell runner (windows)
├── src/
│   ├── lib.rs                       ← TestHarness, gateway lifecycle, WebDriver session
│   └── fixtures.rs                  ← deterministic tiny PNGs / corpora
└── tests/
    ├── ui_catalog.rs                ← homepage, tree view, breadcrumb, lazy folder expand
    ├── ui_about.rs                  ← /about marketing
    ├── ui_search.rs                 ← /search query + band filter
    ├── ui_holo.rs                   ← /holo streaming PNG progression (hybrid screenshot)
    └── … (more in Phase 2)
```

## Cross-platform notes

* The gateway binary is located via `std::env::consts::EXE_SUFFIX`,
  so `holofs-web.exe` is picked up automatically on Windows.
* Tempdirs come from the `tempfile` crate — works on all three OSes.
* The harness's `Drop` impl uses `reqwest::blocking` (not tokio) so
  resources are released even when a test panics outside any async
  runtime.
* Newlines in tests use Rust string literals, so CRLF / LF on disk
  doesn't matter; just don't introduce shell-only quoting in test
  bodies.

## Known limitations

* The first test in any session that hits `/api/search` with
  `enable_embed = true` will download ~155 MiB of CLIP weights into
  `~/.cache/huggingface/hub/`. Subsequent runs reuse the cache. CI
  should pre-warm this cache in the runner image.
* Headless Chrome occasionally hits
  `DevToolsActivePort file doesn't exist` when host shm is tight
  (< 1 GB). On CI use `--disable-dev-shm-usage` (already passed by
  the harness) and ensure the docker container has `shm_size: "2gb"`.
