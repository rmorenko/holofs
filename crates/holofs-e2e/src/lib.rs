//! Browser-driven end-to-end test harness for the holofs gateway.
//!
//! Each test spawns a fresh `holofs-web` child process bound to an
//! ephemeral port, opens a brand-new WebDriver session against the
//! configured chromedriver / Selenium hub, drives the browser through
//! the scenario, and then tears everything down on `Drop`. Per the
//! `crates/holofs-e2e/README.md`, the only out-of-tree prerequisites
//! are a running WebDriver endpoint (default `http://localhost:9515`)
//! and a release build of the gateway binary
//! (`cargo build --release --features ssr --bin holofs-web`).
//!
//! ## Cross-platform
//!
//! * Binary name: `holofs-web` on macOS / Linux, `holofs-web.exe` on
//!   Windows — picked up via `std::env::consts::EXE_SUFFIX`.
//! * Temp storage: `tempfile::TempDir` (already cross-platform).
//! * Child process: `std::process::Child` so `Drop` can `kill()` it
//!   synchronously without needing a tokio runtime to be alive.
//! * WebDriver session teardown: a thread-local synchronous DELETE
//!   request via `reqwest::blocking` so an early panic doesn't leak
//!   the chromedriver session.
//!
//! ## Env knobs
//!
//! * `HOLOFS_E2E_WEBDRIVER` — WebDriver URL (default `http://localhost:9515`).
//! * `HOLOFS_E2E_HEADED` — if set, run Chrome with a visible window;
//!   otherwise headless. Useful when debugging a flaky test locally.
//! * `HOLOFS_E2E_BINARY` — override path to the gateway binary.
//! * `HOLOFS_E2E_VERBOSE_GATEWAY` — if set, the gateway's stdout /
//!   stderr is forwarded to the test runner instead of being silenced.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tempfile::TempDir;
use thirtyfour::prelude::*;

pub mod fixtures;

/// Configuration knobs for a single `TestHarness` instance.
#[derive(Clone)]
pub struct HarnessConfig {
    /// Hand the gateway `--enable-embed`. Note: the first
    /// `/api/search` call downloads ~155 MiB of CLIP weights from
    /// HuggingFace; the cache lives in `~/.cache/huggingface/hub/`
    /// and persists across test runs.
    pub enable_embed: bool,
    /// Hand the gateway `--enable-versions`. Default-on so versioning
    /// scenarios don't need to think about it; harmless when unused.
    pub enable_versions: bool,
    /// Run Chrome in headless mode. Off when `HOLOFS_E2E_HEADED` is
    /// set in the env.
    pub headless: bool,
    /// WebDriver endpoint to connect to. Read once from the
    /// `HOLOFS_E2E_WEBDRIVER` env var (default `http://localhost:9515`).
    pub webdriver_url: String,
    /// Window size, used to keep screenshot comparisons stable across
    /// runs. Width × Height.
    pub window: (u32, u32),
    /// Quiet the gateway's background scanners — they're noisy and
    /// not the system under test for any E2E scenario.
    pub quiet_background_scanners: bool,
    /// Extra `(key, value)` env vars to set on the spawned gateway
    /// process. Used to flip env-knob features (e.g.
    /// `HOLOFS_VERSIONS_KEEP_LAST`) in a single test without
    /// polluting the process-wide env of the test runner.
    pub extra_env: Vec<(String, String)>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            enable_embed: false,
            enable_versions: true,
            headless: std::env::var("HOLOFS_E2E_HEADED").is_err(),
            webdriver_url: std::env::var("HOLOFS_E2E_WEBDRIVER")
                .unwrap_or_else(|_| "http://localhost:9515".to_string()),
            window: (1280, 900),
            quiet_background_scanners: true,
            extra_env: Vec::new(),
        }
    }
}

