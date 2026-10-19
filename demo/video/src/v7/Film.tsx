import React from "react";
import { AbsoluteFill, Easing, Img, interpolate, Sequence, staticFile, useCurrentFrame } from "remotion";
import { Video } from "@remotion/media";
import { C, FPS, mono, sans } from "../brand";
import T from "./timeline.json";

// v7 cut: one edit list (timeline.json) drives both layouts. Blocks sit on
// the music's bars; footage shots inside a block are real recordings, cropped
// to the line that matters and held. Audio is mixed outside Remotion
// (audio-work/v7/mix.py) from the same timeline, so these compositions are
// silent.

type Cam = { at: number; x: number; y: number; s: number };
type Tone = "accent" | "fail" | "pass";
type Mark = { x: number; y: number; w: number; h: number; at: number; tone: Tone };
type Shot = {
  src: string;
  from?: number;
  to?: number;
  rate?: number;
  endBars?: number;
  endOffset?: number;
  source?: number[];
  cam: Cam[];
  marks?: Mark[];
};
type Block = { id: string; bars: number; text: string; label?: string; card?: string; step?: number; shots?: Shot[] };
export type CutName = keyof typeof T.cuts;

export const BAR = 240 / T.bpm;
const fr = (s: number) => Math.round(s * FPS);
const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;
const easeOut = Easing.bezier(0.16, 1, 0.3, 1);
const easeInOut = Easing.bezier(0.65, 0, 0.35, 1);

/** Piecewise-linear file-time -> real-time map, extended past both ends. */
const clock = (pts: number[][], t: number) => {
  const next = pts.findIndex((p) => p[0] > t);
  const i = Math.max(1, next === -1 ? pts.length - 1 : next);
  const [[a, ra], [b, rb]] = [pts[i - 1], pts[i]];
  return ra + ((t - a) * (rb - ra)) / (b - a);
};

/** Block and shot times in seconds; the same rules as audio-work/v7/mix.py. */
const layout = (cut: CutName) => {
  let bar = 0;
  let card = "";
  let cardBar = 0;
  return (T.cuts[cut].blocks as Block[]).map((b) => {
    const start = bar * BAR;
    if (b.card !== card) [card, cardBar] = [b.card ?? "", bar];
    const cardOffset = bar - cardBar;
    bar += b.bars;
    let t = 0;
    const shots = (b.shots ?? []).map((s) => {
      const end = s.endBars !== undefined ? s.endBars * BAR + (s.endOffset ?? 0) : b.bars * BAR;
      const at = t;
      const dur = end - t;
      t = end;
      const isVideo = s.src.endsWith(".mp4");
      const rate = !isVideo ? 0 : s.to !== undefined ? (s.to - (s.from ?? 0)) / dur : (s.rate ?? 1);
      const pts = (T.clocks as Record<string, number[][]>)[s.src];
      const from = s.from ?? 0;
      const speed = !isVideo ? null : pts ? (clock(pts, from + rate * dur) - clock(pts, from)) / dur : rate;
      return { ...s, at, dur, rate, speed };
    });
    return { ...b, start, cardOffset, shots };
  });
};

const total = (cut: CutName) => (T.cuts[cut].blocks as Block[]).reduce((n, b) => n + b.bars, 0);
export const framesOf = (cut: CutName) => fr(total(cut) * BAR);

const GRID = {
  main: { pad: 120, headTop: 96, head: 56, win: { x: 120, y: 300, w: 1680, h: 620 }, labelY: 950, label: 22 },
  vertical: { pad: 40, headTop: 480, head: 64, win: { x: 40, y: 740, w: 1000, h: 640 }, labelY: 1412, label: 24 },
} as const;
type Grid = (typeof GRID)[CutName];

const TONE: Record<Tone, string> = { accent: C.teal, fail: C.fail, pass: C.pass };

const fadeUp = (frame: number, at = 0) => ({
  opacity: interpolate(frame, [at, at + 8], [0, 1], clamp),
  translate: `0px ${interpolate(frame, [at, at + 10], [10, 0], { ...clamp, easing: easeOut })}px`,
});

const Headline: React.FC<{ text: string; g: Grid }> = ({ text, g }) => {
  const frame = useCurrentFrame();
  return (
    <div
      style={{
        position: "absolute",
        left: g.pad,
        top: g.headTop,
        width: g.win.w - (g.pad - g.win.x) * 2,
        color: C.ink,
        fontFamily: sans,
        fontWeight: 600,
        fontSize: g.head,
        lineHeight: 1.18,
        letterSpacing: "-0.012em",
        ...fadeUp(frame),
      }}
    >
      {text}
    </div>
  );
};

