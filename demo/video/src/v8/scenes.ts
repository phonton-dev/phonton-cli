import SHADOW from "./ansi-shadow.json";
import {
  box, clamp01, decode, ease, easeOut, frac, grad, Grid, hash, mix, noiseGlyph, P, RAMP, typed, type RGB,
} from "./ascii";

// v8 scenes. Each draws one frame from `lb`, bars since the scene started.
// Every number and line of text below comes from the shown take (Actions run
// 39137397648, receipt.png) or the benchmark README; see README.md.

/** Three character layers: fine 12×24 (small text), mid 18×36 (the circuit), hero 24×48 (logo). */
export type Ctx = { fine: Grid; mid: Grid; hero: Grid; f: number; t: number };
export type Shot = {
  src: string;
  from: number;
  to?: number;
  rate?: number;
  /** Output bars, absolute. */
  start: number;
  end: number;
  source?: [number, number];
  cam: { x: number; y: number; s: number };
  speed: string;
};
export type Scene = { id: string; from: number; to: number; footage?: boolean; draw: (c: Ctx, lb: number) => void };

export const BAR = 240 / 174;
const SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
const spin = (f: number) => SPIN[Math.floor(f / 2) % SPIN.length];

// The TUI's ANSI Shadow logo (phonton-cli/src/main.rs LOGO).
export const LOGO = [
  "██████╗ ██╗  ██╗ ██████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ██╗",
  "██╔══██╗██║  ██║██╔═══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗  ██║",
  "██████╔╝███████║██║   ██║██╔██╗ ██║   ██║   ██║   ██║██╔██╗ ██║",
  "██╔═══╝ ██╔══██║██║   ██║██║╚██╗██║   ██║   ██║   ██║██║╚██╗██║",
  "██║     ██║  ██║╚██████╔╝██║ ╚████║   ██║   ╚██████╔╝██║ ╚████║",
  "╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝  ╚═══╝",
  "  ░▒▓█████████████████████████████████████████████████████▓▒░  ",
];

/** Logo cell color, ported from the TUI's logo_line shimmer. */
function logoColor(ch: string, x: number, row: number, phase: number): RGB {
  const base = grad(x * 0.9 + phase * 0.8 + row * 0.05);
  const dist = (w: number) => {
    const d = Math.abs(x - w);
    return Math.min(d, 1 - d);
  };
  const da = dist(frac(phase + row * 0.085));
  const db = dist(frac(phase * 1.6 - row * 0.045 + 0.37));
  if ("░▒▓".includes(ch)) {
    const body = ch === "▓" ? 0.55 : ch === "▒" ? 0.32 : 0.16;
    return mix(P.shadow, base, Math.min(0.9, body + (da < 0.14 ? (1 - da / 0.14) * 0.45 : 0)));
  }
  if ("╗╔╝╚║═".includes(ch)) return mix(mix(P.shadow, base, 0.6), P.glow, da < 0.07 ? (1 - da / 0.07) * 0.35 : 0);
  return mix(base, P.glow, db < 0.08 ? (1 - db / 0.08) * 0.4 : 0);
}

/** The logo, resolving out of noise while `reveal` < 1, flashing toward glow by `flash`. */
export function drawLogo(g: Grid, x0: number, y0: number, t: number, f: number, { reveal = 1, flash = 0, a = 1 } = {}) {
  const phase = t * 0.22;
  LOGO.forEach((row, r) =>
    [...row].forEach((ch, i) => {
      if (ch === " ") return;
      const col = mix(logoColor(ch, i / 64, r, phase), P.glow, flash);
      if (reveal < 1) {
        const start = (0.55 * hash(i, r, 7) + 0.45 * (Math.abs(i - 32) / 32)) * 0.8;
        if (reveal < start) return;
        if (reveal < start + 0.2) {
          g.put(x0 + i, y0 + r, noiseGlyph(i, r, f, 3), mix(col, P.glow, 0.5), a);
          return;
        }
      }
      g.put(x0 + i, y0 + r, ch, col, a);
    }),
  );
}

const GLYPHS = SHADOW as Record<string, string[]>;
export const bigWidth = (s: string) => [...s].reduce((w, c) => w + (c === " " ? 4 : (GLYPHS[c]?.[0].length ?? 0)), 0);
/** ANSI Shadow block letters (6 rows). */
export function bigText(g: Grid, x: number, y: number, s: string, col: (x: number, y: number) => RGB, a = 1) {
  let cx = x;
  for (const c of s) {
    const rows = GLYPHS[c];
    if (!rows) {
      cx += 4;
      continue;
    }
    rows.slice(0, 6).forEach((row, r) => [...row].forEach((ch, i) => ch !== " " && g.put(cx + i, y + r, ch, col(cx + i, y + r), a)));
    cx += rows[0].length;
  }
  return cx - x;
}

/** A slow interference field of ramp glyphs, the noise the logo decodes out of. */
export function field(g: Grid, t: number, amp: number, focus?: { x: number; y: number; w: number; h: number; k: number }) {
  if (amp <= 0) return;
  for (let y = 0; y < g.rows; y++)
    for (let x = 0; x < g.cols; x++) {
      let v = 0.42 + 0.3 * Math.sin(x * 0.085 + t * 0.8 + 1.7 * Math.sin(y * 0.21 + t * 0.35)) + 0.22 * Math.sin(y * 0.33 - t * 1.1 + x * 0.035);
      if (focus) {
        const dx = Math.max(0, Math.abs(x - focus.x) - focus.w / 2) / 10;
        const dy = Math.max(0, Math.abs(y - focus.y) - focus.h / 2) / 4;
        v += focus.k * Math.exp(-(dx * dx + dy * dy));
      }
      const d = clamp01(v * amp);
      const idx = Math.floor(d * 9.99);
      if (idx < 1) continue;
      g.put(x, y, RAMP[idx], grad((x / g.cols) * 0.8 + (y / g.rows) * 0.25 + t * 0.05), 0.1 + 0.6 * d * d);
    }
}

/** A faint dot matrix, so empty space still reads as a grid of characters. */
function dots(g: Grid, t: number, a = 0.2) {
  for (let y = 1; y < g.rows; y += 2)
    for (let x = 2; x < g.cols; x += 4) g.put(x, y, "·", grad(x / g.cols + (y / g.rows) * 0.3 + t * 0.03), a);
}

/** Plain caption text: settles out of noise in a fifth of a bar. */
export function caption(g: Grid, x: number, y: number, s: string, lb: number, at: number, f: number, col: RGB = P.ink, until = Infinity) {
  if (lb < at || lb >= until) return;
  decode(g, x, y, s, (lb - at) / 0.2, f, col, { seed: 11, spread: 0.5, window: 0.3 });
}
const centered = (g: Grid, s: string) => Math.floor((g.cols - [...s].length) / 2);

