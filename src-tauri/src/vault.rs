//! Knowledge vault management.
//!
//! The vault is a plain folder (an Obsidian vault) holding an entities graph
//! (people / companies / products), a strategies graph, and meeting reports.
//! `setup` scaffolds the structure on an empty folder, or adopts an existing
//! graph non-destructively (it never overwrites user files). The app writes raw
//! transcripts into `.vexa/transcripts/`; Claude Code reads those plus
//! `CLAUDE.md` to maintain the graph and write reports.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::storage::SegmentRow;

#[derive(Debug, Serialize)]
pub struct VaultStatus {
    pub path: String,
    pub exists: bool,
    /// True if the folder already looks like a knowledge graph (ours or kg/sg).
    pub adopted: bool,
    /// True once our structure + templates + CLAUDE.md are present.
    pub ready: bool,
}

/// Folders we ensure exist (relative to the vault root).
const DIRS: &[&str] = &[
    "entities/people",
    "entities/companies",
    "entities/products",
    "strategies",
    "reports",
    "templates",
    ".vexa/transcripts",
    ".vexa/prompts",
];

fn looks_like_graph(root: &Path) -> bool {
    root.join("entities").is_dir() && root.join("strategies").is_dir()
        || root.join("kg").is_dir() && root.join("sg").is_dir()
}

/// Create the vault structure + seed files if missing. Idempotent and
/// non-destructive: existing files are left untouched.
pub fn setup(root: &Path) -> Result<VaultStatus> {
    let adopted = root.is_dir() && looks_like_graph(root);

    std::fs::create_dir_all(root).with_context(|| format!("create vault {}", root.display()))?;
    for d in DIRS {
        std::fs::create_dir_all(root.join(d)).ok();
    }
    // .gitkeep so empty entity folders survive git.
    for d in ["entities/people", "entities/companies", "entities/products", "reports"] {
        write_if_absent(&root.join(d).join(".gitkeep"), "")?;
    }

    // Seed templates + conventions (only if absent).
    write_if_absent(&root.join("templates/person.md"), TPL_PERSON)?;
    write_if_absent(&root.join("templates/company.md"), TPL_COMPANY)?;
    write_if_absent(&root.join("templates/product.md"), TPL_PRODUCT)?;
    write_if_absent(&root.join("templates/strategy.md"), TPL_STRATEGY)?;
    write_if_absent(&root.join("templates/report.md"), TPL_REPORT)?;
    write_if_absent(&root.join("CLAUDE.md"), CLAUDE_MD)?;
    write_if_absent(&root.join("agent.md"), AGENT_MD)?;
    write_if_absent(&root.join("story.md"), STORY_MD)?;
    write_if_absent(&root.join("README.md"), README_MD)?;
    write_if_absent(&root.join(".vexa/prompts/process-meeting.md"), PROMPT_PROCESS)?;

    Ok(VaultStatus {
        path: root.to_string_lossy().to_string(),
        exists: true,
        adopted,
        ready: root.join("CLAUDE.md").is_file(),
    })
}