const Window: React.FC<{ g: Grid; children: React.ReactNode }> = ({ g, children }) => (
  <div
    style={{
      position: "absolute",
      left: g.win.x,
      top: g.win.y,
      width: g.win.w,
      height: g.win.h,
      borderRadius: 14,
      border: `1px solid ${C.line}`,
      background: C.term,
      overflow: "hidden",
    }}
  >
    {children}
  </div>
);

const Footer: React.FC<{ g: Grid; left?: string; right?: string }> = ({ g, left, right }) => (
  <div
    style={{
      position: "absolute",
      left: g.win.x,
      top: g.labelY,
      width: g.win.w,
      display: "flex",
      justifyContent: "space-between",
      gap: 24,
      color: C.muted,
      fontFamily: mono,
      fontSize: g.label,
    }}
  >
    <span>{left}</span>
    <span>{right}</span>
  </div>
);

const speedLabel = (speed: number | null) =>
  speed === null ? "" : Math.abs(speed - 1) < 0.2 ? "real time" : `${speed >= 10 ? Math.round(speed) : speed.toFixed(1)}× speed`;

/** One shot: source pixels mapped into the window by the camera, marks in source pixels. */
const ShotView: React.FC<{ shot: ReturnType<typeof layout>[number]["shots"][number]; g: Grid }> = ({ shot, g }) => {
  const frame = useCurrentFrame();
  const t = frame / FPS;
  const [sw, sh] = shot.source ?? [1920, 1080];
  const keys = shot.cam;
  const pick = (k: keyof Cam) =>
    keys.length === 1 ? keys[0][k] : interpolate(t, keys.map((c) => c.at), keys.map((c) => c[k]), { ...clamp, easing: easeInOut });
  const s = pick("s");
  const src = staticFile(shot.src);
  return (
    <div
      style={{
        position: "absolute",
        left: 0,
        top: 0,
        width: sw,
        height: sh,
        transformOrigin: "0 0",
        transform: `translate(${g.win.w / 2 - pick("x") * s}px, ${g.win.h / 2 - pick("y") * s}px) scale(${s})`,
      }}
    >
      {shot.rate ? (
        <Video src={src} trimBefore={Math.round((shot.from ?? 0) * FPS)} playbackRate={shot.rate} muted style={{ width: sw, height: sh }} />
      ) : (
        <Img src={src} style={{ width: sw, height: sh }} />
      )}
      {(shot.marks ?? []).map((m) => (
        <div
          key={`${m.x}-${m.y}`}
          style={{
            position: "absolute",
            left: m.x,
            top: m.y,
            width: m.w,
            height: m.h,
            borderRadius: 3,
            borderLeft: `2px solid ${TONE[m.tone]}`,
            background: `${TONE[m.tone]}29`,
            opacity: interpolate(frame, [fr(m.at), fr(m.at) + 5], [0, 1], clamp),
          }}
        />
      ))}
    </div>
  );
};

// Benchmark run full-4b-0.25.0 (2026-10-17): 51 Aider Polyglot exercises,
// qwen3.5:4b on an RTX 4050 laptop. Source:
// phonton-brain/05-benchmarks/polyglot-local-2026-10/README.md; raw results
// are the polyglot-qwen3.5-4b-rtx4050-2026-10-17 asset on the phonton-cli
// v0.25.1 release. "Tests failed anyway" counts claimed successes whose tests
// fail.
const ARMS = [
  { label: "Bare model", note: "Aider's prompt, two tries", done: 43, wrong: 37, passed: 6, at: 1 },
  { label: "OpenCode 1.14.19", note: "run --pure", done: 51, wrong: 50, passed: 1, at: 2 },
  { label: "Phonton 0.25.0", note: "your tests decide", done: 1, wrong: 0, passed: 2, at: 4 },
];
const PASSED_AT = 8;

