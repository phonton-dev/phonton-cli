import React from "react";
import { AbsoluteFill, interpolate, Sequence, Series, staticFile, useCurrentFrame, Easing } from "remotion";
import { Audio } from "@remotion/media";
import { C, mono, sans } from "./brand";
import { Chip, Footage, Hit, Still } from "./parts";
import { Outro } from "./Outro";
import { f, FAIL_AT, goalClock, SFX_VOLUME, speed, type Sfx } from "./cut";

// 15 s vertical cut for X: no voice, big headlines, the same real take as the
// main cut (run 39137397648, "record (explain-msg, 1)"). Terminal footage is
// 1920x1080 framed into 1080x1920; camera keyframes are source pixels.

const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;
const ease = Easing.bezier(0.16, 1, 0.3, 1);
const TERM = { width: 1920, height: 1080 };

const HIT = 3.0;
const HOOK_SECONDS = 5.5;
const HOOK_RATE = 0.5; // about real time (see cut.ts)
const HOOK_START = FAIL_AT - HIT * HOOK_RATE;
const RUN = { start: 34, end: 77.4, seconds: 3.5 };
const VERIFIED_SECONDS = 3.2;
const OUTRO_SECONDS = 2.8;
export const VERTICAL_FRAMES = f(HOOK_SECONDS + RUN.seconds + VERIFIED_SECONDS + OUTRO_SECONDS);

const ATTEMPTS = { x: 935, y: 250 };
const RECEIPT = { x: 955, y: 290 };

type Head = { from: number; text: React.ReactNode };

const Headline: React.FC<{ lines: Head[] }> = ({ lines }) => {
  const frame = useCurrentFrame();
  const current = [...lines].reverse().find((l) => frame >= l.from);
  if (!current) return null;
  const t = frame - current.from;
  return (
    <AbsoluteFill style={{ justifyContent: "flex-start", alignItems: "center", paddingTop: 300, padding: "300px 72px 0" }}>
      <div
        style={{
          opacity: interpolate(t, [0, 6], [0, 1], clamp),
          translate: `0px ${interpolate(t, [0, 10], [18, 0], { ...clamp, easing: ease })}px`,
          color: C.ink,
          fontFamily: sans,
          fontWeight: 700,
          fontSize: 76,
          lineHeight: 1.1,
          letterSpacing: "-0.02em",
          textAlign: "center",
        }}
      >
        {current.text}
      </div>
    </AbsoluteFill>
  );
};

const red = (s: string) => <span style={{ color: C.fail }}>{s}</span>;
const green = (s: string) => <span style={{ color: C.pass }}>{s}</span>;

const sfx: Sfx[] = [
  { name: "error", at: HIT - 0.05 },
  { name: "whoosh", at: HOOK_SECONDS - 0.4 },
  { name: "impact", at: HOOK_SECONDS + RUN.seconds + 0.28 },
  { name: "chime", at: HOOK_SECONDS + RUN.seconds + 0.3 },
  { name: "thump", at: HOOK_SECONDS + RUN.seconds + VERIFIED_SECONDS },
];
// Music drop (16 s into the track) lands on VERIFIED.
const musicTrim = f(16 - (HOOK_SECONDS + RUN.seconds));

