import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { marked } from "marked";
import { Crepe } from "@milkdown/crepe";
import "@milkdown/crepe/theme/common/style.css";
import "@milkdown/crepe/theme/nord.css";

// ---- Types mirroring the Rust side ----
interface Settings {
  endpoint_url: string;
  api_token: string;
  language: string;
  capture_mic: boolean;
  capture_system: boolean;
  vault_path: string;
  claude_model: string;
  claude_path: string;
  max_budget_usd: number | null;
  capture_frames: boolean;
  frame_interval_secs: number;
}
interface ChatRow { role: string; text: string; created_at: string }
interface DeltaEvent { session: string; kind: string; text: string }
interface ToolEvent { session: string; kind: string; name: string; file: string | null }
interface ResultEvent { session: string; kind: string; text: string; cost_usd: number | null }
interface ClaudeErrEvent { session: string; kind: string; message: string }
interface VaultClaude {
  vault: { path: string; exists: boolean; adopted: boolean; ready: boolean } | null;
  claude_version: string | null;
  claude_path: string | null;
  claude_error: string | null;
}
interface TreeNode { name: string; path: string; dir: boolean; children: TreeNode[] }
interface SessionRow {
  id: string;
  title: string;
  started_at: string;
  ended_at: string | null;
  duration_secs: number | null;
  audio_path: string;
  segment_count: number;
}
interface SegmentRow {
  id: number;
  start: number;
  end: number;
  text: string;
  language: string | null;
  source: string; // "mic" | "system"
}
interface SegmentEvent {
  session_id: string;
  id: number;
  start: number;
  end: number;
  text: string;
  language: string | null;
  source: string; // "mic" | "system"
}
interface StartedEvent {
  session_id: string;
  active_sources: string[];
  warnings: string[];
}
interface StoppedEvent {
  session_id: string;
  duration_secs: number;
}
interface ErrorEvent {
  session_id: string;
  message: string;
}

// ---- State ----
let recording = false;
let viewSessionId: string | null = null; // session shown in the transcript pane
let timerStart = 0;
let timerHandle: number | undefined;
let appSettings: Settings | null = null; // last-loaded settings (preserve on save)
let streamingMsgEl: HTMLElement | null = null; // current streaming assistant bubble
let claudeBusy = false;
// Live polishing: raw attributed turns get rewritten into a clean, attributed,
// entity-highlighted "story" in place (one merged window).
interface TurnLog { speaker: string; source: string; start: number; end: number; text: string; el: HTMLElement }
let turnLog: TurnLog[] = []; // source of truth for each rendered raw turn
let lastLog: TurnLog | null = null; // mirrors lastBubble grouping
let polishedCount = 0; // prefix of turnLog already folded into the doc
let polishedDoc = ""; // the living condensed markdown document (whole-doc edits)
let polishBusy = false;
let polishTimer: number | undefined;
const POLISH_INTERVAL_MS = 8000; // auto-polish cadence while recording
const POLISH_SETTLE_S = 2.5; // leave the live tail (newer than this) raw
const POLISH_MIN_TURNS = 1; // polish as soon as a turn settles
// Notes vault workspace
let vaultPath = "";
let currentNotePath: string | null = null;
let currentNoteRaw = "";
let editing = false;
let crepe: Crepe | null = null; // active Milkdown rich editor

