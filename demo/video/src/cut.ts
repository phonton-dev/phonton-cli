import type { Mark, Zoom } from "./parts";
import type { BenchCues } from "./Bench";
import VO from "./vo.json";
import { FPS } from "./brand";

// Edit decision list. Footage times are seconds in the source recording;
// scene lengths are seconds of output at 30 fps.
//
// Sources (all real; npm phonton-cli@0.25.1 on a fresh GitHub Actions runner,
// 4 vCPU, 16 GB, no GPU, qwen3.5:4b on the Phonton-managed runtime; workflow
// demo-recording.yml on phonton-dev/phonton-cli branch demo/recording-0.25,
// run 39137397648, job "record (explain-msg, 1)"):
//   v6/setup.mp4    phonton models setup qwen3.5:4b
//   v6/goal.mp4     the parsePort goal in the TUI: candidate 1 fails the
//                   tests, candidate 2 repairs from that output and passes
//   v6/*.png        frames of those recordings (VHS screenshots or ffmpeg)
//   v6/desktop.mp4  Desktop 0.5.2 browser build on the same engine
//
// Speed: VHS drops frames while the CPU runs the model, so its files run
// faster than real time. Badges compare against real time instead: the TUI's
// own elapsed clock for goal.mp4, Ollama's request log for setup.mp4. Desktop
// footage is Playwright video, which is real time.

export const f = (s: number) => Math.round(s * FPS);

export type Section = keyof typeof VO;
export type Sfx = { name: "whoosh" | "error" | "chime" | "impact" | "thump" | "riser" | "typing" | "tick"; at: number };

type Base = {
  id: string;
  seconds: number;
  /** Voice-over section and when its first word starts, in scene seconds. */
  vo?: { section: Section; at: number };
  /** Captions are on for voiced scenes unless the scene shows the words itself. */
  captions?: boolean;
  sfx?: Sfx[];
  hit?: { at: number; tone: "fail" | "pass" };
};

export type Scene = Base &
  (
    | { kind: "footage"; src: string; startSec: number; endSec: number; speed: number; zooms?: Zoom[]; marks?: Mark[] }
    | { kind: "still"; src: string; zooms?: Zoom[]; marks?: Mark[] }
    | { kind: "title"; lines: string[]; sub?: string }
    | { kind: "bench"; cues: BenchCues }
    | { kind: "honest" }
    | { kind: "outro" }
  );

