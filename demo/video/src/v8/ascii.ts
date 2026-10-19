// A character-grid renderer for the v8 cut. Scenes write glyphs and colors
// into a Grid each frame; `paint` draws it on a canvas. Box-drawing, block
// and shading characters are drawn as pixels (like a terminal emulator does)
// so lines join and the ANSI Shadow logo stays crisp at any cell size.

export type RGB = readonly [number, number, number];

// Phonton TUI logo palette (phonton-cli/src/main.rs: GRAD_A..D, LOGO_GLOW,
// LOGO_SHADOW). Danger and ok match Desktop's nebula --ph-danger / --ph-ok.
export const P = {
  violet: [159, 122, 234],
  pink: [237, 100, 166],
  blue: [69, 144, 255],
  cyan: [99, 179, 237],
  glow: [209, 232, 255],
  shadow: [42, 48, 82],
  bg: [9, 11, 18],
  ink: [222, 228, 240],
  muted: [113, 128, 150],
  dim: [52, 60, 80],
  danger: [255, 96, 108],
  ok: [110, 224, 154],
} as const satisfies Record<string, RGB>;

const STOPS: RGB[] = [P.violet, P.pink, P.blue, P.cyan];

export const clamp01 = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);
export const frac = (v: number) => v - Math.floor(v);
export const ease = (v: number) => {
  const t = clamp01(v);
  return t * t * (3 - 2 * t);
};
export const easeOut = (v: number) => 1 - (1 - clamp01(v)) ** 3;
export const mix = (a: RGB, b: RGB, t: number): RGB => {
  const k = clamp01(t);
  return [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k];
};
/** The logo gradient as a loop: violet → pink → blue → cyan → violet. */
export const grad = (t: number): RGB => {
  const u = frac(t) * 4;
  const i = Math.floor(u);
  return mix(STOPS[i % 4], STOPS[(i + 1) % 4], u - i);
};

/** Deterministic hash of integers to [0, 1). */
export const hash = (x: number, y: number, s = 0) => {
  let h = Math.imul(x | 0, 374761393) ^ Math.imul(y | 0, 668265263) ^ Math.imul(s | 0, 1442695041);
  h = Math.imul(h ^ (h >>> 13), 1274126177);
  h ^= h >>> 16;
  return (h >>> 0) / 4294967296;
};

export const RAMP = " .:-=+*#%@";
const NOISE = ".:-=+*#%@░▒▓";
/** A glyph that changes every other frame, for decoding and wipes. */
export const noiseGlyph = (x: number, y: number, frame: number, seed = 0) =>
  NOISE[Math.floor(hash(x, y, seed + Math.floor(frame / 2)) * NOISE.length)];

export class Grid {
  readonly n: number;
  ch: string[];
  r: Float32Array;
  g: Float32Array;
  b: Float32Array;
  a: Float32Array;
  /** Opaque background per cell (hides footage under it). */
  bg: Float32Array;
  constructor(
    readonly cols: number,
    readonly rows: number,
    readonly cw: number,
    readonly chh: number,
    readonly font: number,
    readonly ox = 0,
    readonly oy = 0,
  ) {
    this.n = cols * rows;
    this.ch = new Array(this.n).fill("");
    this.r = new Float32Array(this.n);
    this.g = new Float32Array(this.n);
    this.b = new Float32Array(this.n);
    this.a = new Float32Array(this.n);
    this.bg = new Float32Array(this.n);
  }
  reset() {
    this.ch.fill("");
    this.a.fill(0);
    this.bg.fill(0);
  }
  index(x: number, y: number) {
    x = Math.round(x);
    y = Math.round(y);
    return x < 0 || y < 0 || x >= this.cols || y >= this.rows ? -1 : y * this.cols + x;
  }
  put(x: number, y: number, c: string, col: RGB, a = 1) {
    const i = this.index(x, y);
    if (i < 0 || a <= 0.003) return;
    this.ch[i] = c;
    this.r[i] = col[0];
    this.g[i] = col[1];
    this.b[i] = col[2];
    this.a[i] = Math.min(1, a);
  }
  erase(x: number, y: number, w: number, h: number, opaque = false) {
    for (let yy = y; yy < y + h; yy++)
      for (let xx = x; xx < x + w; xx++) {
        const i = this.index(xx, yy);
        if (i < 0) continue;
        this.ch[i] = "";
        this.a[i] = 0;
        if (opaque) this.bg[i] = 1;
      }
  }
  /** Text from (x, y); spaces leave cells untouched. `col` may vary per character. */
  text(x: number, y: number, s: string, col: RGB | ((i: number) => RGB), a = 1) {
    const chars = [...s];
    chars.forEach((c, i) => {
      if (c !== " ") this.put(x + i, y, c, typeof col === "function" ? col(i) : col, a);
    });
    return chars.length;
  }
  /** Like `text`, but spaces clear the cells under them (labels set into borders). */
  label(x: number, y: number, s: string, col: RGB | ((i: number) => RGB), a = 1) {
    this.erase(x, y, [...s].length, 1);
    return this.text(x, y, s, col, a);
  }
  copyCell(from: Grid, i: number) {
    this.ch[i] = from.ch[i];
    this.r[i] = from.r[i];
    this.g[i] = from.g[i];
    this.b[i] = from.b[i];
    this.a[i] = from.a[i];
    this.bg[i] = from.bg[i];
  }
}