// ---- DOM helpers ----
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const scrollStream = () => { const s = $("stream"); if (s) s.scrollTop = s.scrollHeight; };
const fmtTime = (s: number) => {
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return `${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
};
function banner(msg: string, kind: "info" | "error" = "info") {
  const el = $("status-banner");
  el.textContent = msg;
  el.className = `banner ${kind}`;
  if (!msg) el.classList.add("hidden");
  else el.classList.remove("hidden");
}

// ---- Transcript rendering ----
// Consecutive segments from the same source are merged into one bubble (like
// Granola), breaking only when the source switches or after a long pause.
const GROUP_GAP = 30; // seconds; larger gaps start a new bubble
let lastBubble: { bubbleEl: HTMLElement; source: string; who: string; end: number } | null = null;
// Rendered mic bubbles, kept so we can retroactively remove echoes once the
// (later-arriving) system transcript reveals them as bleed.
let micRows: { start: number; end: number; text: string; el: HTMLElement }[] = [];

// ---- Speaker diarization labels (system audio → Speaker 1/2/3…) ----
interface DiarLabelEvent { session: string; speaker_id: string; start: number; end: number }
let diarLabels: { speakerId: string; start: number; end: number }[] = [];
const speakerNums = new Map<string, number>(); // speaker_id → display number (first-seen order)
const speakerNames = new Map<number, string>(); // display number → real name (from frames)
function speakerNumFor(start: number, end: number): number | null {
  let best: { speakerId: string } | null = null;
  let bestOv = 0;
  for (const d of diarLabels) {
    const ov = Math.min(end, d.end) - Math.max(start, d.start);
    if (ov > bestOv) { bestOv = ov; best = d; }
  }
  return best && bestOv > 0 ? speakerNums.get(best.speakerId) ?? null : null;
}
function speakerDisplay(num: number | null): string {
  if (num == null) return "Others";
  return speakerNames.get(num) ?? `Speaker ${num}`;
}
function applySpeakerNames() {
  document.querySelectorAll<HTMLElement>("#transcript .seg-row[data-speaker]").forEach((row) => {
    const tag = row.querySelector(".speaker-tag");
    if (tag) tag.textContent = speakerDisplay(Number(row.dataset.speaker));
  });
}
// Backfill speaker labels on already-rendered system rows as diarization commits
// catch up (lets us show segments fast without waiting for their diar label).
function reattributeSystemRows() {
  for (const e of turnLog) {
    if (e.source !== "system" || !e.el.isConnected) continue;
    const num = speakerNumFor(e.start, e.end);
    if (num == null || e.el.dataset.speaker === String(num)) continue;
    e.el.dataset.speaker = String(num);
    const tag = e.el.querySelector(".speaker-tag");
    if (tag) tag.textContent = speakerDisplay(num);
    e.speaker = speakerDisplay(num); // keep the polish log in sync
  }
}

// ---- Cross-source dedup (echo / speaker bleed) ----
// On speakers the mic re-hears system audio. We render INCREMENTALLY (append
// only, no full rebuild) with a short hold-back, deciding once per segment
// whether it's a mic echo of the system stream — stable and smooth.
const DUP_WINDOW = 8; // seconds: start-time gap when matching a mic↔system twin (saved view)
const HOLDBACK_MS = 1200; // wait this long before committing a segment (twin likely arrived)
interface Seg { start: number; end: number; text: string; source: string }
let pending: (Seg & { recvAt: number })[] = [];
let systemNorm = ""; // accumulated normalized system text, for echo matching
let flushTimer: number | undefined;

function normText(s: string): string {
  return s.toLowerCase().replace(/[^\p{L}\p{N}\s]/gu, " ").replace(/\s+/g, " ").trim();
}
function similar(a: string, b: string): boolean {
  const na = normText(a), nb = normText(b);
  if (!na || !nb) return false;
  if (na === nb || na.includes(nb) || nb.includes(na)) return true;
  const A = new Set(na.split(" ")), B = new Set(nb.split(" "));
  let inter = 0;
  A.forEach((w) => { if (B.has(w)) inter++; });
  const uni = new Set([...A, ...B]).size;
  return uni > 0 && inter / uni >= 0.5;
}
// Drop mic segments that are just the speakers bleeding back in. Because mic and
// system are chunked independently, we match each mic fragment against the WHOLE
// system transcript (not one aligned segment): a near-timed twin, an exact
// substring of the system stream, or a high word-overlap all count as echo.
function dedupeLoaded(segs: Seg[]): Seg[] {
  const sorted = [...segs].sort((a, b) => a.start - b.start);
  const sysSegs = sorted.filter((s) => s.source === "system");
  const sysAll = " " + sysSegs.map((s) => normText(s.text)).join(" ") + " ";
  const sysWords = new Set(sysAll.split(" ").filter(Boolean));
  const out: Seg[] = [];
  for (const seg of sorted) {
    if (seg.source === "mic") {
      const micNorm = normText(seg.text);
      const micWords = micNorm.split(" ").filter(Boolean);
      const nearTwin = sysSegs.some(
        (o) => Math.abs(o.start - seg.start) <= DUP_WINDOW && similar(o.text, seg.text),
      );
      const substring = micNorm.length > 0 && sysAll.includes(` ${micNorm} `);
      const overlap =
        micWords.length >= 3
          ? micWords.filter((w) => sysWords.has(w)).length / micWords.length
          : 0;
      if (nearTwin || substring || overlap >= 0.8) continue; // echo → drop
    }
    out.push(seg);
  }
  return out;
}
// Is this mic segment just the speakers bleeding back in (echo of system audio)?
function isMicEcho(seg: Seg): boolean {
  if (seg.source !== "mic") return false;
  const micNorm = normText(seg.text);
  const micWords = micNorm.split(" ").filter(Boolean);
  if (micWords.length < 2) return false;
  // Exact phrase appears in the system stream → definitely echo.
  if (systemNorm.includes(` ${micNorm} `)) return true;
  // Most of the mic phrase's words appear in the system stream → echo.
  const sys = new Set(systemNorm.split(" ").filter(Boolean));
  const hit = micWords.filter((w) => sys.has(w)).length;
  return hit / micWords.length >= 0.6;
}
// Append matured (held-back) segments incrementally — no full rebuild, no churn.
function flushPending(force = false) {
  const now = performance.now();
  const matured = pending.filter((p) => force || now - p.recvAt >= HOLDBACK_MS);
  if (matured.length) {
    const set = new Set(matured);
    pending = pending.filter((p) => !set.has(p));
    matured.sort((a, b) => a.start - b.start);
    for (const seg of matured) if (!isMicEcho(seg)) appendSegment(seg);
  }
  if (!pending.length && flushTimer) {
    clearInterval(flushTimer);
    flushTimer = undefined;
  }
}
function pushLive(seg: Seg) {
  if (seg.source === "system") {
    systemNorm += ` ${normText(seg.text)} `;
    pruneMicEchoes(); // new system text may reveal earlier mic bubbles as echo
  }
  pending.push({ ...seg, recvAt: performance.now() });
  if (!flushTimer) flushTimer = window.setInterval(() => flushPending(), 200);
}

function clearTranscript(placeholder?: string) {
  const t = $("transcript");
  t.innerHTML = placeholder ? `<div class="empty">${placeholder}</div>` : "";
  lastBubble = null;
  micRows = [];
  pending = [];
  systemNorm = "";
  diarLabels = [];
  speakerNums.clear();
  speakerNames.clear();
  nameState.clear();
  namingBusy = false;
  lastQueueRun = 0;
  if (queueThrottle != null) { clearTimeout(queueThrottle); queueThrottle = undefined; }
  resetPolish();
  if (flushTimer) {
    clearInterval(flushTimer);
    flushTimer = undefined;
  }
}

function makeRow(source: string, who: string, text: string, start: number): HTMLElement {
  const row = document.createElement("div");
  row.className = "seg-row";
  const tag = source === "system" ? `<div class="speaker-tag"></div>` : "";
  row.innerHTML =
    `<div class="seg-ts">${fmtTime(start)}</div>${tag}` +
    `<div class="bubble-line ${source}"><div class="bubble"></div></div>`;
  if (source === "system") (row.querySelector(".speaker-tag") as HTMLElement).textContent = who;
  (row.querySelector(".bubble") as HTMLElement).textContent = text;
  return row;
}
// Drop already-rendered mic bubbles that a now-fuller system stream reveals as echo.
function pruneMicEchoes() {
  if (!micRows.length) return;
  micRows = micRows.filter((r) => {
    if (isMicEcho({ start: r.start, end: r.end, text: r.text, source: "mic" })) {
      r.el.remove();
      turnLog = turnLog.filter((e) => e.el !== r.el); // keep the polish log in sync
      return false;
    }
    return true;
  });
}
function appendSegment(seg: { start: number; end: number; text: string; source: string }) {
  const t = $("transcript");
  const empty = t.querySelector(".empty");
  if (empty) empty.remove();
  const text = seg.text.trim();
  if (!text) return;

  if (seg.source !== "system") {
    // Mic: don't group (so we can remove individual echoes later). Skip known echoes.
    if (isMicEcho({ ...seg, source: "mic" })) return;
    const row = makeRow("mic", "You", text, seg.start);
    t.appendChild(row);
    micRows.push({ start: seg.start, end: seg.end, text: seg.text, el: row });
    turnLog.push({ speaker: "You", source: "mic", start: seg.start, end: seg.end, text, el: row });
    lastBubble = null; // a mic turn breaks system grouping
    lastLog = null;
    scrollStream();
    return;
  }

  // System: group consecutive same-speaker turns.
  const num = speakerNumFor(seg.start, seg.end);
  const who = speakerDisplay(num);
  if (lastBubble && lastLog && lastBubble.who === who && seg.start - lastBubble.end < GROUP_GAP) {
    lastBubble.bubbleEl.textContent = `${lastBubble.bubbleEl.textContent} ${text}`.trim();
    lastBubble.end = seg.end;
    lastLog.text = `${lastLog.text} ${text}`.trim();
    lastLog.end = seg.end;
    scrollStream();
    return;
  }
  const row = makeRow("system", who, text, seg.start);
  if (num != null) row.dataset.speaker = String(num);
  t.appendChild(row);
  lastBubble = { bubbleEl: row.querySelector(".bubble") as HTMLElement, source: "system", who, end: seg.end };
  lastLog = { speaker: who, source: "system", start: seg.start, end: seg.end, text, el: row };
  turnLog.push(lastLog);
  scrollStream();
}
// ---- Dynamic speaker naming ----
// Goal: one Claude CLI call per *genuinely new* speaker. When a new cluster has
// spoken enough and a screen frame exists for its moment, we name it once. A
// named speaker is never re-checked. An UNKNOWN result only retries if that
// speaker keeps talking (new visual info to try) — never on a bare timer — and
// is capped. Single-flight so at most one CLI call runs at a time.
interface NameState {
  tries: number; // actual CLI attempts that came back UNKNOWN/failed
  named: boolean;
  lastCli: number; // ts of last actual CLI invocation (retry safety floor)
  speechAtCli: number; // total speech (s) at last CLI invocation
  triedFrames: Set<number>; // frame timestamps (ms) already used — vary across retries
}
const nameState = new Map<string, NameState>();
let namingBusy = false;
let lastQueueRun = 0;
let queueThrottle: number | undefined;
const MIN_SPEECH_S = 3; // need this much total speech before the first attempt
const RETRY_SPEECH_DELTA_S = 2; // retry once the speaker has spoken ~this much more (i.e. again)
const NAME_MAX_TRIES = 8; // try hard before giving up
const NAME_RETRY_FLOOR_MS = 9000; // but never retry the same speaker faster than this
const FRAME_MAX_GAP_MS = 8000; // only use a frame within this of the speaking moment
const FRAMES_PER_ATTEMPT = 2; // distinct speaking-moment frames per attempt (more evidence)
const QUEUE_THROTTLE_MS = 3000; // don't recompute the queue more often than this

function nstate(sid: string): NameState {
  let st = nameState.get(sid);
  if (!st) {
    st = { tries: 0, named: false, lastCli: 0, speechAtCli: 0, triedFrames: new Set() };
    nameState.set(sid, st);
  }
  return st;
}
function totalSpeech(sid: string): number {
  let s = 0;
  for (const d of diarLabels) if (d.speakerId === sid) s += d.end - d.start;
  return s;
}
// This speaker's speaking spans, longest first (each a candidate frame moment).
function speakerSpans(sid: string): { start: number; end: number }[] {
  return diarLabels
    .filter((d) => d.speakerId === sid && d.end - d.start >= 0.8)
    .map((d) => ({ start: d.start, end: d.end }))
    .sort((a, b) => b.end - b.start - (a.end - a.start));
}
// Eligible = unnamed, under the try cap, has enough speech, and (if already
// attempted) has waited the floor AND spoken again since (so a fresh frame exists).
function eligible(sid: string, now: number): boolean {
  const st = nstate(sid);
  if (st.named || st.tries >= NAME_MAX_TRIES) return false;
  const speech = totalSpeech(sid);
  if (speech < MIN_SPEECH_S) return false;
  if (st.tries >= 1) {
    if (now - st.lastCli < NAME_RETRY_FLOOR_MS) return false;
    if (speech < st.speechAtCli + RETRY_SPEECH_DELTA_S) return false; // hasn't spoken again yet
  }
  return true;
}
// Called on each diar label (and the user's manual button). Throttled so we
// don't rescan on every single label; trailing run guarantees eventual check.
function scheduleNaming() {
  if (!viewSessionId) return;
  const now = Date.now();
  const wait = QUEUE_THROTTLE_MS - (now - lastQueueRun);
  if (wait <= 0) {
    lastQueueRun = now;
    void runNamingQueue();
  } else if (queueThrottle == null) {
    queueThrottle = window.setTimeout(() => {
      queueThrottle = undefined;
      lastQueueRun = Date.now();
      void runNamingQueue();
    }, wait);
  }
}
async function runNamingQueue() {
  if (namingBusy) return;
  namingBusy = true;
  try {
    const triedThisPass = new Set<string>(); // avoid re-picking a deferred speaker → no infinite loop
    while (true) {
      const now = Date.now();
      let next: string | null = null;
      for (const [sid] of speakerNums) {
        if (!triedThisPass.has(sid) && eligible(sid, now)) { next = sid; break; }
      }
      if (!next) break;
      triedThisPass.add(next);
      await nameOne(next);
    }
  } finally {
    namingBusy = false;
  }
}
async function nameOne(sid: string) {
  const st = nstate(sid);
  const num = speakerNums.get(sid);
  if (num == null) return;
  const spans = speakerSpans(sid);
  if (!spans.length) return; // nothing usable yet — cheap defer, no CLI, no state change
  let frames: { ms: number; path: string }[] = [];
  try {
    frames = await invoke<{ ms: number; path: string }[]>("list_frames", { sessionId: viewSessionId });
  } catch { /* none */ }
  if (!frames.length) return; // frames not captured yet → defer (no CLI)
  // Pick up to FRAMES_PER_ATTEMPT frames at DISTINCT, not-yet-tried speaking
  // moments (so retries look at fresh evidence, never the same frame twice).
  const pick: string[] = [];
  const pickMs = new Set<number>();
  for (const sp of spans) {
    if (pick.length >= FRAMES_PER_ATTEMPT) break;
    const midMs = ((sp.start + sp.end) / 2) * 1000;
    const f = frames.reduce((a, b) => (Math.abs(b.ms - midMs) < Math.abs(a.ms - midMs) ? b : a));
    if (Math.abs(f.ms - midMs) > FRAME_MAX_GAP_MS) continue;
    if (st.triedFrames.has(f.ms) || pickMs.has(f.ms)) continue;
    pick.push(f.path);
    pickMs.add(f.ms);
  }
  if (!pick.length) return; // no fresh frame to try yet → defer (no CLI, no state change)
  // Committing to an actual CLI call now.
  pickMs.forEach((ms) => st.triedFrames.add(ms));
  st.lastCli = Date.now();
  st.speechAtCli = totalSpeech(sid);
  const label = `Speaker ${num}`;
  try {
    const res = await invoke<{ label: string; name: string }[]>("name_speakers", {
      items: [{ label, frames: pick }],
    });
    const r = res.find((x) => x.label === label);
    if (r && r.name && r.name.toUpperCase() !== "UNKNOWN") {
      speakerNames.set(num, r.name);
      st.named = true;
      applySpeakerNames();
      banner(`Identified ${label} → ${r.name}.`, "info");
      setTimeout(() => banner(""), 2500);
    } else {
      st.tries++;
    }
  } catch (e) {
    st.tries++;
    console.warn("[name] failed", e);
  }
}
// Manual fallback: force an immediate pass over all still-unnamed speakers.
function nameNow() {
  if (!viewSessionId) return;
  if (!speakerNums.size) {
    banner("No speakers detected yet — record with system audio first.", "info");
    return;
  }
  for (const [sid] of speakerNums) {
    const st = nstate(sid);
    if (!st.named) {
      st.tries = 0;
      st.lastCli = 0;
      st.speechAtCli = 0;
      st.triedFrames.clear(); // re-examine all frames on a manual retry
    }
  }
  banner("Naming speakers from screen frames…", "info");
  setTimeout(() => banner(""), 2500);
  lastQueueRun = Date.now();
  void runNamingQueue();
}

// ---- Data loading ----
async function refreshSessions() {
  const sessions = await invoke<SessionRow[]>("list_sessions");
  const list = $("session-list");
  list.innerHTML = "";
  for (const s of sessions) {
    const li = document.createElement("li");
    li.className = "session-item" + (s.id === viewSessionId ? " active" : "");
    const dur = s.duration_secs ? ` · ${fmtTime(s.duration_secs)}` : "";
    li.innerHTML = `<div class="si-title"></div><div class="si-meta">${new Date(
      s.started_at,
    ).toLocaleString()}${dur} · ${s.segment_count} segs</div>`;
    (li.querySelector(".si-title") as HTMLElement).textContent = s.title;
    li.onclick = () => selectSession(s);
    list.appendChild(li);
  }
}

async function selectSession(s: SessionRow) {
  viewSessionId = s.id;
  $("session-title").textContent = s.title;
  const dur = s.duration_secs ? fmtTime(s.duration_secs) : "—";
  $("session-meta").textContent = `${new Date(s.started_at).toLocaleString()} · ${dur}`;
  clearTranscript("Loading…");
  const raw = await invoke<SegmentRow[]>("get_session_segments", { id: s.id });
  const segs = dedupeLoaded(raw);
  clearTranscript(segs.length ? undefined : "No transcript for this session yet.");
  for (const seg of segs) appendSegment(seg);
  setChatStatus("");
  await loadChat(s.id);
  setBusy(claudeBusy); // refresh Process button enabled state for the new session
  // Saved session: show its raw transcript + load the polished story from disk.
  stopPolishAuto();
  try {
    polishedDoc = (await invoke<string>("read_story", { sessionId: s.id })) || "";
  } catch {
    polishedDoc = "";
  }
  if (polishedDoc) renderPolished();
  await refreshSessions();
}

// ---- Recording controls ----
async function toggleRecording() {
  if (recording) await stopRecording();
  else await startRecording();
}

interface WindowInfo { id: number; app: string; title: string }
// Modal: pick which window to capture for frames. Resolves to a window id
// (string), "fullscreen", "none" (audio only), or null to abort the recording.
function chooseCaptureTarget(): Promise<string | null> {
  return new Promise((resolve) => {
    const overlay = $("capture-overlay");
    const list = $("capture-list");
    const close = (val: string | null) => {
      overlay.classList.add("hidden");
      list.innerHTML = "";
      resolve(val);
    };
    list.innerHTML = `<div class="cap-empty">Loading windows…</div>`;
    overlay.classList.remove("hidden");
    invoke<WindowInfo[]>("list_windows")
      .then((wins) => {
        list.innerHTML = "";
        if (!wins.length) {
          list.innerHTML = `<div class="cap-empty">No windows found. Use Full screen, or grant Screen Recording in System Settings.</div>`;
        }
        for (const w of wins) {
          const row = document.createElement("button");
          row.className = "cap-row";
          row.innerHTML = `<span class="cap-app"></span><span class="cap-title"></span>`;
          (row.querySelector(".cap-app") as HTMLElement).textContent = w.app;
          (row.querySelector(".cap-title") as HTMLElement).textContent = w.title;
          row.onclick = () => close(String(w.id));
          list.appendChild(row);
        }
      })
      .catch(() => {
        list.innerHTML = `<div class="cap-empty">Couldn't list windows. Use Full screen.</div>`;
      });
    $("capture-fullscreen").onclick = () => close("fullscreen");
    $("capture-none").onclick = () => close("none");
    $("capture-cancel").onclick = () => close(null);
  });
}