/// Ensure `agent.md` exists (older vaults predate it); return its contents.
pub fn ensure_agent_md(root: &Path) -> Option<String> {
    let p = root.join("agent.md");
    if !p.exists() {
        let _ = std::fs::write(&p, AGENT_MD);
    }
    std::fs::read_to_string(&p)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Ensure `story.md` exists; return its contents (the running-story agent brain).
pub fn ensure_story_md(root: &Path) -> Option<String> {
    let p = root.join("story.md");
    if !p.exists() {
        let _ = std::fs::write(&p, STORY_MD);
    }
    std::fs::read_to_string(&p)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

pub fn status(root: &Path) -> VaultStatus {
    VaultStatus {
        path: root.to_string_lossy().to_string(),
        exists: root.is_dir(),
        adopted: looks_like_graph(root),
        ready: root.join("CLAUDE.md").is_file(),
    }
}

fn write_if_absent(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn fmt_ts(secs: f64) -> String {
    let m = (secs / 60.0).floor() as i64;
    let s = (secs % 60.0).floor() as i64;
    format!("{:02}:{:02}", m, s)
}

fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cheap echo check: is `a` ~the same utterance as `b`?
fn similar(a: &str, b: &str) -> bool {
    let (na, nb) = (norm(a), norm(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.contains(&nb) || nb.contains(&na)
}

/// Write a labeled transcript to `.vexa/transcripts/<id>.md`. Mic segments that
/// merely echo a near-simultaneous system segment (speaker-bleed) are dropped,
/// matching the UI dedup, so Claude sees one clean stream.
/// Attributed, echo-deduped transcript lines (`[mm:ss] Me|Others: text`).
fn attributed_lines(segments: &[SegmentRow]) -> String {
    let mut segs: Vec<&SegmentRow> = segments.iter().collect();
    segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = String::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.source == "mic" {
            let echo = segs.iter().enumerate().any(|(j, o)| {
                j != i
                    && o.source == "system"
                    && (o.start - seg.start).abs() <= 4.0
                    && similar(&o.text, &seg.text)
            });
            if echo {
                continue;
            }
        }
        let who = if seg.source == "system" { "Others" } else { "Me" };
        out.push_str(&format!("[{}] {}: {}\n", fmt_ts(seg.start), who, seg.text.trim()));
    }
    out
}

pub fn write_transcript(
    root: &Path,
    session_id: &str,
    title: &str,
    started_at: &str,
    segments: &[SegmentRow],
) -> Result<PathBuf> {
    let mut body = String::new();
    body.push_str(&format!("# Transcript — {title}\n\n"));
    body.push_str(&format!("- Session: `{session_id}`\n"));
    body.push_str(&format!("- Started: {started_at}\n\n"));
    body.push_str("Speakers: **Me** = your microphone, **Others** = system audio (call/video).\n\n");
    body.push_str(&attributed_lines(segments));

    let path = root.join(".vexa/transcripts").join(format!("{session_id}.md"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, body).with_context(|| format!("write transcript {}", path.display()))?;
    Ok(path)
}

/// Attributed transcript text for a set of segments (used inline in story prompts).
pub fn attributed(segments: &[SegmentRow]) -> String {
    attributed_lines(segments)
}

// ---------------------------------------------------------------------------
// In-app notes browser: file tree + read/write, all confined to the vault.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub children: Vec<Node>,
}

/// Build the markdown file tree under `root` (skips dotfiles like `.vexa`).
pub fn tree(root: &Path) -> Vec<Node> {
    build_tree(root)
}

fn build_tree(dir: &Path) -> Vec<Node> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // .vexa, .git, .obsidian, .gitkeep …
        }
        let p = entry.path();
        if p.is_dir() {
            out.push(Node {
                name,
                path: p.to_string_lossy().to_string(),
                dir: true,
                children: build_tree(&p),
            });
        } else if name.ends_with(".md") {
            out.push(Node {
                name,
                path: p.to_string_lossy().to_string(),
                dir: false,
                children: Vec::new(),
            });
        }
    }
    // Folders first, then alphabetical.
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Resolve `path` against `root`, guaranteeing it stays inside the vault.
fn confined(root: &Path, path: &Path) -> Result<PathBuf> {
    let canon_root = root
        .canonicalize()
        .with_context(|| format!("vault not found: {}", root.display()))?;
    let canon = if path.exists() {
        path.canonicalize()?
    } else {
        let parent = path.parent().ok_or_else(|| anyhow::anyhow!("invalid path"))?;
        let file = path.file_name().ok_or_else(|| anyhow::anyhow!("invalid path"))?;
        parent.canonicalize()?.join(file)
    };
    if !canon.starts_with(&canon_root) {
        anyhow::bail!("path escapes the vault");
    }
    Ok(canon)
}

pub fn read_note(root: &Path, path: &Path) -> Result<String> {
    let p = confined(root, path)?;
    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))
}

pub fn write_note(root: &Path, path: &Path, content: &str) -> Result<()> {
    let p = confined(root, path)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, content).with_context(|| format!("write {}", p.display()))
}

/// Find `<name>.md` anywhere under the vault (for `[[wikilink]]` navigation).
pub fn resolve_link(root: &Path, name: &str) -> Option<PathBuf> {
    let target = format!("{}.md", name.trim().to_lowercase());
    fn walk(dir: &Path, target: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                if let Some(found) = walk(&p, target) {
                    return Some(found);
                }
            } else if name.to_lowercase() == target {
                return Some(p);
            }
        }
        None
    }
    walk(root, &target)
}

