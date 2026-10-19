import React, { useLayoutEffect, useMemo, useRef, useState } from "react";
import { AbsoluteFill, continueRender, delayRender, Img, Sequence, staticFile, useCurrentFrame } from "remotion";
import { Video } from "@remotion/media";
import { loadFont } from "@remotion/google-fonts/JetBrainsMono";
import { grad, Grid, hash, P, paint, RAMP } from "./ascii";
import { BAR, end, FRAME_PX, framed, loop, numbers, open, VERTICAL_LOOP, type Callout, type Ctx, type Scene, type Shot } from "./scenes";
import CUT from "./cut8.json";

// v8: the ASCII cut. Everything that moves is characters on a grid in the
// Phonton logo gradient; the real recordings sit inside ASCII frames.
// Silent: audio is mixed by audio-work/v8/mix8.py from cut8.json.

const { fontFamily, waitUntilDone } = loadFont("normal", { weights: ["400", "700"], subsets: ["latin", "greek"] });
const FPS = 30;
const WIPE = 0.5;
const fr = (bars: number) => Math.round(bars * BAR * FPS);
export type CutName = keyof typeof CUT.cuts;

// File seconds -> real seconds for goal.mp4 (the TUI's own elapsed clock; see v7 timeline).
const GOAL_CLOCK = [[0, -4.2], [4.2, 0], [5, 0.8], [7, 3.4], [9, 6.2], [10, 7.7], [12, 11.5], [20, 24.8], [33, 49.5], [33.6, 50.7], [34, 51.6], [36, 55.6], [50, 77], [70, 107], [74, 115], [77, 120]];
const clock = (t: number) => {
  const next = GOAL_CLOCK.findIndex((p) => p[0] > t);
  const i = Math.max(1, next === -1 ? GOAL_CLOCK.length - 1 : next);
  const [[a, ra], [b, rb]] = [GOAL_CLOCK[i - 1], GOAL_CLOCK[i]];
  return ra + ((t - a) * (rb - ra)) / (b - a);
};
const speedOf = (from: number, to: number, start: number, end: number) => {
  const x = (clock(to) - clock(from)) / ((end - start) * BAR);
  return Math.abs(x - 1) < 0.2 ? "real time" : `${Math.round(x)}× speed`;
};

const TUI = { cam: { x: 1195, y: 307, s: 1.17 } };
const S = BAR;
const f1a: Shot = { src: "v6/goal.mp4", from: 10, to: 33.2, start: 20, end: 22 - 0.8 / S, ...TUI, speed: speedOf(10, 33.2, 20, 22 - 0.8 / S) };
const f1b: Shot = { src: "v6/goal.mp4", from: 33.2, rate: 0.5, start: f1a.end, end: 24, ...TUI, speed: "real time" };
const f2a: Shot = { src: "v6/goal.mp4", from: 34.979, to: 77.0, start: 24, end: 26 - 0.37 / S, ...TUI, speed: speedOf(34.979, 77.0, 24, 26 - 0.37 / S) };
const f2b: Shot = { src: "v6/goal.mp4", from: 77.0, rate: 0.6, start: f2a.end, end: 26, ...TUI, speed: "real time" };
const f2c: Shot = { src: "v6/receipt.png", from: 0, start: 26, end: 28, ...TUI, speed: "screenshot" };
const f3: Shot = { src: "v6/desktop.mp4", from: 112.0, rate: 1, start: 28, end: 32 + WIPE, source: [1440, 900], cam: { x: 720, y: 643, s: 1.4 }, speed: "real time" };
const MAIN_SHOTS = [f1a, f1b, f2a, f2b, f2c, f3];

const RUN = "real run · phonton 0.25.1 · qwen3.5:4b · 4 vCPU, no GPU";
const FIXTURE = "the test's failure message was written for this demo · details in the README";
const call = (shot: Shot, c: Callout) => ({ ...c, shot });