async function startRecording() {
  banner("");
  const frameTarget = await chooseCaptureTarget();
  if (frameTarget === null) return; // cancelled — don't start
  try {
    const res = await invoke<{ session_id: string }>("start_recording", { title: null, frameTarget });
    viewSessionId = res.session_id;
    recording = true;
    $("session-title").textContent = "Recording…";
    $("session-meta").textContent = new Date().toLocaleString();
    clearTranscript("Listening… speak and transcripts will appear here.");
    setRecordingUI(true);
    startTimer();
    startPolishAuto(); // raw transcript is polished into the story while recording
  } catch (e) {
    banner(`Could not start recording: ${e}`, "error");
  }
}

async function stopRecording() {
  try {
    await invoke<number>("stop_recording");
  } catch (e) {
    banner(`${e}`, "error");
  }
  recording = false;
  setRecordingUI(false);
  stopTimer();
  stopPolishAuto();
  flushPending(true); // commit any held-back segments immediately
  setTimeout(() => polishTick(true), 400); // final pass: polish everything remaining
  await refreshSessions();
}

function setRecordingUI(on: boolean) {
  const btn = $("new-recording");
  btn.textContent = on ? "■ Stop recording" : "● Start recording";
  btn.classList.toggle("recording", on);
  $("rec-indicator").classList.toggle("hidden", !on);
  $("timer").classList.toggle("hidden", !on);
  $("story-live").classList.toggle("hidden", !on);
}
function startTimer() {
  timerStart = Date.now();
  $("timer").textContent = "00:00";
  timerHandle = window.setInterval(() => {
    $("timer").textContent = fmtTime((Date.now() - timerStart) / 1000);
  }, 500);
}
function stopTimer() {
  if (timerHandle) window.clearInterval(timerHandle);
}

