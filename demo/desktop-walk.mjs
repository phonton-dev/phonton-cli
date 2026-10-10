// Drives the Desktop local workbench (browser build) against a real
// `phonton serve` and a real local model. Records video and screenshots.
// env: OUT (output dir), REPO (repository path), GOAL, PW (playwright module path)
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PW || 'playwright');

const OUT = process.env.OUT;
const REPO = process.env.REPO;
const GOAL = process.env.GOAL;
const size = { width: 1440, height: 900 };

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: size,
  colorScheme: 'dark',
  recordVideo: { dir: OUT, size },
});
const page = await context.newPage();
// A visible pointer that follows the real mouse events, so the video shows
// where each click lands.
await page.addInitScript(() => {
  addEventListener('DOMContentLoaded', () => {
    const dot = document.createElement('div');
    dot.style.cssText = 'position:fixed;z-index:2147483647;width:18px;height:18px;margin:-9px 0 0 -9px;border-radius:50%;background:rgba(255,255,255,.92);box-shadow:0 0 0 2px rgba(0,0,0,.55),0 4px 14px rgba(0,0,0,.5);pointer-events:none;left:-40px;top:-40px;transition:transform .12s';
    document.body.appendChild(dot);
    addEventListener('mousemove', (e) => { dot.style.left = e.clientX + 'px'; dot.style.top = e.clientY + 'px'; }, true);
    addEventListener('mousedown', () => { dot.style.transform = 'scale(.7)'; }, true);
    addEventListener('mouseup', () => { dot.style.transform = 'scale(1)'; }, true);
  });
});

let n = 0;
const shot = async (name, fullPage = false) => {
  n += 1;
  await page.screenshot({ path: `${OUT}/${String(n).padStart(2, '0')}-${name}.png`, fullPage });
  console.log('shot', name);
};
const moveTo = async (locator) => {
  const box = await locator.boundingBox();
  if (!box) throw new Error('element not visible');
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 28 });
  await page.waitForTimeout(350);
};
const click = async (locator) => {
  await moveTo(locator);
  await locator.click();
  await page.waitForTimeout(500);
};

await page.mouse.move(720, 450);
await page.goto('http://localhost:1420', { waitUntil: 'networkidle' });
await page.getByText(/engine \d/).waitFor({ timeout: 60000 });
await page.waitForTimeout(4000);
await shot('start');

await click(page.getByRole('button', { name: /Open repository/ }));
await page.locator('#repository-path').fill(REPO);
await page.keyboard.press('Enter');
await page.waitForTimeout(1500);
await click(page.locator('#local-goal'));
await page.locator('#local-goal').pressSequentially(GOAL, { delay: 28 });
await page.waitForTimeout(800);
await click(page.locator('summary', { hasText: 'Verification & permissions' }));
await click(page.locator('label.lw-approval input').first());
await shot('composed');

await click(page.getByRole('button', { name: /Review plan/ }));
await page.getByRole('button', { name: /Run local goal/ }).waitFor({ timeout: 180000 });
await page.waitForTimeout(2500);
await shot('plan', true);

await click(page.getByRole('button', { name: /Run local goal/ }));
const done = /Apply|apply this|Review ready|review-ready|stopped|failed before|budget|exhausted|No candidate/i;
const started = Date.now();
let i = 0;
while (Date.now() - started < 20 * 60 * 1000) {
  await page.waitForTimeout(5000);
  i += 1;
  if (i % 3 === 0) await shot(`running-${i}`);
  const text = await page.locator('section.lw-run').innerText().catch(() => '');
  if (!/Cancel run/.test(text) && done.test(text)) break;
}
await page.waitForTimeout(3000);
await shot('result');
await shot('result-full', true);
const run = page.locator('section.lw-run');
await run.scrollIntoViewIfNeeded().catch(() => {});
await page.mouse.wheel(0, 600);
await page.waitForTimeout(1500);
await shot('result-scrolled');

await page.goto('http://localhost:1420', { waitUntil: 'networkidle' });
await page.waitForTimeout(3000);
await click(page.getByRole('button', { name: 'Local models' }));
await page.waitForTimeout(15000);
await shot('models', true);

await context.close();
await browser.close();
