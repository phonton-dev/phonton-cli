import React from "react";
import { AbsoluteFill, interpolate, Sequence, Series, staticFile } from "remotion";
import { Audio } from "@remotion/media";
import { C } from "./brand";
import { Chip, Footage, Hit, Still, Title, VoiceCaption, type CaptionWord } from "./parts";
import { Honest } from "./Honest";
import { Outro } from "./Outro";
import { Bench } from "./Bench";
import { CHAPTERS, CUT, DROP_SCENE, f, MUSIC_DROP_SEC, SFX_VOLUME, type Scene } from "./cut";
import VO from "./vo.json";

// Every footage slice is a real recording: npm phonton-cli@0.25.1 on a fresh
// GitHub Actions runner (4 vCPU, no GPU), qwen3.5:4b on the Phonton-managed
// runtime, and Desktop 0.5.2 on the same engine. Speed-ups against real time
// carry an on-screen badge (see cut.ts).

const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;

const frames = CUT.map((s) => f(s.seconds));
const starts = frames.map((_, i) => frames.slice(0, i).reduce((a, b) => a + b, 0));
export const DEMO_FRAMES = frames.reduce((a, b) => a + b, 0);
const startOf = (id: string) => starts[CUT.findIndex((s) => s.id === id)];

// Voice segments in composition frames. A section is cut from the single
// voice-over take with a little air on both sides.
const PRE = 0.1;
const POST = 0.15;
const voice = CUT.flatMap((s, i) => {
  if (!s.vo) return [];
  const sec = VO[s.vo.section];
  const from = starts[i] + f(s.vo.at - PRE);
  return [{ scene: s, sec, from, frames: f(sec.end - sec.start + PRE + POST), trim: f(sec.start - PRE) }];
});

// Caption lines: a section's words grouped at sentence ends (and long
// clauses), each shown from its first word until the next line starts.
type Line = { words: CaptionWord[]; from: number; to: number };
const lines: Line[] = voice.flatMap(({ scene, sec }) => {
  if (scene.captions === false || !scene.vo) return [];
  const origin = starts[CUT.indexOf(scene)] + f(scene.vo.at);
  const abs = (t: number) => origin + f(t - sec.start);
  const groups: CaptionWord[][] = [[]];
  for (const w of sec.words) {
    const g = groups[groups.length - 1];
    g.push({ text: w.text, from: abs(w.start), to: abs(w.end) });
    if (/[.?!:]$/.test(w.text) || (g.length >= 7 && /,$/.test(w.text))) groups.push([]);
  }
  const used = groups.filter((g) => g.length);
  return used.map((g, i) => ({
    words: g,
    from: g[0].from - 3,
    to: i + 1 < used.length ? used[i + 1][0].from - 3 : g[g.length - 1].to + 14,
  }));
});

// Lines that start while the goal is being typed sit at the top, off the
// input bar.
const TOP_SCENES = ["type"];
const sceneAt = (frame: number) => CUT[starts.filter((s) => s <= frame).length - 1];

// Music: the intro plays under the cold open and the drop lands on the
// title. It ducks under the voice and fades out at the end.
const dropAt = startOf(DROP_SCENE);
const musicTrim = f(MUSIC_DROP_SEC) - dropAt;
const speaking = voice.map((v) => [v.from + f(PRE), v.from + v.frames - f(POST)] as const);
const presence = (frame: number) =>
  Math.max(0, ...speaking.map(([a, b]) => interpolate(frame, [a - 8, a, b, b + 14], [0, 1, 1, 0], clamp)));
const musicVolume = (frame: number) => {
  const bed = frame < dropAt ? 0.9 : interpolate(presence(frame), [0, 1], [0.42, 0.16]);
  const fade = interpolate(frame, [0, 6, DEMO_FRAMES - 75, DEMO_FRAMES - 4], [0, 1, 1, 0], clamp);
  return bed * fade;
};

const SceneBody: React.FC<{ scene: Scene }> = ({ scene }) => {
  switch (scene.kind) {
    case "footage":
      return (
        <Footage
          src={staticFile(scene.src)}
          startSec={scene.startSec}
          endSec={scene.endSec}
          rate={(scene.endSec - scene.startSec) / scene.seconds}
          speed={scene.speed}
          zooms={scene.zooms}
          marks={scene.marks}
        />
      );
    case "still":
      return <Still src={staticFile(scene.src)} zooms={scene.zooms} marks={scene.marks} />;
    case "title":
      return <Title lines={scene.lines} sub={scene.sub} />;
    case "bench":
      return <Bench cues={scene.cues} />;
    case "honest":
      return <Honest />;
    case "outro":
      return <Outro />;
  }
};

export const Demo: React.FC = () => (
  <AbsoluteFill style={{ background: C.bg }}>
    <Series>
      {CUT.map((scene, i) => (
        <Series.Sequence key={scene.id} name={scene.id} durationInFrames={frames[i]}>
          {scene.hit ? (
            <Hit at={f(scene.hit.at)} tone={scene.hit.tone}>
              <SceneBody scene={scene} />
            </Hit>
          ) : (
            <SceneBody scene={scene} />
          )}
        </Series.Sequence>
      ))}
    </Series>

    <Chip label="REAL RUN · qwen3.5:4b · 4 CPU cores, no GPU" from={4} to={f(3.1)} />
    {CHAPTERS.map((c) => {
      const first = CUT.findIndex((s) => s.id === c.scenes[0]);
      const last = CUT.findIndex((s) => s.id === c.scenes[c.scenes.length - 1]);
      return <Chip key={c.step} step={c.step} label={c.label} from={starts[first] + 4} to={starts[last] + frames[last] - 2} />;
    })}

    {lines.map((l) => (
      <VoiceCaption key={l.from} words={l.words} from={l.from} to={l.to} top={TOP_SCENES.includes(sceneAt(l.from + 3).id)} />
    ))}

    <Audio src={staticFile("audio/music.wav")} trimBefore={musicTrim} volume={musicVolume} />
    {voice.map((v) => (
      <Sequence key={v.scene.id} name={`vo ${v.scene.id}`} from={v.from} durationInFrames={v.frames} layout="none">
        <Audio src={staticFile("audio/vo-v6.wav")} trimBefore={v.trim} />
      </Sequence>
    ))}
    {CUT.flatMap((s, i) =>
      (s.sfx ?? []).map((x) => (
        <Sequence key={`${s.id}-${x.name}-${x.at}`} name={`sfx ${x.name}`} from={starts[i] + f(x.at)} durationInFrames={f(3)} layout="none">
          <Audio src={staticFile(`audio/sfx/${x.name}.wav`)} volume={SFX_VOLUME[x.name]} />
        </Sequence>
      )),
    )}
  </AbsoluteFill>
);