// ---- Knowledge vault + Claude Code chat ----
function clearChat(placeholder?: string) {
  const c = $("chat");
  c.innerHTML = placeholder ? `<div class="chat-empty">${placeholder}</div>` : "";
  streamingMsgEl = null;
}
function addMsg(role: "user" | "assistant", text: string): HTMLElement {
  const c = $("chat");
  const empty = c.querySelector(".chat-empty");
  if (empty) empty.remove();
  const el = document.createElement("div");
  el.className = `msg ${role}`;
  if (role === "assistant") {
    renderAssistant(el, text);
  } else {
    el.textContent = text;
  }
  c.appendChild(el);
  c.scrollTop = c.scrollHeight;
  return el;
}
// Pull a fenced ```actions block out of the agent's reply and turn each
// `label | instruction` line into a compact button that runs the instruction.
function extractActions(raw: string): {
  body: string;
  actions: { label: string; instruction: string }[];
} {
  const actions: { label: string; instruction: string }[] = [];
  const body = raw.replace(/```actions\s*\n?([\s\S]*?)```/g, (_m, block: string) => {
    for (const line of block.split("\n")) {
      const t = line.trim().replace(/^[-*]\s*/, "");
      if (!t) continue;
      const i = t.indexOf("|");
      if (i === -1) continue;
      const label = t.slice(0, i).trim();
      const instruction = t.slice(i + 1).trim();
      if (label && instruction) actions.push({ label, instruction });
    }
    return "";
  });
  return { body: body.trim(), actions };
}
function renderAssistant(el: HTMLElement, raw: string) {
  const { body, actions } = extractActions(raw);
  el.innerHTML = renderMarkdown(body);
  attachChatLinks(el);
  if (actions.length) {
    const bar = document.createElement("div");
    bar.className = "actions-bar";
    for (const a of actions) {
      const btn = document.createElement("button");
      btn.className = "hot-action";
      btn.textContent = a.label;
      btn.title = a.instruction;
      btn.onclick = () => runAgent(a.instruction, a.label);
      bar.appendChild(btn);
    }
    el.appendChild(bar);
  }
}
// Wire links inside an assistant message: #do: hot-actions, [[wikilinks]], http.
function attachChatLinks(el: HTMLElement) {
  el.querySelectorAll("a").forEach((a) => {
    const href = a.getAttribute("href") || "";
    if (href.startsWith("#do:")) {
      const instr = decodeURIComponent(href.slice(4));
      a.classList.add("hot-action");
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        runAgent(instr, a.textContent || "action");
      });
    } else if (href.startsWith("#wikilink:")) {
      const name = decodeURIComponent(href.slice("#wikilink:".length));
      a.classList.add("wikilink");
      a.addEventListener("click", async (ev) => {
        ev.preventDefault();
        const p = await invoke<string | null>("resolve_wikilink", { name });
        if (p) openVault(p);
      });
    } else if (/^https?:/.test(href)) {
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        openUrl(href).catch(() => {});
      });
    }
  });
}
function addToolChip(name: string, file: string | null) {
  const c = $("chat");
  const empty = c.querySelector(".chat-empty");
  if (empty) empty.remove();
  const chip = document.createElement("div");
  if (file) {
    const rel = file.split("/").slice(-2).join("/");
    chip.className = "tool-chip file";
    chip.textContent = `✎ ${rel}`;
    chip.title = file;
    chip.onclick = () => openVault(file);
  } else {
    chip.className = "tool-chip";
    chip.textContent = `· ${name}`;
  }
  c.appendChild(chip);
  c.scrollTop = c.scrollHeight;
}
function setChatStatus(text: string, kind: "" | "working" | "error" = "") {
  const el = $("chat-status");
  el.textContent = text;
  el.className = `chat-status ${kind}`;
  el.classList.toggle("hidden", !text);
}
function setBusy(b: boolean) {
  claudeBusy = b;
  ($("update-btn") as HTMLButtonElement).disabled = b || !viewSessionId;
  ($("process-btn") as HTMLButtonElement).disabled = b || !viewSessionId;
  ($("chat-send") as HTMLButtonElement).disabled = b;
}
async function openInObsidian(file: string) {
  try {
    await invoke("open_in_obsidian", { file });
  } catch (e) {
    setChatStatus(`Couldn't open in Obsidian: ${e}`, "error");
  }
}
async function loadChat(sessionId: string) {
  try {
    const rows = await invoke<ChatRow[]>("get_chat", { id: sessionId });
    if (!rows.length) {
      clearChat("No assistant activity yet. “Process meeting” or ask a question.");
      return;
    }
    clearChat();
    for (const r of rows) addMsg(r.role === "user" ? "user" : "assistant", r.text);
  } catch {
    clearChat();
  }
}
async function processMeeting() {
  if (!viewSessionId || claudeBusy) return;
  setBusy(true);
  streamingMsgEl = null;
  setChatStatus("Processing meeting into your vault…", "working");
  try {
    await invoke("process_meeting", { sessionId: viewSessionId });
  } catch (e) {
    setBusy(false);
    setChatStatus(`${e}`, "error");
  }
}
// Send a message/instruction to the agent (shared by chat box + hot actions).
async function runAgent(message: string, label?: string) {
  if (claudeBusy || !viewSessionId) return;
  addMsg("user", label ? `▶ ${label}` : message);
  setBusy(true);
  streamingMsgEl = null;
  setChatStatus("Working…", "working");
  try {
    await invoke("chat_send", { sessionId: viewSessionId, message });
  } catch (e) {
    setBusy(false);
    setChatStatus(`${e}`, "error");
  }
}
async function sendChat() {
  const ta = $("chat-text") as HTMLTextAreaElement;
  const text = ta.value.trim();
  if (!text) return;
  ta.value = "";
  await runAgent(text);
}
// "Update": live, proactive in-meeting note via the agent (agent.md persona).
async function updateMeeting() {
  if (!viewSessionId || claudeBusy) return;
  setBusy(true);
  streamingMsgEl = null;
  setChatStatus("Thinking…", "working");
  try {
    await invoke("update_meeting", { sessionId: viewSessionId });
  } catch (e) {
    setBusy(false);
    setChatStatus(`${e}`, "error");
  }
}
async function refreshVaultUI() {
  const vc = await invoke<VaultClaude>("get_vault_status");
  ($("set-vault") as HTMLInputElement).value = vc.vault?.path ?? appSettings?.vault_path ?? "";
  const vs = $("vault-status");
  if (vc.vault?.ready) {
    vs.textContent = `Vault ready (${vc.vault.adopted ? "adopted existing graph" : "scaffolded"}).`;
    vs.className = "hint ok";
  } else if (vc.vault) {
    vs.textContent = "Folder set but not initialized — click Choose to (re)scaffold.";
    vs.className = "hint";
  } else {
    vs.textContent = "Pick a folder for your knowledge graph (then open it in Obsidian).";
    vs.className = "hint";
  }
  const cs = $("claude-status");
  if (vc.claude_version) {
    cs.textContent = `Claude Code detected: ${vc.claude_version}`;
    cs.className = "hint ok";
  } else {
    cs.textContent = vc.claude_error ?? "Claude Code not found.";
    cs.className = "hint error";
  }
}
async function chooseVault() {
  const dir = await openDialog({ directory: true, multiple: false, title: "Choose knowledge folder" });
  if (!dir || typeof dir !== "string") return;
  try {
    await invoke("setup_vault", { path: dir });
    if (appSettings) appSettings.vault_path = dir;
    vaultPath = dir; // switch the in-app Notes/vault context live (no restart)
    ($("set-vault") as HTMLInputElement).value = dir;
    await refreshVaultUI();
    banner(`Knowledge folder switched to ${dir}`, "info");
    setTimeout(() => banner(""), 3000);
  } catch (e) {
    $("vault-status").textContent = `${e}`;
    $("vault-status").className = "hint error";
  }
}

