/**
 * Standalone validation: run Vexa's OnnxLocalDiarizer (pyannote-segmentation-3.0
 * → wespeaker → online clustering) on one of our recorded session WAVs and print
 * the speaker timeline. Proves the diarization works on real captured audio
 * before we integrate it into the app.
 *
 *   npx tsx run.ts --wav "<path to ...-system.wav>"
 */
import { OnnxLocalDiarizer, type CommitEvent } from "./diarization/onnx-local-diarizer";
import * as fs from "fs";

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function readWavMono16(path: string): { samples: Float32Array; sampleRate: number } {
  const buf = fs.readFileSync(path);
  let off = 12, sampleRate = 16000, bits = 16, ch = 1, dataOff = -1, dataLen = 0;
  while (off + 8 <= buf.length) {
    const id = buf.toString("ascii", off, off + 4);
    const size = buf.readUInt32LE(off + 4);
    if (id === "fmt ") { ch = buf.readUInt16LE(off + 10); sampleRate = buf.readUInt32LE(off + 12); bits = buf.readUInt16LE(off + 22); }
    else if (id === "data") { dataOff = off + 8; dataLen = size; }
    off += 8 + size + (size & 1);
  }
  if (dataOff < 0 || bits !== 16) throw new Error("need PCM16 WAV");
  const n = Math.floor(dataLen / 2 / ch);
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    let acc = 0;
    for (let c = 0; c < ch; c++) acc += buf.readInt16LE(dataOff + (i * ch + c) * 2);
    out[i] = acc / ch / 32768;
  }
  return { samples: out, sampleRate };
}

const fmt = (ms: number) => {
  const s = Math.floor(ms / 1000);
  return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
};

async function main() {
  const wav = arg("wav");
  if (!wav) { console.error("usage: tsx run.ts --wav <file>"); process.exit(2); }
  const { samples, sampleRate } = readWavMono16(wav);
  if (sampleRate !== 16000) throw new Error(`expected 16k, got ${sampleRate}`);
  console.log(`[diar] ${(samples.length / 16000).toFixed(0)}s of audio; loading models (first run downloads ~32MB)…`);

  const commits: CommitEvent[] = [];
  const t0 = Date.now();
  const diarizer = await OnnxLocalDiarizer.create({ onCommit: (e: CommitEvent) => commits.push({ ...e }) });
  console.log(`[diar] models ready in ${((Date.now() - t0) / 1000).toFixed(1)}s; diarizing…`);

  const FRAME = 4096;
  for (let i = 0; i < samples.length; i += FRAME) {
    await diarizer.process(samples.subarray(i, Math.min(i + FRAME, samples.length)), (i / 16000) * 1000);
  }

  const clusters = [...new Set(commits.map((c) => c.speakerId))];
  console.log(`\n[diar] DONE: ${commits.length} utterances, ${clusters.length} speakers: ${clusters.join(", ")}\n`);
  for (const c of commits) {
    console.log(`  ${fmt(c.tStartMs)}–${fmt(c.tEndMs)}  ${c.speakerId}${c.isNew ? "  (new)" : ""}`);
  }
  const out = arg("out");
  if (out) {
    fs.writeFileSync(out, JSON.stringify(commits.map((c) => ({
      speakerId: c.speakerId, startMs: c.tStartMs, endMs: c.tEndMs,
    })), null, 2));
    console.log(`\n[diar] wrote ${out}`);
  }
}
main().catch((e) => { console.error(e); process.exit(1); });
