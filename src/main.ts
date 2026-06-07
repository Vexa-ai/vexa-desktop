import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---- Types mirroring the Rust side ----
interface Settings {
  endpoint_url: string;
  api_token: string;
  language: string;
  capture_mic: boolean;
  capture_system: boolean;
}
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

// ---- DOM helpers ----
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
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
let lastBubble: { bubbleEl: HTMLElement; source: string; end: number } | null = null;

// ---- Cross-source dedup (echo / speaker bleed) ----
// On speakers the mic re-hears system audio, so both pipelines transcribe the
// same words. We hold incoming live segments briefly, then drop the mic copy
// when a near-identical system copy exists at the same time.
const HOLDBACK_MS = 2500; // wait this long before committing a live segment
const DUP_WINDOW = 4; // seconds: max start-time gap for a duplicate
interface Seg { start: number; end: number; text: string; source: string }
let pending: (Seg & { recvAt: number })[] = [];
let recent: Seg[] = []; // already-rendered, for matching
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
function hasCounterpart(seg: Seg, target: string): boolean {
  const match = (o: Seg) =>
    o.source === target && Math.abs(o.start - seg.start) <= DUP_WINDOW && similar(o.text, seg.text);
  return pending.some((p) => p !== (seg as any) && match(p)) || recent.some(match);
}
// Drop the mic copy of an utterance that also came through system audio.
function isEcho(seg: Seg): boolean {
  return seg.source === "mic" && hasCounterpart(seg, "system");
}

function commit(seg: Seg) {
  if (isEcho(seg)) return;
  appendSegment(seg);
  recent.push(seg);
  if (recent.length > 80) recent.shift();
}
function flushMatured(force = false) {
  const now = performance.now();
  const ready = pending.filter((p) => force || now - p.recvAt >= HOLDBACK_MS);
  ready.sort((a, b) => a.start - b.start);
  for (const seg of ready) {
    pending = pending.filter((p) => p !== seg);
    commit(seg);
  }
  if (pending.length === 0 && flushTimer) {
    clearInterval(flushTimer);
    flushTimer = undefined;
  }
}
function pushLive(seg: Seg) {
  pending.push({ ...seg, recvAt: performance.now() });
  if (!flushTimer) flushTimer = window.setInterval(() => flushMatured(), 250);
}
// Dedup a fully-loaded (saved) session synchronously.
function dedupeLoaded(segs: Seg[]): Seg[] {
  const sorted = [...segs].sort((a, b) => a.start - b.start);
  const out: Seg[] = [];
  for (const seg of sorted) {
    const echo =
      seg.source === "mic" &&
      sorted.some(
        (o) =>
          o.source === "system" &&
          Math.abs(o.start - seg.start) <= DUP_WINDOW &&
          similar(o.text, seg.text),
      );
    if (!echo) out.push(seg);
  }
  return out;
}

function clearTranscript(placeholder?: string) {
  const t = $("transcript");
  t.innerHTML = placeholder ? `<div class="empty">${placeholder}</div>` : "";
  lastBubble = null;
  pending = [];
  recent = [];
  if (flushTimer) { clearInterval(flushTimer); flushTimer = undefined; }
}

function appendSegment(seg: {
  start: number;
  end: number;
  text: string;
  source: string;
}) {
  const t = $("transcript");
  const empty = t.querySelector(".empty");
  if (empty) empty.remove();
  const source = seg.source === "system" ? "system" : "mic";
  const text = seg.text.trim();
  if (!text) return;

  // Merge into the current bubble if same source and close in time.
  if (
    lastBubble &&
    lastBubble.source === source &&
    seg.start - lastBubble.end < GROUP_GAP
  ) {
    lastBubble.bubbleEl.textContent = `${lastBubble.bubbleEl.textContent} ${text}`.trim();
    lastBubble.end = seg.end;
    t.scrollTop = t.scrollHeight;
    return;
  }

  const row = document.createElement("div");
  row.className = "seg-row";
  row.innerHTML =
    `<div class="seg-ts">${fmtTime(seg.start)}</div>` +
    `<div class="bubble-line ${source}"><div class="bubble"></div></div>`;
  const bubbleEl = row.querySelector(".bubble") as HTMLElement;
  bubbleEl.textContent = text;
  t.appendChild(row);
  lastBubble = { bubbleEl, source, end: seg.end };
  t.scrollTop = t.scrollHeight;
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
  await refreshSessions();
}

// ---- Recording controls ----
async function toggleRecording() {
  if (recording) await stopRecording();
  else await startRecording();
}

async function startRecording() {
  banner("");
  try {
    const res = await invoke<{ session_id: string }>("start_recording", { title: null });
    viewSessionId = res.session_id;
    recording = true;
    $("session-title").textContent = "Recording…";
    $("session-meta").textContent = new Date().toLocaleString();
    clearTranscript("Listening… speak and transcripts will appear here.");
    setRecordingUI(true);
    startTimer();
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
  flushMatured(true); // render any buffered tail immediately
  await refreshSessions();
}

function setRecordingUI(on: boolean) {
  const btn = $("new-recording");
  btn.textContent = on ? "■ Stop recording" : "● Start recording";
  btn.classList.toggle("recording", on);
  $("rec-indicator").classList.toggle("hidden", !on);
  $("timer").classList.toggle("hidden", !on);
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

// ---- Settings ----
async function openSettings() {
  const s = await invoke<Settings>("get_settings");
  ($("set-endpoint") as HTMLInputElement).value = s.endpoint_url;
  ($("set-token") as HTMLInputElement).value = s.api_token;
  ($("set-language") as HTMLInputElement).value = s.language;
  ($("set-mic") as HTMLInputElement).checked = s.capture_mic;
  ($("set-system") as HTMLInputElement).checked = s.capture_system;
  $("settings-overlay").classList.remove("hidden");
}
function closeSettings() {
  $("settings-overlay").classList.add("hidden");
}
async function saveSettings() {
  const newSettings: Settings = {
    endpoint_url: ($("set-endpoint") as HTMLInputElement).value.trim(),
    api_token: ($("set-token") as HTMLInputElement).value.trim(),
    language: ($("set-language") as HTMLInputElement).value.trim(),
    capture_mic: ($("set-mic") as HTMLInputElement).checked,
    capture_system: ($("set-system") as HTMLInputElement).checked,
  };
  try {
    await invoke("save_settings", { newSettings });
    closeSettings();
    banner("Settings saved.", "info");
    setTimeout(() => banner(""), 2000);
  } catch (e) {
    banner(`Could not save settings: ${e}`, "error");
  }
}

// ---- Event wiring ----
async function setupEvents() {
  await listen<SegmentEvent>("transcript-segment", (e) => {
    if (e.payload.session_id === viewSessionId) pushLive(e.payload);
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
}

function wireUI() {
  $("new-recording").onclick = toggleRecording;
  $("settings-btn").onclick = openSettings;
  $("settings-cancel").onclick = closeSettings;
  $("settings-save").onclick = saveSettings;
}

window.addEventListener("DOMContentLoaded", async () => {
  wireUI();
  await setupEvents();
  await refreshSessions();
  const s = await invoke<Settings>("get_settings");
  if (!s.api_token) {
    banner("Set your transcription endpoint & token in ⚙ Settings to enable transcription.", "info");
  }
});
