import React from "react";
import { AbsoluteFill, Img, staticFile } from "remotion";
import { C, mono, sans } from "./brand";

// Product Hunt gallery, 1270x760. Each slide frames a crop of a real
// screenshot (public/shots) under one headline.
type Slide = {
  id: string;
  headline: string;
  body: string;
  image: string;
  // Source crop in screenshot pixels.
  crop: { x: number; y: number; w: number; h: number };
};

export const GALLERY: Slide[] = [
  {
    id: "ph-1-receipt",
    headline: "Local coding models, checked by your own tests",
    body: "A 3B model, no GPU, no API key. $0.00, verified in 14 s.",
    image: "shots/receipt.png",
    crop: { x: 590, y: 40, w: 1320, h: 640 },
  },
  {
    id: "ph-2-calibration",
    headline: "It measures your model first",
    body: "A format your model fails is never asked for.",
    image: "shots/setup.png",
    crop: { x: 20, y: 480, w: 1000, h: 320 },
  },
  {
    id: "ph-3-attempts",
    headline: "Your tests decide, not the model",
    body: "Wrong candidates are caught. Your files stay unchanged.",
    image: "shots/attempts.png",
    crop: { x: 590, y: 140, w: 1320, h: 820 },
  },
  {
    id: "ph-4-plan",
    headline: "The plan comes before any model call",
    body: "Files, exact check commands, model and budget.",
    image: "shots/plan.png",
    crop: { x: 350, y: 150, w: 1215, h: 560 },
  },
  {
    id: "ph-5-desktop",
    headline: "Also a desktop app",
    body: "Same engine, same checks, in a window. No account.",
    image: "shots/desktop.png",
    crop: { x: 300, y: 90, w: 840, h: 520 },
  },
];

const W = 1270;
const H = 760;
const PAD = 56;
const FRAME_TOP = 230;

export const Gallery: React.FC<{ index: number }> = ({ index }) => {
  const s = GALLERY[index];
  const boxW = W - PAD * 2;
  const boxH = H - FRAME_TOP - PAD;
  const scale = Math.min(boxW / s.crop.w, boxH / s.crop.h);
  const shownW = s.crop.w * scale;
  const shownH = s.crop.h * scale;
  return (
    <AbsoluteFill style={{ background: C.bg }}>
      <div style={{ position: "absolute", left: PAD, top: 48, right: PAD }}>
        <div style={{ color: C.teal, fontFamily: mono, fontSize: 20, letterSpacing: "0.08em" }}>PHONTON</div>
        <div
          style={{
            marginTop: 12,
            color: C.ink,
            fontFamily: sans,
            fontWeight: 700,
            fontSize: 50,
            lineHeight: 1.05,
            letterSpacing: "-0.025em",
          }}
        >
          {s.headline}
        </div>
        <div style={{ marginTop: 12, color: C.muted, fontFamily: sans, fontSize: 26 }}>{s.body}</div>
      </div>
      <div
        style={{
          position: "absolute",
          left: (W - shownW) / 2,
          top: FRAME_TOP + (boxH - shownH) / 2,
          width: shownW,
          height: shownH,
          overflow: "hidden",
          borderRadius: 14,
          border: `1px solid ${C.line}`,
          boxShadow: "0 30px 80px rgba(0,0,0,0.6)",
        }}
      >
        <Img
          src={staticFile(s.image)}
          style={{
            position: "absolute",
            left: -s.crop.x * scale,
            top: -s.crop.y * scale,
            width: undefined,
            height: undefined,
            scale: String(scale),
            transformOrigin: "0 0",
          }}
        />
      </div>
    </AbsoluteFill>
  );
};