// ---- Settings ----
async function openSettings() {
  const s = await invoke<Settings>("get_settings");
  appSettings = s;
  ($("set-endpoint") as HTMLInputElement).value = s.endpoint_url;
  ($("set-token") as HTMLInputElement).value = s.api_token;
  ($("set-language") as HTMLInputElement).value = s.language;
  ($("set-mic") as HTMLInputElement).checked = s.capture_mic;
  ($("set-system") as HTMLInputElement).checked = s.capture_system;
  ($("set-frames") as HTMLInputElement).checked = s.capture_frames;
  ($("set-model") as HTMLSelectElement).value = s.claude_model || "sonnet";
  ($("set-vault") as HTMLInputElement).value = s.vault_path;
  $("vault-choose").textContent = s.vault_path ? "Change…" : "Choose…";
  $("settings-overlay").classList.remove("hidden");
  refreshVaultUI();
}
function closeSettings() {
  $("settings-overlay").classList.add("hidden");
}
async function saveSettings() {
  const base = appSettings ?? (await invoke<Settings>("get_settings"));
  const newSettings: Settings = {
    ...base,
    endpoint_url: ($("set-endpoint") as HTMLInputElement).value.trim(),
    api_token: ($("set-token") as HTMLInputElement).value.trim(),
    language: ($("set-language") as HTMLInputElement).value.trim(),
    capture_mic: ($("set-mic") as HTMLInputElement).checked,
    capture_system: ($("set-system") as HTMLInputElement).checked,
    capture_frames: ($("set-frames") as HTMLInputElement).checked,
    claude_model: ($("set-model") as HTMLSelectElement).value,
  };
  try {
    await invoke("save_settings", { newSettings });
    appSettings = newSettings;
    closeSettings();
    banner("Settings saved.", "info");
    setTimeout(() => banner(""), 2000);
  } catch (e) {
    banner(`Could not save settings: ${e}`, "error");
  }
}