const MAIN: Scene[] = [
  { id: "open", from: 0, to: 8, draw: (c, lb) => open(c, lb) },
  { id: "loop", from: 8, to: 20, draw: loop },
  {
    id: "f1", from: 20, to: 24, footage: true,
    draw: (c, lb) => framed(c, lb, {
      title: RUN, shots: [f1a, f1b], B: 20 + lb, note: FIXTURE, capAt: 0.3,
      cap: "This is the real run. Each clip shows its speed in the frame.",
      callouts: [call(f1b, { at: 22, src: [662, 262, 395, 18], tone: P.danger, lines: [["the failing test,", P.danger], ["straight from node --test", P.muted]] })],
    }),
  },
  {
    id: "f2", from: 24, to: 28, footage: true,
    draw: (c, lb) => framed(c, lb, {
      title: RUN, shots: [f2a, f2b, f2c], B: 24 + lb, note: FIXTURE, capAt: 0.2,
      cap: "The second candidate is a repair of the first, built from that output.",
      callouts: [
        call(f2c, { at: 26.05, src: [1178, 142, 135, 22], tone: P.ok, lines: [["✓ VERIFIED", P.ok]] }),
        call(f2c, { at: 26.5, src: [618, 309, 712, 18], tone: P.cyan, lines: [["2m 00s on CPU", P.ink], ["$0.00 API cost", P.ink]] }),
      ],
    }),
  },
  {
    id: "f3", from: 28, to: 32, footage: true,
    draw: (c, lb) => framed(c, lb, {
      title: "real run · Phonton Desktop 0.5.2 · same goal, engine and model", shots: [f3], B: 28 + lb,
      note: "a separate run of the same goal (Actions run 39138396738)", capAt: 0.2,
      cap: "Your files only change when you apply it.",
      callouts: [call(f3, { at: 29.8, src: [242, 703, 150, 18], tone: P.ok, wire: false, lines: [["Applied src/port.js.", P.ok], ["git status: M src/port.js", P.muted]] })],
    }),
  },
  { id: "numbers", from: 32, to: 40, draw: numbers },
  { id: "end", from: 40, to: 49, draw: (c, lb) => end(c, lb, { logoX: 8, logoY: 4, cmdY: 17, urlY: 15, footY: 35, last: 9 }) },
];

/** Piecewise-linear map through [x, y] points, extended past both ends. */
const piecewise = (pts: [number, number][]) => (x: number) => {
  const next = pts.findIndex((p) => p[0] > x);
  const i = Math.max(1, next === -1 ? pts.length - 1 : next);
  const [[a, ra], [b, rb]] = [pts[i - 1], pts[i]];
  return ra + ((x - a) * (rb - ra)) / (b - a);
};
// Vertical bars since the loop started -> main loop bars: the same events,
// compressed so the fail lands on output bar 4 and the pass on bar 6.
const V_LOOP_TIME = piecewise([[0, 2.4], [0.875, 3.875], [1, 4], [1.25, 4.5], [2.25, 6.5], [2.5, 9.25], [2.875, 9.75], [3, 10], [5, 12]]);

const VERTICAL: Scene[] = [
  {
    id: "open", from: 0, to: 3,
    draw: (c, lb) => open(c, lb, { logoX: 1, logoY: 26, lock: 1, capY: 31, capGrid: "mid", lines: ["A local-first coding environment.", "Every change runs your tests first."] }),
  },
  { id: "loop", from: 3, to: 8, draw: (c, lb) => loop(c, V_LOOP_TIME(lb), VERTICAL_LOOP) },
  {
    id: "end", from: 8, to: 11,
    draw: (c, lb) => end(c, lb, { logoX: 1, logoY: 21, cmdY: 28, urlY: 34, footY: 50, last: 3, logoIn: 0.25, typeAt: 0.5, urlAt: 1.5, footAt: 1.9 }),
  },
];

type Spec = {
  w: number;
  h: number;
  scenes: Scene[];
  shots: Shot[];
  /** [output bar, source bar] pieces. */
  pieces: [number, number][];
  /** Output bars where a transition starts, with its kind. */
  wipes: [number, "wipe" | "dissolve"][];
  fine: () => Grid;
  mid: () => Grid;
  hero: () => Grid;
};

