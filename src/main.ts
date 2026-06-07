import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

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
}
interface ChatRow { role: string; text: string; created_at: string }
interface DeltaEvent { session: string; text: string }
interface ToolEvent { session: string; name: string; file: string | null }
interface ResultEvent { session: string; text: string; cost_usd: number | null }
interface ClaudeErrEvent { session: string; message: string }
interface VaultClaude {
  vault: { path: string; exists: boolean; adopted: boolean; ready: boolean } | null;
  claude_version: string | null;
  claude_path: string | null;
  claude_error: string | null;
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
let appSettings: Settings | null = null; // last-loaded settings (preserve on save)
let streamingMsgEl: HTMLElement | null = null; // current streaming assistant bubble
let claudeBusy = false;

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
  setChatStatus("");
  await loadChat(s.id);
  setBusy(claudeBusy); // refresh Process button enabled state for the new session
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
  el.textContent = text;
  c.appendChild(el);
  c.scrollTop = c.scrollHeight;
  return el;
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
    chip.onclick = () => openInObsidian(file);
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
async function sendChat() {
  const ta = $("chat-text") as HTMLTextAreaElement;
  const text = ta.value.trim();
  if (!text || claudeBusy || !viewSessionId) return;
  addMsg("user", text);
  ta.value = "";
  setBusy(true);
  streamingMsgEl = null;
  setChatStatus("Working…", "working");
  try {
    await invoke("chat_send", { sessionId: viewSessionId, message: text });
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
    ($("set-vault") as HTMLInputElement).value = dir;
    await refreshVaultUI();
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
  ($("set-model") as HTMLSelectElement).value = s.claude_model || "sonnet";
  ($("set-vault") as HTMLInputElement).value = s.vault_path;
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

  // Claude Code streaming.
  await listen<DeltaEvent>("claude-delta", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (!streamingMsgEl) streamingMsgEl = addMsg("assistant", "");
    streamingMsgEl.textContent = (streamingMsgEl.textContent || "") + e.payload.text;
    $("chat").scrollTop = $("chat").scrollHeight;
  });
  await listen<ToolEvent>("claude-tool", (e) => {
    if (e.payload.session !== viewSessionId) return;
    addToolChip(e.payload.name, e.payload.file);
    streamingMsgEl = null; // a tool call breaks the current text bubble
  });
  await listen<ResultEvent>("claude-result", (e) => {
    if (e.payload.session !== viewSessionId) return;
    if (!streamingMsgEl && e.payload.text) addMsg("assistant", e.payload.text);
    streamingMsgEl = null;
    setBusy(false);
    const cost = e.payload.cost_usd != null ? ` · $${e.payload.cost_usd.toFixed(3)}` : "";
    setChatStatus(`Done${cost}.`, "");
    setTimeout(() => setChatStatus(""), 4000);
  });
  await listen<ClaudeErrEvent>("claude-error", (e) => {
    if (e.payload.session !== viewSessionId) return;
    streamingMsgEl = null;
    setBusy(false);
    setChatStatus(`Claude error: ${e.payload.message}`, "error");
  });
}

function wireUI() {
  $("new-recording").onclick = toggleRecording;
  $("settings-btn").onclick = openSettings;
  $("settings-cancel").onclick = closeSettings;
  $("settings-save").onclick = saveSettings;
  $("process-btn").onclick = processMeeting;
  $("chat-send").onclick = sendChat;
  $("vault-choose").onclick = chooseVault;
  ($("chat-text") as HTMLTextAreaElement).addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      sendChat();
    }
  });
  ($("process-btn") as HTMLButtonElement).disabled = true; // until a session is selected
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