// ---- Notes vault workspace (built-in Markdown browser/editor) ----
function relToVault(p: string): string {
  if (vaultPath && p.startsWith(vaultPath)) return p.slice(vaultPath.length).replace(/^\//, "");
  return p;
}
function renderMarkdown(md: string): string {
  const pre = md.replace(/\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g, (_m, target, alias) =>
    `[${(alias || target).trim()}](#wikilink:${encodeURIComponent(String(target).trim())})`,
  );
  return marked.parse(pre, { async: false }) as string;
}
function attachPreviewLinks() {
  $("note-preview").querySelectorAll("a").forEach((a) => {
    const href = a.getAttribute("href") || "";
    if (href.startsWith("#wikilink:")) {
      const name = decodeURIComponent(href.slice("#wikilink:".length));
      a.classList.add("wikilink");
      a.addEventListener("click", async (ev) => {
        ev.preventDefault();
        const p = await invoke<string | null>("resolve_wikilink", { name });
        if (p) openNote(p);
        else a.classList.add("missing");
      });
    } else if (/^https?:/.test(href)) {
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        openUrl(href).catch(() => {});
      });
    }
  });
}
function highlightTreeActive(path: string) {
  document.querySelectorAll("#vault-tree .tree-item.file").forEach((el) => {
    el.classList.toggle("active", (el as HTMLElement).dataset.path === path);
  });
}
function renderNode(n: TreeNode): HTMLElement {
  if (n.dir) {
    const wrap = document.createElement("div");
    wrap.className = "tree-dir";
    const head = document.createElement("div");
    head.className = "tree-item dir";
    head.innerHTML = `<span class="caret">▾</span>`;
    head.appendChild(document.createTextNode(n.name));
    const children = document.createElement("div");
    children.className = "tree-children";
    for (const c of n.children) children.appendChild(renderNode(c));
    head.onclick = () => {
      const collapsed = children.classList.toggle("hidden");
      (head.querySelector(".caret") as HTMLElement).textContent = collapsed ? "▸" : "▾";
    };
    wrap.appendChild(head);
    wrap.appendChild(children);
    return wrap;
  }
  const el = document.createElement("div");
  el.className = "tree-item file";
  el.dataset.path = n.path;
  el.textContent = n.name.replace(/\.md$/, "");
  el.onclick = () => openNote(n.path);
  return el;
}
async function loadTree() {
  const tree = $("vault-tree");
  try {
    const nodes = await invoke<TreeNode[]>("vault_tree");
    tree.innerHTML = "";
    if (!nodes.length) {
      tree.innerHTML = `<div class="empty">Empty vault — process a meeting to populate it.</div>`;
      return;
    }
    for (const n of nodes) tree.appendChild(renderNode(n));
  } catch (e) {
    tree.innerHTML = `<div class="empty">${e}</div>`;
  }
}
async function destroyEditor() {
  if (crepe) {
    try {
      await crepe.destroy();
    } catch {
      /* ignore */
    }
    crepe = null;
  }
  $("note-editor").innerHTML = "";
}
function showPreview(md: string) {
  const pv = $("note-preview");
  pv.innerHTML = renderMarkdown(md);
  pv.classList.remove("hidden");
  attachPreviewLinks();
  $("note-editor").classList.add("hidden");
  $("note-save").classList.add("hidden");
  $("note-edit").textContent = "Edit";
  editing = false;
}
async function openNote(path: string) {
  try {
    await destroyEditor();
    const content = await invoke<string>("read_note", { path });
    currentNotePath = path;
    currentNoteRaw = content;
    $("note-path").textContent = relToVault(path);
    showPreview(content);
    for (const id of ["note-edit", "note-obsidian"]) $(id).classList.remove("hidden");
    highlightTreeActive(path);
  } catch (e) {
    $("note-preview").innerHTML = `<div class="empty">${e}</div>`;
  }
}
// Pull the current markdown out of the live editor (if editing).
async function currentMarkdown(): Promise<string> {
  if (crepe) {
    try {
      return crepe.getMarkdown();
    } catch {
      return currentNoteRaw;
    }
  }
  return currentNoteRaw;
}
async function toggleEdit() {
  if (!currentNotePath) return;
  if (!editing) {
    // Enter WYSIWYG edit mode (Milkdown).
    editing = true;
    $("note-preview").classList.add("hidden");
    const host = $("note-editor");
    host.classList.remove("hidden");
    host.innerHTML = "";
    crepe = new Crepe({ root: host, defaultValue: currentNoteRaw });
    await crepe.create();
    $("note-save").classList.remove("hidden");
    $("note-edit").textContent = "Preview";
  } else {
    // Back to read view, keeping (unsaved) edits in memory.
    currentNoteRaw = await currentMarkdown();
    await destroyEditor();
    showPreview(currentNoteRaw);
  }
}
async function saveNote() {
  if (!currentNotePath) return;
  try {
    const md = await currentMarkdown();
    await invoke("write_note", { path: currentNotePath, content: md });
    currentNoteRaw = md;
    await destroyEditor();
    showPreview(md);
    banner("Note saved.", "info");
    setTimeout(() => banner(""), 1500);
  } catch (e) {
    banner(`Could not save: ${e}`, "error");
  }
}
async function openVault(file?: string) {
  const vc = await invoke<VaultClaude>("get_vault_status");
  if (!vc.vault) {
    banner("Set up a knowledge folder in ⚙ Settings first.", "error");
    openSettings();
    return;
  }
  vaultPath = vc.vault.path;
  $("vault-view").classList.remove("hidden");
  await loadTree();
  if (file) openNote(file);
}
async function closeVault() {
  if (editing) currentNoteRaw = await currentMarkdown();
  await destroyEditor();
  $("vault-view").classList.add("hidden");
}

