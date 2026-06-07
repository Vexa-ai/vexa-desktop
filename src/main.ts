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
// Live Story tab (a living, attributed meeting report extended from a sliding window)
let storyText = "";
let storyBusy = false;
let storyTimer: number | undefined;
let storyReportPath = ""; // absolute path to the living report file
let lastStoryEnd = 0; // transcript seconds already folded into the report (window cursor)
const STORY_INTERVAL_MS = 25000; // auto-refresh cadence while recording
// Notes vault workspace
let vaultPath = "";
let currentNotePath: string | null = null;
let currentNoteRaw = "";
let editing = false;
let crepe: Crepe | null = null; // active Milkdown rich editor

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
// same words. Rather than fragile incremental dedup (which depends on which copy
// arrives first), we keep ALL received segments and re-derive the displayed
// transcript on every update: dedupe the full set, then render. This is
// ordering-independent, so a late-arriving system twin still removes its mic echo.
const DUP_WINDOW = 8; // seconds: max start-time gap when matching a mic↔system twin
interface Seg { start: number; end: number; text: string; source: string }
let liveSegs: Seg[] = []; // everything received this recording (both sources)
let renderTimer: number | undefined;

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
// Re-render the whole live transcript from the deduped, grouped segment set.
function renderLive() {
  const segs = dedupeLoaded(liveSegs);
  const t = $("transcript");
  t.innerHTML = "";
  lastBubble = null;
  if (!segs.length) {
    t.innerHTML = `<div class="empty">Listening… speak and transcripts will appear here.</div>`;
    return;
  }
  for (const seg of segs) appendSegment(seg);
}
function pushLive(seg: Seg) {
  liveSegs.push(seg);
  if (!renderTimer) {
    renderTimer = window.setTimeout(() => {
      renderTimer = undefined;
      renderLive();
    }, 350);
  }
}

function clearTranscript(placeholder?: string) {
  const t = $("transcript");
  t.innerHTML = placeholder ? `<div class="empty">${placeholder}</div>` : "";
  lastBubble = null;
  liveSegs = [];
  if (renderTimer) {
    clearTimeout(renderTimer);
    renderTimer = undefined;
  }
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
  // Saved session: load its existing report (cursor at end → ↻ only adds new).
  stopStoryAuto();
  resetStory();
  lastStoryEnd = s.duration_secs ?? 0;
  storyUpdate(); // since = end → ran:false → loads the saved report file if any
  switchTab("transcript");
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
    resetStory();
    setRecordingUI(true);
    startTimer();
    startStoryAuto(); // self-updating live story while recording
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
  stopStoryAuto();
  if (renderTimer) {
    clearTimeout(renderTimer);
    renderTimer = undefined;
  }
  renderLive(); // final pass over the full deduped set
  storyUpdate(); // one final story refresh
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
  ($("set-frames") as HTMLInputElement).checked = s.capture_frames;
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

// ---- Live Story tab ----
function setStoryStatus(text: string, kind: "" | "working" | "error" = "") {
  const el = $("story-status");
  el.textContent = text;
  el.className = `story-status ${kind}`;
}
function resetStory() {
  storyText = "";
  storyReportPath = "";
  lastStoryEnd = 0;
  $("story-body").innerHTML =
    `<div class="empty">A living, attributed meeting report builds itself here while you record. Entities are clickable.</div>`;
}
async function loadStoryFromFile() {
  if (!storyReportPath) return;
  try {
    const md = await invoke<string>("read_note", { path: storyReportPath });
    renderStory(md);
  } catch {
    /* report not created yet */
  }
}
function attachStoryLinks(el: HTMLElement) {
  el.querySelectorAll("a").forEach((a) => {
    const href = a.getAttribute("href") || "";
    if (href.startsWith("#wikilink:")) {
      const name = decodeURIComponent(href.slice("#wikilink:".length));
      a.classList.add("wikilink");
      a.addEventListener("click", async (ev) => {
        ev.preventDefault();
        const p = await invoke<string | null>("resolve_wikilink", { name });
        if (p) openVault(p);
        else
          runAgent(
            `Research "${name}" (mentioned in this meeting) and create the right entity in the vault with a concise summary and links.`,
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
function renderStory(md: string) {
  storyText = md;
  const b = $("story-body");
  b.innerHTML = renderMarkdown(md);
  attachStoryLinks(b);
}
async function storyUpdate() {
  if (!viewSessionId || storyBusy) return;
  storyBusy = true;
  setStoryStatus("Updating report…", "working");
  try {
    const ref = await invoke<{ report_path: string; processed_until: number; ran: boolean }>(
      "story_update",
      { sessionId: viewSessionId, sinceSecs: lastStoryEnd },
    );
    storyReportPath = ref.report_path;
    lastStoryEnd = ref.processed_until;
    if (!ref.ran) {
      // Nothing new this tick — just show the existing report.
      storyBusy = false;
      setStoryStatus(recording ? "Up to date." : "No new transcript.");
      if (!storyText) await loadStoryFromFile();
    }
    // If it ran, the report file is read on the story result event.
  } catch (e) {
    storyBusy = false;
    setStoryStatus(`${e}`, "error");
  }
}
function switchTab(tab: "transcript" | "story") {
  $("tab-transcript").classList.toggle("active", tab === "transcript");
  $("tab-story").classList.toggle("active", tab === "story");
  $("transcript-pane").classList.toggle("hidden", tab !== "transcript");
  $("story-pane").classList.toggle("hidden", tab !== "story");
  if (tab === "story" && !storyText && !storyBusy && viewSessionId) storyUpdate();
}
function startStoryAuto() {
  stopStoryAuto();
  storyTimer = window.setInterval(() => storyUpdate(), STORY_INTERVAL_MS);
}
function stopStoryAuto() {
  if (storyTimer) {
    clearInterval(storyTimer);
    storyTimer = undefined;
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
    if (e.payload.kind === "story") {
      storyBusy = false;
      const md = (e.payload.text || "").trim();
      if (md) {
        renderStory(md);
        // Persist the updated report so it lives in the vault / Obsidian.
        if (storyReportPath) invoke("write_note", { path: storyReportPath, content: md }).catch(() => {});
      }
      setStoryStatus(recording ? "Auto-updating while recording (fast model)." : "Updated.");
      return;
    }
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
    if (e.payload.kind === "story") {
      storyBusy = false;
      setStoryStatus(`Story error: ${e.payload.message}`, "error");
      return;
    }
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
  $("tab-transcript").onclick = () => switchTab("transcript");
  $("tab-story").onclick = () => switchTab("story");
  $("story-refresh").onclick = () => storyUpdate();
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