/// One scenario's worth of state: gateway child process, temp storage,
/// WebDriver session, base URL. Drop tears every resource down even
/// on a panicking test (best-effort).
pub struct TestHarness {
    /// `http://127.0.0.1:<ephemeral-port>` — root of the test gateway.
    pub base_url: String,
    /// Active WebDriver session. Tests may drive it directly when
    /// they need something the harness helpers don't cover.
    pub driver: WebDriver,
    /// URL of the WebDriver endpoint (chromedriver / Selenium hub).
    /// Used for the synchronous cleanup DELETE in `Drop`.
    pub webdriver_url: String,
    gateway: GatewayProcess,
    _storage: TempDir,
    http: reqwest::Client,
    /// Cached config so `restart()` can re-spawn the gateway with
    /// the same flags. Cloning is cheap — a few bools + the env
    /// extras vector.
    config: HarnessConfig,
}

impl TestHarness {
    /// Spawn a fresh gateway + WebDriver session with default config.
    pub async fn fresh() -> Result<Self> {
        Self::fresh_with(HarnessConfig::default()).await
    }

    /// Spawn with custom configuration.
    pub async fn fresh_with(config: HarnessConfig) -> Result<Self> {
        let port = pick_free_port()
            .context("could not find a free TCP port for the gateway")?;
        let storage = TempDir::new()
            .context("could not create temp storage directory")?;
        let gateway = spawn_gateway(port, storage.path(), &config)
            .context("spawning holofs-web")?;
        let base_url = format!("http://127.0.0.1:{port}");
        wait_for_gateway(&base_url)
            .await
            .context("gateway never became reachable")?;
        let driver = build_driver(&config).await.with_context(|| {
            format!(
                "could not start a WebDriver session at {}; is chromedriver running? \
                 Try `cargo run --manifest-path crates/holofs-e2e/Cargo.toml --bin holofs-e2e-doctor` \
                 or see crates/holofs-e2e/README.md.",
                config.webdriver_url
            )
        })?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        let cached_config = config.clone();
        Ok(Self {
            base_url,
            driver,
            webdriver_url: config.webdriver_url,
            gateway,
            _storage: storage,
            http,
            config: cached_config,
        })
    }

    /// Kill the gateway child process and respawn it against the SAME
    /// on-disk storage (TempDir is kept) with the SAME flags. Used to
    /// test persistence — anything the gateway flushed to disk before
    /// the restart should re-appear after.
    ///
    /// The new gateway binds a *different* free TCP port to dodge
    /// `TIME_WAIT` on the old one, so `self.base_url` is rewritten.
    /// The WebDriver session is untouched (the browser-side state is
    /// orthogonal to gateway lifecycle).
    pub async fn restart(&mut self) -> Result<()> {
        self.gateway.kill_and_wait();
        let new_port = pick_free_port()
            .context("could not find a free TCP port for the restarted gateway")?;
        let new_gateway = spawn_gateway(new_port, self._storage.path(), &self.config)
            .context("respawning holofs-web")?;
        self.base_url = format!("http://127.0.0.1:{new_port}");
        wait_for_gateway(&self.base_url)
            .await
            .context("restarted gateway never became reachable")?;
        self.gateway = new_gateway;
        Ok(())
    }