/**
 * Text that resolves out of noise. `p` runs 0 → 1; each character flickers
 * through noise for a short window at its own moment, then settles.
 */
export function decode(
  g: Grid,
  x: number,
  y: number,
  s: string,
  p: number,
  frame: number,
  col: RGB | ((i: number) => RGB),
  { seed = 0, spread = 0.3, window = 0.22, a = 1 } = {},
) {
  const chars = [...s];
  const n = Math.max(1, chars.length - 1);
  chars.forEach((c, i) => {
    if (c === " ") return;
    const start = ((i / n) * (1 - spread) + hash(i, y, seed) * spread) * (1 - window);
    if (p < start) return;
    const final = typeof col === "function" ? col(i) : col;
    if (p < start + window) g.put(x + i, y, noiseGlyph(x + i, y, frame, seed), mix(final, P.glow, 0.45), a);
    else g.put(x + i, y, c, final, a);
  });
}

/** Characters typed one at a time from `t0`, `rate` characters per bar. */
export function typed(s: string, lb: number, t0: number, rate: number) {
  return [...s].slice(0, Math.max(0, Math.floor((lb - t0) * rate))).join("");
}

const BOXES = {
  round: ["╭", "╮", "╰", "╯", "─", "│"],
  light: ["┌", "┐", "└", "┘", "─", "│"],
  double: ["╔", "╗", "╚", "╝", "═", "║"],
  heavy: ["┏", "┓", "┗", "┛", "━", "┃"],
} as const;
export type BoxStyle = keyof typeof BOXES;

/**
 * A box outline, drawn on clockwise from the top-left as `p` goes 0 → 1.
 * `col` receives the perimeter position 0..1 for gradients.
 */
