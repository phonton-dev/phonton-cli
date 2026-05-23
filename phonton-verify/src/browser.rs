use anyhow::{anyhow, Result};
use phonton_types::{VerifyLayer, VerifyResult};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Layer 5: Non-interactive Playwright-based browser verification routines
/// for generated web/Vite/HTML outputs.
///
/// Detects if there is an `index.html` or a web project. If present, spawns a
/// background zero-dependency Node static server, launches headless Chromium
/// via Playwright to run DOM click/interaction tests, takes a screenshot,
/// and returns appropriate `VerifyResult` verdicts.
pub async fn verify_browser_check(working_dir: &Path) -> Result<Option<VerifyResult>> {
    // 1. Detect if this is a web/HTML project
    let has_html = working_dir.join("index.html").is_file();
    let package_json = working_dir.join("package.json");
    if !has_html && !package_json_looks_browser_runnable(&package_json) {
        return Ok(None); // Skip if no web indicators are present
    }

    // 2. Generate temporary Node.js scripts in the working directory
    let server_script = working_dir.join("phonton-server.js");
    let playwright_script = working_dir.join("phonton-playwright.js");
    let screenshot_name = "phonton-screenshot.png";
    let _screenshot_path = working_dir.join(screenshot_name);

    let server_content = r#"
const http = require('http');
const fs = require('fs');
const path = require('path');

const port = process.argv[2] || 0; // 0 lets OS assign a free port
const docRoot = process.argv[3] || '.';

const mimeTypes = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.gif': 'image/gif',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
};

const server = http.createServer((req, res) => {
  let filePath = path.join(docRoot, req.url === '/' ? 'index.html' : req.url.split('?')[0]);
  const extname = String(path.extname(filePath)).toLowerCase();
  const contentType = mimeTypes[extname] || 'application/octet-stream';

  fs.readFile(filePath, (error, content) => {
    if (error) {
      if (error.code === 'ENOENT') {
        res.writeHead(404, { 'Content-Type': 'text/plain' });
        res.end('404 Not Found');
      } else {
        res.writeHead(500);
        res.end(`Server Error: ${error.code}`);
      }
    } else {
      res.writeHead(200, { 'Content-Type': contentType });
      res.end(content, 'utf-8');
    }
  });
});

server.listen(port, '127.0.0.1', () => {
  console.log(`SERVER_RUNNING:${server.address().port}`);
});
"#;

    let playwright_content = r#"
const { chromium } = require('playwright');