/** Start of the nth occurrence of a word in a section, seconds from the section start. */
const word = (section: Section, text: string, nth = 0) => {
  const s = VO[section];
  const hits = s.words.filter((w) => w.text.replace(/[^\w$"]/g, "") === text);
  const w = hits[nth];
  if (!w) throw new Error(`"${text}" not in ${section}`);
  return w.start - s.start;
};

/** Piecewise-linear map from file seconds to real seconds. */
const clock = (points: [number, number][]) => (t: number) => {
  const next = points.findIndex(([v]) => v > t);
  const i = Math.min(Math.max((next === -1 ? points.length : next) - 1, 0), points.length - 2);
  const [[v0, r0], [v1, r1]] = [points[i], points[i + 1]];
  return r0 + ((t - v0) * (r1 - r0)) / (v1 - v0);
};
// goal.mp4: the TUI's elapsed clock, read off frames (file s -> clock s).
// Before the goal is submitted (4.2 s) nothing loads the CPU; taken as 1:1.
export const goalClock = clock([
  [0, -4.2], [4.2, 0], [5, 0.8], [7, 3.4], [9, 6.2], [10, 7.7], [12, 11.5], [20, 24.8], [33, 49.5],
  [33.6, 50.7], [34, 51.6], [36, 55.6], [50, 77], [70, 107], [74, 115], [77, 120],
]);
// setup.mp4: Ollama starts at 22.0 s (log 14:46:59), the pull ends at 36 s
// (14:47:15, +16.4 s) and calibration at 99 s (14:48:38, +82 s). The download
// before it runs with an idle CPU; taken as 1:1.
const setupClock = clock([[0, 0], [22, 22], [36, 38.4], [99, 120.4]]);
/** Real-time speed of a slice shown in `seconds` of output. */
export const speed = (c: (t: number) => number, start: number, end: number, seconds: number) => (c(end) - c(start)) / seconds;

// Camera keyframes use source pixels (1920x1080 terminal, 1440x900 desktop).
const FULL = { x: 960, y: 540, scale: 1 };
const ATTEMPTS = { x: 900, y: 220 };
// y sits above the frame top so the first lines clear the chip.
const SETUP_TOP = { x: 680, y: 330, scale: 1.4 };
const PROBES = { x: 560, y: 690, scale: 1.9 };
const PLAN = { x: 760, y: 320, scale: 1.8 };
const RECEIPT = { x: 960, y: 290, scale: 2.0 };
const DESK_OUTCOME = { x: 960, y: 330, scale: 1.5 };
const DESK_APPLY = { x: 960, y: 700, scale: 1.35 };

// Candidate 1's failing check appears at 33.6 s in goal.mp4. The hook plays
// at real time and lands it on "It didn't".
export const FAIL_AT = 33.6;
const HOOK_AT = 0.1;
const HIT = HOOK_AT + word("hook", "didnt");
const HOOK_SECONDS = 6.7;
const HOOK_RATE = 0.5; // file seconds per output second, about real time here
const HOOK_START = FAIL_AT - HIT * HOOK_RATE;
const HOOK_END = HOOK_START + HOOK_SECONDS * HOOK_RATE;

const CAUGHT_SECONDS = 5.3;
const CAL_AT = 0.2;
const CAL_SECONDS = 2.5;
const TYPE_SECONDS = 2.5;
const ASK_AT = 0.2;
const inPlan = (text: string, nth = 0) => f(ASK_AT + word("ask", text, nth) - TYPE_SECONDS);

const VERIFIED_AT = 0.35;
const BENCH_AT = 0.3;

const footage = (
  src: string,
  c: (t: number) => number,
  startSec: number,
  endSec: number,
  seconds: number,
) => ({ kind: "footage" as const, src, startSec, endSec, seconds, speed: speed(c, startSec, endSec, seconds) });

export const CUT: Scene[] = [
  {
    id: "hook",
    ...footage("v6/goal.mp4", goalClock, HOOK_START, HOOK_END, HOOK_SECONDS),
    vo: { section: "hook", at: HOOK_AT },
    hit: { at: HIT, tone: "fail" },
    sfx: [{ name: "error", at: HIT - 0.05 }],
    zooms: [
      { at: 0, ...ATTEMPTS, scale: 1.9 },
      { at: f(HIT), ...ATTEMPTS, scale: 2.15 },
      { at: f(HIT) + 8, ...ATTEMPTS, scale: 2.4 },
      { at: f(HOOK_SECONDS), ...ATTEMPTS, scale: 2.5 },
    ],
    marks: [{ x: 612, y: 236, w: 630, h: 50, at: f(HIT) + 2, tone: "fail" }],
  },
  {
    id: "caught",
    kind: "still",
    src: "v6/attempts.png",
    seconds: CAUGHT_SECONDS,
    vo: { section: "caught", at: 0.25 },
    sfx: [{ name: "riser", at: CAUGHT_SECONDS - 1.76 }],
    zooms: [
      { at: 0, ...ATTEMPTS, scale: 1.7 },
      { at: f(CAUGHT_SECONDS), ...ATTEMPTS, scale: 1.85 },
    ],
    marks: [
      { x: 612, y: 236, w: 640, h: 26, at: f(0.25 + word("caught", "tests")), tone: "fail" },
      { x: 652, y: 260, w: 410, h: 26, at: f(0.25 + word("caught", "failed")), tone: "fail" },
    ],
  },
  {
    id: "title",
    kind: "title",
    lines: ["Local coding models,", "checked by your own tests."],
    sub: "phonton · free · open source · no API key",
    seconds: 4.4,
    vo: { section: "title", at: 0.3 },
    captions: false,
    sfx: [{ name: "impact", at: 0 }],
  },
  {
    id: "setup",
    ...footage("v6/setup.mp4", setupClock, 0.5, 36, 6.0),
    vo: { section: "setup", at: 0.15 },
    sfx: [{ name: "whoosh", at: -0.4 }],
    zooms: [{ at: 0, ...SETUP_TOP }],
  },
  {
    id: "calibrate",
    ...footage("v6/setup.mp4", setupClock, 36, 100.5, CAL_SECONDS),
    vo: { section: "calibrate", at: CAL_AT },
    zooms: [
      { at: 0, ...SETUP_TOP },
      { at: f(CAL_SECONDS), x: 680, y: 560, scale: 1.4 },
    ],
  },
  {
    id: "result",
    kind: "still",
    src: "v6/setup.png",
    // The calibrate line runs on into this still: the failed probe, then
    // "Phonton asks qwen3.5:4b for search/replace edits only" on "edits".
    seconds: 2.5,
    sfx: [{ name: "tick", at: 0.4 }],
    zooms: [
      { at: 0, x: 680, y: 560, scale: 1.4 },
      { at: f(0.4), ...PROBES },
    ],
    marks: [
      { x: 172, y: 726, w: 430, h: 24, at: f(0.55), tone: "fail" },
      { x: 50, y: 779, w: 670, h: 24, at: f(CAL_AT + word("calibrate", "edits") - CAL_SECONDS), tone: "pass" },
    ],
  },
  {
    id: "type",
    ...footage("v6/goal.mp4", goalClock, 0.6, 4.4, TYPE_SECONDS),
    vo: { section: "ask", at: ASK_AT },
    sfx: [
      { name: "whoosh", at: -0.4 },
      { name: "typing", at: 0.05 },
    ],
    // The goal input and the welcome tips under it, full width of the typed goal.
    zooms: [{ at: 0, x: 605, y: 730, scale: 1.65 }],
  },
  {
    id: "plan",
    kind: "still",
    src: "v6/plan.png",
    seconds: 2.6,
    sfx: [{ name: "tick", at: 0.35 }],
    zooms: [
      { at: 0, ...FULL },
      { at: f(0.4), ...PLAN },
    ],
    marks: [
      { x: 352, y: 257, w: 780, h: 28, at: inPlan("files"), until: inPlan("test"), tone: "pass" },
      { x: 352, y: 281, w: 466, h: 28, at: inPlan("test"), until: inPlan("budget"), tone: "pass" },
      { x: 352, y: 351, w: 590, h: 28, at: inPlan("budget"), tone: "pass" },
    ],
  },
  {
    id: "run",
    ...footage("v6/goal.mp4", goalClock, 33.0, 77.4, 4.7),
    vo: { section: "run", at: 0.25 },
    sfx: [{ name: "whoosh", at: -0.4 }],
    zooms: [
      { at: 0, ...ATTEMPTS, scale: 1.6 },
      { at: f(4.7), ...ATTEMPTS, scale: 1.7 },
    ],
  },
  {
    id: "verified",
    kind: "still",
    src: "v6/receipt.png",
    seconds: 7.7,
    vo: { section: "verified", at: VERIFIED_AT },
    hit: { at: 0.3, tone: "pass" },
    sfx: [
      { name: "impact", at: 0.28 },
      { name: "chime", at: 0.3 },
    ],
    zooms: [
      { at: 0, x: 960, y: 290, scale: 1.6 },
      { at: f(0.3), ...RECEIPT },
    ],
    marks: [
      { x: 1176, y: 140, w: 142, h: 28, at: f(0.3), tone: "pass" },
      { x: 612, y: 164, w: 460, h: 26, at: f(0.3) + 6, tone: "pass" },
      { x: 612, y: 305, w: 706, h: 24, at: f(VERIFIED_AT + word("verified", "two")), tone: "pass" },
      { x: 612, y: 329, w: 706, h: 24, at: f(VERIFIED_AT + word("verified", "$0")), tone: "pass" },
      { x: 590, y: 422, w: 708, h: 28, at: f(VERIFIED_AT + word("verified", "Applying")), tone: "pass" },
    ],
  },
  {
    id: "bench",
    kind: "bench",
    seconds: 15.7,
    vo: { section: "bench", at: BENCH_AT },
    captions: false,
    sfx: [{ name: "whoosh", at: -0.4 }],
    cues: {
      bare: f(BENCH_AT + word("bench", "bare")),
      opencode: f(BENCH_AT + word("bench", "OpenCode")),
      phonton: f(BENCH_AT + word("bench", "Phonton")),
    },
  },
  {
    id: "honest",
    kind: "honest",
    seconds: 4.7,
    vo: { section: "honest", at: 0.2 },
    captions: false,
    sfx: [{ name: "whoosh", at: -0.4 }],
  },
  {
    // Playwright video is real time. 1440x900 letterboxed into 1920x1080:
    // composition px = (96 + 1.2 x, 1.2 y).
    id: "desktop",
    kind: "footage",
    src: "v6/desktop.mp4",
    startSec: 104.2,
    endSec: 106.4,
    seconds: 2.2,
    speed: 1,
    vo: { section: "desktop", at: 0.35 },
    sfx: [{ name: "whoosh", at: -0.4 }],
    zooms: [
      { at: 0, ...FULL },
      { at: f(0.8), ...DESK_OUTCOME },
    ],
    marks: [{ x: 365, y: 152, w: 1190, h: 43, at: f(0.9), tone: "pass" }],
  },
  {
    id: "apply",
    kind: "footage",
    src: "v6/desktop.mp4",
    startSec: 113.4,
    endSec: 116.0,
    seconds: 2.6,
    speed: 1,
    zooms: [{ at: 0, ...DESK_APPLY }],
    // The click lands at 114.4 s; "Applied src/port.js." shows at 114.6 s.
    marks: [{ x: 378, y: 841, w: 178, h: 24, at: f(1.3), tone: "pass" }],
  },
  { id: "outro", kind: "outro", seconds: 4.6, vo: { section: "outro", at: 0.4 }, captions: false, sfx: [{ name: "thump", at: 0 }] },
];

/** Steps named in the top-left chip while their scenes play. */
export const CHAPTERS = [
  { step: 1, label: "MEASURE THE MODEL", scenes: ["setup", "calibrate", "result"] },
  { step: 2, label: "APPROVE THE PLAN", scenes: ["type", "plan"] },
  { step: 3, label: "VERIFY WITH YOUR TESTS", scenes: ["run", "verified"] },
];

/** Where the music's drop (16.0 s into the track) should land. */
export const MUSIC_DROP_SEC = 16.0;
export const DROP_SCENE = "title";

export const SFX_VOLUME: Record<Sfx["name"], number> = {
  whoosh: 0.22,
  error: 0.42,
  chime: 0.6,
  impact: 0.7,
  thump: 0.5,
  riser: 0.32,
  typing: 0.45,
  tick: 0.4,
};