/** A ring of ramp glyphs expanding from a point (canvas pixels). */
function ripple(g: Grid, cx: number, cy: number, sb: number, col: RGB, dur = 0.5, reach = 900) {
  if (sb < 0 || sb >= dur) return;
  const k = sb / dur;
  const R = 30 + reach * easeOut(k);
  for (let y = 0; y < g.rows; y++)
    for (let x = 0; x < g.cols; x++) {
      const px = g.ox + (x + 0.5) * g.cw;
      const py = g.oy + (y + 0.5) * g.chh;
      const e = 1 - Math.abs(Math.hypot(px - cx, (py - cy) * 1.6) - R) / 34;
      if (e > 0) g.put(x, y, RAMP[Math.max(1, Math.round(e * 9))], col, e * (1 - k) * 0.8);
    }
}

// ---------------------------------------------------------------------------
// Open: noise → logo (locks on the first drop)
// ---------------------------------------------------------------------------

const OPEN_LINES = ["Phonton is a local-first coding environment.", "Every change the model makes runs against your tests before you see it."];

export function open(c: Ctx, lb: number, { logoX = 8, logoY = 7, lock = 4, capY = 32, lines = OPEN_LINES, capGrid = "fine" as "fine" | "mid" } = {}) {
  const { fine, hero, t, f } = c;
  const cg = c[capGrid];
  const reveal = ease((lb - (lock - 2)) / 2);
  const ampUp = 0.5 + 0.4 * clamp01(lb / lock);
  const amp = lb < lock ? ampUp : ampUp * (1 - 0.8 * easeOut((lb - lock) / 0.6));
  const r = hero.cw / fine.cw;
  const lf = {
    x: (hero.ox + (logoX + 32) * hero.cw) / fine.cw,
    y: (hero.oy + (logoY + 3.5) * hero.chh) / fine.chh,
    w: 64 * r,
    h: (7 * hero.chh) / fine.chh,
  };
  field(fine, t, amp, { ...lf, k: lb < lock ? 0.5 * reveal : 0 });
  if (lb >= lock - 2) {
    const flash = lb >= lock ? 0.7 * (1 - clamp01((lb - lock) / 0.3)) ** 2 : 0;
    drawLogo(hero, logoX, logoY, t, f, { reveal: lb >= lock ? 1 : Math.min(0.999, reveal), flash });
    if (lb >= lock) {
      const k = clamp01((lb - lock) / 0.4);
      fine.erase(Math.round(lf.x - lf.w / 2 - 2 * k), Math.round(lf.y - lf.h / 2 - k), Math.round(lf.w + 4 * k), Math.round(lf.h + 2 * k));
    }
  }
  lines.forEach((s, k) => caption(cg, centered(cg, s), capY + k * 2, s, lb, lock + 0.5 + k * 0.75, f, k ? P.muted : P.ink));
}

// ---------------------------------------------------------------------------
// The loop: goal → plan → edit → verify → review → remember (mid layer)
// ---------------------------------------------------------------------------

type StageState = "idle" | "busy" | "done" | "fail" | "wait";
export type LoopLayout = {
  goal: { x: number; y: number; w: number; lines: string[] };
  facts: { x: number; y: number };
  /** Fine-grid note saying where the diagram comes from. */
  note: { x: number; y: number; text: string };
  stages: { name: string; x: number; y: number; w: number }[];
  sh: number;
  /** Forward wires between stages, mid cells in order. */
  wires: [number, number][][];
  /** The repair wire from verify back to edit: mid cells with glyphs, verify end first. */
  back: [number, number, string][];
  /** Junction on verify's border where the repair wire leaves. */
  tee: [number, number, string];
  /** Mid cell of character k of the failing line (k = -1 is the photon) after travelling `base` cells; `d` > 0 while it dissolves into edit, null once absorbed. */
  ride: (k: number, n: number, base: number) => { x: number; y: number; d: number } | null;
  travel: number;
  /** Fine-grid box for the candidate's diff (main), or mid-grid log lines (vertical). */
  diff?: { x: number; y: number; w: number };
  log?: { x: number; y: number; status: number };
  /** Fine-grid box with the node --test output of each check (main). */
  checks?: { x: number; y: number; w: number };
  /** Where "waiting for you" goes once review is reached. */
  wait: { x: number; y: number };
  stamp: { x: number; y: number };
  receipt?: { x: number; y: number; w: number };
  /** Mid-grid caption row; a caption's 4th field is a row offset. */
  cap: number;
  captions: [string, number, number, number?][];
};

const hline = (x0: number, x1: number, y: number) =>
  Array.from({ length: Math.abs(x1 - x0) + 1 }, (_, i) => [x0 + (x1 > x0 ? i : -i), y] as [number, number]);

/** The repair wire's bottom run, labelled like Desktop's CandidateLoop: "└──── repair on failure ◀────┘". */
function backRun(xRight: number, xLeft: number, y: number, label = "repair on failure") {
  const n = xRight - xLeft + 1;
  const s = `${"─".repeat(4)} ${label} ◀${"─".repeat(Math.max(1, n - label.length - 7))}`.slice(0, n).padEnd(n, "─");
  return [...s].map((ch, i) => [xLeft + i, y, ch] as [number, number, string]).reverse();
}

const FACTS = "qwen3.5:4b · 4 CPU cores · no GPU";
const NOTE = "drawn from the real run · the test's failure message was written for this demo";

export const MAIN_LOOP: LoopLayout = {
  goal: { x: 3, y: 1, w: 102, lines: ["Make parsePort in @src/port.js reject ports outside 1-65535", "and trailing garbage so npm test passes"] },
  facts: { x: 3, y: 5 },
  note: { x: 157 - NOTE.length, y: 8, text: NOTE },
  stages: [
    { name: "goal", x: 3, y: 8, w: 10 },
    { name: "plan", x: 17, y: 8, w: 10 },
    { name: "edit", x: 31, y: 8, w: 10 },
    { name: "verify", x: 59, y: 8, w: 12 },
    { name: "review", x: 75, y: 8, w: 12 },
    { name: "remember", x: 91, y: 8, w: 14 },
  ],
  sh: 7,
  wires: [hline(13, 16, 11), hline(27, 30, 11), hline(41, 58, 11), hline(71, 74, 11), hline(87, 90, 11)],
  back: [
    ...[15, 16, 17, 18, 19].map((y) => [69, y, "│"] as [number, number, string]), [69, 20, "┘"],
    ...backRun(68, 33, 20),
    [32, 20, "└"], ...[19, 18, 17, 16].map((y) => [32, y, "│"] as [number, number, string]), [32, 15, "▲"],
  ],
  tee: [69, 14, "┬"],
  // row 18 between the risers, then up edit's riser into the box
  ride: (k, n, base) => {
    const x = 34 + k - base;
    if (x >= 33) return { x: Math.floor(x), y: 18, d: 0 };
    const y = 18 - Math.floor(33 - x);
    return y < 15 ? null : { x: 32, y, d: (19 - y) / 3.5 };
  },
  travel: 39,
  diff: { x: 4, y: 32, w: 98 },
  checks: { x: 107, y: 23, w: 51 },
  wait: { x: 76, y: 15 },
  stamp: { x: 3, y: 16 },
  receipt: { x: 94, y: 24, w: 64 },
  cap: 28,
  captions: [
    ["One parser bug, one small local model, no GPU.", 0.6, 4.25],
    ["The first fix fails test 3. Phonton keeps it out of your files and sends the failure back.", 4.25, 10.1],
    ["The second fix passes. 2 minutes on CPU, $0.00.", 10.1, Infinity],
  ],
};

