import React from "react";
import { AbsoluteFill, Easing, Img, interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { Video } from "@remotion/media";
import { C, mono, sans } from "./brand";

const ease = Easing.bezier(0.16, 1, 0.3, 1);
const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;

/**
 * Honest speed marker shown whenever footage plays faster than real time.
 * `rate` is the speed against the wall clock, not against the file.
 */
export const SpeedBadge: React.FC<{ rate: number }> = ({ rate }) =>
  Math.abs(rate - 1) < 0.1 ? null : (
    <div
      style={{
        position: "absolute",
        top: 40,
        right: 48,
        padding: "8px 16px",
        borderRadius: 999,
        background: "rgba(8,10,14,0.85)",
        border: `1px solid ${C.line}`,
        color: C.muted,
        fontFamily: mono,
        fontSize: 26,
      }}
    >
      {rate >= 10 ? Math.round(rate) : Math.round(rate * 10) / 10}× speed
    </div>
  );

export type Zoom = { at: number; x: number; y: number; scale: number };

/**
 * A highlight box drawn around real on-screen text, in source pixels, so it
 * moves with the camera. `at`/`until` are frames from the scene start.
 */
export type Mark = { x: number; y: number; w: number; h: number; at: number; until?: number; tone: "fail" | "pass" };

const Marks: React.FC<{ marks: Mark[] }> = ({ marks }) => {
  const frame = useCurrentFrame();
  return (
    <>
      {marks.map((m) => {
        const end = m.until ?? Infinity;
        if (frame < m.at || frame > end) return null;
        const color = m.tone === "fail" ? C.fail : C.pass;
        const t = interpolate(frame, [m.at, m.at + 7], [0, 1], { ...clamp, easing: ease });
        const out = end === Infinity ? 1 : interpolate(frame, [end - 6, end], [1, 0], clamp);
        return (
          <div
            key={`${m.x}-${m.y}-${m.at}`}
            style={{
              position: "absolute",
              left: m.x,
              top: m.y,
              width: m.w,
              height: m.h,
              borderRadius: 6,
              border: `2px solid ${color}`,
              background: `${color}1f`,
              boxShadow: `0 0 18px ${color}88`,
              opacity: t * out,
              scale: String(interpolate(t, [0, 1], [1.12, 1])),
            }}
          />
        );
      })}
    </>
  );
};

/**
 * A slice of real footage. `zooms` are camera keyframes in output frames:
 * x/y is the source pixel to centre, scale 1 shows the full frame.
 */
export const Footage: React.FC<{
  src: string;
  startSec: number;
  endSec: number;
  /** File seconds per output second. */
  rate: number;
  /** Real-time speed for the badge; VHS files can run faster than real time. */
  speed: number;
  zooms?: Zoom[];
  marks?: Mark[];
  /** Source frame size when it differs from the composition (vertical cut). */
  source?: { width: number; height: number };
}> = ({ src, startSec, endSec, rate, speed, zooms = [], marks = [], source }) => {
  const frame = useCurrentFrame();
  const { fps, width, height } = useVideoConfig();
  const size = source ?? { width, height };
  const keys = zooms.length ? zooms : [{ at: 0, x: width / 2, y: height / 2, scale: 1 }];
  const at = keys.map((k) => k.at);
  const pick = (f: (k: Zoom) => number) =>
    keys.length === 1 ? f(keys[0]) : interpolate(frame, at, keys.map(f), { ...clamp, easing: ease });
  const scale = pick((k) => k.scale);
  const x = pick((k) => k.x);
  const y = pick((k) => k.y);
  return (
    <AbsoluteFill style={{ backgroundColor: C.term, overflow: "hidden" }}>
      <AbsoluteFill
        style={{
          scale: String(scale),
          translate: `${(width / 2 - x) * scale}px ${(height / 2 - y) * scale}px`,
        }}
      >
        <Video
          src={src}
          trimBefore={Math.round(startSec * fps)}
          trimAfter={Math.round(endSec * fps)}
          playbackRate={rate}
          muted
          objectFit="contain"
          style={size}
        />
        <Marks marks={marks} />
      </AbsoluteFill>
      <SpeedBadge rate={speed} />
    </AbsoluteFill>
  );
};

/** A held frame from the same recording (its last frame), with camera moves. */
export const Still: React.FC<{
  src: string;
  zooms?: Zoom[];
  marks?: Mark[];
  source?: { width: number; height: number };
}> = ({ src, zooms = [], marks = [], source }) => {
  const frame = useCurrentFrame();
  const { width, height } = useVideoConfig();
  const size = source ?? { width, height };
  const keys = zooms.length ? zooms : [{ at: 0, x: width / 2, y: height / 2, scale: 1 }];
  const pick = (f: (k: Zoom) => number) =>
    keys.length === 1
      ? f(keys[0])
      : interpolate(frame, keys.map((k) => k.at), keys.map(f), { ...clamp, easing: ease });
  const scale = pick((k) => k.scale);
  return (
    <AbsoluteFill style={{ backgroundColor: C.term, overflow: "hidden" }}>
      <AbsoluteFill
        style={{
          scale: String(scale),
          translate: `${(width / 2 - pick((k) => k.x)) * scale}px ${(height / 2 - pick((k) => k.y)) * scale}px`,
        }}
      >
        <Img src={src} style={size} />
        <Marks marks={marks} />
      </AbsoluteFill>
    </AbsoluteFill>
  );
};

/** Full-frame type for the few non-footage beats. */
export const Title: React.FC<{ lines: React.ReactNode[]; sub?: React.ReactNode }> = ({ lines, sub }) => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ background: C.bg, justifyContent: "center", padding: "0 160px" }}>
      {lines.map((line, i) => (
        <div
          key={i}
          style={{
            opacity: interpolate(frame, [i * 14, i * 14 + 14], [0, 1], clamp),
            translate: `0px ${interpolate(frame, [i * 14, i * 14 + 18], [30, 0], { ...clamp, easing: ease })}px`,
            color: C.ink,
            fontFamily: sans,
            fontWeight: 700,
            fontSize: 104,
            lineHeight: 1.08,
            letterSpacing: "-0.03em",
          }}
        >
          {line}
        </div>
      ))}
      {sub ? (
        <div
          style={{
            opacity: interpolate(frame, [lines.length * 14 + 6, lines.length * 14 + 22], [0, 1], clamp),
            marginTop: 36,
            color: C.teal,
            fontFamily: mono,
            fontSize: 40,
          }}
        >
          {sub}
        </div>
      ) : null}
    </AbsoluteFill>
  );
};