const SPECS: Record<CutName, Spec> = {
  main: {
    w: 1920, h: 1080, scenes: MAIN, shots: MAIN_SHOTS, pieces: [[0, 0]],
    wipes: [[8, "dissolve"], [20, "wipe"], [32, "wipe"], [40, "dissolve"]],
    fine: () => new Grid(160, 45, 12, 24, 20),
    mid: () => new Grid(106, 30, 18, 36, 30, 6, 0),
    hero: () => new Grid(80, 22, 24, 48, 40, 0, 12),
  },
  test: {
    w: 1920, h: 1080, scenes: MAIN, shots: [], pieces: [[0, 2], [4, 11]],
    wipes: [[4, "wipe"]],
    fine: () => new Grid(160, 45, 12, 24, 20),
    mid: () => new Grid(106, 30, 18, 36, 30, 6, 0),
    hero: () => new Grid(80, 22, 24, 48, 40, 0, 12),
  },
  vertical: {
    w: 1080, h: 1920, scenes: VERTICAL, shots: [], pieces: [[0, 0]], wipes: [[3, "wipe"], [8, "dissolve"]],
    fine: () => new Grid(90, 80, 12, 24, 20),
    mid: () => new Grid(59, 53, 18, 36, 30, 9, 6),
    hero: () => new Grid(67, 60, 16, 32, 80 / 3, 12, 0),
  },
};

export const framesOf = (cut: CutName) => fr(CUT.cuts[cut].bars);

const toSource = (spec: Spec, out: number) => {
  const [o, s] = spec.pieces.filter(([po]) => po <= out).pop() ?? spec.pieces[0];
  return s + (out - o);
};
const sceneAt = (spec: Spec, src: number) => spec.scenes.find((s) => src >= s.from && src < s.to) ?? spec.scenes[spec.scenes.length - 1];

function drawScene(spec: Spec, scene: Scene, src: number, f: number) {
  const ctx: Ctx = { fine: spec.fine(), mid: spec.mid(), hero: spec.hero(), f, t: src * BAR };
  scene.draw(ctx, src - scene.from);
  return ctx;
}

/** Blends two frames through a band of glyphs in the logo gradient. */
function blend(spec: Spec, a: Ctx, b: Ctx, p: number, kind: "wipe" | "dissolve", f: number, opaqueA: boolean, opaqueB: boolean) {
  const band = kind === "wipe" ? 0.14 : 0.22;
  const front = -band + p * (1 + 2 * band);
  const out: Ctx = { ...b, fine: spec.fine(), mid: spec.mid(), hero: spec.hero() };
  for (const layer of ["fine", "mid", "hero"] as const) {
    const g = out[layer];
    for (let i = 0; i < g.n; i++) {
      const px = g.ox + ((i % g.cols) + 0.5) * g.cw;
      const py = g.oy + (Math.floor(i / g.cols) + 0.5) * g.chh;
      const cell = Math.floor(px / 12) + Math.floor(py / 24) * 1000;
      const v = kind === "wipe" ? (px / spec.w) * 0.78 + (py / spec.h) * 0.22 + (hash(cell, 1, 3) - 0.5) * 0.04 : hash(cell, 2, 5) * 0.8 + (px / spec.w) * 0.2;
      if (v < front - band) {
        g.copyCell(b[layer], i);
        if (opaqueB) g.bg[i] = 1;
      } else if (v < front) {
        g.bg[i] = layer === "fine" ? 1 : 0;
        if (layer === "fine") {
          const depth = (front - v) / band;
          const ch = RAMP[Math.max(1, Math.min(9, Math.round(9 - depth * 8)))];
          g.put(i % g.cols, Math.floor(i / g.cols), ch, grad(px / spec.w * 0.9 + py / spec.h * 0.2 + f * 0.004), 0.95 - depth * 0.4);
        }
      } else {
        g.copyCell(a[layer], i);
        if (opaqueA) g.bg[i] = 1;
      }
    }
  }
  return out;
}