const vline = (x: number, y0: number, y1: number) =>
  Array.from({ length: Math.abs(y1 - y0) + 1 }, (_, i) => [x, y0 + (y1 > y0 ? i : -i)] as [number, number]);
const cells = (pts: [number, number][], ch: string) => pts.map(([x, y]) => [x, y, ch] as [number, number, string]);

/** The same loop stacked for 1080×1920; the repair wire climbs a riser to the right of the stages. */
export const VERTICAL_LOOP: LoopLayout = {
  goal: { x: 1, y: 1, w: 57, lines: ["Make parsePort in @src/port.js reject", "ports outside 1-65535 and trailing", "garbage so npm test passes"] },
  facts: { x: 1, y: 6 },
  note: { x: 1, y: 11, text: "drawn from a real run · the failing test's message was written for this demo" },
  stages: ["goal", "plan", "edit", "verify", "review", "remember"].map((name, i) => ({ name, x: 8, y: [8, 13, 18, 26, 31, 36][i], w: 18 })),
  sh: 3,
  wires: [vline(16, 11, 12), vline(16, 16, 17), vline(16, 21, 25), vline(16, 29, 30), vline(16, 34, 35)],
  back: [
    ...cells(hline(26, 31, 27), "─"), [32, 27, "┘"], ...cells(vline(32, 26, 20), "│"), [32, 19, "┐"],
    ...cells(hline(31, 27, 19), "─"), [26, 19, "◀"],
    ...[..."repair on failure"].map((ch, i) => [34 + i, 27, ch] as [number, number, string]),
  ],
  tee: [25, 27, "├"],
  // two lines beside the riser, rising to edit's row, then dissolving left-first while the photon runs in
  ride: (k, n, base) => {
    const rise = Math.min(base, 6);
    if (k === -1) {
      if (base < 6) return { x: 32, y: 26 - Math.floor(rise), d: 0 };
      const x = 31 - Math.floor((base - 6) * 3);
      return x < 27 ? null : { x, y: 19, d: 0 };
    }
    if (k === 10) return null;
    const line = k < 10 ? 0 : 1;
    const col = line ? k - 11 : k;
    const d = (base - 6) * 1.5 - col / 12;
    if (d >= 1) return null;
    return { x: 34 + col, y: 25 + line - Math.floor(rise), d: Math.max(0, d) };
  },
  travel: 8,
  log: { x: 1, y: 43, status: 44 },
  wait: { x: 28, y: 32 },
  stamp: { x: 0, y: 39 },
  cap: 50,
  captions: [
    ["One parser bug, one small local model.", 2.4, 4.0],
    ["The first fix fails test 3.", 4.0, 10.0],
    ["Phonton sends the failure back.", 4.5, 10.0, 1],
    ["The second fix passes.", 10.0, Infinity],
    ["2 minutes on CPU, $0.00.", 10.4, Infinity, 1],
  ],
};

// Timing in bars from the scene start. Photon runs: [wire, from, to].
const RUNS: [number, number, number][] = [[0, 2.0, 2.35], [1, 2.5, 2.85], [2, 3.5, 3.875], [2, 9.25, 9.75], [3, 10.75, 11.1]];
const FAIL_WIRE = 2;
const FAIL_AT = 4;
const PASS_AT = 10;
const CARRY: [number, number] = [4.5, 6.5];
const FAIL_LINE = "not ok 3 - rejects trailing garbage";
/** The repair wire, the excerpt and the baseline check are on screen from here, before candidate 1. */
const BACK_AT = 1.0;
const EXCERPT_AT = 1.0;
const CHECKS_AT = 1.3;
// src/port.js lines 4-7 as the model saw them (the run's context excerpt, "parsed symbol parsePort").
const EXCERPT = ["function parsePort(value) {", "  const port = parseInt(value, 10);", "  return port;", "}"];
// node --test --test-reporter=tap output of the run's three checks (check-1..3.json), TAP detail lines omitted.
export const TAP = {
  baseline: ["ok 1 - parses a plain port", "not ok 2 - rejects ports outside 1-65535", "not ok 3 - rejects trailing garbage", "1..3", "# pass 1", "# fail 2"],
  cand1: [
    "ok 1 - parses a plain port", "ok 2 - rejects ports outside 1-65535", "not ok 3 - rejects trailing garbage",
    "  error: 'Missing expected exception:", '    parsePort("80abc") returned 80:', "    parseInt ignores trailing characters'",
    "1..3", "# pass 2", "# fail 1",
  ],
  cand2: ["ok 1 - parses a plain port", "ok 2 - rejects ports outside 1-65535", "ok 3 - rejects trailing garbage", "1..3", "# pass 3", "# fail 0"],
};
// The lines each candidate added to src/port.js (after line 5), from the shown take's candidate sources.
const THROW = '    throw new Error(`parsePort("${value}") returned ${port}: invalid port`);';
const CAND = [
  ["  if (isNaN(port) || port < 1 || port > 65535) {", THROW, "  }"],
  ["  if (isNaN(port) || port < 1 || port > 65535 || value.trim() !== String(port)) {", THROW, "  }"],
];
const REPAIR = "|| value.trim() !== String(port)";

function stageState(i: number, lb: number): StageState {
  switch (i) {
    case 0: return lb < 2 ? "busy" : "done";
    case 1: return lb < 2.35 ? "idle" : lb < 2.5 ? "busy" : "done";
    case 2: return lb < 2.85 ? "idle" : lb < 3.5 || (lb >= 6.5 && lb < 9.25) ? "busy" : "done";
    case 3: return lb < 3.875 ? "idle" : lb < FAIL_AT ? "busy" : lb < 9.75 ? "fail" : lb < PASS_AT ? "busy" : "done";
    case 4: return lb < 11.1 ? "idle" : "wait";
    default: return "idle";
  }
}
const stateGlyph = (s: StageState, f: number) => (s === "busy" ? spin(f) : s === "done" ? "✓" : s === "fail" ? "✗" : s === "wait" ? "◇" : "○");
const stateCol = (s: StageState): RGB => (s === "done" ? P.ok : s === "fail" ? P.danger : s === "idle" ? P.muted : P.cyan);