    /// Absolute URL for a path on the test gateway.
    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            return path.to_string();
        }
        let trimmed = path.trim_start_matches('/');
        format!("{}/{}", self.base_url, trimmed)
    }

    /// Navigate the browser to `path` on the test gateway. `path` may
    /// be absolute (`/foo`) or relative (`foo`).
    pub async fn goto(&self, path: &str) -> Result<()> {
        self.driver.goto(self.url(path)).await?;
        Ok(())
    }

    /// PUT raw bytes to `name`. Returns the JSON the gateway emits
    /// on a successful upload (`{name, object_id, …}`).
    pub async fn put_bytes(&self, name: &str, body: Vec<u8>) -> Result<serde_json::Value> {
        let resp = self
            .http
            .put(self.url(name))
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Convenience: PUT a UTF-8 string under `name`.
    pub async fn put_text(&self, name: &str, body: &str) -> Result<serde_json::Value> {
        self.put_bytes(name, body.as_bytes().to_vec()).await
    }

    /// Mkdir at `parent/name`. Status 303 (redirect) and 200 are both
    /// treated as success; 409 (already exists) is silently swallowed
    /// so harness setup is idempotent.
    pub async fn mkdir(&self, parent: &str, name: &str) -> Result<()> {
        let parent = parent.trim_start_matches('/');
        let form = [
            ("parent", parent),
            ("name", name),
            ("return_to", "/"),
        ];
        let resp = self
            .http
            .post(self.url("/api/mkdir"))
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 303 || status.as_u16() == 409 {
            return Ok(());
        }
        Err(anyhow!(
            "mkdir parent={parent} name={name} returned HTTP {status}"
        ))
    }

    /// Best-effort recursive mkdir for a path like `photos/abstract`.
    /// Each prefix is mkdir'd in order; existing prefixes are no-ops.
    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        for i in 0..parts.len() {
            let parent = parts[..i].join("/");
            let name = parts[i];
            self.mkdir(&parent, name).await?;
        }
        Ok(())
    }

    /// DELETE an object by full path.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let resp = self.http.delete(self.url(name)).send().await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        Err(anyhow!("DELETE {name} returned HTTP {status}"))
    }

    /// GET raw bytes from the gateway. Bypasses the browser — useful
    /// for checking that a server-rendered route returns the right
    /// bytes, separately from how the browser renders them.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self
            .http
            .get(self.url(path))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }

    /// GET → JSON parsed into a `serde_json::Value`. The gateway has
    /// several JSON endpoints (`/api/stats`, `/api/search`, …) that
    /// E2E tests want to cross-check against the rendered DOM.
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value> {
        let body = self
            .http
            .get(self.url(path))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// POST `name=…` to `/api/file_metrics` (and similar leptos
    /// server-fn endpoints). The endpoint rejects GET; the form-body
    /// shape is what the framework expects.
    pub async fn post_form(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<serde_json::Value> {
        let body = self
            .http
            .post(self.url(path))
            .form(fields)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Seed the canonical "minimal" test corpus: one image, one
    /// text, one folder. ~4 seconds. Most catalog / UI tests need
    /// only this.
    pub async fn seed_minimal(&self) -> Result<()> {
        self.mkdir_p("photos/abstract").await?;
        self.mkdir_p("docs/notes").await?;
        self.put_bytes(
            "photos/abstract/mandala.png",
            fixtures::tiny_image_png().to_vec(),
        )
        .await?;
        self.put_text(
            "docs/notes/hello.txt",
            "hello holofs e2e\nline two\n",
        )
        .await?;
        Ok(())
    }

    /// Seed a single high-frequency-content image suitable for the
    /// streaming-hologram tests (`/preview/stream`, `/holo/<name>`).
    /// Returns the path the image was PUT to.
    pub async fn seed_textured_image(&self) -> Result<&'static str> {
        self.mkdir_p("photos/abstract").await?;
        self.put_bytes(
            "photos/abstract/texture.png",
            fixtures::textured_image_png().to_vec(),
        )
        .await?;
        Ok("photos/abstract/texture.png")
    }

    /// Seed enough content for /search to behave realistically:
    /// six images with content the CLIP encoder will rank
    /// differently. Requires `enable_embed = true`.
    pub async fn seed_for_search(&self) -> Result<()> {
        self.mkdir_p("photos/abstract").await?;
        for (name, bytes) in fixtures::search_corpus() {
            self.put_bytes(&format!("photos/abstract/{name}"), bytes.to_vec())
                .await?;
        }
        // Trigger embedding so subsequent /search hits are hot.
        let _ = self.http.post(self.url("/api/embed_all")).send().await?;
        Ok(())
    }

    /// Wait for a CSS selector to appear and return its first match.
    /// Polls every 100 ms up to `timeout` (default 10 s). The
    /// short polling interval keeps test latency low when the
    /// element appears quickly — the timeout exists to absorb
    /// hydration jitter on slower CI hardware.
    pub async fn wait_for(&self, css: &str, timeout: Duration) -> Result<WebElement> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(elem) = self.driver.find(By::Css(css)).await {
                return Ok(elem);
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "element {css:?} did not appear within {timeout:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait for a predicate to hold against the current DOM. The
    /// predicate returns `Ok(Some(value))` when satisfied; on
    /// `Ok(None)` we keep polling until `timeout`.
    pub async fn wait_until<F, T>(&self, mut f: F, timeout: Duration) -> Result<T>
    where
        F: AsyncFnMut(&WebDriver) -> Result<Option<T>>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            match f(&self.driver).await? {
                Some(v) => return Ok(v),
                None => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("wait_until: predicate never held"));
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
    }

    /// Take a screenshot as PNG bytes. Used for hybrid-mode
    /// asserts: rather than diffing against a baseline, tests
    /// compare two screenshots taken from the same harness at
    /// different points in time (e.g. before/after a streaming
    /// hologram frame arrives).
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        Ok(self.driver.screenshot_as_png().await?)
    }

    /// Graceful teardown. Tests should `harness.close().await` at
    /// the end; Drop is a backstop for panic paths.
    pub async fn close(mut self) -> Result<()> {
        let _ = self.driver.clone().quit().await;
        self.gateway.kill_and_wait();
        Ok(())
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Best-effort: blocking DELETE to chromedriver to release the
        // browser tab, then kill the gateway child. Drop is the
        // backstop for panic paths; the happy path is
        // `harness.close().await` which runs an async quit. We do
        // the network bit on a freshly-spawned OS thread because
        // `reqwest::blocking` builds its own tokio runtime and
        // panics if instantiated from within an existing one — and
        // Drop is reached *from* the test's tokio runtime when the
        // harness goes out of scope.
        let session_id = self.driver.session_id().to_string();
        let url = format!(
            "{}/session/{}",
            self.webdriver_url.trim_end_matches('/'),
            session_id
        );
        let _ = std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .ok()?;
            client.delete(&url).send().ok().map(|_| ())
        })
        .join();
        self.gateway.kill_and_wait();
    }
}