export const Vertical: React.FC = () => {
  const frame = useCurrentFrame();
  const music = 0.55 * interpolate(frame, [0, 6, VERTICAL_FRAMES - 30, VERTICAL_FRAMES - 2], [0, 1, 1, 0], clamp);
  return (
    <AbsoluteFill style={{ background: C.bg }}>
      <Series>
        <Series.Sequence durationInFrames={f(HOOK_SECONDS)} name="hook">
          <Hit at={f(HIT)} tone="fail">
            <Footage
              src={staticFile("v6/goal.mp4")}
              source={TERM}
              startSec={HOOK_START}
              endSec={HOOK_START + HOOK_SECONDS * HOOK_RATE}
              rate={HOOK_RATE}
              speed={speed(goalClock, HOOK_START, HOOK_START + HOOK_SECONDS * HOOK_RATE, HOOK_SECONDS)}
              zooms={[
                { at: 0, ...ATTEMPTS, scale: 1.55 },
                { at: f(HIT), ...ATTEMPTS, scale: 1.6 },
                { at: f(HOOK_SECONDS), ...ATTEMPTS, scale: 1.65 },
              ]}
              marks={[{ x: 612, y: 236, w: 630, h: 50, at: f(HIT) + 2, tone: "fail" }]}
            />
          </Hit>
          <Headline
            lines={[
              { from: 4, text: <>Your local model says it fixed the bug.</> },
              { from: f(HIT), text: <>Your tests say {red("no")}.</> },
            ]}
          />
        </Series.Sequence>
        <Series.Sequence durationInFrames={f(RUN.seconds)} name="run">
          <Footage
            src={staticFile("v6/goal.mp4")}
            source={TERM}
            startSec={RUN.start}
            endSec={RUN.end}
            rate={(RUN.end - RUN.start) / RUN.seconds}
            speed={speed(goalClock, RUN.start, RUN.end, RUN.seconds)}
            zooms={[
              { at: 0, ...ATTEMPTS, scale: 1.65 },
              { at: f(RUN.seconds), ...ATTEMPTS, scale: 1.7 },
            ]}
          />
          <Headline lines={[{ from: 0, text: <>Phonton retries with the failing output.</> }]} />
        </Series.Sequence>
        <Series.Sequence durationInFrames={f(VERIFIED_SECONDS)} name="verified">
          <Hit at={f(0.3)} tone="pass">
            <Still
              src={staticFile("v6/receipt.png")}
              source={TERM}
              zooms={[
                { at: 0, ...RECEIPT, scale: 1.3 },
                { at: f(0.3), ...RECEIPT, scale: 1.4 },
              ]}
              marks={[
                { x: 1176, y: 140, w: 142, h: 28, at: f(0.3), tone: "pass" },
                { x: 612, y: 164, w: 460, h: 26, at: f(0.3) + 6, tone: "pass" },
              ]}
            />
          </Hit>
          <Headline lines={[{ from: f(0.3), text: <>{green("Verified")} before it touches your code.</> }]} />
        </Series.Sequence>
        <Series.Sequence durationInFrames={f(OUTRO_SECONDS)} name="outro">
          <AbsoluteFill style={{ scale: "0.8" }}>
            <Outro />
          </AbsoluteFill>
        </Series.Sequence>
      </Series>

      <Chip label="REAL RUN · qwen3.5:4b · no GPU" from={4} to={f(HOOK_SECONDS + RUN.seconds + VERIFIED_SECONDS) - 2} />
      <div
        style={{
          position: "absolute",
          bottom: 110,
          left: "50%",
          translate: "-50% 0px",
          whiteSpace: "nowrap",
          padding: "12px 26px",
          borderRadius: 999,
          background: "rgba(8,10,14,0.9)",
          border: `1px solid ${C.line}`,
          opacity: frame < f(HOOK_SECONDS + RUN.seconds + VERIFIED_SECONDS) ? 1 : 0,
          color: C.muted,
          fontFamily: mono,
          fontSize: 30,
        }}
      >
        phonton 0.25.1 · real run · speed-ups badged
      </div>

      <Audio src={staticFile("audio/music.wav")} trimBefore={musicTrim} volume={music} />
      {sfx.map((x) => (
        <Sequence key={`${x.name}-${x.at}`} from={f(x.at)} durationInFrames={f(3)} layout="none">
          <Audio src={staticFile(`audio/sfx/${x.name}.wav`)} volume={SFX_VOLUME[x.name]} />
        </Sequence>
      ))}
    </AbsoluteFill>
  );
};
