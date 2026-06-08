/**
 * Real-time diarization sidecar.
 *
 * Reads a stream of raw little-endian Float32 PCM (16 kHz mono) on stdin and
 * emits one JSON line per committed utterance on stdout:
 *   {"speakerId":"speaker_0","startMs":1234,"endMs":2345}
 *
 * Runs the EXACT online OnnxLocalDiarizer Vexa uses, frame-by-frame, so labels
 * appear live as audio arrives. The app pipes the live system-audio stream in
 * and attaches `speaker_N` to transcript segments by time overlap.
 */
import { OnnxLocalDiarizer, type CommitEvent } from "./diarization/onnx-local-diarizer";

const FRAME = 4096; // samples (matches the browser ScriptProcessor cadence)

async function main() {
  process.stdout.on("error", () => process.exit(0)); // EPIPE when the app closes the pipe
  const diarizer = await OnnxLocalDiarizer.create({
    // Commit a label sooner on long monologues (10 s default → 6 s) to cut
    // speaker-label latency; change-point detection still splits on turn changes.
    maxUtteranceMs: 6000,
    onCommit: (e: CommitEvent) => {
      process.stdout.write(
        JSON.stringify({ speakerId: e.speakerId, startMs: e.tStartMs, endMs: e.tEndMs }) + "\n",
      );
    },
  });
  process.stderr.write("READY\n"); // app waits for this before streaming

  let pending = Buffer.alloc(0);
  let sampleCount = 0;
  let chain: Promise<unknown> = Promise.resolve();

  process.stdin.on("data", (chunk: Buffer) => {
    pending = pending.length ? Buffer.concat([pending, chunk]) : chunk;
    const bytesPerFrame = FRAME * 4;
    while (pending.length >= bytesPerFrame) {
      const f32 = new Float32Array(FRAME);
      for (let i = 0; i < FRAME; i++) f32[i] = pending.readFloatLE(i * 4);
      pending = pending.subarray(bytesPerFrame);
      const ts = (sampleCount / 16000) * 1000;
      sampleCount += FRAME;
      chain = chain.then(() => diarizer.process(f32, ts));
    }
  });
  process.stdin.on("end", async () => {
    await chain;
    process.exit(0);
  });
}
main().catch((e) => {
  process.stderr.write(`ERR ${e}\n`);
  process.exit(1);
});
