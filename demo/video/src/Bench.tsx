import React from "react";
import { AbsoluteFill, Easing, interpolate, useCurrentFrame } from "remotion";
import { C, mono, sans } from "./brand";

const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;
const ease = Easing.bezier(0.16, 1, 0.3, 1);

// Run full-4b-0.25.0 (2026-10-17): 51 Aider Polyglot exercises (polyglot-
// benchmark 7e0611e, 17 each of Python, JavaScript, Rust), qwen3.5:4b on
// Ollama 0.34.2, RTX 4050 laptop. "Said done" = the arm reported success; a
// false one is a claimed success whose original tests fail. Source:
// phonton-brain/05-benchmarks/polyglot-local-2026-10/README.md; raw results
// are the polyglot-qwen3.5-4b-rtx4050-2026-10-17.tar.gz asset on the
// phonton-cli v0.25.1 release; runner benchmarks/polyglot/run.mjs.
export const TOTAL = 51;
const arms = [
  { key: "bare", label: "Bare model", note: "Aider's prompt, two tries", claimed: 43, falses: 37 },
  { key: "opencode", label: "OpenCode 1.14.19", note: "run --pure", claimed: 51, falses: 50 },
  { key: "phonton", label: "Phonton 0.25.0", note: "goal --local, the tests as its check", claimed: 1, falses: 0 },
] as const;

/** Frames from the scene start, taken from the voice-over word timings. */
export type BenchCues = { bare: number; opencode: number; phonton: number };

export const Bench: React.FC<{ cues: BenchCues }> = ({ cues }) => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ background: C.bg, padding: "0 150px", justifyContent: "center" }}>
      <div
        style={{
          opacity: interpolate(frame, [0, 10], [0, 1], clamp),
          color: C.ink,
          fontFamily: sans,
          fontWeight: 700,
          fontSize: 76,
          letterSpacing: "-0.03em",
        }}
      >
        51 coding exercises. One 4B model.
      </div>
      <div style={{ opacity: interpolate(frame, [6, 16], [0, 1], clamp), marginTop: 12, color: C.muted, fontFamily: sans, fontSize: 38 }}>
        How often each said “done”, and how often that was false.
      </div>
      <div style={{ marginTop: 64, display: "flex", flexDirection: "column", gap: 46 }}>
        {arms.map((a) => {
          const at = cues[a.key];
          const t = interpolate(frame, [at, at + 10], [0, 1], { ...clamp, easing: ease });
          const grow = interpolate(frame, [at + 2, at + 26], [0, 1], { ...clamp, easing: ease });
          const truePart = (a.claimed - a.falses) / TOTAL;
          const falsePart = a.falses / TOTAL;
          const hero = a.key === "phonton";
          return (
            <div key={a.key} style={{ opacity: t, translate: `0px ${interpolate(t, [0, 1], [24, 0])}px` }}>
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
                <span style={{ color: hero ? C.teal : C.ink, fontFamily: mono, fontSize: 40, fontWeight: 600 }}>
                  {a.label} <span style={{ color: C.muted, fontSize: 26, fontWeight: 400 }}>· {a.note}</span>
                </span>
                <span style={{ fontFamily: mono, fontSize: 40 }}>
                  <span style={{ color: C.ink }}>said done {a.claimed}×</span>
                  <span style={{ color: C.muted }}> · </span>
                  <span style={{ color: a.falses ? C.fail : C.pass, fontWeight: 600 }}>{a.falses} false</span>
                </span>
              </div>
              <div
                style={{
                  marginTop: 14,
                  height: 22,
                  borderRadius: 11,
                  background: "rgba(255,255,255,0.07)",
                  display: "flex",
                  overflow: "hidden",
                }}
              >
                <div style={{ width: `${Math.max(truePart, hero ? 0.012 : 0) * 100 * grow}%`, background: C.pass }} />
                <div style={{ width: `${falsePart * 100 * grow}%`, background: C.fail }} />
              </div>
            </div>
          );
        })}
      </div>
      <div
        style={{
          opacity: interpolate(frame, [cues.phonton + 20, cues.phonton + 32], [0, 1], clamp),
          marginTop: 52,
          color: C.muted,
          fontFamily: mono,
          fontSize: 25,
          lineHeight: 1.5,
        }}
      >
        Aider Polyglot · 17 Python, 17 JavaScript, 17 Rust · qwen3.5:4b on an RTX 4050 laptop
        <br />
        Raw logs: phonton-cli v0.25.1 release assets · rerun it: benchmarks/polyglot
      </div>
    </AbsoluteFill>
  );
};