function frameAt(spec: Spec, f: number) {
  const out = f / FPS / BAR;
  const src = toSource(spec, out);
  const cur = sceneAt(spec, src);
  const w = spec.wipes.find(([at]) => out >= at && out < at + WIPE);
  if (!w) return drawScene(spec, cur, src, f);
  const [at, kind] = w;
  const prevSrc = toSource(spec, at - 1e-6) + (out - at);
  const prev = sceneAt(spec, toSource(spec, at - 1e-6));
  const anyFootage = !!(prev.footage || cur.footage);
  return blend(spec, drawScene(spec, prev, prevSrc, f), drawScene(spec, cur, src, f), (out - at) / WIPE, kind, f, anyFootage && !prev.footage, anyFootage && !cur.footage);
}

const AsciiCanvas: React.FC<{ grid: Grid; w: number; h: number }> = ({ grid, w, h }) => {
  const ref = useRef<HTMLCanvasElement>(null);
  const [handle] = useState(() => delayRender("ascii font"));
  const released = useRef(false);
  const [ready, setReady] = useState(false);
  useLayoutEffect(() => {
    let on = true;
    waitUntilDone()
      .then(() => document.fonts.load(`20px "${fontFamily}"`, "φ"))
      .catch(() => undefined)
      .then(() => on && setReady(true));
    return () => {
      on = false;
    };
  }, []);
  useLayoutEffect(() => {
    if (!ready || !ref.current) return;
    const ctx = ref.current.getContext("2d");
    if (ctx) paint(ctx, grid, fontFamily);
    if (!released.current) {
      released.current = true;
      continueRender(handle);
    }
  }, [ready, grid, handle]);
  return <canvas ref={ref} width={w} height={h} style={{ position: "absolute", left: 0, top: 0 }} />;
};

const Footage: React.FC<{ shot: Shot }> = ({ shot }) => {
  const [sw, sh] = shot.source ?? [1920, 1080];
  const { x, y, s } = shot.cam;
  const src = staticFile(shot.src);
  const rate = shot.rate ?? (shot.to !== undefined ? (shot.to - shot.from) / ((shot.end - shot.start) * BAR) : 1);
  return (
    <div style={{ position: "absolute", left: FRAME_PX.x, top: FRAME_PX.y, width: FRAME_PX.w, height: FRAME_PX.h, overflow: "hidden", background: "#11141c" }}>
      <div style={{ position: "absolute", width: sw, height: sh, transformOrigin: "0 0", transform: `translate(${FRAME_PX.w / 2 - x * s}px, ${FRAME_PX.h / 2 - y * s}px) scale(${s})` }}>
        {shot.src.endsWith(".png") ? (
          <Img src={src} style={{ width: sw, height: sh }} />
        ) : (
          <Video src={src} trimBefore={Math.round(shot.from * FPS)} playbackRate={rate} muted style={{ width: sw, height: sh }} />
        )}
      </div>
    </div>
  );
};

export const Film8: React.FC<{ cut: CutName }> = ({ cut }) => {
  const f = useCurrentFrame();
  const spec = SPECS[cut];
  const grids = useMemo(() => frameAt(spec, f), [spec, f]);
  return (
    <AbsoluteFill style={{ background: `rgb(${P.bg.join(",")})` }}>
      {spec.shots.map((s) => (
        <Sequence key={`${s.src}-${s.start}`} from={fr(s.start)} durationInFrames={fr(s.end) - fr(s.start)} layout="none">
          <Footage shot={s} />
        </Sequence>
      ))}
      <AsciiCanvas grid={grids.fine} w={spec.w} h={spec.h} />
      <AsciiCanvas grid={grids.mid} w={spec.w} h={spec.h} />
      <AsciiCanvas grid={grids.hero} w={spec.w} h={spec.h} />
    </AbsoluteFill>
  );
};
