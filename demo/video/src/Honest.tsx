import React from "react";
import { AbsoluteFill, Easing, interpolate, useCurrentFrame } from "remotion";
import { C, mono, sans } from "./brand";
import { TOTAL } from "./Bench";

// Same run as Bench.tsx (full-4b-0.25.0): exercises whose original tests
// pass after the arm's edit. Source: phonton-brain/05-benchmarks/
// polyglot-local-2026-10/README.md. 18 Phonton runs ended
// resource_pressure on 0.25.0's 1.5 GiB host-RAM reserve; 0.25.1 lowers it.
const rows = [
  { label: "Bare model", passed: 6 },
  { label: "Phonton 0.25.0", passed: 2 },
  { label: "OpenCode 1.14.19", passed: 1 },
];

const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;
const ease = Easing.bezier(0.16, 1, 0.3, 1);

export const Honest: React.FC = () => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ background: C.bg, padding: "110px 160px", justifyContent: "center" }}>
      <div
        style={{
          opacity: interpolate(frame, [0, 12], [0, 1], clamp),
          color: C.ink,
          fontFamily: sans,
          fontWeight: 700,
          fontSize: 84,
          letterSpacing: "-0.03em",
          lineHeight: 1.08,
        }}
      >
        A 4B model still fails most of them.
      </div>
      <div
        style={{
          opacity: interpolate(frame, [8, 20], [0, 1], clamp),
          marginTop: 18,
          color: C.muted,
          fontFamily: sans,
          fontSize: 44,
        }}
      >
        Phonton doesn’t make it smarter. It won’t call a failure done.
      </div>
      <div style={{ marginTop: 64, display: "flex", flexDirection: "column", gap: 36 }}>
        {rows.map((r, i) => {
          const start = 22 + i * 10;
          const pct = r.passed / TOTAL;
          return (
            <div key={r.label} style={{ opacity: interpolate(frame, [start, start + 10], [0, 1], clamp) }}>
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
                <span style={{ color: C.ink, fontFamily: mono, fontSize: 38 }}>
                  {r.label} <span style={{ color: C.muted, fontSize: 30 }}>· tests really pass</span>
                </span>
                <span style={{ color: C.ink, fontFamily: mono, fontSize: 44, fontWeight: 600 }}>
                  {r.passed}/{TOTAL}
                </span>
              </div>
              <div style={{ marginTop: 14, height: 16, borderRadius: 8, background: "rgba(255,255,255,0.08)" }}>
                <div
                  style={{
                    height: "100%",
                    borderRadius: 8,
                    background: C.muted,
                    width: `${interpolate(frame, [start + 4, start + 30], [0, Math.max(pct, 0.02) * 100], { ...clamp, easing: ease })}%`,
                  }}
                />
              </div>
            </div>
          );
        })}
      </div>
      <div
        style={{
          opacity: interpolate(frame, [60, 72], [0, 1], clamp),
          marginTop: 48,
          color: C.muted,
          fontFamily: mono,
          fontSize: 26,
          lineHeight: 1.5,
        }}
      >
        Same run · 18 Phonton runs stopped on 0.25.0’s RAM reserve, lowered in 0.25.1
        <br />
        phonton-cli v0.25.1 release assets · rerun it: benchmarks/polyglot
      </div>
    </AbsoluteFill>
  );
};
