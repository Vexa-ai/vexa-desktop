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
    write_if_absent(&root.join("README.md"), README_MD)?;
    write_if_absent(&root.join(".vexa/prompts/process-meeting.md"), PROMPT_PROCESS)?;

    Ok(VaultStatus {
        path: root.to_string_lossy().to_string(),
        exists: true,
        adopted,
        ready: root.join("CLAUDE.md").is_file(),
    })
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
pub fn write_transcript(
    root: &Path,
    session_id: &str,
    title: &str,
    started_at: &str,
    segments: &[SegmentRow],
) -> Result<PathBuf> {
    let mut segs: Vec<&SegmentRow> = segments.iter().collect();
    segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));

    let mut body = String::new();
    body.push_str(&format!("# Transcript — {title}\n\n"));
    body.push_str(&format!("- Session: `{session_id}`\n"));
    body.push_str(&format!("- Started: {started_at}\n\n"));
    body.push_str("Speakers: **Me** = your microphone, **Others** = system audio (call/video).\n\n");

    for (i, seg) in segs.iter().enumerate() {
        // Drop mic echoes of system audio.
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
        body.push_str(&format!("[{}] {}: {}\n", fmt_ts(seg.start), who, seg.text.trim()));
    }

    let path = root.join(".vexa/transcripts").join(format!("{session_id}.md"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, body).with_context(|| format!("write transcript {}", path.display()))?;
    Ok(path)
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
