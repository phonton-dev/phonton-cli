// Harness-owned render smoke check. Functional assertions belong in the
// repository's explicit tests; clicking a guessed counter/chess board proves none.
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const root = fs.realpathSync(process.cwd());
const moduleRoots = JSON.parse(process.argv[2]);
const errors = [];
let server, browser, engine = 'chromium';
const report = (result) => console.log('PHONTON_JSON:' + JSON.stringify(result));

(async () => {
  let chromium;
  try {
    ({ chromium } = require(require.resolve('playwright', { paths: moduleRoots })));
  } catch (error) {
    report({ unavailable: true, errors: [String(error.message)] });
    return;
  }
  try {
    server = http.createServer((request, response) => {
      try {
        const route = decodeURIComponent(new URL(request.url, 'http://localhost').pathname);
        const candidate = path.resolve(root, '.' + (route === '/' ? '/index.html' : route));
        const file = fs.realpathSync(candidate);
        if (!file.startsWith(root + path.sep) || !fs.statSync(file).isFile()) {
          response.writeHead(403).end(); return;
        }
        const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png' };
        response.writeHead(200, { 'Content-Type': types[path.extname(file)] || 'application/octet-stream' });
        fs.createReadStream(file).on('error', () => response.destroy()).pipe(response);
      } catch { response.writeHead(404).end(); }
    });
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
    const origin = `http://127.0.0.1:${server.address().port}`;
    try { browser = await chromium.launch({ headless: true, timeout: 10000 }); }
    catch (error) {
      // Reuse installed branded browsers only when the bundled executable is
      // absent. A broken launch is not grounds for repeated fallback attempts.
      const failures = [String(error.message)];
      if (String(error.message).includes("Executable doesn't exist")) {
        for (const channel of ['chrome', 'msedge']) {
          try { browser = await chromium.launch({ channel, headless: true, timeout: 10000 }); engine = channel; break; }
          catch (failure) { failures.push(String(failure.message)); }
        }
      }
      if (!browser) { report({ unavailable: true, errors: failures }); return; }
    }
    const page = await browser.newPage();
    await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
    page.on('pageerror', error => { if (errors.length < 20) errors.push(String(error.message).slice(0, 2000)); });
    page.on('console', message => { if (message.type() === 'error' && errors.length < 20) errors.push(message.text().slice(0, 2000)); });
    const response = await page.goto(origin, { waitUntil: 'load', timeout: 15000 });
    if (!response || !response.ok()) errors.push(`Document HTTP status ${response?.status() ?? 'missing'}`);
    if (!(await page.locator('body').innerText()).trim()) errors.push('Page body has no visible text');
    report({ success: errors.length === 0, errors, engine, version: browser.version(), summary: 'Static render smoke only; no functional interaction assertions.' });
  } catch (error) {
    report({ unavailable: true, errors: [String(error.message).slice(0, 2000)] });
  } finally {
    if (browser) await browser.close();
    if (server) await new Promise(resolve => server.close(resolve));
  }
})();