(async () => {
  const url = process.env.PHONTON_URL || 'http://127.0.0.1:8080';
  const screenshotPath = process.env.PHONTON_SCREENSHOT || 'phonton-screenshot.png';

  const errors = [];
  const logMessages = [];

  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage();

    page.on('pageerror', (err) => {
      errors.push(`Page Error: ${err.message}`);
    });
    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        errors.push(`Console Error: ${msg.text()}`);
      } else {
        logMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });

    await page.goto(url, { waitUntil: 'networkidle', timeout: 15000 });

    // Check basic rendering
    const title = await page.title();
    const content = await page.content();
    if (!content || content.trim().length === 0) {
      errors.push('Page is completely empty');
    }

    // Interaction checks - e.g., Counter Click test
    const buttons = await page.$$('button');
    let counterClicked = false;
    for (const btn of buttons) {
      const txt = await btn.textContent();
      if (txt.toLowerCase().includes('click') || txt.toLowerCase().includes('count') || txt.toLowerCase().includes('+')) {
        await btn.click();
        counterClicked = true;
        await page.waitForTimeout(200);
        break;
      }
    }

    // Chess board click test:
    const board = await page.$('.board, #board, [class*="board"], [id*="board"]');
    let chessInteraction = false;
    if (board) {
      const squares = await page.$$('.square, [class*="square"], [id*="square"], svg g g');
      if (squares.length >= 2) {
        await squares[0].click();
        await page.waitForTimeout(200);
        await squares[1].click();
        await page.waitForTimeout(200);
        chessInteraction = true;
      }
    }

    // Check for obvious crash text
    const bodyText = await page.innerText('body');
    if (bodyText.toLowerCase().includes('error') && !bodyText.toLowerCase().includes('no error') && !chessInteraction) {
      errors.push(`Possible error text visible: "${bodyText.substring(0, 100)}..."`);
    }

    // Take screenshot
    await page.screenshot({ path: screenshotPath, fullPage: true });

    const summary = [
      `Title: "${title}"`,
      counterClicked ? 'Executed counter button click simulation successfully.' : 'No counter button clicked.',
      chessInteraction ? 'Detected chess board/squares and simulated pieces movement successfully.' : 'No active chess interaction simulated.',
      `Unhandled Errors/Exceptions: ${errors.length}`,
      `Total console log trace rows: ${logMessages.length}`
    ].join(' | ');

    console.log('PHONTON_JSON:' + JSON.stringify({
      success: errors.length === 0,
      errors: errors,
      summary: summary
    }));

  } catch (err) {
    console.log('PHONTON_JSON:' + JSON.stringify({
      success: false,
      errors: [String(err.message || err)],
      summary: 'Playwright execution encountered an exception'
    }));
  } finally {
    if (browser) {
      await browser.close();
    }
  }
})();
"#;

    std::fs::write(&server_script, server_content)?;
    std::fs::write(&playwright_script, playwright_content)?;

    // Helper to cleanup temporary scripts
    let cleanup = || {
        let _ = std::fs::remove_file(&server_script);
        let _ = std::fs::remove_file(&playwright_script);
    };

    // 3. Start the background Node static file server
    let mut server_child = match Command::new("node")
        .arg("phonton-server.js")
        .arg("0") // Listen on a free port
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            cleanup();
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec![format!("failed to spawn node background server: {e}")],
                attempt: 1,
            }));
        }
    };

    // 4. Discover the port the server bound to from its stdout
    let stdout = server_child.stdout.take().ok_or_else(|| {
        cleanup();
        anyhow!("failed to read node server stdout")
    })?;
    let mut reader = BufReader::new(stdout).lines();

    let port_discovery = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(rest) = line.strip_prefix("SERVER_RUNNING:") {
                return Ok(rest.trim().parse::<u16>()?);
            }
        }
        Err(anyhow!("node server closed stdout before printing port"))
    })
    .await;

    let port = match port_discovery {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            let _ = server_child.kill().await;
            cleanup();
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec![format!("node server failed to start: {e}")],
                attempt: 1,
            }));
        }
        Err(_) => {
            let _ = server_child.kill().await;
            cleanup();
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec!["node server port discovery timed out after 5s".into()],
                attempt: 1,
            }));
        }
    };

    let url = format!("http://127.0.0.1:{port}");

    // Construct robust NODE_PATH to resolve 'playwright' module
    let mut node_paths = Vec::new();
    node_paths.push(working_dir.join("node_modules"));

    let mut current = working_dir;
    while let Some(parent) = current.parent() {
        node_paths.push(parent.join("node_modules"));
        current = parent;
    }

    if let Ok(cwd) = std::env::current_dir() {
        let mut cur = cwd.as_path();
        node_paths.push(cur.join("node_modules"));
        while let Some(parent) = cur.parent() {
            node_paths.push(parent.join("node_modules"));
            cur = parent;
        }
    }

    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
        if let Some(parent) = Path::new(manifest_dir).parent() {
            node_paths.push(parent.join("node_modules"));
        }
    }

    let mut path_strings = Vec::new();
    for p in node_paths {
        if p.exists() {
            if let Some(s) = p.to_str() {
                path_strings.push(s.to_string());
            }
        }
    }

    if let Ok(val) = std::env::var("NODE_PATH") {
        path_strings.push(val);
    }

    let separator = if cfg!(windows) { ";" } else { ":" };
    let joined_paths = path_strings.join(separator);

    // 5. Run the Playwright verification script using `npx playwright test`
    // 5. Run the Playwright verification script using `node`
    let mut cmd = Command::new("node");
    cmd.arg("phonton-playwright.js")
        .env("PHONTON_URL", &url)
        .env("PHONTON_SCREENSHOT", screenshot_name)
        .current_dir(working_dir);

    if !joined_paths.is_empty() {
        cmd.env("NODE_PATH", joined_paths);
    }

    let playwright_cmd = cmd.output();

    let playwright_output = tokio::time::timeout(Duration::from_secs(45), playwright_cmd).await;

    // Shutdown background server
    let _ = server_child.kill().await;
    cleanup();

    let output = match playwright_output {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec![format!("could not invoke playwright check: {e}")],
                attempt: 1,
            }));
        }
        Err(_) => {
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec!["playwright check timed out after 45s".into()],
                attempt: 1,
            }));
        }
    };

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout_str
        .lines()
        .find(|line| line.starts_with("PHONTON_JSON:"))
        .map(|line| &line["PHONTON_JSON:".len()..]);

    let parsed: serde_json::Value = match json_line {
        Some(json_str) => match serde_json::from_str(json_str) {
            Ok(val) => val,
            Err(e) => {
                let stderr_str = String::from_utf8_lossy(&output.stderr);
                return Ok(Some(VerifyResult::Fail {
                    layer: VerifyLayer::BrowserCheck,
                    errors: vec![
                        format!("failed to parse playwright JSON output: {e}"),
                        format!("Playwright stdout: {stdout_str}"),
                        format!("Playwright stderr: {stderr_str}"),
                    ],
                    attempt: 1,
                }));
            }
        },
        None => {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            return Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::BrowserCheck,
                errors: vec![
                    "playwright test did not emit PHONTON_JSON prefix".to_string(),
                    format!("Playwright stdout: {stdout_str}"),
                    format!("Playwright stderr: {stderr_str}"),
                ],
                attempt: 1,
            }));
        }
    };

    let success = parsed
        .get("success")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let _summary = parsed
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let errors = parsed
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    if success {
        // Save the screenshot_path and rendering_summary in outcome ledger context if passed
        Ok(Some(VerifyResult::Pass {
            layer: VerifyLayer::BrowserCheck,
        }))
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::BrowserCheck,
            errors,
            attempt: 1,
        }))
    }
}

fn package_json_looks_browser_runnable(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };

    let frontend_names = [
        "vite",
        "next",
        "react-scripts",
        "webpack",
        "parcel",
        "@sveltejs/kit",
        "astro",
    ];
    for section in ["dependencies", "devDependencies"] {
        if let Some(deps) = value.get(section).and_then(|v| v.as_object()) {
            if frontend_names.iter().any(|name| deps.contains_key(*name)) {
                return true;
            }
        }
    }

    value
        .get("scripts")
        .and_then(|v| v.as_object())
        .map(|scripts| {
            scripts.iter().any(|(name, command)| {
                matches!(name.as_str(), "dev" | "start" | "serve" | "preview")
                    && command
                        .as_str()
                        .map(|cmd| {
                            frontend_names
                                .iter()
                                .any(|needle| cmd.to_ascii_lowercase().contains(needle))
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