/** Desktop-style "label ····· value" row. */
export function leader(g: Grid, x: number, y: number, w: number, label: string, value: string, col: RGB = P.ink) {
  g.text(x, y, label, P.muted);
  const vx = x + w - [...value].length;
  for (let k = x + label.length + 1; k < vx - 1; k++) g.put(k, y, "·", P.dim);
  g.text(vx, y, value, col);
}

/** Edit lights up while the failing characters arrive. */
const absorbGlow = (lb: number) => (lb >= 4.9 && lb < 6.5 ? 0.8 * clamp01((lb - 4.9) / 0.3) : 0);

export function loop(c: Ctx, lb: number, L: LoopLayout = MAIN_LOOP) {
  const { fine, mid: g, t, f } = c;
  dots(fine, t);

  // the goal and the facts of this run
  const G = L.goal;
  box(g, G.x, G.y, G.w, G.lines.length + 2, (u) => mix(P.dim, grad(u * 1.5 + t * 0.05), 0.65), { p: easeOut(lb / 0.4), title: "goal", titleCol: P.ink });
  G.lines.forEach((s, k) => decode(g, G.x + 2, G.y + 1 + k, s, (lb - 0.2 - k * 0.3) / 0.8, f, P.ink, { seed: 2 + k }));
  decode(g, L.facts.x, L.facts.y, FACTS, (lb - 0.9) / 0.5, f, P.cyan, { seed: 4 });
  decode(fine, L.note.x, L.note.y, L.note.text, (lb - 1.1) / 0.5, f, P.muted, { seed: 5 });

  // forward wires: dim until the photon passes, then a current in the logo gradient
  L.wires.forEach((cells, w) => {
    const vert = cells.length > 1 && cells[0][0] === cells[1][0];
    const n = cells.length;
    const shown = Math.floor(n * easeOut((lb - (0.65 + w * 0.25)) / 0.25));
    for (let k = 0; k < shown; k++) {
      const [x, y] = cells[k];
      let lit = 0;
      let head = 0;
      for (const [rw, a, b] of RUNS) {
        if (rw !== w) continue;
        const tp = a + ((k + 0.5) / n) * (b - a);
        if (lb < tp) continue;
        lit = Math.max(lit, 0.7 + 0.3 * Math.exp(-(lb - tp) * 2.5));
        if (lb - tp < 0.15) head = Math.max(head, 1 - (lb - tp) / 0.15);
      }
      let col: RGB = lit ? mix(P.dim, grad(x * 0.035 + y * 0.05 - t * 0.5), lit) : P.dim;
      col = mix(col, P.glow, head * 0.8);
      if (w === FAIL_WIRE && lb >= FAIL_AT && lb < 9.25) col = mix(col, P.danger, 0.6 + 0.4 * Math.exp(-(lb - FAIL_AT) * 2));
      g.put(x, y, k === n - 1 ? (vert ? "▼" : "▶") : lit ? (vert ? "┃" : "━") : vert ? "│" : "─", col);
    }
  });

  // stage boxes; a working stage fills with moving shade
  L.stages.forEach((s, i) => {
    const p = easeOut((lb - (0.5 + i * 0.25)) / 0.3);
    if (p <= 0) return;
    const st = stageState(i, lb);
    const failFlash = i === 3 && lb >= FAIL_AT ? Math.exp(-(lb - FAIL_AT) * 3) : 0;
    const absorb = i === 2 ? absorbGlow(lb) : 0;
    const col = (u: number): RGB =>
      st === "fail" ? mix(P.danger, P.glow, failFlash * 0.5)
        : st === "busy" ? grad(u * 2 + t * 0.6)
          : st === "done" ? mix(mix(grad(u * 2 + t * 0.1), P.dim, 0.35), P.pink, absorb)
            : st === "wait" ? grad(u + t * 0.2)
              : mix(P.dim, P.pink, absorb);
    box(g, s.x, s.y, s.w, L.sh, col, { p, style: st === "fail" ? "heavy" : "round" });
    if (p < 1) return;
    const row = s.y + Math.floor(L.sh / 2);
    for (let yy = s.y + 1; yy < s.y + L.sh - 1; yy++) {
      for (let xx = yy === row ? s.x + 5 + s.name.length : s.x + 1; xx < s.x + s.w - 1; xx++) {
        if (st === "busy") {
          const v = 0.5 + 0.5 * Math.sin(xx * 0.9 - t * 9 + yy * 1.3);
          if (v > 0.25) g.put(xx, yy, "░▒▓"[Math.min(2, Math.floor((v - 0.25) * 4))], grad(xx * 0.03 + t * 0.4), 0.7);
        } else if (failFlash > 0.05) g.put(xx, yy, failFlash > 0.5 ? "▓" : failFlash > 0.2 ? "▒" : "░", P.danger, failFlash);
        else if (absorb > 0.05) g.put(xx, yy, hash(xx, yy, f >> 1) > 0.5 ? "▒" : "░", P.pink, absorb * 0.8);
      }
    }
    g.put(s.x + 2, row, stateGlyph(st, f), stateCol(st));
    g.text(s.x + 4, row, s.name, st === "idle" ? P.muted : st === "fail" ? P.danger : P.ink);
  });
  if (lb >= 11.1) decode(g, L.wait.x, L.wait.y, "waiting for you", (lb - 11.1) / 0.3, f, P.muted, { seed: 9 });

  // photons on the forward wires
  for (const [w, a, b] of RUNS) {
    if (lb < a || lb >= b) continue;
    const cells = L.wires[w];
    const [x, y] = cells[Math.min(cells.length - 1, Math.floor(((lb - a) / (b - a)) * cells.length))];
    g.put(x, y, "φ", P.glow);
  }

  // the check result ripples out of verify
  const v = L.stages[3];
  const vcx = g.ox + (v.x + v.w / 2) * g.cw;
  const vcy = g.oy + (v.y + L.sh / 2) * g.chh;
  ripple(fine, vcx, vcy, lb - FAIL_AT, P.danger);
  ripple(fine, vcx, vcy, lb - PASS_AT, P.cyan, 0.6, 1100);

  // the repair wire: idle from the start, red from verify back to edit once candidate 1 fails
  const idle = mix(P.dim, P.muted, 0.45);
  if (lb < PASS_AT) {
    const shown = Math.floor(L.back.length * easeOut((lb - BACK_AT) / 0.35));
    const carrying = lb >= CARRY[0] && lb < CARRY[1];
    const redTo = lb >= FAIL_AT ? L.back.length * easeOut((lb - (FAIL_AT + 0.1)) / 0.35) : 0;
    for (let k = 0; k < shown; k++) {
      const [x, y, ch] = L.back[k];
      const red = k < redTo;
      const col = !"─│┘└┐┌◀▲".includes(ch) ? P.muted
        : !red ? idle
          : carrying ? mix(P.danger, P.glow, 0.2 * (1 - frac(lb * 4))) : lb < CARRY[0] ? P.danger : mix(P.dim, P.pink, 0.45);
      g.put(x, y, red && carrying && ch === "─" ? "━" : ch, col);
    }
    if (shown > 0) g.put(L.tee[0], L.tee[1], L.tee[2], stageState(3, lb) === "fail" ? P.danger : idle);
  }

  // the failing line, carried back along the repair wire to the model
  if (lb >= FAIL_AT && lb < PASS_AT) {

    const chars = [...FAIL_LINE];
    const n = chars.length;
    const base = L.travel * ease((lb - CARRY[0]) / (CARRY[1] - CARRY[0]));
    const appear = (lb - FAIL_AT - 0.15) / 0.4;
    chars.forEach((ch, k) => {
      const at = L.ride(k, n, base);
      if (!at || ch === " ") return;
      const start = (k / n) * 0.6;
      if (appear < start) return;
      // on the riser each character dissolves into shade on its way into edit
      const glyph = appear < start + 0.3 ? noiseGlyph(at.x, at.y, f, 21) : at.d > 0 ? "▓▒░"[Math.min(2, Math.floor(at.d * 3))] : ch;
      g.put(at.x, at.y, glyph, at.d > 0 ? mix(P.pink, P.glow, 0.3) : mix(P.danger, P.pink, (base / L.travel) * 0.8));
    });
    const ph = appear >= 1 && lb < CARRY[1] ? L.ride(-1, n, base) : null;
    if (ph) g.put(ph.x, ph.y, "φ", P.glow);
  }

  // the candidate's diff, typed while edit works; replaced by the stamp once candidate 2 passes
  if (lb >= PASS_AT) stampAndReceipt(c, lb - PASS_AT, L);
  else {
    if (L.diff && lb >= EXCERPT_AT) diffBox(c, lb, L.diff);
    if (L.checks && lb >= CHECKS_AT) checksBox(c, lb, L.checks);
    if (L.log && lb >= 2.85) logLines(c, lb, L.log);
  }

  for (const [s, at, until, dy = 0] of L.captions) caption(g, centered(g, s), L.cap + dy, s, lb, at, f, dy ? P.muted : P.ink, until);
}