export function box(
  g: Grid,
  x: number,
  y: number,
  w: number,
  h: number,
  col: RGB | ((u: number) => RGB),
  { style = "round" as BoxStyle, p = 1, a = 1, title = "", titleCol = undefined as RGB | undefined } = {},
) {
  const [tl, tr, bl, br, hz, vt] = BOXES[style];
  const cells: [number, number, string][] = [];
  for (let i = 0; i < w; i++) cells.push([x + i, y, i === 0 ? tl : i === w - 1 ? tr : hz]);
  for (let j = 1; j < h; j++) cells.push([x + w - 1, y + j, j === h - 1 ? br : vt]);
  for (let i = w - 2; i >= 0; i--) cells.push([x + i, y + h - 1, i === 0 ? bl : hz]);
  for (let j = h - 2; j >= 1; j--) cells.push([x, y + j, vt]);
  const shown = Math.floor(cells.length * clamp01(p));
  for (let k = 0; k < shown; k++) {
    const [cx, cy, c] = cells[k];
    g.put(cx, cy, c, typeof col === "function" ? col(k / cells.length) : col, a);
  }
  if (title && p >= 0.15) g.label(x + 2, y, ` ${title} `, titleCol ?? (typeof col === "function" ? col(0) : col), a);
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

// up, right, down, left: 1 light, 2 heavy
const ARMS: Record<string, [number, number, number, number]> = {
  "─": [0, 1, 0, 1], "│": [1, 0, 1, 0], "┌": [0, 1, 1, 0], "┐": [0, 0, 1, 1], "└": [1, 1, 0, 0], "┘": [1, 0, 0, 1],
  "├": [1, 1, 1, 0], "┤": [1, 0, 1, 1], "┬": [0, 1, 1, 1], "┴": [1, 1, 0, 1], "┼": [1, 1, 1, 1],
  "━": [0, 2, 0, 2], "┃": [2, 0, 2, 0], "┏": [0, 2, 2, 0], "┓": [0, 0, 2, 2], "┗": [2, 2, 0, 0], "┛": [2, 0, 0, 2],
  "╴": [0, 0, 0, 1], "╶": [0, 1, 0, 0], "╵": [1, 0, 0, 0], "╷": [0, 0, 1, 0],
};
const ROUND: Record<string, [number, number]> = { "╭": [1, 1], "╮": [-1, 1], "╰": [1, -1], "╯": [-1, -1] };
const EIGHTH_LOW = " ▁▂▃▄▅▆▇█";
const EIGHTH_LEFT = " ▏▎▍▌▋▊▉█";

function glyph(ctx: CanvasRenderingContext2D, c: string, x: number, y: number, w: number, h: number): boolean {
  const cx = Math.round(x + w / 2);
  const cy = Math.round(y + h / 2);
  const t = Math.max(2, Math.round(w * 0.14));
  const arms = ARMS[c];
  if (arms) {
    arms.forEach((k, d) => {
      if (!k) return;
      const s = k === 2 ? t * 2 : t;
      const o = Math.floor(s / 2);
      if (d === 0) ctx.fillRect(cx - o, y, s, cy - y + o + (s % 2));
      if (d === 1) ctx.fillRect(cx - o, cy - o, x + w - cx + o, s);
      if (d === 2) ctx.fillRect(cx - o, cy - o, s, y + h - cy + o);
      if (d === 3) ctx.fillRect(x, cy - o, cx - x + o + (s % 2), s);
    });
    return true;
  }
  const round = ROUND[c];
  if (round) {
    const [dx, dy] = round;
    const r = Math.min(w, h) / 2;
    ctx.save();
    ctx.lineWidth = t;
    ctx.strokeStyle = ctx.fillStyle;
    ctx.beginPath();
    // arc from the horizontal arm to the vertical arm through the cell centre
    const ex = dx > 0 ? x + w : x;
    const ey = dy > 0 ? y + h : y;
    ctx.moveTo(ex, cy);
    ctx.lineTo(cx + dx * r, cy);
    ctx.quadraticCurveTo(cx, cy, cx, cy + dy * r);
    ctx.lineTo(cx, ey);
    ctx.stroke();
    ctx.restore();
    return true;
  }
  const dt = Math.max(1, Math.round(w * 0.11));
  const g2 = Math.max(2, Math.round(w * 0.18));
  const hl = (yy: number, x0: number, x1: number) => ctx.fillRect(Math.round(x0), yy, Math.round(x1 - x0), dt);
  const vl = (xx: number, y0: number, y1: number) => ctx.fillRect(xx, Math.round(y0), dt, Math.round(y1 - y0));
  const a = cx - g2;
  const b = cx + g2 - dt;
  const u = cy - g2;
  const v = cy + g2 - dt;
  switch (c) {
    case "═": hl(u, x, x + w); hl(v, x, x + w); return true;
    case "║": vl(a, y, y + h); vl(b, y, y + h); return true;
    case "╔": hl(u, a, x + w); hl(v, b, x + w); vl(a, u, y + h); vl(b, v, y + h); return true;
    case "╗": hl(u, x, b + dt); hl(v, x, a + dt); vl(b, u, y + h); vl(a, v, y + h); return true;
    case "╚": hl(v, a, x + w); hl(u, b, x + w); vl(a, y, v + dt); vl(b, y, u + dt); return true;
    case "╝": hl(v, x, b + dt); hl(u, x, a + dt); vl(b, y, v + dt); vl(a, y, u + dt); return true;
  }
  const low = EIGHTH_LOW.indexOf(c);
  if (low > 0) {
    const hh = Math.round((h * low) / 8);
    ctx.fillRect(x, y + h - hh, w, hh);
    return true;
  }
  const left = EIGHTH_LEFT.indexOf(c);
  if (left > 0) {
    ctx.fillRect(x, y, Math.round((w * left) / 8), h);
    return true;
  }
  if (c === "▀") { ctx.fillRect(x, y, w, Math.round(h / 2)); return true; }
  if (c === "▐") { ctx.fillRect(x + Math.round(w / 2), y, w - Math.round(w / 2), h); return true; }
  if (c === "░" || c === "▒" || c === "▓") {
    const p = Math.max(1, Math.round(w / 6));
    for (let iy = 0; iy * p < h; iy++)
      for (let ix = 0; ix * p < w; ix++) {
        const on = c === "▒" ? (ix + iy) % 2 === 0 : c === "░" ? ix % 2 === 0 && iy % 2 === (ix / 2) % 2 : !(ix % 2 === 0 && iy % 2 === 0);
        if (on) ctx.fillRect(x + ix * p, y + iy * p, Math.min(p, w - ix * p), Math.min(p, h - iy * p));
      }
    return true;
  }
  if (c === "▶" || c === "◀" || c === "▲" || c === "▼") {
    const s = w * 0.36;
    ctx.beginPath();
    if (c === "▶") { ctx.moveTo(cx - s, cy - s); ctx.lineTo(cx + s, cy); ctx.lineTo(cx - s, cy + s); }
    if (c === "◀") { ctx.moveTo(cx + s, cy - s); ctx.lineTo(cx - s, cy); ctx.lineTo(cx + s, cy + s); }
    if (c === "▲") { ctx.moveTo(cx - s, cy + s * 0.8); ctx.lineTo(cx, cy - s * 0.9); ctx.lineTo(cx + s, cy + s * 0.8); }
    if (c === "▼") { ctx.moveTo(cx - s, cy - s * 0.8); ctx.lineTo(cx, cy + s * 0.9); ctx.lineTo(cx + s, cy - s * 0.8); }
    ctx.closePath();
    ctx.fill();
    return true;
  }
  if (c === "●" || c === "○" || c === "◇" || c === "◆") {
    const r = w * 0.3;
    ctx.beginPath();
    if (c === "●" || c === "○") ctx.arc(cx, cy, r, 0, Math.PI * 2);
    else { ctx.moveTo(cx, cy - r * 1.2); ctx.lineTo(cx + r * 1.2, cy); ctx.lineTo(cx, cy + r * 1.2); ctx.lineTo(cx - r * 1.2, cy); ctx.closePath(); }
    if (c === "●" || c === "◆") ctx.fill();
    else { ctx.save(); ctx.lineWidth = Math.max(1.5, w * 0.1); ctx.strokeStyle = ctx.fillStyle; ctx.stroke(); ctx.restore(); }
    return true;
  }
  if (c === "✓" || c === "✗") {
    ctx.save();
    ctx.lineWidth = Math.max(2, w * 0.16);
    ctx.lineCap = "round";
    ctx.strokeStyle = ctx.fillStyle;
    ctx.beginPath();
    const s = w * 0.3;
    if (c === "✓") { ctx.moveTo(cx - s, cy); ctx.lineTo(cx - s * 0.25, cy + s * 0.8); ctx.lineTo(cx + s, cy - s * 0.9); }
    else { ctx.moveTo(cx - s, cy - s); ctx.lineTo(cx + s, cy + s); ctx.moveTo(cx + s, cy - s); ctx.lineTo(cx - s, cy + s); }
    ctx.stroke();
    ctx.restore();
    return true;
  }
  return false;
}

/** Draws the grid. The canvas stays transparent where cells are empty. */
export function paint(ctx: CanvasRenderingContext2D, g: Grid, family: string) {
  ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
  const [br, bgc, bb] = P.bg;
  for (let i = 0; i < g.n; i++) {
    if (g.bg[i] <= 0) continue;
    ctx.fillStyle = `rgba(${br},${bgc},${bb},${g.bg[i]})`;
    ctx.fillRect(g.ox + (i % g.cols) * g.cw, g.oy + Math.floor(i / g.cols) * g.chh, g.cw, g.chh);
  }
  ctx.font = `${g.font}px "${family}"`;
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  for (let i = 0; i < g.n; i++) {
    const c = g.ch[i];
    if (!c || g.a[i] <= 0) continue;
    const x = g.ox + (i % g.cols) * g.cw;
    const y = g.oy + Math.floor(i / g.cols) * g.chh;
    ctx.fillStyle = `rgba(${g.r[i] | 0},${g.g[i] | 0},${g.b[i] | 0},${g.a[i]})`;
    if (glyph(ctx, c, x, y, g.cw, g.chh)) continue;
    if (c === "φ") {
      // the photon: bold and a size up
      ctx.font = `700 ${g.font * 1.3}px "${family}"`;
      ctx.fillText(c, x + g.cw / 2, y + g.chh / 2 + g.font * 0.04);
      ctx.font = `${g.font}px "${family}"`;
    } else ctx.fillText(c, x + g.cw / 2, y + g.chh / 2 + g.font * 0.04);
  }
}