// ---------------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------------

struct GatewayProcess {
    child: Option<Child>,
}

impl GatewayProcess {
    fn kill_and_wait(&mut self) {
        if let Some(mut c) = self.child.take() {
            // Coverage-instrumented binaries write their .profraw file
            // in an atexit handler — SIGKILL skips it, so a SIGKILL'd
            // gateway leaves no profile data behind. Send SIGTERM first,
            // give the process up to 500 ms to flush, and only fall back
            // to SIGKILL if it doesn't exit. Non-instrumented release
            // builds shut down on SIGTERM too (axum graceful shutdown),
            // so this path costs at most a few extra ms either way.
            #[cfg(unix)]
            {
                // The workspace forbids unsafe, so libc::kill is off
                // the table. Shell out to /bin/kill -TERM <pid>
                // instead — same effect, no FFI. Best-effort; any
                // failure is silently absorbed and the SIGKILL
                // fallback below covers it.
                let _ = std::process::Command::new("/bin/kill")
                    .arg("-TERM")
                    .arg(c.id().to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(not(unix))]
            {
                let _ = c.kill();
            }
            // Poll up to 500 ms for clean exit.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() >= deadline => break,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
            // Still running — fall back to SIGKILL.
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn spawn_gateway(
    port: u16,
    storage: &Path,
    config: &HarnessConfig,
) -> Result<GatewayProcess> {
    let binary = locate_binary()?;
    // The gateway's main.rs reads `crates/holofs-web/Cargo.toml`
    // *relative to its CWD* to bootstrap the leptos config. We must
    // therefore spawn it with the workspace root as its working
    // directory regardless of where `cargo test` placed us — by
    // default cargo sets CWD to the per-package dir
    // (`crates/holofs-e2e/`), which would make the gateway crash
    // immediately with `failed to read leptos config: ConfigNotFound`.
    let workspace_root = workspace_root_from_binary(&binary)?;
    let mut cmd = Command::new(&binary);
    cmd.current_dir(&workspace_root)
        .arg("--storage")
        .arg(storage)
        .arg("--addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--log")
        .arg("error")
        .arg("--log-format")
        .arg("text");
    if config.enable_embed {
        cmd.arg("--enable-embed");
    }
    if config.enable_versions {
        cmd.arg("--enable-versions");
    }
    if config.quiet_background_scanners {
        // 1 h between monitor / auditor ticks → effectively off for a
        // single-scenario test run. Keeps the gateway's log free of
        // unrelated noise and removes one source of network races.
        cmd.env("HOLOFS_MONITOR_INTERVAL", "3600");
        cmd.env("HOLOFS_AUDIT_INTERVAL", "3600");
    }
    for (k, v) in &config.extra_env {
        cmd.env(k, v);
    }
    if std::env::var("HOLOFS_E2E_VERBOSE_GATEWAY").is_err() {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let child = cmd.spawn().with_context(|| {
        format!(
            "could not spawn {}. Did you run `cargo build --release --features ssr --bin holofs-web` ?",
            binary.display()
        )
    })?;
    Ok(GatewayProcess { child: Some(child) })
}

/// The gateway binary lives at `<root>/target/release/holofs-web` (or
/// `.exe`). Walk three parents up from a canonicalized path to find
/// `<root>`. Fall back to walking up looking for the `crates/` dir if
/// the layout deviates (e.g. cross-compile output directories).
fn workspace_root_from_binary(binary: &Path) -> Result<PathBuf> {
    if let Some(parent) = binary.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
        if parent.join("crates").join("holofs-web").join("Cargo.toml").exists() {
            return Ok(parent.to_path_buf());
        }
    }
    // Fallback: walk up from the binary until a sibling `crates/holofs-web/Cargo.toml` shows up.
    let mut cur = binary.to_path_buf();
    while let Some(p) = cur.parent() {
        if p.join("crates").join("holofs-web").join("Cargo.toml").exists() {
            return Ok(p.to_path_buf());
        }
        cur = p.to_path_buf();
    }
    Err(anyhow!(
        "could not locate the workspace root above {}",
        binary.display()
    ))
}

fn locate_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("HOLOFS_E2E_BINARY") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(anyhow!(
            "HOLOFS_E2E_BINARY={} does not exist",
            p.display()
        ));
    }
    let exe = format!("holofs-web{}", std::env::consts::EXE_SUFFIX);
    let candidates = [
        format!("target/release/{exe}"),
        format!("../../target/release/{exe}"),
        format!("../target/release/{exe}"),
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p
                .canonicalize()
                .with_context(|| format!("canonicalize {}", p.display()));
        }
    }
    Err(anyhow!(
        "could not find {exe}. Searched: {:?}. \
         Run `cargo build --release --features ssr --bin holofs-web` first \
         or set HOLOFS_E2E_BINARY to the absolute path.",
        candidates
    ))
}

async fn wait_for_gateway(base_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;
    let url = format!("{base_url}/api/stats");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => last_err = Some(format!("HTTP {}", r.status())),
            Err(e) => last_err = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Err(anyhow!(
        "gateway at {base_url}/api/stats never returned 2xx within 30s; last error: {:?}",
        last_err
    ))
}

async fn build_driver(config: &HarnessConfig) -> Result<WebDriver> {
    let mut caps = DesiredCapabilities::chrome();
    if config.headless {
        caps.add_arg("--headless=new")?;
        caps.add_arg("--no-sandbox")?;
        caps.add_arg("--disable-gpu")?;
        caps.add_arg("--disable-dev-shm-usage")?;
    }
    caps.add_arg(&format!(
        "--window-size={},{}",
        config.window.0, config.window.1
    ))?;
    // Less terminal spam in CI; the actual DevTools console is still
    // reachable via `driver.execute(...)`.
    caps.add_arg("--log-level=3")?;
    let driver = WebDriver::new(&config.webdriver_url, caps).await?;
    Ok(driver)
}
