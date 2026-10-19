import React from "react";
import { AbsoluteFill, Easing, interpolate, useCurrentFrame } from "remotion";
import { C, mono, sans } from "./brand";

const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;
const ease = Easing.bezier(0.16, 1, 0.3, 1);

const commands = ["npm install -g phonton-cli", "phonton models setup qwen3.5:4b", "phonton"];

export const Outro: React.FC = () => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ background: C.bg, justifyContent: "center", alignItems: "center" }}>
      <div
        style={{
          opacity: interpolate(frame, [0, 12], [0, 1], clamp),
          translate: `0px ${interpolate(frame, [0, 16], [24, 0], { ...clamp, easing: ease })}px`,
          color: C.ink,
          fontFamily: sans,
          fontWeight: 700,
          fontSize: 96,
          letterSpacing: "-0.03em",
          textAlign: "center",
          lineHeight: 1.08,
        }}
      >
        Local coding models,
        <br />
        checked by your own tests.
      </div>
      <div
        style={{
          marginTop: 64,
          padding: "34px 48px",
          borderRadius: 20,
          background: C.term,
          border: `1px solid ${C.line}`,
          minWidth: 1000,
        }}
      >
        {commands.map((cmd, i) => (
          <div
            key={cmd}
            style={{
              opacity: interpolate(frame, [14 + i * 8, 22 + i * 8], [0, 1], clamp),
              color: C.ink,
              fontFamily: mono,
              fontSize: 40,
              lineHeight: 1.6,
            }}
          >
            <span style={{ color: C.teal }}>$ </span>
            {cmd}
          </div>
        ))}
      </div>
      <div
        style={{
          opacity: interpolate(frame, [44, 56], [0, 1], clamp),
          marginTop: 56,
          color: C.muted,
          fontFamily: mono,
          fontSize: 34,
        }}
      >
        phonton.dev · open source · Windows · Linux · macOS
      </div>
    </AbsoluteFill>
  );
};
