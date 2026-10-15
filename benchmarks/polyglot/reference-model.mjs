#!/usr/bin/env node
// Scorer check for run.mjs: an OpenAI-compatible endpoint that answers the
// bare arm with each exercise's reference solution (every run must pass) or,
// with --stub, echoes the stub back (only already-working stubs may pass).
//
//   node benchmarks/polyglot/reference-model.mjs ../polyglot-benchmark 11999 [--stub]
//   node benchmarks/polyglot/run.mjs --polyglot ../polyglot-benchmark --model reference \
//     --endpoint http://127.0.0.1:11999/v1 --arms bare --per-lang 3
import fs from "node:fs";
import http from "node:http";
import path from "node:path";

const [polyglot, port = "11999", flag] = process.argv.slice(2);
const stub = flag === "--stub";
const exercises = [];
for (const lang of ["python", "javascript", "rust"]) {
  const dir = path.join(polyglot, lang, "exercises", "practice");
  for (const name of fs.readdirSync(dir)) {
    const src = path.join(dir, name);
    const meta = JSON.parse(fs.readFileSync(path.join(src, ".meta", "config.json"), "utf8"));
    const instructions = fs.readFileSync(path.join(src, ".docs", "instructions.md"), "utf8");
    exercises.push({ src, meta, instructions });
  }
}

http
  .createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const prompt = JSON.parse(body || "{}").messages?.[1]?.content ?? "";
      // Instructions repeat across languages; the solution file name does not.
      const ex = exercises.find((e) => prompt.includes(e.instructions) && prompt.includes(`${e.meta.files.solution[0]}\n`));
      let content = "no exercise matched";
      if (ex) {
        const file = ex.meta.files.solution[0];
        const text = fs.readFileSync(path.join(ex.src, stub ? file : ex.meta.files.example[0]), "utf8");
        content = `${file}\n\`\`\`\n${text}\`\`\``;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ choices: [{ message: { role: "assistant", content } }], usage: { prompt_tokens: 0, completion_tokens: 0 } }));
    });
  })
  .listen(Number(port), "127.0.0.1", () => console.log(`reference model (${stub ? "stub" : "solutions"}) on ${port}`));