const Bench: React.FC<{ offsetBars: number }> = ({ offsetBars }) => {
  const frame = useCurrentFrame();
  const at = (bars: number) => fr((bars - offsetBars) * BAR);
  const cols = "1fr 250px 330px 250px";
  const head = { fontFamily: mono, fontSize: 22, color: C.muted, textAlign: "right" } as const;
  const num = { fontFamily: sans, fontWeight: 600, fontSize: 64, textAlign: "right", fontVariantNumeric: "tabular-nums" } as const;
  return (
    <div style={{ position: "absolute", inset: 0, padding: "44px 64px", display: "flex", flexDirection: "column" }}>
      <div style={{ display: "grid", gridTemplateColumns: cols, alignItems: "end", paddingBottom: 14, borderBottom: `1px solid ${C.line}` }}>
        <span style={{ ...head, textAlign: "left" }}>51 exercises · qwen3.5:4b</span>
        <span style={head}>Said done</span>
        <span style={head}>Tests failed anyway</span>
        <span style={{ ...head, ...fadeUp(frame, at(PASSED_AT)) }}>Tests passed</span>
      </div>
      {ARMS.map((a) => {
        const me = a.label.startsWith("Phonton");
        return (
          <div
            key={a.label}
            style={{
              display: "grid",
              gridTemplateColumns: cols,
              alignItems: "center",
              height: 118,
              borderBottom: `1px solid ${C.line}`,
              ...fadeUp(frame, at(a.at)),
            }}
          >
            <div>
              <div style={{ fontFamily: sans, fontWeight: 600, fontSize: 38, color: me ? C.teal : C.ink }}>{a.label}</div>
              <div style={{ fontFamily: mono, fontSize: 20, color: C.muted, marginTop: 4 }}>{a.note}</div>
            </div>
            <span style={{ ...num, color: C.ink }}>{a.done}</span>
            <span style={{ ...num, color: a.wrong ? C.fail : C.pass }}>{a.wrong}</span>
            <span style={{ ...num, color: C.ink, ...fadeUp(frame, at(PASSED_AT)) }}>{a.passed}</span>
          </div>
        );
      })}
      <div style={{ marginTop: "auto", fontFamily: mono, fontSize: 19, lineHeight: 1.45, color: C.muted }}>
        Aider Polyglot, 17 each of Python, JavaScript and Rust. qwen3.5:4b on an RTX 4050 laptop. 18 Phonton runs hit
        0.25.0's memory reserve (fixed in 0.25.1). Raw logs: phonton-cli v0.25.1 release.
      </div>
    </div>
  );
};

const COMMANDS = ["npm install -g phonton-cli", "phonton models setup qwen3.5:4b", "phonton"];

const Install: React.FC<{ vertical: boolean }> = ({ vertical }) => {
  const frame = useCurrentFrame();
  const at = (bars: number) => fr(bars * BAR);
  const lines = vertical ? COMMANDS.slice(0, 1) : COMMANDS;
  return (
    <div
      style={{
        position: "absolute",
        inset: 0,
        padding: vertical ? "72px 48px" : "72px 96px",
        display: "flex",
        flexDirection: "column",
        gap: 22,
      }}
    >
      {lines.map((cmd, i) => (
        <div key={cmd} style={{ fontFamily: mono, fontSize: vertical ? 36 : 44, color: C.ink, ...fadeUp(frame, at(i)) }}>
          <span style={{ color: C.muted }}>$ </span>
          {cmd}
        </div>
      ))}
      <div
        style={{
          marginTop: "auto",
          fontFamily: sans,
          fontWeight: 600,
          fontSize: vertical ? 88 : 76,
          color: C.teal,
          letterSpacing: "-0.02em",
          ...fadeUp(frame, at(vertical ? 0.5 : 4)),
        }}
      >
        phonton.dev
      </div>
    </div>
  );
};

export const Film: React.FC<{ cut: CutName }> = ({ cut }) => {
  const g = GRID[cut];
  const blocks = layout(cut);
  return (
    <AbsoluteFill style={{ background: C.bg }}>
      {blocks.map((b) => {
        const from = fr(b.start);
        const dur = fr(b.start + b.bars * BAR) - from;
        return (
          <Sequence key={b.id} name={b.id} from={from} durationInFrames={dur}>
            <Headline text={b.text} g={g} />
            <Window g={g}>
              {b.card === "bench" ? <Bench offsetBars={b.cardOffset} /> : null}
              {b.card === "install" || b.card === "end" ? <Install vertical={cut === "vertical"} /> : null}
              {b.shots.map((s) => {
                const sf = fr(b.start + s.at) - from;
                return (
                  <Sequence key={`${s.src}-${s.at}`} from={sf} durationInFrames={fr(b.start + s.at + s.dur) - from - sf}>
                    <ShotView shot={s} g={g} />
                  </Sequence>
                );
              })}
            </Window>
            {b.shots.map((s) => {
              const sf = fr(b.start + s.at) - from;
              return (
                <Sequence key={`label-${s.src}-${s.at}`} from={sf} durationInFrames={fr(b.start + s.at + s.dur) - from - sf}>
                  <Footer g={g} left={b.label} right={speedLabel(s.speed)} />
                </Sequence>
              );
            })}
          </Sequence>
        );
      })}
    </AbsoluteFill>
  );
};