// ---- Live transcript polishing ----
function resetPolish() {
  turnLog = [];
  lastLog = null;
  polishedCount = 0;
  polishedDoc = "";
  polishBusy = false;
  const pol = document.getElementById("polished");
  if (pol) pol.innerHTML = "";
}
// Re-render the whole living document (it is reconsidered holistically each pass).
function renderPolished() {
  const pol = $("polished");
  pol.innerHTML = renderMarkdown(polishedDoc);
  attachResearchLinks(pol);
}
// Make [[entities]] in polished text clickable → research via the Assistant.
function attachResearchLinks(el: HTMLElement) {
  el.querySelectorAll("a").forEach((a) => {
    const href = a.getAttribute("href") || "";
    if (href === "#act") {
      // A proposed quick action — clicking runs it through the Assistant.
      const instruction = (a.textContent || "").trim();
      a.classList.add("act");
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        if (instruction) runAgent(instruction, instruction);
      });
    } else if (href.startsWith("#wikilink:")) {
      const name = decodeURIComponent(href.slice("#wikilink:".length));
      a.classList.add("kw");
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        runAgent(
          `Research "${name}" (mentioned in this meeting) and reply with concise findings. ` +
            `If it's a person, company, or product, create or update its entity note in the vault with links.`,
          `Research ${name}`,
        );
      });
    } else if (/^https?:/.test(href)) {
      a.addEventListener("click", (ev) => {
        ev.preventDefault();
        openUrl(href).catch(() => {});
      });
    }
  });
}
// Current display label for a logged turn (picks up names resolved since render).
function turnSpeaker(e: TurnLog): string {
  if (e.source !== "system") return "You";
  const ds = e.el.dataset.speaker;
  return ds != null ? speakerDisplay(Number(ds)) : e.speaker;
}
// Fold the settled (non-tail) raw turns into the polished story. `flush` ignores
// the settle window (used on stop) and polishes everything remaining.
async function polishTick(flush = false) {
  if (!viewSessionId || polishBusy) return;
  const tail = turnLog.length ? turnLog[turnLog.length - 1].end : 0;
  let upto = polishedCount;
  while (
    upto < turnLog.length &&
    (flush || turnLog[upto].end <= tail - POLISH_SETTLE_S)
  )
    upto++;
  const batch = turnLog.slice(polishedCount, upto);
  if (batch.length < (flush ? 1 : POLISH_MIN_TURNS)) return;
  polishBusy = true;
  $("story-live").classList.remove("hidden");
  const raw = batch
    .map((e) => `${turnSpeaker(e)} [${fmtTime(e.start)}]: ${e.text}`)
    .join("\n");
  try {
    // Whole-document edit, file-backed: the backend reads the saved doc, folds in
    // the new raw lines (holistic, first-person rewrite), writes it, returns it.
    const md = (await invoke<string>("polish_doc", { sessionId: viewSessionId, raw })).trim();
    if (md && (md.includes("####") || !polishedDoc)) {
      polishedDoc = md;
      renderPolished();
      for (const e of batch) e.el.remove(); // drop the raw rows now folded in
      polishedCount = upto;
      const empty = $("transcript").querySelector(".empty");
      if (empty && !turnLog.slice(polishedCount).length) empty.remove();
      scrollStream();
    } else {
      console.warn("[polish] discarded non-doc reply");
    }
  } catch (e) {
    console.warn("[polish] failed", e);
  } finally {
    polishBusy = false;
    if (!recording) $("story-live").classList.add("hidden");
  }
}
function startPolishAuto() {
  stopPolishAuto();
  polishTimer = window.setInterval(() => polishTick(), POLISH_INTERVAL_MS);
}
function stopPolishAuto() {
  if (polishTimer) {
    clearInterval(polishTimer);
    polishTimer = undefined;
  }
}