/// `obsidian://open?path=<url-encoded absolute path>` deep link.
pub fn obsidian_uri(file: &Path) -> String {
    let abs = file.to_string_lossy();
    let mut enc = String::new();
    for b in abs.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                enc.push(*b as char)
            }
            _ => enc.push_str(&format!("%{:02X}", b)),
        }
    }
    format!("obsidian://open?path={enc}")
}

// ---------------------------------------------------------------------------
// Seed content (written only when absent). Modeled on the reference graph.
// ---------------------------------------------------------------------------

const TPL_PERSON: &str = r#"# {{name}}
#contact

## Contact Information
- **Name:** {{name}}
- **Company:** [[ ]]
- **Title:**
- **LinkedIn:**
- **Status:**

## Relevance
-

## Related Entities
- **Company:** [[ ]]
- **Strategy:** [[ ]]

## Notes
-

<!-- routine-updates-start -->
<!-- routine-updates-end -->
"#;

const TPL_COMPANY: &str = r#"# {{name}}
#company

## Overview
**Type:**
**Focus:**

## Relevance
-

## Related Entities
- **People:** [[ ]]
- **Products:** [[ ]]
- **Strategy:** [[ ]]

## Resources
-

<!-- routine-updates-start -->
<!-- routine-updates-end -->
"#;

const TPL_PRODUCT: &str = r#"# {{name}}
#product

## Overview
**Vendor:** [[ ]]
**Category:**

## Notes
-

## Related Entities
- **Company:** [[ ]]

<!-- routine-updates-start -->
<!-- routine-updates-end -->
"#;

const TPL_STRATEGY: &str = r#"# {{name}}
`#strategy`

## Summary
-

## Linked Entities
-

## Actions
- [ ]
"#;

const TPL_REPORT: &str = r#"# {{date}} · {{title}}
#meeting

**Date:** {{date}}
**Platform:**
**Source:** Vexa Desktop recording

## Attendees
| Name | Org | Role |
|---|---|---|
| [[ ]] | [[ ]] |  |

## Top-line outcome


## What was discussed


## Decisions


## Action items
- [ ]

## Linked strategies
- [[ ]]
"#;

const README_MD: &str = r#"# Knowledge vault