export type CaptionWord = { text: string; from: number; to: number };

/**
 * Subtitle line synced to the voice: words already spoken are bright, the
 * rest of the line waits dimmed. Frames are absolute composition frames.
 */
export const VoiceCaption: React.FC<{ words: CaptionWord[]; from: number; to: number; top?: boolean }> = ({
  words,
  from,
  to,
  top,
}) => {
  const frame = useCurrentFrame();
  if (frame < from || frame > to) return null;
  const opacity = interpolate(frame, [from, from + 5, to - 5, to], [0, 1, 1, 0], clamp);
  return (
    <AbsoluteFill
      style={{ justifyContent: top ? "flex-start" : "flex-end", alignItems: "center", paddingTop: 130, paddingBottom: 64 }}
    >
      <div
        style={{
          opacity,
          translate: `0px ${interpolate(frame, [from, from + 8], [12, 0], { ...clamp, easing: ease })}px`,
          maxWidth: 1560,
          padding: "18px 32px",
          borderRadius: 16,
          background: "rgba(8,10,14,0.9)",
          border: `1px solid ${C.line}`,
          borderLeft: `5px solid ${C.teal}`,
          fontFamily: sans,
          fontWeight: 600,
          fontSize: 48,
          lineHeight: 1.22,
          letterSpacing: "-0.01em",
          boxShadow: "0 24px 60px rgba(0,0,0,0.5)",
          textAlign: "center",
        }}
      >
        {words.map((w, i) => (
          <span key={i} style={{ color: frame >= w.from ? C.ink : "rgba(238,241,246,0.38)" }}>
            {w.text}
            {i < words.length - 1 ? " " : ""}
          </span>
        ))}
      </div>
    </AbsoluteFill>
  );
};

/** Small pill in the top-left corner: a step name or a fact about the footage. */
export const Chip: React.FC<{ label: React.ReactNode; step?: number; from: number; to: number }> = ({ label, step, from, to }) => {
  const frame = useCurrentFrame();
  if (frame < from || frame > to) return null;
  const opacity = interpolate(frame, [from, from + 8, to - 8, to], [0, 1, 1, 0], clamp);
  return (
    <div
      style={{
        position: "absolute",
        top: 40,
        left: 48,
        opacity,
        translate: `${interpolate(frame, [from, from + 10], [-16, 0], { ...clamp, easing: ease })}px 0px`,
        display: "flex",
        alignItems: "center",
        gap: 14,
        padding: "10px 20px 10px 12px",
        borderRadius: 999,
        background: "rgba(8,10,14,0.88)",
        border: `1px solid ${C.line}`,
        color: C.ink,
        fontFamily: mono,
        fontSize: 26,
        letterSpacing: "0.04em",
      }}
    >
      {step ? (
        <span
          style={{
            display: "inline-flex",
            width: 38,
            height: 38,
            borderRadius: 999,
            alignItems: "center",
            justifyContent: "center",
            background: C.teal,
            color: C.bg,
            fontWeight: 600,
          }}
        >
          {step}
        </span>
      ) : (
        <span style={{ width: 12, height: 12, marginLeft: 8, borderRadius: 999, background: C.fail }} />
      )}
      {label}
    </div>
  );
};

/**
 * A short colored edge glow and camera kick at `at` (frames from the scene
 * start), for the moment a check fails or passes.
 */
export const Hit: React.FC<{ at: number; tone: "fail" | "pass"; children: React.ReactNode }> = ({ at, tone, children }) => {
  const frame = useCurrentFrame();
  const t = frame - at;
  const kick = t >= 0 && t < 10 ? Math.sin(t * 2.4) * 7 * (1 - t / 10) : 0;
  const glow = interpolate(t, [0, 3, 20], [0, 1, 0], clamp);
  const color = tone === "fail" ? "255,90,100" : "127,216,143";
  return (
    <AbsoluteFill>
      <AbsoluteFill style={{ translate: `${kick}px ${kick * 0.4}px` }}>{children}</AbsoluteFill>
      <AbsoluteFill style={{ pointerEvents: "none", boxShadow: `inset 0 0 220px rgba(${color},${0.55 * glow})` }} />
    </AbsoluteFill>
  );
};
