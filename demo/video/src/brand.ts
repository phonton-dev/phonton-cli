import { loadFont as loadSans } from "@remotion/google-fonts/Outfit";
import { loadFont as loadMono } from "@remotion/google-fonts/JetBrainsMono";

// Same typefaces and accent as phonton.dev.
export const sans = loadSans("normal", { weights: ["500", "600", "700"], subsets: ["latin"] }).fontFamily;
export const mono = loadMono("normal", { weights: ["400", "600"], subsets: ["latin"] }).fontFamily;

export const C = {
  bg: "#0b0c0f",
  term: "#161a23",
  ink: "#eef1f6",
  muted: "#9aa3b2",
  teal: "#2fbfa8",
  pass: "#7fd88f",
  fail: "#ff8a92",
  line: "rgba(255,255,255,0.10)",
};

export const FPS = 30;