function logLines(c: Ctx, lb: number, L: NonNullable<LoopLayout["log"]>) {
  const { mid: g, f } = c;
  g.text(L.x, L.y, "candidate 1 · direct implementation", P.ink);
  if (lb < FAIL_AT) g.text(L.status, L.y, `${spin(f)} writing`, P.muted);
  else decode(g, L.status, L.y, "✗ failed test 3", (lb - FAIL_AT) / 0.25, f, P.danger, { seed: 15 });
  if (lb < 6.5) return;
  decode(g, L.x, L.y + 1, "candidate 2 · repair from failed candidate", (lb - 6.5) / 0.3, f, P.ink, { seed: 17 });
  g.text(L.status, L.y + 1, lb < 9.75 ? `${spin(f)} writing` : `${spin(f)} checking`, P.muted);
}

/** A box title that decodes in when it changes. */
function boxTitle(g: Grid, x: number, y: number, s: string, since: number, f: number) {
  g.erase(x, y, [...s].length + 2, 1);
  decode(g, x + 1, y, s, since / 0.3, f, P.ink, { seed: 27 });
}

function diffBox(c: Ctx, lb: number, D: NonNullable<LoopLayout["diff"]>) {
  const { fine, t, f } = c;
  const cand = lb >= 2.85;
  const two = lb >= 6.5;
  const k = two ? 1 : 0;
  const failed = cand && !two && lb >= FAIL_AT;
  const h = EXCERPT.length + CAND[0].length + 2;
  box(fine, D.x, D.y, D.w, h, (u) => (failed ? mix(P.dim, P.danger, 0.6) : mix(P.dim, grad(u * 2 + t * 0.2), 0.7)), { p: easeOut((lb - EXCERPT_AT) / 0.3) });
  if (lb < EXCERPT_AT + 0.15) return;
  const title = !cand ? "src/port.js · lines 4-7 · the excerpt the model sees"
    : two ? "src/port.js · candidate 2 · repair from failed candidate" : "src/port.js · candidate 1 · direct implementation";
  boxTitle(fine, D.x + 2, D.y, title, lb - (two ? 6.5 : cand ? 2.85 : EXCERPT_AT), f);
  // candidate 1 is written in 0.65 bar, candidate 2 over 2.5 bars; added lines push 6-7 down
  let budget = cand ? Math.max(0, Math.floor((lb - (two ? 6.6 : 2.9)) * (two ? 75 : 210))) : 0;
  const rows: { src: string; s: string; add: boolean }[] = EXCERPT.slice(0, 2).map((s) => ({ src: s, s, add: false }));
  for (const line of CAND[k]) {
    if (budget <= 0) break;
    rows.push({ src: line, s: line.slice(0, budget), add: true });
    budget -= line.length;
  }
  rows.push(...EXCERPT.slice(2).map((s) => ({ src: s, s, add: false })));
  rows.forEach((r, i) => {
    const y = D.y + 1 + i;
    fine.text(D.x + 2, y, String(4 + i).padStart(3), P.dim);
    if (!r.add) {
      decode(fine, D.x + 8, y, r.s, (lb - EXCERPT_AT - 0.15 - i * 0.06) / 0.3, f, P.muted, { seed: 23 + i });
      return;
    }
    fine.text(D.x + 6, y, "+", failed ? P.danger : P.ok);
    const r0 = two ? r.src.indexOf(REPAIR) : -1;
    fine.text(D.x + 8, y, r.s, (j) => (failed ? mix(P.muted, P.danger, 0.35) : r0 >= 0 && j >= r0 && j < r0 + REPAIR.length ? mix(P.cyan, P.glow, 0.5 + 0.5 * Math.sin(t * 6)) : P.ink));
    if (r.s.length < r.src.length && Math.floor(f / 4) % 2 === 0) fine.put(D.x + 8 + r.s.length, y, "█", P.cyan, 0.8);
  });
  const status = !cand ? "parsed symbol parsePort" : failed ? "✗ failed test 3 · kept out of your files" : lb >= (two ? 9.75 : 3.875) ? `${spin(f)} checking` : `${spin(f)} writing`;
  const st = ` ${status} `;
  const sx = D.x + D.w - 2 - st.length;
  if (failed) {
    fine.erase(sx, D.y + h - 1, st.length, 1);
    decode(fine, sx, D.y + h - 1, st, (lb - FAIL_AT) / 0.25, f, P.danger, { seed: 15 });
  } else fine.label(sx, D.y + h - 1, st, P.muted);
}

