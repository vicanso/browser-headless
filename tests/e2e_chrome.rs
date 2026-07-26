//! Optional end-to-end smoke against a live Chrome + HTTP server.
//!
//! Not run by default (`cargo test` skips `#[ignore]`). Enable with:
//!
//! ```bash
//! # macOS example
//! export CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
//! cargo test --test e2e_chrome -- --ignored --nocapture
//! ```
//!
//! CI runs this job after installing Chromium (see `.github/workflows/ci.yml`).

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn chrome_bin() -> Option<String> {
    if let Ok(p) = std::env::var("CHROME")
        && !p.is_empty()
    {
        return Some(p);
    }
    for candidate in [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ] {
        if std::path::Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

struct Server {
    child: Child,
    addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_ready(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    for _ in 0..60 {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("server did not become ready".into())
}

async fn spawn_server(chrome: &str) -> Result<Server, String> {
    // Bind port 0 via a temporary listener to pick a free port, then drop it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    drop(listener);

    // The binary hardcodes 0.0.0.0:3000 — for e2e we still use 3000 when free,
    // otherwise skip. Prefer exclusive use of 3000 in CI.
    let port = if port_free(3000) { 3000 } else { port };
    if port != 3000 {
        return Err(format!(
            "e2e expects the binary to listen on :3000 (port 3000 busy; free port was {port})"
        ));
    }

    let bin = env!("CARGO_BIN_EXE_browser-headless");
    let child = Command::new(bin)
        .env("CHROME", chrome)
        .env("BROWSER_HEADLESS_POOL_SIZE", "1")
        .env("BROWSER_HEADLESS_MAX_PAGES", "1")
        .env("BROWSER_HEADLESS_ASYNC_JOBS", "false")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn server: {e}"))?;

    wait_ready(3000).await?;
    Ok(Server {
        child,
        addr: SocketAddr::from(([127, 0, 0, 1], 3000)),
    })
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[tokio::test]
#[ignore = "requires Chrome/Chromium; run with --ignored"]
async fn smoke_health_and_content_only() {
    let chrome = chrome_bin().expect("CHROME not set and no chromium on PATH");
    let server = spawn_server(&chrome)
        .await
        .expect("failed to start browser-headless");

    let base = format!("http://{}", server.addr);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    let ready = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200, "readyz body: {:?}", ready.text().await);

    let summary = client
        .get(format!(
            "{base}/summary?url=https://example.com&profile=content&timeout_ms=20000"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        summary.status(),
        200,
        "summary failed: {:?}",
        summary.text().await
    );
    let body: serde_json::Value = summary.json().await.unwrap();
    assert!(
        body.get("data").is_some(),
        "content_only envelope missing data: {body}"
    );
    assert!(body.get("char_count").is_some());
    assert!(body.get("final_url").is_some());
}
