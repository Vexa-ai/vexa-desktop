# Vexa — local meeting recorder

An open-source, **Granola-style** desktop app for Vexa. Instead of sending a *bot* into a
meeting, it records **your microphone + your computer's system audio** locally,
streams the mixed audio to the **Vexa transcription service**, and shows a live
transcript — all stored on your machine.

Cross-platform (macOS / Windows / Linux) via [Tauri](https://tauri.app)
(Rust core + a small web UI).

```
mic  ─┐
      ├─►  mix → 16 kHz mono → chunk on pauses ──► POST /v1/audio/transcriptions
sys ──┘                                                     │
                                              live segments ▼
                                          SQLite + WAV  +  UI (Tauri events)
```

## How audio is captured

| OS | Microphone | System audio | User action |
|----|-----------|-------------|-------------|
| macOS | `cpal` | **ScreenCaptureKit** (`--features sck`) | grant *Screen & System Audio Recording* once |
| Windows | `cpal` | WASAPI loopback (`cpal`) | none |
| Linux | `cpal` | PipeWire/PulseAudio `*.monitor` (`cpal`) | none |

The transcription path mirrors the Vexa bot exactly
(`services/vexa-bot/.../transcription-client.ts`): each chunk is a 16-bit PCM WAV
POSTed as `multipart/form-data` to `{endpoint}/v1/audio/transcriptions`
(OpenAI-compatible), authenticated with `X-API-Key`.

## Prerequisites

- [Rust](https://rustup.rs) (stable) and Node 18+.
- A Vexa transcription endpoint + token (hosted, or self-hosted
  `transcription-service`). Set both in the app's **⚙ Settings**.

## Develop

```bash
cd clients/desktop
npm install

# macOS (native system audio via ScreenCaptureKit):
npm run mac:dev

# Windows / Linux:
npm run tauri dev
```

On first recording, macOS prompts for **Screen & System Audio Recording** and
**Microphone** permission. Grant both, then start a recording again.

## Build an installer

```bash
# macOS → .app + .dmg
npm run mac:build
# Windows → .msi / .exe ; Linux → .deb / .rpm / .AppImage
npm run tauri build
```

Output lands in `src-tauri/target/release/bundle/`. The macOS `.dmg` is the
standard drag-to-Applications installer. For distribution to other Macs without
Gatekeeper warnings, sign + notarize with an Apple Developer ID (set the
`APPLE_*` env vars; Tauri signs during `build`).

## Where data lives

- Database + recordings: the OS app-data dir for `ai.vexa.granola`
  (macOS: `~/Library/Application Support/ai.vexa.granola/`).
- Settings JSON: the OS app-config dir for `ai.vexa.granola`.

## Status (MVP)

Implemented: capture (mic + system), mix, pause-based chunking, transcription
client, SQLite + WAV persistence, live transcript UI, session history, settings.

Not yet: per-speaker diarization, in-app audio playback synced to transcript,
AI summaries/notes, in-app permission pre-flight UI.

## License

Licensed under either of **MIT** ([LICENSE-MIT](LICENSE-MIT)) or **Apache-2.0**
([LICENSE-APACHE](LICENSE-APACHE)) at your option.

All dependencies are permissively licensed (MIT / Apache-2.0 / BSD / ISC).
A few transitive crates pulled in by Tauri use **MPL-2.0** (`cssparser`,
`selectors`, `dtoa-short`, `option-ext`) — weak, file-level copyleft that does
not affect this project's license — plus permissive **Unicode-3.0** (ICU) and
**CDLA-Permissive-2.0** (CA-cert data) licenses. No GPL/AGPL/LGPL-only or
proprietary dependencies. The editor is [Milkdown](https://milkdown.dev) (MIT,
ProseMirror-based).