/** The check's own output: the baseline, then each candidate's run, printed as it comes. */
function checksBox(c: Ctx, lb: number, D: NonNullable<LoopLayout["checks"]>) {
  const { fine, t, f } = c;
  const phase = lb >= 9.75 ? 2 : lb >= 3.875 ? 1 : 0;
  const lines = [TAP.baseline, TAP.cand1, TAP.cand2][phase];
  const t0 = [CHECKS_AT + 0.15, 3.9, 9.8][phase];
  const step = [0.1, 0.05, 0.035][phase];
  const h = TAP.cand1.length + 2;
  const tone = phase === 2 && lb >= 9.99 ? P.ok : phase === 1 && lb >= FAIL_AT ? P.danger : undefined;
  box(fine, D.x, D.y, D.w, h, (u) => (tone ? mix(P.dim, tone, 0.6) : mix(P.dim, grad(u * 2 + t * 0.2 + 0.5), 0.7)), { p: easeOut((lb - CHECKS_AT) / 0.3) });
  if (lb < CHECKS_AT + 0.15) return;
  boxTitle(fine, D.x + 2, D.y, `node --test · ${["baseline", "candidate 1", "candidate 2"][phase]}`, lb - [CHECKS_AT, 3.875, 9.75][phase], f);
  lines.forEach((s, i) => {
    if (lb < t0 + i * step) return;
    const y = D.y + 1 + i;
    const fail = s.startsWith("not ok");
    const col: RGB = fail ? P.danger : s.startsWith("ok") ? P.ok : s.startsWith("  ") ? mix(P.muted, P.danger, 0.3) : P.muted;
    decode(fine, D.x + 2, y, s, (lb - t0 - i * step) / 0.12, f, col, { seed: 61 + i });
    if (phase === 1 && i === 2 && lb >= FAIL_AT) fine.put(D.x + 3 + s.length, y, "◀", P.danger, 0.6 + 0.4 * Math.sin(t * 8));
  });
  const done = lb >= t0 + lines.length * step;
  const status = phase === 0 ? "fails on the unchanged source" : !done ? `${spin(f)} running` : phase === 1 ? "exit code 1" : "exit code 0";
  const st = ` ${status} `;
  fine.label(D.x + D.w - 2 - st.length, D.y + h - 1, st, phase === 1 && done ? P.danger : phase === 2 && done ? P.ok : P.muted);
}

/** Block-letter VERIFIED landing in a heavy frame (mid layer), the receipt printing beside it a line per beat (fine). */
export function stampAndReceipt(c: Ctx, sb: number, L: Pick<LoopLayout, "stamp" | "receipt">) {
  const { mid: g, fine, t, f } = c;
  const w = bigWidth("VERIFIED");
  const { x, y } = L.stamp;
  const land = clamp01(sb / 0.15);
  const settle = clamp01((sb - 0.1) / 0.5);
  // a shading halo collapsing onto the frame as it lands
  if (land < 1) {
    const r = Math.round(3 * (1 - land));
    const ring = ["░", "▒", "▓"][Math.min(2, Math.floor(land * 3))];
    for (let xx = x - r; xx < x + w + 4 + r; xx++) {
      g.put(xx, y - r, ring, P.glow, 0.7);
      g.put(xx, y + 9 + r, ring, P.glow, 0.7);
    }
    for (let yy = y - r; yy <= y + 9 + r; yy++) {
      g.put(x - r, yy, ring, P.glow, 0.7);
      g.put(x + w + 3 + r, yy, ring, P.glow, 0.7);
    }
  }
  const sweep = frac(t * 0.35);
  const col = (cx: number): RGB => {
    const u = (cx - x) / (w + 4);
    const shine = Math.max(0, 1 - Math.abs(u - sweep) / 0.06) * 0.5;
    return mix(mix(P.glow, grad(u * 0.9 + t * 0.12), settle), P.glow, shine);
  };
  box(g, x, y, w + 4, 10, (u) => mix(P.glow, grad(u + t * 0.1), settle), { style: "heavy" });
  bigText(g, x + 2, y + 2, "VERIFIED", (cx) => col(cx));

  if (!L.receipt) return;
  const { x: rx, y: ry, w: rw } = L.receipt;
  box(fine, rx, ry, rw, 13, (u) => mix(P.dim, grad(u + t * 0.05), 0.55), { p: easeOut(sb / 0.25), title: "receipt", titleCol: P.ink });
  const lineAt = (k: number) => 0.15 + k * 0.2;
  const count = (k: number) => easeOut((sb - lineAt(k)) / 0.3);
  const at = (k: number, fn: () => void) => sb >= lineAt(k) && fn();
  const lx = rx + 2;
  const iw = rw - 4;
  at(0, () => decode(fine, lx, ry + 2, "Candidate 2 passed 1 check on qwen3.5:4b", (sb - lineAt(0)) / 0.2, f, P.ink, { seed: 31 }));
  at(1, () => leader(fine, lx, ry + 4, iw, "files", "1  +3 -0"));
  at(2, () => leader(fine, lx, ry + 5, iw, "checks", "1 passed", P.ok));
  at(3, () => leader(fine, lx, ry + 6, iw, "tokens in", String(Math.round(805 * count(3)))));
  at(4, () => leader(fine, lx, ry + 7, iw, "tokens out", String(Math.round(300 * count(4)))));
  at(5, () => {
    const s = Math.round(120 * count(5));
    leader(fine, lx, ry + 8, iw, "time", `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`);
  });
  at(6, () => leader(fine, lx, ry + 9, iw, "API cost", "$0.00 · managed local runtime"));
  at(7, () => leader(fine, lx, ry + 10, iw, "streak", "1 verified in a row", P.ok));
}

// ---------------------------------------------------------------------------
// Footage in ASCII frames (frame on the mid layer, notes on the fine layer)
// ---------------------------------------------------------------------------

// Full-width frame cropped to the footage's content; callout labels and the run's panels sit under it.
export const FRAME = { x: 2, y: 1, w: 102, h: 16 };
const MID = { ox: 6, cw: 18, ch: 36 };
export const FRAME_PX = {
  x: MID.ox + (FRAME.x + 1) * MID.cw,
  y: (FRAME.y + 1) * MID.ch,
  w: (FRAME.w - 2) * MID.cw,
  h: (FRAME.h - 2) * MID.ch,
};

/** `wire: false` for targets with UI text to their right: markers only, no leader across the footage. */
/** Marks a source rectangle inside the frame (▶ ◀ either side, ◀ only, or ▼ above) and labels it under the frame. */
export type Callout = { at: number; until?: number; src: [number, number, number, number]; lines: [string, RGB][]; tone: RGB; mark?: "line" | "right" | "above" };

/** Source pixel → canvas pixel, through the shot's camera. */
export function toPx(shot: Shot, sx: number, sy: number) {
  return [FRAME_PX.x + FRAME_PX.w / 2 + (sx - shot.cam.x) * shot.cam.s, FRAME_PX.y + FRAME_PX.h / 2 + (sy - shot.cam.y) * shot.cam.s] as const;
}

