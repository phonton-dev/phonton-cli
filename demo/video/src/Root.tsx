import React from "react";
import { Composition, Folder } from "remotion";
import { Demo, DEMO_FRAMES } from "./Demo";
import { Honest } from "./Honest";
import { Bench } from "./Bench";
import { Outro } from "./Outro";
import { Gallery, GALLERY } from "./Gallery";
import { FPS } from "./brand";
import { Vertical, VERTICAL_FRAMES } from "./Vertical";
import { Film, framesOf } from "./v7/Film";
import { Film8, framesOf as framesOf8 } from "./v8/Film8";

export const RemotionRoot: React.FC = () => (
  <>
    <Composition id="Demo" component={Demo} width={1920} height={1080} fps={FPS} durationInFrames={DEMO_FRAMES} />
    <Composition id="Vertical" component={Vertical} width={1080} height={1920} fps={FPS} durationInFrames={VERTICAL_FRAMES} />
    <Composition id="DemoV7" component={Film} defaultProps={{ cut: "main" as const }} width={1920} height={1080} fps={FPS} durationInFrames={framesOf("main")} />
    <Composition id="VerticalV7" component={Film} defaultProps={{ cut: "vertical" as const }} width={1080} height={1920} fps={FPS} durationInFrames={framesOf("vertical")} />
    <Composition id="DemoV8" component={Film8} defaultProps={{ cut: "main" as const }} width={1920} height={1080} fps={FPS} durationInFrames={framesOf8("main")} />
    <Composition id="VerticalV8" component={Film8} defaultProps={{ cut: "vertical" as const }} width={1080} height={1920} fps={FPS} durationInFrames={framesOf8("vertical")} />
    <Composition id="StyleTestV8" component={Film8} defaultProps={{ cut: "test" as const }} width={1920} height={1080} fps={FPS} durationInFrames={framesOf8("test")} />
    <Folder name="Scenes">
      <Composition id="Bench" component={Bench} defaultProps={{ cues: { bare: 20, opencode: 45, phonton: 70 } }} width={1920} height={1080} fps={FPS} durationInFrames={300} />
      <Composition id="Honest" component={Honest} width={1920} height={1080} fps={FPS} durationInFrames={150} />
      <Composition id="Outro" component={Outro} width={1920} height={1080} fps={FPS} durationInFrames={150} />
    </Folder>
    <Folder name="ProductHunt">
      {GALLERY.map((slide, i) => (
        <Composition
          key={slide.id}
          id={slide.id}
          component={Gallery}
          defaultProps={{ index: i }}
          width={1270}
          height={760}
          fps={FPS}
          durationInFrames={1}
        />
      ))}
    </Folder>
  </>
);