// ---- Event wiring ----
async function setupEvents() {
  await listen<SegmentEvent>("transcript-segment", (e) => {
    if (e.payload.session_id === viewSessionId) pushLive(e.payload);
  });
  await listen<DiarLabelEvent>("diar-label", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (!speakerNums.has(e.payload.speaker_id)) {
      speakerNums.set(e.payload.speaker_id, speakerNums.size + 1);
    }
    diarLabels.push({ speakerId: e.payload.speaker_id, start: e.payload.start, end: e.payload.end });
    reattributeSystemRows(); // backfill labels on rows shown before this label arrived
    scheduleNaming(); // dynamically name new/under-named speakers in the background
  });
  await listen<StartedEvent>("recording-started", (e) => {
    const { active_sources, warnings } = e.payload;
    let msg = `Recording — capturing: ${active_sources.join(", ") || "nothing"}.`;
    if (warnings.length) msg += ` ⚠ ${warnings.join("; ")}`;
    banner(msg, warnings.length ? "error" : "info");
  });
  await listen<StoppedEvent>("recording-stopped", () => {
    banner("Recording saved.", "info");
    setTimeout(() => banner(""), 2500);
  });
  await listen<ErrorEvent>("transcript-error", (e) => {
    banner(`Transcription issue: ${e.payload.message}`, "error");
  });

  // Claude Code streaming.
  await listen<DeltaEvent>("claude-delta", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (e.payload.kind === "story") return; // story renders on result only
    // Stream as plain text; we re-render as Markdown on result.
    if (!streamingMsgEl) {
      const c = $("chat");
      const empty = c.querySelector(".chat-empty");
      if (empty) empty.remove();
      streamingMsgEl = document.createElement("div");
      streamingMsgEl.className = "msg assistant";
      c.appendChild(streamingMsgEl);
    }
    streamingMsgEl.textContent = (streamingMsgEl.textContent || "") + e.payload.text;
    $("chat").scrollTop = $("chat").scrollHeight;
  });
  await listen<ToolEvent>("claude-tool", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (e.payload.kind === "story") return;
    // Finalize any streamed text as Markdown + actions before the tool chip.
    if (streamingMsgEl) {
      renderAssistant(streamingMsgEl, streamingMsgEl.textContent || "");
      streamingMsgEl = null;
    }
    addToolChip(e.payload.name, e.payload.file);
  });
  await listen<ResultEvent>("claude-result", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (e.payload.kind === "story") return; // story tab removed; ignore
    if (streamingMsgEl) {
      renderAssistant(streamingMsgEl, streamingMsgEl.textContent || e.payload.text);
    } else if (e.payload.text) {
      addMsg("assistant", e.payload.text);
    }
    streamingMsgEl = null;
    setBusy(false);
    const cost = e.payload.cost_usd != null ? ` · $${e.payload.cost_usd.toFixed(3)}` : "";
    setChatStatus(`Done${cost}.`, "");
    setTimeout(() => setChatStatus(""), 4000);
  });
  await listen<ClaudeErrEvent>("claude-error", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (e.payload.kind === "story") return; // story tab removed; ignore
    streamingMsgEl = null;
    setBusy(false);
    setChatStatus(`Claude error: ${e.payload.message}`, "error");
  });
}

// ---- Theme (night mode) ----
function applyTheme(dark: boolean) {
  document.documentElement.dataset.theme = dark ? "dark" : "light";
  $("theme-btn").textContent = dark ? "☀️" : "🌙";
  localStorage.setItem("theme", dark ? "dark" : "light");
}
function toggleTheme() {
  applyTheme(document.documentElement.dataset.theme !== "dark");
}
function initTheme() {
  applyTheme(localStorage.getItem("theme") === "dark");
}

function wireUI() {
  $("new-recording").onclick = toggleRecording;
  $("settings-btn").onclick = openSettings;
  $("theme-btn").onclick = toggleTheme;
  $("settings-cancel").onclick = closeSettings;
  $("settings-save").onclick = saveSettings;
  $("update-btn").onclick = updateMeeting;
  $("process-btn").onclick = processMeeting;
  $("chat-send").onclick = sendChat;
  $("name-speakers").onclick = () => nameNow();
  $("vault-choose").onclick = chooseVault;
  $("open-notes").onclick = () => openVault();
  $("vault-close").onclick = closeVault;
  $("note-edit").onclick = toggleEdit;
  $("note-save").onclick = saveNote;
  $("note-obsidian").onclick = () => {
    if (currentNotePath) openInObsidian(currentNotePath);
  };
  ($("chat-text") as HTMLTextAreaElement).addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      sendChat();
    }
  });
  ($("update-btn") as HTMLButtonElement).disabled = true; // until a session is selected
  ($("process-btn") as HTMLButtonElement).disabled = true;
}

window.addEventListener("DOMContentLoaded", async () => {
  initTheme();
  wireUI();
  await setupEvents();
  await refreshSessions();
  const s = await invoke<Settings>("get_settings");
  if (!s.api_token) {
    banner("Set your transcription endpoint & token in ⚙ Settings to enable transcription.", "info");
  }
});