const LABEL_ROW = Math.ceil(((FRAME.y + FRAME.h) * MID.ch) / 24);
/** Two boxes under the frame (fine grid): the run's figures and its check output. */
export const PANEL = { y: LABEL_ROW + 3, h: 11, w: 58, left: 20, right: 82 };

export function framed(
  c: Ctx,
  lb: number,
  o: {
    title: string; shots: Shot[]; B: number; note: string; cap: string; capAt: number; callouts: (Callout & { shot: Shot })[];
    /** Draws the panels under the frame. */
    below?: (c: Ctx, B: number, lb: number) => void;
  },
) {
  const { fine, mid, t, f } = c;
  dots(fine, t, 0.08);
  const shot = o.shots.find((s) => o.B >= s.start && o.B < s.end) ?? o.shots[o.shots.length - 1];
  fine.erase(Math.floor(FRAME_PX.x / 12), Math.floor(FRAME_PX.y / 24), Math.ceil(FRAME_PX.w / 12) + 1, Math.ceil(FRAME_PX.h / 24) + 1);
  box(mid, FRAME.x, FRAME.y, FRAME.w, FRAME.h, (u) => mix(P.shadow, grad(u * 1.5 + t * 0.06), 0.85), { style: "double" });
  mid.label(FRAME.x + 2, FRAME.y, ` ${o.title} `, P.ink);
  mid.label(FRAME.x + 2, FRAME.y + FRAME.h - 1, ` ${o.note} `, P.muted);
  const sp = ` ${shot.speed} `;
  mid.label(FRAME.x + FRAME.w - 2 - sp.length, FRAME.y + FRAME.h - 1, sp, shot.speed === "real time" ? P.muted : P.cyan);
  caption(mid, centered(mid, o.cap), 27, o.cap, lb, o.capAt, f);
  o.below?.(c, o.B, lb);

  for (const co of o.callouts) {
    const sb = o.B - co.at;
    if (sb < 0 || o.B >= (co.until ?? Infinity) || o.B < co.shot.start || o.B >= co.shot.end) continue;
    const [sx, sy, sw, sh] = co.src;
    const [lpx, lpy] = toPx(co.shot, sx, sy + sh / 2);
    const [rpx] = toPx(co.shot, sx + sw, sy);
    const row = Math.floor(lpy / 24);
    const mark = co.mark ?? "line";
    if (mark === "above") fine.put(Math.round((lpx + rpx) / 24), row - 1, "▼", co.tone);
    else {
      if (mark === "line") fine.put(Math.floor(lpx / 12) - 1, row, "▶", co.tone);
      fine.put(Math.ceil(rpx / 12), row, "◀", co.tone);
    }
    const width = Math.max(...co.lines.map(([s]) => [...s].length)) + 2;
    const lx = Math.max(4, Math.min(fine.cols - 4 - width, Math.floor(lpx / 12)));
    co.lines.forEach(([s, col], k) =>
      decode(fine, lx, LABEL_ROW + k, (k === 0 ? "▲ " : "  ") + s, (sb - 0.1 - k * 0.12) / 0.3, f, (i) => (i === 0 ? co.tone : col), { seed: 40 + k }),
    );
  }
}

/** A panel's frame under the footage; its interior is cleared of the dot grid. */
function panelBox(c: Ctx, x: number, lb: number, tone?: RGB) {
  const { fine, t } = c;
  const { y, w, h } = PANEL;
  box(fine, x, y, w, h, (u) => (tone ? mix(P.dim, tone, 0.6) : mix(P.dim, grad(u * 2 + t * 0.2 + x / 80), 0.7)), { p: easeOut(lb / 0.3) });
  fine.erase(x + 1, y + 1, w - 2, h - 2);
}

export type Facts = { title: string; sub: string; at: number; rows: [string, string][]; tone: RGB };

/** Figures from the run: spinners until `at` (output bars), then the values decode in. */
export function factsPanel(c: Ctx, x: number, B: number, lb: number, o: Facts) {
  const { fine, f } = c;
  const done = B >= o.at;
  const { y, w } = PANEL;
  panelBox(c, x, lb, done ? o.tone : undefined);
  if (lb < 0.15) return;
  boxTitle(fine, x + 2, y, o.title, lb - 0.15, f);
  decode(fine, x + 2, y + 1, o.sub, (lb - 0.2) / 0.3, f, P.muted, { seed: 96 });
  o.rows.forEach(([label, value], i) => {
    if (lb < 0.3 + i * 0.08) return;
    const yy = y + 3 + i;
    const last = i === o.rows.length - 1;
    const v = done ? value : spin(f + i * 3);
    leader(fine, x + 2, yy, w - 4, label, v, done && last ? o.tone : done ? P.ink : P.muted);
    if (done && B - o.at < 0.3) {
      const vx = x + w - 2 - [...value].length;
      fine.erase(vx, yy, [...value].length, 1);
      decode(fine, vx, yy, value, (B - o.at) / 0.25, f, last ? o.tone : P.ink, { seed: 97 + i });
    }
  });
}

export type TapPhase = { at: number; title: string; lines: string[]; step: number; done: string; tone?: RGB };

/** node --test output under the frame: the latest phase whose `at` (output bars) has passed, printed a line per `step`. */
export function tapPanel(c: Ctx, x: number, B: number, lb: number, phases: TapPhase[]) {
  const { fine, t, f } = c;
  let k = 0;
  phases.forEach((p, i) => B >= p.at && (k = i));
  const ph = phases[k];
  const t0 = ph.at + 0.05;
  const finished = B >= t0 + ph.lines.length * ph.step;
  const tone = finished ? ph.tone : undefined;
  const { y, w, h } = PANEL;
  panelBox(c, x, lb, tone);
  if (lb < 0.15) return;
  boxTitle(fine, x + 2, y, `node --test · ${ph.title}`, Math.min(lb - 0.15, B - ph.at), f);
  ph.lines.forEach((s, i) => {
    const at = t0 + i * ph.step;
    if (B < at) return;
    const yy = y + 1 + i;
    const col: RGB = s.startsWith("not ok") ? P.danger : s.startsWith("ok") ? P.ok : s.startsWith("  ") ? mix(P.muted, P.danger, 0.3) : P.muted;
    decode(fine, x + 2, yy, s, Math.min(lb - 0.15, B - at) / 0.12, f, col, { seed: 61 + i });
    if (finished && tone === P.danger && s.startsWith("not ok 3")) fine.put(x + 3 + s.length, yy, "◀", P.danger, 0.6 + 0.4 * Math.sin(t * 8));
  });
  const st = ` ${finished ? ph.done : `${spin(f)} running`} `;
  fine.label(x + w - 2 - st.length, y + h - 1, st, tone ?? P.muted);
}

// ---------------------------------------------------------------------------
// Numbers: 51 exercises, said done vs. false (mid layer)
// ---------------------------------------------------------------------------