This folder is a [Vexa Desktop](https://github.com/Vexa-ai/vexa-desktop) knowledge
vault. Open it as an **Obsidian vault** (Obsidian → Open folder as vault → pick
this folder).

## Layout
- `entities/people`, `entities/companies`, `entities/products` — the entities graph
- `strategies/` — the strategies graph
- `reports/` — meeting reports (one per meeting)
- `templates/` — note templates
- `CLAUDE.md` — conventions the in-app Claude Code assistant follows
- `.vexa/` — app-managed inputs (raw transcripts, prompts)

Tip: keep this folder under `git` so the assistant's edits are reviewable.
"#;

const CLAUDE_MD: &str = r#"# Knowledge vault — conventions

You maintain a personal knowledge graph stored as Markdown for Obsidian. Follow
these rules exactly when processing meeting transcripts.

## Layout
- `entities/people/<Name>.md` — a person (template: `templates/person.md`)
- `entities/companies/<Name>.md` — a company (`templates/company.md`)
- `entities/products/<Name>.md` — a product (`templates/product.md`)
- `strategies/<Name>.md` — a strategy node (`templates/strategy.md`)
- `reports/<YYYY-MM-DD>-<slug>.md` — one meeting report (`templates/report.md`)
- Raw transcripts live in `.vexa/transcripts/` — read them, never edit them.

## Conventions
- Filenames are human Title Case (e.g. `Armon Dadgar.md`, `Anthropic.md`).
- Link entities with Obsidian wikilinks: `[[Anthropic]]`, `[[Armon Dadgar]]`.
- Tag notes: people `#contact`, companies `#company`, products `#product`,
  strategies `#strategy`, reports `#meeting`.
- Every entity has a `## Related Entities` section — keep its links current.

## Upsert rule (idempotent)
- Before creating an entity, search for an existing file (by name, case-insensitive,
  and obvious aliases). If it exists, UPDATE it; do not create a duplicate.
- Never overwrite a user's prose. Add new facts inside the
  `<!-- routine-updates-start -->` … `<!-- routine-updates-end -->` region as a
  dated bullet: `### <YYYY-MM-DD> · meeting · confidence <0-100>` then the facts.

## Processing a meeting (what to produce)
1. Read the transcript file you are given.
2. Extract people, companies, products mentioned. Upsert each entity, filling
   what the transcript supports and adding `[[wikilinks]]` between them.
3. Write ONE report to `reports/<date>-<slug>.md` from `templates/report.md`:
   attendees table (link people↔companies), top-line outcome, what was discussed,
   decisions, action items.
4. Cross-link: add a `[[report]]` link from each attendee/company touched, and
   link the report to any relevant `strategies/` node (create a stub strategy
   only if clearly warranted).
5. End with a short summary of exactly which files you created vs updated.

Keep edits minimal, factual, and sourced to the meeting. When unsure, prefer a
brief note over speculation.
"#;

const AGENT_MD: &str = r#"# Vexa meeting agent

You are a **live meeting copilot**. You run repeatedly **during** a meeting; each
run you get the transcript so far. Be extremely concise and proactive — surface
what's useful right now and offer one-click next steps. You can read/write the
Obsidian vault you run in (follow CLAUDE.md for any writes).

## Output contract (every run)
1. A **Now** heading followed by **1–3 short bullets**: what's being discussed and
   the single most useful thing this moment. No preamble, no recap.
2. Then 2–5 one-click actions as a fenced block tagged `actions`, one per line as
   `emoji short-label | full instruction to execute when clicked`:

```actions
🔎 Research JEPA | Research Yann LeCun's JEPA and VJEPA; create entities/products/JEPA.md with a 3-line summary and key links.
👤 Stub speaker | Create entities/people/Speaker.md as an unnamed contact with a note to fill the name in when known.
✅ Capture claim | Append the key claim just made to today's report under Decisions.
```

Keep labels SHORT (2–4 words). Put ALL detail in the instruction after `|`, never
in the label. Use real names/claims from the transcript. Prefer: research an
entity/claim mentioned, stub a person/company/product, capture a decision or to-do.

## Rules
- Terse. The body is bullets only; the actions are the buttons.
- A plain **Update** is read-only thinking + suggested actions. Only write files
  when an action is clicked.
- When an action instruction is sent to you, execute it concisely and reply in ONE
  line with a `[[wikilink]]` to any note you touched.

Edit this file to change the agent — it's read fresh on every run.
"#;

const STORY_MD: &str = r#"# Story agent — living meeting report

You maintain ONE living **meeting report** that grows during the meeting. Each run
you get:
1. The report file so far (read it).
2. Only the NEW transcript since the last update — a sliding window, attributed
   ("Me" = the user's mic, "Others" = system audio / the call).

Your job: **extend** the report with a concise, faithful, **attributed**
representation of the new content. You decide what to add or revise:
- Append as the discussion continues. Correct earlier lines only if the new
  transcript clarifies/changes them — don't rewrite the whole thing.
- **Attribute**: make clear who said what (Me / Others, or a real name once
  identified). This is a condensed *record*, not an abstract summary — keep the
  substance, specifics, numbers, claims, decisions.
- Be concise: tighten rambling into clear lines, but stay faithful to what was
  actually said. No invented content.
- Wrap notable entities (people, companies, products) in `[[wikilinks]]`.
- Structure for scanning: short bullets / short lines under light headings. Group
  by topic as it unfolds. Never a wall of text.

Edit the report file in place. Reply with ONE short line on what you added.

Suggested shape (adapt freely):
```
# <Meeting title> — live report

**Participants:** Me, Others ([[names once known]])

## <Topic as it comes up>
- **Others:** <attributed point / quote, concise> ([[Entity]])
- **Me:** <attributed point>
```
"#;

const PROMPT_PROCESS: &str = r#"Process the meeting transcript at: {{transcript}}

Follow CLAUDE.md in this vault exactly. Steps:
1. Read the transcript.
2. Upsert the people, companies, and products discussed (entities/*), adding
   wikilinks between them. Update existing entities in their routine-updates
   region; do not duplicate.
3. Write one meeting report to reports/<YYYY-MM-DD>-<slug>.md using
   templates/report.md.
4. Cross-link the report with the entities and any relevant strategies/ node.
5. Reply with a concise bullet list of files Created vs Updated.
"#;