// Benchmark run full-4b-0.25.0: phonton-brain/05-benchmarks/polyglot-local-2026-10/README.md
const ARMS = [
  { label: "Bare model", note: "Aider's prompt, two tries", done: 43, wrong: 37, passed: 6 },
  { label: "OpenCode 1.14.19", note: "run --pure", done: 51, wrong: 50, passed: 1 },
  { label: "Phonton 0.25.0", note: "your tests decide", done: 1, wrong: 0, passed: 2 },
];

export function numbers(c: Ctx, lb: number) {
  const { fine, mid: g, t, f } = c;
  dots(fine, t, 0.08);
  const c1 = "We ran the same model on 51 Aider Polyglot exercises, three ways.";
  const c2 = "None of them solved much. Phonton just didn't call the failures done.";
  caption(g, centered(g, c1), 1, c1, lb, 0.3, f, P.ink, 4);
  caption(g, centered(g, c2), 1, c2, lb, 4, f);
  const fx = (mx: number) => Math.round((g.ox + mx * g.cw) / fine.cw);
  decode(fine, fx(60), 6, "said done", (lb - 0.4) / 0.3, f, P.muted, { seed: 51 });
  decode(fine, fx(82), 6, "said done, tests failed", (lb - 0.4) / 0.3, f, P.muted, { seed: 52 });

  ARMS.forEach((arm, k) => {
    const y0 = 5 + k * 8;
    const me = k === 2;
    const at = 0.3 + k * 0.15;
    decode(g, 3, y0, arm.label, (lb - at) / 0.3, f, me ? P.cyan : P.ink, { seed: 60 + k });
    decode(fine, fx(3 + arm.label.length) + 2, Math.floor((y0 * 36 + 18) / 24), arm.note, (lb - at - 0.1) / 0.3, f, P.muted, { seed: 63 + k });
    const fill = easeOut((lb - at - 0.2) / 1.2);
    const claimed = Math.round(arm.done * fill);
    const firstFalse = arm.done - arm.wrong;
    let crumbled = 0;
    for (let j = 0; j < 51 && lb >= at + 0.1; j++) {
      const x = 3 + j;
      if (j >= claimed) {
        g.put(x, y0 + 2, "░", P.dim, 0.8);
        g.put(x, y0 + 3, "░", P.dim, 0.8);
        continue;
      }
      const col = grad(j / 70 + t * 0.06);
      const tc = 2.0 + 1.4 * ((j - firstFalse) / Math.max(1, arm.wrong)) + 0.35 * hash(j, k, 5);
      if (j < firstFalse || lb < tc) {
        const pulse = me ? 0.5 * (1 - frac(lb * 2)) ** 2 : 0;
        g.put(x, y0 + 2, "█", mix(col, P.glow, pulse));
        g.put(x, y0 + 3, "█", mix(col, P.glow, pulse));
        continue;
      }
      crumbled++;
      if (lb - tc > 0.5) {
        g.put(x, y0 + 2, "░", P.danger, 0.3);
        g.put(x, y0 + 3, "░", P.danger, 0.3);
      }
      // a false "done" falls apart: block → shade → dust, dropping and turning red
      for (let r = 0; r < 2; r++) {
        const dt = lb - tc - r * 0.05;
        if (dt < 0) {
          g.put(x, y0 + 2 + r, "█", col);
          continue;
        }
        if (dt > 0.9) continue;
        const stage = "▓▒░:."[Math.min(4, Math.floor(dt / 0.12))];
        const drop = 3.2 * dt * dt;
        const dx = Math.round((hash(j, r, 9) - 0.5) * 2.5 * dt);
        g.put(x + dx, y0 + 2 + r + Math.round(drop), stage, mix(col, P.danger, dt * 5), 1 - dt / 0.9);
      }
    }
    if (lb >= at + 0.2) bigText(g, 60, y0, String(claimed), (x) => grad(x / 110 + t * 0.06));
    if (lb >= at + 0.2) bigText(g, 82, y0, String(crumbled), () => (!arm.wrong ? P.ok : crumbled ? P.danger : mix(P.dim, P.danger, 0.45)));
    if (lb >= 4.25 + k * 0.15) {
      const s = `tests passed ${arm.passed}`;
      const py = Math.floor(((y0 + 4) * 36 + 12) / 24);
      decode(fine, fx(3), py, s, (lb - 4.25 - k * 0.15) / 0.3, f, P.muted, { seed: 70 + k });
      for (let j = 0; j < arm.passed; j++) fine.put(fx(3) + s.length + 2 + j * 2, py, "█", P.ok);
    }
  });
  const foot = [
    "Aider Polyglot: 17 Python, 17 JavaScript, 17 Rust · qwen3.5:4b on an RTX 4050 laptop, not the runner in the footage",
    "18 Phonton runs hit 0.25.0's memory reserve (fixed in 0.25.1) · \"false\" = said done, tests failed · raw logs: phonton-cli v0.25.1 release",
  ];
  foot.forEach((s, k) => decode(fine, 5, 42 + k, s, (lb - 1.0) / 0.5, f, P.muted, { seed: 80 + k, a: 0.85 }));
}

// ---------------------------------------------------------------------------
// End: logo, npm install typed out, phonton.dev
// ---------------------------------------------------------------------------

export function end(
  c: Ctx,
  lb: number,
  o: { logoX: number; logoY: number; cmdY: number; urlY: number; footY: number; last: number; logoIn?: number; typeAt?: number; urlAt?: number; footAt?: number },
) {
  const { fine, mid, hero, t, f } = c;
  const { logoIn = 0.9, typeAt = 1.0, urlAt = 3, footAt = 3.5 } = o;
  const fade = 1 - clamp01((lb - (o.last - 0.4)) / 0.4);
  drawLogo(hero, o.logoX, o.logoY, t, f, { reveal: Math.min(1, lb / logoIn), flash: lb >= logoIn ? 0.6 * (1 - clamp01((lb - logoIn) / 0.3)) ** 2 : 0, a: fade });
  const cmd = "$ npm install -g phonton-cli";
  const cx = centered(mid, cmd);
  if (lb >= typeAt - 0.1) {
    const s = lb < typeAt ? "$" : typed(cmd, lb, typeAt, 17.5);
    mid.text(cx, o.cmdY, s, (i) => (i === 0 ? P.muted : P.ink), fade);
    if (Math.floor(lb * 4) % 2 === 0 || s.length < cmd.length) mid.put(cx + [...s].length, o.cmdY, "█", P.cyan, fade * 0.9);
  }
  const url = "phonton.dev";
  decode(hero, centered(hero, url), o.urlY, url, (lb - urlAt) / 0.5, f, (i) => grad(i / 11 + t * 0.1), { seed: 91, a: fade });
  const foot = "free and open source · MIT or Apache-2.0";
  decode(fine, centered(fine, foot), o.footY, foot, (lb - footAt) / 0.4, f, P.muted, { seed: 92, a: fade });
}
