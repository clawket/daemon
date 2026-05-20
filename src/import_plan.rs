use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

use crate::envelope::sign as env_sign;
use crate::models::{Plan, Project, Task, Unit};
use crate::repo::{plans, projects, tasks, units};
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct ParsedTask {
    pub idx: i64,
    pub title: String,
    pub body: String,
    /// Ticket key parsed from the strict task heading (`LM-N`, `NEW-N`,
    /// or any `[A-Z]{1,8}-[0-9]+`). Empty for loose-mode parses.
    /// Used by the dependency-graph parser (LM-262) to resolve mermaid
    /// edge endpoints back to tasks.
    #[allow(dead_code)]
    pub ticket: String,
    /// Strict-mode envelope (LM-261). `None` for loose-mode parses.
    /// LM-263+ wires this into `task_envelopes` on insert; until then
    /// it's only read by the strict tests.
    #[allow(dead_code)]
    pub envelope: Option<ParsedEnvelope>,
    /// Dependency edges resolved against this plan (LM-262). Bullet
    /// `depends_on` value is canonical; the mermaid graph is a
    /// cross-check. Conflict between the two surfaces as
    /// `DependsOnMismatch`.
    #[allow(dead_code)]
    pub depends_on: Vec<String>,
    /// LM-263 / L1.3.b — promoted from the envelope so the strict
    /// importer can write directly to the first-class column added in
    /// migration 008. Defaults to `"small"` when the bullet is absent.
    /// Invalid values are rejected at parse time with
    /// `BadAtomicSizeHint`, matching the SQLite CHECK constraint, so
    /// the daemon never sees a row that would fail INSERT.
    #[allow(dead_code)]
    pub atomic_size_hint: String,
    /// LM-263 / L1.3.b — promoted from the envelope. Free-form string
    /// per ADR-0001 (e.g. `"atomic"`, `"tree(max_depth=3)"`); default
    /// `"auto"` lets the daemon scheduler derive the policy from
    /// `atomic_size_hint` when not explicitly declared.
    #[allow(dead_code)]
    pub decomposition_policy: String,
}

impl Default for ParsedTask {
    /// Match the SQLite defaults from migration 008 so a freshly
    /// `Default::default()`'d task never fails the `atomic_size_hint`
    /// CHECK constraint downstream. Loose-mode parses (which never
    /// see an envelope) get the same defaults as strict-mode parses
    /// where the bullet is omitted.
    fn default() -> Self {
        Self {
            idx: 0,
            title: String::new(),
            body: String::new(),
            ticket: String::new(),
            envelope: None,
            depends_on: Vec::new(),
            atomic_size_hint: DEFAULT_ATOMIC_SIZE_HINT.to_string(),
            decomposition_policy: DEFAULT_DECOMPOSITION_POLICY.to_string(),
        }
    }
}

/// Parsed envelope bullet block. Fields are kept as a `Vec` (not a `Map`)
/// so the canonical declaration order — fixed by `ENVELOPE_KEYS` — is
/// preserved through the round-trip; serde_json's default `Map` is a
/// `BTreeMap` that would re-sort keys alphabetically.
#[derive(Debug, Default, Clone)]
pub struct ParsedEnvelope {
    pub fields: Vec<(String, serde_json::Value)>,
}

impl ParsedEnvelope {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// 19-field ADR-0001 envelope keys in canonical render order. MUST stay
/// in lockstep with `cli/src/main.rs::ENVELOPE_FIELDS`. Round-trip parity
/// is asserted by `cli/tests/plan_export_roundtrip.rs` (LM-264) once the
/// strict importer is wired end-to-end.
pub const ENVELOPE_KEYS: [&str; 19] = [
    "version",
    "intent",
    "target_repo",
    "target_model",
    "max_turns",
    "prompt_template",
    "context_refs",
    "scope_boundary",
    "atomic_size_hint",
    "success_criteria",
    "verification_cmd",
    "depends_on",
    "blocked_by",
    "planned_sha",
    "decomposition_policy",
    "checkpoint_interval",
    "rollback_strategy",
    "origin",
    "assigned_model",
];

/// Required tier per ADR-0001 §3 — strict importer rejects any envelope
/// that omits any of these. The non-required keys default per the
/// envelope schema and need not appear in the bullet list.
pub const REQUIRED_ENVELOPE_KEYS: [&str; 7] = [
    "version",
    "intent",
    "target_repo",
    "context_refs",
    "success_criteria",
    "verification_cmd",
    "decomposition_policy",
];

/// Allowed values for `atomic_size_hint`. Mirrors the SQLite CHECK in
/// migration 008 (`tiny|small|medium|large`). The strict parser rejects
/// any other value with `BadAtomicSizeHint` so the daemon never sees an
/// envelope it would have to translate or quietly clamp.
pub const ATOMIC_SIZE_HINT_VALUES: [&str; 4] = ["tiny", "small", "medium", "large"];

/// Defaults for the two envelope keys promoted to first-class task
/// columns (LM-255). Tracks the migration 008 column DEFAULTs so loose
/// parses, omitted bullets, and SQL inserts all converge on the same
/// canonical values.
pub const DEFAULT_ATOMIC_SIZE_HINT: &str = "small";
pub const DEFAULT_DECOMPOSITION_POLICY: &str = "auto";

#[derive(Debug, Default, Clone)]
pub struct ParsedUnit {
    pub idx: i64,
    pub title: String,
    pub tasks: Vec<ParsedTask>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedPlan {
    pub title: Option<String>,
    pub description: String,
    pub units: Vec<ParsedUnit>,
}

static UNIT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^##\s+Unit\s*(\d+)?\s*[:.]?\s*(.*)$").unwrap());
static H3_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^###\s+(.+)$").unwrap());
static NUMBERED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*\d+\.\s+\*?\*?([^*]+)\*?\*?(.*)$").unwrap());
static HEADING_ANY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^##?#?\s+").unwrap());
static NUMBERED_START_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\d+\.\s+").unwrap());
static TITLE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^#\s+").unwrap());

pub fn parse_plan_markdown(md: &str) -> ParsedPlan {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut plan = ParsedPlan::default();

    if let Some(t) = lines.iter().find(|l| TITLE_RE.is_match(l)) {
        let stripped = TITLE_RE.replace(t, "").trim().to_string();
        plan.title = Some(stripped);
    }

    let mut unit_markers: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = UNIT_RE.captures(line) {
            let num = caps.get(1).map(|m| m.as_str().to_string());
            let name = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            let title = name.unwrap_or_else(|| {
                format!(
                    "Unit {}",
                    num.unwrap_or_else(|| (unit_markers.len() + 1).to_string())
                )
            });
            unit_markers.push((i, title));
        }
    }

    if unit_markers.is_empty() {
        plan.units.push(ParsedUnit {
            idx: 0,
            title: "Unit 1".to_string(),
            tasks: extract_tasks_from_body(&lines, 0, lines.len()),
        });
    } else {
        plan.description = lines[..unit_markers[0].0].join("\n").trim().to_string();
        for (p, (line_idx, title)) in unit_markers.iter().enumerate() {
            let end = unit_markers
                .get(p + 1)
                .map(|(i, _)| *i)
                .unwrap_or(lines.len());
            plan.units.push(ParsedUnit {
                idx: p as i64,
                title: title.clone(),
                tasks: extract_tasks_from_body(&lines, *line_idx + 1, end),
            });
        }
    }

    plan
}

fn extract_tasks_from_body(lines: &[&str], start: usize, end: usize) -> Vec<ParsedTask> {
    let mut h3: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        if let Some(caps) = H3_RE.captures(line) {
            h3.push((i, caps[1].trim().to_string()));
        }
    }
    if !h3.is_empty() {
        let mut out = Vec::new();
        for (j, (s, title)) in h3.iter().enumerate() {
            let e = h3.get(j + 1).map(|(i, _)| *i).unwrap_or(end);
            out.push(ParsedTask {
                idx: j as i64,
                title: title.clone(),
                body: lines[s + 1..e].join("\n").trim().to_string(),
                ticket: String::new(),
                envelope: None,
                depends_on: Vec::new(),
                atomic_size_hint: DEFAULT_ATOMIC_SIZE_HINT.to_string(),
                decomposition_policy: DEFAULT_DECOMPOSITION_POLICY.to_string(),
            });
        }
        return out;
    }

    let mut out: Vec<ParsedTask> = Vec::new();
    for i in start..end {
        if let Some(caps) = NUMBERED_RE.captures(lines[i]) {
            let title = caps
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let tail = caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let body = format!("{}\n{}", tail, gather_continuation(lines, i + 1, end))
                .trim()
                .to_string();
            out.push(ParsedTask {
                idx: out.len() as i64,
                title,
                body,
                ticket: String::new(),
                envelope: None,
                depends_on: Vec::new(),
                atomic_size_hint: DEFAULT_ATOMIC_SIZE_HINT.to_string(),
                decomposition_policy: DEFAULT_DECOMPOSITION_POLICY.to_string(),
            });
        }
    }
    out
}

fn gather_continuation(lines: &[&str], from: usize, end: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines.iter().take(end).skip(from) {
        if HEADING_ANY_RE.is_match(line) || NUMBERED_START_RE.is_match(line) {
            break;
        }
        out.push(line);
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// LM-258 / L1.1.b — strict parser skeleton.
//
// `parse_plan_strict` lives next to `parse_plan_markdown` (loose) so the two
// modes share `ParsedPlan` / `ParsedUnit` / `ParsedTask` types but never
// share parsing logic — strict mode rejects what loose mode would silently
// recover from. The contract is `cli/docs/plans/strict-format.md` (LM-253).
//
// Scope of this leaf: skeleton presence checks only. The 19-field envelope
// bullet block is recognized but its contents are NOT parsed here — L1.2.b
// (LM-261) fills that in. Likewise, the mermaid `Dependency Graph` block is
// only checked for *existence and fence shape*; edge parsing is L1.2.c.
//
// Why a hand-written line walker instead of a markdown crate: the spec is
// strict about line-level shape (heading depth, exact section order,
// explicit section names). A pull-parser would over-tolerate. Keeping the
// parser in the same line-vector vocabulary as `parse_plan_markdown` makes
// future de-duplication of the two parsers tractable.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportErrorKind {
    BadHeadingDepth,
    MissingMeta,
    MissingOverview,
    MissingEnvelopeBlock,
    MissingDepsGraph,
    /// Envelope bullet has the wrong shape (no backticks around key,
    /// missing `:`, or stray non-bullet line in the envelope block).
    BadEnvelopeBullet,
    /// Bullet key is not one of the 19 canonical ENVELOPE_KEYS.
    BadEnvelopeKey,
    /// Bullets declared in non-canonical order. Strict mode pins the
    /// order so round-trip parity is byte-stable.
    EnvelopeOrderMismatch,
    /// Bullet value not parseable as a JSON literal / number / bool /
    /// multi-line block scalar per strict-format.md §5.
    BadEnvelopeValue,
    /// ADR-0001 required-tier key missing from the envelope.
    MissingRequiredKey,
    /// Mermaid fence language is not `mermaid` (e.g. ```dot, ```graphviz).
    BadDepsGraphFence,
    /// Mermaid block has the wrong shape — first line not `graph LR`,
    /// unrecognized line, unclosed fence, etc.
    BadDepsGraph,
    /// An edge endpoint doesn't resolve to any task in this plan.
    UnknownDependsOnRef,
    /// `depends_on` graph contains a cycle. The hint includes the cycle
    /// path so callers can render it in the strict-guide message.
    DependencyCycle,
    /// A task's bullet `depends_on` set disagrees with the mermaid graph
    /// edges for that task. Strict mode requires both to agree.
    DependsOnMismatch,
    /// `atomic_size_hint` value not in `ATOMIC_SIZE_HINT_VALUES`. Strict
    /// mode rejects at parse time so the SQLite CHECK never fires —
    /// failing inside `INSERT` would surface as a generic
    /// "constraint violation" with no line context.
    BadAtomicSizeHint,
}

#[derive(Debug, Clone)]
pub struct ImportError {
    pub line: usize,
    pub column: usize,
    pub kind: ImportErrorKind,
    pub hint: String,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "strict parse error at {}:{} — {:?}: {}",
            self.line, self.column, self.kind, self.hint
        )
    }
}

impl std::error::Error for ImportError {}

static STRICT_TASK_HEADING_RE: Lazy<Regex> = Lazy::new(|| {
    // ### {title} ({TICKET}) — _{status}_
    // Capture group 1 = title, group 2 = ticket. Status enum mirrors
    // the spec; em-dash (U+2014) is part of the contract.
    Regex::new(r"^###\s+(.+?)\s+\(([A-Z]{1,8}-[0-9]+|NEW-[0-9]+)\)\s+\u{2014}\s+_(?:todo|in_progress|blocked|done|cancelled)_\s*$")
        .unwrap()
});

static STRICT_UNIT_HEADING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^##\s+Unit\s+(\d+)\s*:\s*(.+?)\s*$").unwrap());

/// Mermaid node line: `  {id}["{label}"]` — two-space indent (or any
/// leading whitespace), an identifier, a bracketed quoted label.
static MERMAID_NODE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*([A-Za-z][A-Za-z0-9_-]*)\["([^"]+)"\]\s*$"#).unwrap());

/// Mermaid edge line: `  {from} --> {to}` — single-headed arrow.
static MERMAID_EDGE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*([A-Za-z][A-Za-z0-9_-]*)\s+-->\s+([A-Za-z][A-Za-z0-9_-]*)\s*$").unwrap()
});

pub fn parse_plan_strict(md: &str) -> Result<ParsedPlan, ImportError> {
    let lines: Vec<&str> = md.split('\n').collect();

    // Exactly one H1.
    let h1_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            // `# ` at line start, but not `## ` / `### `.
            if l.starts_with("# ") && !l.starts_with("## ") {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    let title_line = match h1_indices.as_slice() {
        [] => {
            return Err(ImportError {
                line: 1,
                column: 1,
                kind: ImportErrorKind::BadHeadingDepth,
                hint: "exactly one H1 (`# title`) expected, found 0".into(),
            });
        }
        [i] => *i,
        many => {
            return Err(ImportError {
                line: many[1] + 1,
                column: 1,
                kind: ImportErrorKind::BadHeadingDepth,
                hint: format!("exactly one H1 expected, found {}", many.len()),
            });
        }
    };
    let title = lines[title_line].trim_start_matches('#').trim().to_string();

    // ## Meta — required, before any unit heading.
    let meta_idx = find_h2(&lines, "Meta");
    let Some(meta_line) = meta_idx else {
        return Err(ImportError {
            line: title_line + 2,
            column: 1,
            kind: ImportErrorKind::MissingMeta,
            hint: "missing `## Meta` block — required after H1 title".into(),
        });
    };

    // ## Overview — required, after Meta.
    let overview_idx = find_h2(&lines, "Overview");
    let Some(_overview_line) = overview_idx else {
        return Err(ImportError {
            line: meta_line + 2,
            column: 1,
            kind: ImportErrorKind::MissingOverview,
            hint: "missing `## Overview` table — required after `## Meta`".into(),
        });
    };

    // Per-task envelope bullet block presence. Walk every `### …` heading
    // matching the strict task regex and confirm an `**Envelope:**` marker
    // appears before the next `###` or `##` heading.
    let mut units: Vec<ParsedUnit> = Vec::new();
    let mut current_unit: Option<(usize, i64, String, Vec<ParsedTask>)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(caps) = STRICT_UNIT_HEADING_RE.captures(line) {
            if let Some((_, idx, ttl, tasks)) = current_unit.take() {
                units.push(ParsedUnit {
                    idx,
                    title: ttl,
                    tasks,
                });
            }
            let idx: i64 = caps[1].parse().unwrap_or(0);
            let ttl = caps[2].trim().to_string();
            current_unit = Some((i, idx, ttl, Vec::new()));
            i += 1;
            continue;
        }
        if let Some(task_caps) = STRICT_TASK_HEADING_RE.captures(line) {
            let task_title_text = task_caps[1].trim().to_string();
            let task_ticket_text = task_caps[2].to_string();
            // Locate the `**Envelope:**` marker between this heading and
            // the next sibling heading.
            let mut envelope_line: Option<usize> = None;
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j].trim_start();
                if l.starts_with("### ") || l.starts_with("## ") || l.starts_with("# ") {
                    break;
                }
                if l.starts_with("**Envelope:**") {
                    envelope_line = Some(j);
                    break;
                }
                j += 1;
            }
            let env_idx = match envelope_line {
                Some(idx) => idx,
                None => {
                    return Err(ImportError {
                        line: i + 1,
                        column: 1,
                        kind: ImportErrorKind::MissingEnvelopeBlock,
                        hint: "task heading must be followed by an `**Envelope:**` bullet block before the next heading".into(),
                    });
                }
            };
            // Find the end of the envelope block (next sibling heading
            // or EOF). Bullet block runs from env_idx+1..end.
            let mut end = env_idx + 1;
            while end < lines.len() {
                let l = lines[end].trim_start();
                if l.starts_with("### ") || l.starts_with("## ") || l.starts_with("# ") {
                    break;
                }
                end += 1;
            }
            // Body — text between task heading and `**Envelope:**`,
            // preserved verbatim modulo trailing whitespace.
            let body = if env_idx > i + 1 {
                lines[i + 1..env_idx].join("\n").trim().to_string()
            } else {
                String::new()
            };
            // Parse the envelope bullets. Required-tier check happens
            // after the block parse so we surface the most specific
            // error first (BadEnvelopeKey > MissingRequiredKey).
            let envelope = parse_envelope_block(&lines, env_idx + 1, end)?;
            check_required_envelope_keys(&envelope, env_idx + 1)?;
            // LM-263 / L1.3.b — promote `atomic_size_hint` and
            // `decomposition_policy` to first-class fields on
            // ParsedTask. Default to the migration-008 column defaults
            // when the bullet is absent so the round-trip stays
            // byte-stable: re-exporting a plan that omits these
            // bullets must not introduce them.
            let atomic_size_hint = match envelope.get("atomic_size_hint") {
                Some(v) => {
                    let s = v.as_str().ok_or_else(|| ImportError {
                        line: env_idx + 1,
                        column: 1,
                        kind: ImportErrorKind::BadEnvelopeValue,
                        hint: "`atomic_size_hint` must be a JSON string".into(),
                    })?;
                    if !ATOMIC_SIZE_HINT_VALUES.contains(&s) {
                        return Err(ImportError {
                            line: env_idx + 1,
                            column: 1,
                            kind: ImportErrorKind::BadAtomicSizeHint,
                            hint: format!(
                                "`atomic_size_hint` = {s:?}; allowed values are {:?}",
                                ATOMIC_SIZE_HINT_VALUES
                            ),
                        });
                    }
                    s.to_string()
                }
                None => DEFAULT_ATOMIC_SIZE_HINT.to_string(),
            };
            let decomposition_policy = match envelope.get("decomposition_policy") {
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| ImportError {
                        line: env_idx + 1,
                        column: 1,
                        kind: ImportErrorKind::BadEnvelopeValue,
                        hint: "`decomposition_policy` must be a JSON string".into(),
                    })?
                    .to_string(),
                None => DEFAULT_DECOMPOSITION_POLICY.to_string(),
            };
            if let Some((_, _, _, ref mut tasks)) = current_unit {
                tasks.push(ParsedTask {
                    idx: tasks.len() as i64,
                    title: task_title_text,
                    body,
                    ticket: task_ticket_text,
                    envelope: Some(envelope),
                    depends_on: Vec::new(),
                    atomic_size_hint,
                    decomposition_policy,
                });
            }
            i = end;
            continue;
        }
        i += 1;
    }
    if let Some((_, idx, ttl, tasks)) = current_unit.take() {
        units.push(ParsedUnit {
            idx,
            title: ttl,
            tasks,
        });
    }

    // ## Dependency Graph — required when at least one task's bullet
    // `depends_on` is non-empty, omitted otherwise (strict-format.md §6).
    // When present we parse the mermaid block, resolve every edge to a
    // task in this plan, detect cycles, and cross-validate against the
    // bullet `depends_on` values.
    let any_bullet_deps = units.iter().any(|u| {
        u.tasks.iter().any(|t| {
            t.envelope
                .as_ref()
                .and_then(|e| e.get("depends_on"))
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        })
    });
    let deps_idx = find_h2(&lines, "Dependency Graph");
    match (deps_idx, any_bullet_deps) {
        (None, true) => {
            return Err(ImportError {
                line: lines.len(),
                column: 1,
                kind: ImportErrorKind::MissingDepsGraph,
                hint: "plan has tasks with non-empty `depends_on` bullets; `## Dependency Graph` mermaid block is required".into(),
            });
        }
        (None, false) => {
            // Zero edges — section legitimately omitted. Bullet
            // depends_on (all empty) is already authoritative; nothing
            // to cross-check.
        }
        (Some(idx), _) => {
            parse_dependency_graph(&lines, idx, &mut units)?;
        }
    }

    Ok(ParsedPlan {
        title: Some(title),
        description: String::new(),
        units,
    })
}

fn find_h2(lines: &[&str], name: &str) -> Option<usize> {
    let needle = format!("## {name}");
    lines
        .iter()
        .position(|l| l.trim_end() == needle || l.trim_end().starts_with(&format!("{needle} ")))
}

/// Parse an envelope bullet block per strict-format.md §5. Lines from
/// `start` (inclusive) up to `end` (exclusive) hold the bullets, blank
/// lines, and any multi-line block scalar continuations. Returns the
/// fields in declaration order; the caller is responsible for the
/// required-tier check.
fn parse_envelope_block(
    lines: &[&str],
    start: usize,
    end: usize,
) -> Result<ParsedEnvelope, ImportError> {
    let mut fields: Vec<(String, serde_json::Value)> = Vec::new();
    let mut last_key_idx: Option<usize> = None;
    let mut i = start;
    while i < end {
        let raw = lines[i];
        // Skip blank lines between bullets.
        if raw.trim().is_empty() {
            i += 1;
            continue;
        }
        let trimmed = raw.trim_start();
        if !trimmed.starts_with("- `") {
            return Err(ImportError {
                line: i + 1,
                column: 1,
                kind: ImportErrorKind::BadEnvelopeBullet,
                hint: "envelope block must contain only `- `key`: value` bullets and blank lines"
                    .into(),
            });
        }
        // Extract the backtick-quoted key.
        let after_dash = &trimmed[3..]; // skip "- `"
        let key_end = after_dash.find('`').ok_or_else(|| ImportError {
            line: i + 1,
            column: 1,
            kind: ImportErrorKind::BadEnvelopeBullet,
            hint: "envelope bullet missing closing backtick around key".into(),
        })?;
        let key = &after_dash[..key_end];
        let after_key = after_dash[key_end + 1..].trim_start();
        if !after_key.starts_with(':') {
            return Err(ImportError {
                line: i + 1,
                column: 1,
                kind: ImportErrorKind::BadEnvelopeBullet,
                hint: format!("envelope bullet for `{key}` missing `:` after key"),
            });
        }
        let value_str = after_key[1..].trim_start();

        // Validate key membership.
        let key_idx = ENVELOPE_KEYS
            .iter()
            .position(|k| *k == key)
            .ok_or_else(|| ImportError {
                line: i + 1,
                column: 1,
                kind: ImportErrorKind::BadEnvelopeKey,
                hint: format!(
                    "unknown envelope key `{key}` — must be one of the 19 ADR-0001 fields (see strict-format.md §4)"
                ),
            })?;
        // Canonical order — strictly increasing.
        if let Some(prev) = last_key_idx {
            if key_idx <= prev {
                return Err(ImportError {
                    line: i + 1,
                    column: 1,
                    kind: ImportErrorKind::EnvelopeOrderMismatch,
                    hint: format!(
                        "envelope key `{key}` (canonical position {key_idx}) appears after position {prev} — keys must be declared in canonical order"
                    ),
                });
            }
        }
        last_key_idx = Some(key_idx);

        let (value, consumed) = parse_envelope_value(lines, i, value_str, end)?;
        fields.push((key.to_string(), value));
        i += consumed;
    }
    Ok(ParsedEnvelope { fields })
}

/// Parse a single envelope-bullet value. Returns the parsed `Value` and
/// the number of source lines consumed (1 for inline values, ≥2 for
/// multi-line block scalars). `end` bounds the block so a runaway block
/// scalar can't read past the next heading.
fn parse_envelope_value(
    lines: &[&str],
    bullet_line: usize,
    value_str: &str,
    end: usize,
) -> Result<(serde_json::Value, usize), ImportError> {
    // YAML-style block scalar: `|` on the bullet line followed by 4-space
    // indented continuation lines.
    if value_str.trim() == "|" {
        let mut buf: Vec<String> = Vec::new();
        let mut j = bullet_line + 1;
        while j < end {
            let l = lines[j];
            if l.is_empty() {
                buf.push(String::new());
                j += 1;
                continue;
            }
            if let Some(stripped) = l.strip_prefix("    ") {
                buf.push(stripped.to_string());
                j += 1;
                continue;
            }
            // Less-indented non-empty line ends the block scalar.
            break;
        }
        // Trim trailing blank lines.
        while buf.last().map(|s| s.is_empty()).unwrap_or(false) {
            buf.pop();
        }
        return Ok((
            serde_json::Value::String(buf.join("\n")),
            (j - bullet_line).max(1),
        ));
    }

    // Single-line JSON literal: string ("..."), number, bool, array, object.
    let parsed: serde_json::Value = serde_json::from_str(value_str).map_err(|e| ImportError {
        line: bullet_line + 1,
        column: 1,
        kind: ImportErrorKind::BadEnvelopeValue,
        hint: format!("envelope value not a valid JSON literal: {e}"),
    })?;
    Ok((parsed, 1))
}

/// Verify the ADR-0001 required-tier keys are all present. Surfaces the
/// first missing key — the hook strict guide reports one violation at a
/// time, so a fix-and-retry loop is well-defined.
fn check_required_envelope_keys(
    env: &ParsedEnvelope,
    block_line: usize,
) -> Result<(), ImportError> {
    for required in REQUIRED_ENVELOPE_KEYS {
        if env.get(required).is_none() {
            return Err(ImportError {
                line: block_line,
                column: 1,
                kind: ImportErrorKind::MissingRequiredKey,
                hint: format!(
                    "envelope missing required key `{required}` (ADR-0001 required tier — see strict-format.md §5)"
                ),
            });
        }
    }
    Ok(())
}

/// Parse the `## Dependency Graph` mermaid block (LM-262). Validates the
/// fence language (`mermaid`), the canonical first line (`graph LR`),
/// then walks node/edge lines. Edge endpoints resolve to tasks via
/// either the ticket key (`LM-N`) or a `TASK-…["LM-N"]` node label.
/// Detects cycles via DFS and cross-checks each task's resolved edges
/// against the bullet `depends_on` value (set-equal, order-insensitive).
fn parse_dependency_graph(
    lines: &[&str],
    section_line: usize,
    units: &mut [ParsedUnit],
) -> Result<(), ImportError> {
    // Locate the opening fence.
    let mut i = section_line + 1;
    while i < lines.len() {
        let l = lines[i].trim();
        if l.starts_with("```") {
            break;
        }
        if l.starts_with("# ") || l.starts_with("## ") {
            return Err(ImportError {
                line: section_line + 1,
                column: 1,
                kind: ImportErrorKind::BadDepsGraph,
                hint: "## Dependency Graph requires a fenced ```mermaid block".into(),
            });
        }
        i += 1;
    }
    if i >= lines.len() {
        return Err(ImportError {
            line: section_line + 1,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: "## Dependency Graph missing fenced code block".into(),
        });
    }
    let fence_line = i;
    let fence_lang = lines[i].trim().trim_start_matches('`').trim();
    if fence_lang != "mermaid" {
        return Err(ImportError {
            line: fence_line + 1,
            column: 1,
            kind: ImportErrorKind::BadDepsGraphFence,
            hint: format!("dependency graph fence must be ```mermaid, got ```{fence_lang}"),
        });
    }
    // Find the closing fence.
    let body_start = fence_line + 1;
    let mut body_end = body_start;
    while body_end < lines.len() {
        if lines[body_end].trim_start().starts_with("```") {
            break;
        }
        body_end += 1;
    }
    if body_end >= lines.len() {
        return Err(ImportError {
            line: body_start,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: "unclosed mermaid fence".into(),
        });
    }
    // First non-blank line MUST be `graph LR`.
    let mut first = body_start;
    while first < body_end && lines[first].trim().is_empty() {
        first += 1;
    }
    if first >= body_end {
        return Err(ImportError {
            line: body_start,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: "empty mermaid block; omit `## Dependency Graph` if zero edges".into(),
        });
    }
    if lines[first].trim() != "graph LR" {
        return Err(ImportError {
            line: first + 1,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: "first line of mermaid block must be `graph LR` (no `graph TD`, no `flowchart`)"
                .into(),
        });
    }

    // Walk nodes + edges.
    let mut node_to_label: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut edges: Vec<(String, String, usize)> = Vec::new();
    for (li, &raw) in lines.iter().enumerate().take(body_end).skip(first + 1) {
        if raw.trim().is_empty() {
            continue;
        }
        if let Some(caps) = MERMAID_EDGE_RE.captures(raw) {
            edges.push((caps[1].to_string(), caps[2].to_string(), li));
            continue;
        }
        if let Some(caps) = MERMAID_NODE_RE.captures(raw) {
            node_to_label.insert(caps[1].to_string(), caps[2].to_string());
            continue;
        }
        return Err(ImportError {
            line: li + 1,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: format!("unrecognized mermaid line: {:?}", raw.trim()),
        });
    }
    // Reject `graph LR` body with no edges — strict-format.md §6 says
    // omit the section in that case rather than emit an empty block.
    if edges.is_empty() {
        return Err(ImportError {
            line: first + 1,
            column: 1,
            kind: ImportErrorKind::BadDepsGraph,
            hint: "mermaid block has no edges; omit `## Dependency Graph` instead".into(),
        });
    }

    // Index tasks by ticket so refs can resolve.
    let mut ticket_to_pos: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for (ui, u) in units.iter().enumerate() {
        for (ti, t) in u.tasks.iter().enumerate() {
            if !t.ticket.is_empty() {
                ticket_to_pos.insert(t.ticket.clone(), (ui, ti));
            }
        }
    }

    // Resolve each edge endpoint to a ticket (canonical form).
    let resolve = |raw: &str, ln: usize| -> Result<String, ImportError> {
        if ticket_to_pos.contains_key(raw) {
            return Ok(raw.to_string());
        }
        if let Some(label) = node_to_label.get(raw) {
            if ticket_to_pos.contains_key(label) {
                return Ok(label.clone());
            }
        }
        Err(ImportError {
            line: ln + 1,
            column: 1,
            kind: ImportErrorKind::UnknownDependsOnRef,
            hint: format!("dependency reference `{raw}` does not match any task in this plan"),
        })
    };
    let mut resolved: Vec<(String, String, usize)> = Vec::new();
    for (from, to, ln) in &edges {
        let f = resolve(from, *ln)?;
        let t = resolve(to, *ln)?;
        resolved.push((f, t, *ln));
    }

    // Cycle detection.
    detect_dependency_cycle(&resolved)?;

    // Cross-check: bullet `depends_on` must equal the graph edges (set).
    let mut graph_deps: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (from, to, _) in &resolved {
        graph_deps.entry(from.clone()).or_default().push(to.clone());
    }
    for u in units.iter_mut() {
        for t in u.tasks.iter_mut() {
            let bullet = t
                .envelope
                .as_ref()
                .and_then(|e| e.get("depends_on"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                });
            let graph = graph_deps.get(&t.ticket).cloned().unwrap_or_default();
            match bullet {
                Some(b) => {
                    let mut bs = b.clone();
                    bs.sort();
                    let mut gs = graph.clone();
                    gs.sort();
                    if bs != gs {
                        return Err(ImportError {
                            line: section_line + 1,
                            column: 1,
                            kind: ImportErrorKind::DependsOnMismatch,
                            hint: format!(
                                "task `{}` bullet depends_on {:?} disagrees with mermaid graph {:?}",
                                t.ticket, b, graph
                            ),
                        });
                    }
                    t.depends_on = b;
                }
                None => {
                    // Bullet absent — graph is canonical for this task.
                    t.depends_on = graph;
                }
            }
        }
    }
    Ok(())
}

/// Iterative DFS cycle detector. Returns `Err(DependencyCycle)` with a
/// human-readable cycle path on the first cycle found; `Ok(())` if the
/// graph is a DAG.
fn detect_dependency_cycle(resolved: &[(String, String, usize)]) -> Result<(), ImportError> {
    use std::collections::{HashMap, HashSet};
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut nodes: HashSet<String> = HashSet::new();
    for (f, t, _) in resolved {
        adj.entry(f.clone()).or_default().push(t.clone());
        nodes.insert(f.clone());
        nodes.insert(t.clone());
    }
    // 0 = unvisited, 1 = on current path, 2 = fully processed.
    let mut color: HashMap<String, u8> = HashMap::new();
    for n in &nodes {
        color.insert(n.clone(), 0);
    }
    // Iterative DFS using an explicit stack of (node, child_idx).
    for start in &nodes {
        if *color.get(start).unwrap_or(&0) != 0 {
            continue;
        }
        let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        color.insert(start.clone(), 1);
        while let Some((node, idx)) = stack.last().cloned() {
            let kids = adj.get(&node).cloned().unwrap_or_default();
            if idx >= kids.len() {
                color.insert(node.clone(), 2);
                stack.pop();
                continue;
            }
            // Advance the parent's child cursor before recursing.
            stack.last_mut().unwrap().1 = idx + 1;
            let nxt = &kids[idx];
            match color.get(nxt).copied().unwrap_or(0) {
                0 => {
                    color.insert(nxt.clone(), 1);
                    stack.push((nxt.clone(), 0));
                }
                1 => {
                    // Back edge — cycle. Build the path slice from
                    // where `nxt` first appears in the stack.
                    let cycle_start = stack.iter().position(|(n, _)| n == nxt).unwrap_or(0);
                    let mut path: Vec<String> = stack[cycle_start..]
                        .iter()
                        .map(|(n, _)| n.clone())
                        .collect();
                    path.push(nxt.clone());
                    return Err(ImportError {
                        line: 0,
                        column: 1,
                        kind: ImportErrorKind::DependencyCycle,
                        hint: format!("cycle: {}", path.join(" → ")),
                    });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub struct ImportOptions<'a> {
    pub project_name: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub source: &'a str,
    /// Original markdown file path, when the importer reads from disk.
    /// Stored on the plan as `source_path` and used as the project-name
    /// fallback (file stem) when `project_name` is absent. `None` for
    /// content-based imports (the strict route accepts inline markdown
    /// from the plan-sync hook), in which case the parsed plan title is
    /// used as the fallback.
    pub source_path: Option<&'a str>,
    pub dry_run: bool,
}

#[derive(serde::Serialize)]
pub struct DryRunUnit {
    pub title: String,
    pub tasks: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum ImportResult {
    DryRun {
        dry_run: bool,
        project: Project,
        plan_title: String,
        unit_count: usize,
        task_count: usize,
        units: Vec<DryRunUnit>,
    },
    Created {
        project: Project,
        plan: Plan,
        units: Vec<CreatedUnit>,
        summary: Summary,
    },
}

#[derive(serde::Serialize)]
pub struct CreatedUnit {
    pub unit: Unit,
    pub tasks: Vec<Task>,
}

#[derive(serde::Serialize)]
pub struct Summary {
    pub unit_count: usize,
    pub task_count: usize,
}

pub fn import_plan_file(
    conn: &mut Connection,
    file_path: &str,
    opts: ImportOptions<'_>,
) -> Result<ImportResult> {
    let md = std::fs::read_to_string(file_path)?;
    let parsed = parse_plan_markdown(&md);
    let opts = ImportOptions {
        source_path: opts.source_path.or(Some(file_path)),
        ..opts
    };
    import_parsed_plan(conn, parsed, opts)
}

/// Persist a `ParsedPlan` (loose or strict) into the database. Shared
/// between the file-based loose importer (`import_plan_file`) and the
/// content-based strict importer (`POST /plans/import/strict`) so both
/// paths produce identical task rows when the parsed input is identical.
///
/// Strict-only side effects:
///   * `pt.depends_on` ticket strings are resolved against tasks created
///     in the same call and inserted into `task_depends_on`.
///   * `pt.envelope` is signed via `envelope::sign::sign_envelope_from_json`,
///     which validates the required tier (`Severity::Error`) before
///     delegating to `task_envelopes::sign_for_task`; that call also
///     stamps `tasks.active_envelope_id`. The `_from_json` variant
///     preserves the source file's bullet order — re-serializing from
///     `Value` would alphabetize because the default `serde_json::Map`
///     is `BTreeMap` without the `preserve_order` feature.
///
/// Loose-mode `ParsedTask` carries no envelope and no depends_on, so the
/// shared path is a no-op for those side effects.
pub fn import_parsed_plan(
    conn: &mut Connection,
    parsed: ParsedPlan,
    opts: ImportOptions<'_>,
) -> Result<ImportResult> {
    let Some(title) = parsed.title.clone() else {
        bail!("no plan title (first # heading) found");
    };

    let effective_cwd = opts.cwd.map(String::from).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    let project = if let Some(name) = opts.project_name {
        match projects::get_by_name(conn, name)? {
            Some(p) => p,
            None => projects::create(
                conn,
                projects::CreateInput {
                    name,
                    description: None,
                    cwd: Some(&effective_cwd),
                    key: None,
                },
            )?
            .ok_or_else(|| anyhow::anyhow!("project create failed"))?,
        }
    } else {
        match projects::get_by_cwd(conn, &effective_cwd, false)? {
            Some(p) => p,
            None => {
                let fallback = opts
                    .source_path
                    .and_then(|p| {
                        Path::new(p)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| title.clone())
                    .chars()
                    .take(40)
                    .collect::<String>();
                let fallback = if fallback.trim().is_empty() {
                    "plan".to_string()
                } else {
                    fallback
                };
                projects::create(
                    conn,
                    projects::CreateInput {
                        name: &fallback,
                        description: None,
                        cwd: Some(&effective_cwd),
                        key: None,
                    },
                )?
                .ok_or_else(|| anyhow::anyhow!("project create failed"))?
            }
        }
    };

    if opts.dry_run {
        let unit_count = parsed.units.len();
        let task_count: usize = parsed.units.iter().map(|u| u.tasks.len()).sum();
        let units_out: Vec<DryRunUnit> = parsed
            .units
            .iter()
            .map(|u| DryRunUnit {
                title: u.title.clone(),
                tasks: u.tasks.iter().map(|t| t.title.clone()).collect(),
            })
            .collect();
        return Ok(ImportResult::DryRun {
            dry_run: true,
            project,
            plan_title: title,
            unit_count,
            task_count,
            units: units_out,
        });
    }

    let description = if parsed.description.is_empty() {
        None
    } else {
        Some(parsed.description.as_str())
    };
    let plan = plans::create(
        conn,
        plans::CreateInput {
            project_id: &project.id,
            title: &title,
            description,
            source: Some(opts.source),
            source_path: opts.source_path,
        },
    )?
    .ok_or_else(|| anyhow::anyhow!("plan create failed"))?;
    // `plans::create` lands the row as `draft`, but `tasks::create`
    // rejects every task under a draft plan (repo/tasks.rs §57) — so
    // the import would 500 the moment we try to add the first task.
    // The user's intent in `clawket plan import` is "seed this plan
    // and its tasks", which is implicitly an approval gesture.
    let plan = plans::approve(conn, &plan.id)?
        .ok_or_else(|| anyhow::anyhow!("plan approve failed for {}", plan.id))?;

    let mut created_units: Vec<CreatedUnit> = Vec::new();
    // Ticket → newly-created task ID. Built as we create tasks so the
    // second pass (depends_on wiring) can resolve every edge endpoint
    // against the rows that just landed. Loose-mode parses leave
    // `pt.ticket` empty, so the map is only useful for strict imports.
    let mut ticket_to_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for pu in &parsed.units {
        let unit = units::create(
            conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: &pu.title,
                goal: None,
                idx: Some(pu.idx),
                execution_mode: None,
            },
        )?
        .ok_or_else(|| anyhow::anyhow!("unit create failed"))?;
        let mut created_tasks = Vec::new();
        for pt in &pu.tasks {
            let task = tasks::create(
                conn,
                tasks::CreateInput {
                    unit_id: &unit.id,
                    title: &pt.title,
                    body: Some(&pt.body),
                    assignee: None,
                    idx: Some(pt.idx),
                    // depends_on is resolved in a second pass once every
                    // task in the plan has a row — strict-mode tickets
                    // can point forward across units.
                    depends_on: Vec::new(),
                    parent_task_id: None,
                    priority: None,
                    complexity: None,
                    estimated_edits: None,
                    cycle_id: None,
                    reporter: None,
                    type_: None,
                    // Loose-mode parses default these to "small" / "auto"
                    // (see ParsedTask::default), so the strict-mode round
                    // trip (LM-264) and loose-mode imports converge on the
                    // same task row even when the source markdown omits
                    // the bullets.
                    atomic_size_hint: Some(&pt.atomic_size_hint),
                    decomposition_policy: Some(&pt.decomposition_policy),
                    tier: None,
                    qa_status: None,
                    scenario_id: None,
                    defect_task: None,
                    scenario_amendment: None,
                    evidence: None,
                    batch_id: None,
                },
            )?
            .ok_or_else(|| anyhow::anyhow!("task create failed"))?;
            if !pt.ticket.is_empty() {
                ticket_to_id.insert(pt.ticket.clone(), task.id.clone());
            }
            created_tasks.push(task);
        }
        created_units.push(CreatedUnit {
            unit,
            tasks: created_tasks,
        });
    }

    // Pass 2: resolve `depends_on` ticket strings to task IDs and insert
    // edges. parse_plan_strict has already validated that every ticket
    // resolves and that the graph is acyclic (LM-262), so we'd only fail
    // here on a programming error that left a stale ticket in
    // `pt.depends_on`. Loose-mode parses leave `pt.depends_on` empty.
    for (pu_idx, pu) in parsed.units.iter().enumerate() {
        for (pt_idx, pt) in pu.tasks.iter().enumerate() {
            if pt.depends_on.is_empty() {
                continue;
            }
            let task_id = created_units[pu_idx].tasks[pt_idx].id.clone();
            for dep_ticket in &pt.depends_on {
                let dep_id = ticket_to_id.get(dep_ticket).ok_or_else(|| {
                    anyhow::anyhow!(
                        "depends_on ticket `{}` did not resolve to a task in this plan",
                        dep_ticket
                    )
                })?;
                conn.execute(
                    "INSERT INTO task_depends_on (task_id, depends_on_task_id) VALUES (?1, ?2)",
                    rusqlite::params![task_id, dep_id],
                )?;
            }
        }
    }

    // Pass 3: sign the strict-mode envelopes so the round-trip
    // re-export reads them back via `/tasks/{id}/envelope?resolve=true`.
    // Loose-mode parses have `envelope = None`, so this is a no-op for
    // file-based imports until the loose parser gains envelope support.
    for (pu_idx, pu) in parsed.units.iter().enumerate() {
        for (pt_idx, pt) in pu.tasks.iter().enumerate() {
            let Some(env) = pt.envelope.as_ref() else {
                continue;
            };
            let task_id = created_units[pu_idx].tasks[pt_idx].id.clone();
            let json = parsed_envelope_to_json(env);
            // Build a Value mirror for the validator while persisting
                // the original key-ordered JSON. parsed_envelope_to_json
                // emits well-formed JSON by construction, so the parse
                // round-trip is infallible; the `?` covers only a
                // genuine ParsedEnvelope::Value corruption bug.
            let value: serde_json::Value = serde_json::from_str(&json)
                .map_err(|e| anyhow::anyhow!("parsed envelope json round-trip failed: {e}"))?;
            // signed_by = opts.source so strict-import-originated
            // envelopes are distinguishable from interactive signs in
            // the audit trail.
            env_sign::sign_envelope_from_json(conn, &task_id, &value, &json, opts.source)
                .map_err(|e| match e {
                    env_sign::EnvelopeSignError::Validation(violations) => {
                        let summary = violations
                            .iter()
                            .map(|v| format!("{}: {}", v.field, v.message))
                            .collect::<Vec<_>>()
                            .join("; ");
                        anyhow::anyhow!("ENVELOPE_INVALID for {}: {}", task_id, summary)
                    }
                    env_sign::EnvelopeSignError::Storage(err) => err,
                })?;
        }
    }

    let unit_count = created_units.len();
    let task_count: usize = created_units.iter().map(|u| u.tasks.len()).sum();
    Ok(ImportResult::Created {
        project,
        plan,
        units: created_units,
        summary: Summary {
            unit_count,
            task_count,
        },
    })
}

/// Serialize a `ParsedEnvelope` to a JSON object string. We build the
/// string by hand (rather than via `serde_json::Map`) because the daemon
/// pulls in `serde_json` without `preserve_order`, so a `Map` would
/// alphabetize keys and break round-trip parity for any consumer that
/// reads the stored JSON in declaration order. The render path
/// (`render_envelope_bullets`) iterates the canonical `ENVELOPE_FIELDS`
/// list regardless of map order, but the stored JSON is also surfaced
/// raw via `/tasks/{id}/envelope`, so preserving order keeps that view
/// honest.
fn parsed_envelope_to_json(env: &ParsedEnvelope) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in env.fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()));
        out.push(':');
        out.push_str(&serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
    }
    out.push('}');
    out
}

#[cfg(test)]
mod strict_tests {
    use super::*;

    // Happy-path strict plan. Envelope bullets cover the full ADR-0001
    // required tier so the post-LM-261 required-key check passes. No
    // task has a non-empty `depends_on` bullet, so the `## Dependency
    // Graph` section is legitimately omitted (LM-262 / strict-format.md §6).
    const HAPPY_PLAN: &str = r#"# v11 — Structured Task Contracts

## Meta

- id: `PLAN-NEW`
- project: `PROJ-lattice-mono`
- status: `active`

## Overview

| Unit | Title       | Tasks | todo | doing | done |
|------|-------------|------:|-----:|------:|-----:|
| 0    | Foundations |     1 |    0 |     1 |    0 |

## 표기 규약 (Envelope 19 fields per ADR-0001)

- `version`
- `intent`

## Unit 0: Foundations

**Goal**: skeleton.

### Schema migration (LM-20) — _in_progress_

**Envelope:**

- `version`: 1
- `intent`: "Add task_envelopes sidecar table"
- `target_repo`: "daemon"
- `context_refs`: [{"kind":"decision","id":"DEC-1"}]
- `success_criteria`: ["migration up/down passes"]
- `verification_cmd`: "cargo test"
- `decomposition_policy`: "atomic"
"#;

    #[test]
    fn strict_happy_path_parses_skeleton() {
        let plan = parse_plan_strict(HAPPY_PLAN).expect("happy plan must parse");
        assert_eq!(
            plan.title.as_deref(),
            Some("v11 — Structured Task Contracts")
        );
        assert_eq!(plan.units.len(), 1);
        assert_eq!(plan.units[0].title, "Foundations");
        assert_eq!(plan.units[0].tasks.len(), 1);
        assert_eq!(plan.units[0].tasks[0].title, "Schema migration");
    }

    #[test]
    fn strict_rejects_missing_h1_with_bad_heading_depth() {
        let md = "## Meta\n\n- id: `PLAN-X`\n";
        let err = parse_plan_strict(md).expect_err("missing H1 must reject");
        assert_eq!(err.kind, ImportErrorKind::BadHeadingDepth);
    }

    #[test]
    fn strict_rejects_two_h1_with_bad_heading_depth() {
        let md = "# First\n\n# Second\n\n## Meta\n";
        let err = parse_plan_strict(md).expect_err("two H1s must reject");
        assert_eq!(err.kind, ImportErrorKind::BadHeadingDepth);
        // Second H1 is at line 3 (1-indexed) of the input.
        assert_eq!(err.line, 3);
    }

    #[test]
    fn strict_rejects_missing_meta() {
        let md = "# Title\n\n## Overview\n\n| a | b |\n| - | - |\n";
        let err = parse_plan_strict(md).expect_err("missing Meta must reject");
        assert_eq!(err.kind, ImportErrorKind::MissingMeta);
    }

    #[test]
    fn strict_rejects_missing_overview() {
        let md = "# Title\n\n## Meta\n\n- id: `PLAN-X`\n";
        let err = parse_plan_strict(md).expect_err("missing Overview must reject");
        assert_eq!(err.kind, ImportErrorKind::MissingOverview);
    }

    #[test]
    fn strict_rejects_task_heading_without_envelope_block() {
        let md = r#"# T

## Meta

- id: `PLAN-X`

## Overview

| Unit | Title | Tasks | todo | doing | done |
|------|-------|------:|-----:|------:|-----:|
| 0    | U     |     1 |    0 |     1 |    0 |

## Unit 0: U

### Lonely task (LM-99) — _todo_

(no envelope block here)

## Dependency Graph

```mermaid
graph LR
```
"#;
        let err = parse_plan_strict(md).expect_err("missing envelope block must reject");
        assert_eq!(err.kind, ImportErrorKind::MissingEnvelopeBlock);
    }

    #[test]
    fn strict_rejects_missing_dependency_graph_when_bullets_declare_edges() {
        // After LM-262, `## Dependency Graph` is required only when at
        // least one task has a non-empty `depends_on` bullet. We craft a
        // single-task plan whose envelope claims a dependency but omits
        // the mermaid block — should reject with MissingDepsGraph.
        let md = r#"# T

## Meta

- id: `PLAN-X`

## Overview

| Unit | Title | Tasks | todo | doing | done |
|------|-------|------:|-----:|------:|-----:|
| 0    | U     |     1 |    0 |     1 |    0 |

## Unit 0: U

### A task (LM-1) — _todo_

**Envelope:**

- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `depends_on`: ["LM-2"]
- `decomposition_policy`: "atomic"
"#;
        let err = parse_plan_strict(md).expect_err("missing deps graph must reject");
        assert_eq!(err.kind, ImportErrorKind::MissingDepsGraph);
    }

    /// LM-261 / L1.2.b — envelope-bullet parser tests.
    /// These live in the same module as the skeleton tests so the strict
    /// fixture (HAPPY_PLAN) can be reused; the cargo test path
    /// `import_plan::strict::envelope` from the task envelope's
    /// verification_cmd resolves to `strict_tests::envelope_*`.
    mod envelope {
        use super::*;

        /// Helper — wrap a per-task bullet block in the minimal strict
        /// scaffolding so we can exercise envelope parsing without
        /// re-stating Meta / Overview each time. The plan has a single
        /// task with empty `depends_on`, so the `## Dependency Graph`
        /// section is legitimately omitted (LM-262).
        fn wrap_with_envelope(bullets: &str) -> String {
            format!(
                r#"# T

## Meta

- id: `PLAN-X`
- project: `PROJ-X`
- status: `active`

## Overview

| Unit | Title | Tasks | todo | doing | done |
|------|-------|------:|-----:|------:|-----:|
| 0    | U     |     1 |    0 |     1 |    0 |

## Unit 0: U

### Some task (LM-99) — _in_progress_

**Envelope:**

{bullets}
"#
            )
        }

        /// Full 19-field envelope must round-trip — every key recognized,
        /// canonical order accepted, no errors.
        #[test]
        fn envelope_parses_all_19_fields() {
            let bullets = r#"- `version`: 1
- `intent`: "do thing"
- `target_repo`: "daemon"
- `target_model`: "opus"
- `max_turns`: 12
- `prompt_template`: "tmpl"
- `context_refs`: [{"kind":"decision","id":"DEC-1"}]
- `scope_boundary`: "daemon/src/"
- `atomic_size_hint`: "small"
- `success_criteria`: ["a","b"]
- `verification_cmd`: "cargo test"
- `depends_on`: []
- `blocked_by`: []
- `planned_sha`: "abc123"
- `decomposition_policy`: "atomic"
- `checkpoint_interval`: 5
- `rollback_strategy`: "git revert"
- `origin`: "RL-X-01"
- `assigned_model`: "opus""#;
            let plan = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect("19-field envelope must parse");
            let env = plan.units[0].tasks[0]
                .envelope
                .as_ref()
                .expect("envelope must be populated");
            assert_eq!(env.fields.len(), 19, "all 19 fields preserved");
            // Order matches ENVELOPE_KEYS.
            for (i, (k, _)) in env.fields.iter().enumerate() {
                assert_eq!(k, ENVELOPE_KEYS[i], "field {i} mismatch");
            }
            // Spot-check value typing.
            assert_eq!(env.get("version").unwrap().as_i64(), Some(1));
            assert_eq!(env.get("intent").unwrap().as_str(), Some("do thing"));
            assert!(env.get("success_criteria").unwrap().is_array());
            assert!(env.get("context_refs").unwrap().is_array());
        }

        /// Unknown keys → `BadEnvelopeKey`. The message names the offender.
        #[test]
        fn envelope_rejects_unknown_key() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `mood`: "happy"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect_err("unknown key must reject");
            assert_eq!(err.kind, ImportErrorKind::BadEnvelopeKey);
            assert!(
                err.hint.contains("mood"),
                "hint must name the bad key: {err:?}"
            );
        }

        /// Required-tier omission → `MissingRequiredKey`. Test both
        /// `intent` (per task envelope's success_criteria) and
        /// `decomposition_policy` to confirm all 7 are checked.
        #[test]
        fn envelope_rejects_missing_required_intent() {
            let bullets = r#"- `version`: 1
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect_err("missing intent must reject");
            assert_eq!(err.kind, ImportErrorKind::MissingRequiredKey);
            assert!(
                err.hint.contains("intent"),
                "hint must name `intent`: {err:?}"
            );
        }

        #[test]
        fn envelope_rejects_missing_required_decomposition_policy() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x""#;
            let err = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect_err("missing decomposition_policy must reject");
            assert_eq!(err.kind, ImportErrorKind::MissingRequiredKey);
            assert!(err.hint.contains("decomposition_policy"));
        }

        /// Bad JSON in array value → `BadEnvelopeValue`. Unquoted bare
        /// words inside `[]` are not legal JSON.
        #[test]
        fn envelope_rejects_bad_json_in_success_criteria() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: [a, b]
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err =
                parse_plan_strict(&wrap_with_envelope(bullets)).expect_err("bad JSON must reject");
            assert_eq!(err.kind, ImportErrorKind::BadEnvelopeValue);
        }

        /// Multi-line YAML `|` block scalar — newline-preserving string.
        #[test]
        fn envelope_parses_multiline_prompt_template() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `prompt_template`: |
    line one
    line two

    line four
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let plan = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect("multi-line block scalar must parse");
            let pt = plan.units[0].tasks[0]
                .envelope
                .as_ref()
                .unwrap()
                .get("prompt_template")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(pt, "line one\nline two\n\nline four");
        }

        /// Out-of-order keys → `EnvelopeOrderMismatch`. Strict mode pins
        /// the canonical order so round-trip parity is byte-stable.
        #[test]
        fn envelope_rejects_out_of_order_keys() {
            let bullets = r#"- `intent`: "x"
- `version`: 1
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect_err("out-of-order must reject");
            assert_eq!(err.kind, ImportErrorKind::EnvelopeOrderMismatch);
            assert!(
                err.hint.contains("version"),
                "hint must name the late key: {err:?}"
            );
        }

        /// Bullet without backticks around the key → `BadEnvelopeBullet`.
        #[test]
        fn envelope_rejects_bullet_without_backticks() {
            let bullets = r#"- version: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err = parse_plan_strict(&wrap_with_envelope(bullets))
                .expect_err("missing backticks must reject");
            assert_eq!(err.kind, ImportErrorKind::BadEnvelopeBullet);
        }

        /// ENVELOPE_KEYS lockstep with cli ENVELOPE_FIELDS — count is
        /// pinned at 19 so a schema bump must update both spec, daemon,
        /// and CLI in one go.
        #[test]
        fn envelope_keys_count_locked_at_19() {
            assert_eq!(ENVELOPE_KEYS.len(), 19);
            assert_eq!(REQUIRED_ENVELOPE_KEYS.len(), 7);
        }
    }

    /// LM-262 / L1.2.c — mermaid Dependency Graph parsing tests. The
    /// envelope's verification_cmd binds these under
    /// `import_plan::strict::deps_graph`.
    mod deps_graph {
        use super::*;

        /// Two-task fixture (LM-1 → LM-2) where LM-2 depends on LM-1.
        /// `bullet_for_lm_2` lets each test customize the dependency
        /// claim while keeping the rest of the strict scaffold stable.
        fn two_task_plan(bullet_for_lm_2: &str, mermaid_body: &str) -> String {
            format!(
                r#"# Two

## Meta

- id: `PLAN-X`

## Overview

| Unit | Title | Tasks | todo | doing | done |
|------|-------|------:|-----:|------:|-----:|
| 0    | U     |     2 |    0 |     2 |    0 |

## Unit 0: U

### First (LM-1) — _todo_

**Envelope:**

- `version`: 1
- `intent`: "first"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic"

### Second (LM-2) — _todo_

**Envelope:**

- `version`: 1
- `intent`: "second"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `depends_on`: {bullet_for_lm_2}
- `decomposition_policy`: "atomic"

## Dependency Graph

```mermaid
graph LR
{mermaid_body}
```
"#
            )
        }

        #[test]
        fn deps_graph_parses_happy_edges() {
            let md = two_task_plan(r#"["LM-1"]"#, "  LM-2 --> LM-1");
            let plan = parse_plan_strict(&md).expect("happy edges must parse");
            // LM-1 has no edges; LM-2 depends on LM-1.
            let lm1 = &plan.units[0].tasks[0];
            let lm2 = &plan.units[0].tasks[1];
            assert_eq!(lm1.depends_on, Vec::<String>::new());
            assert_eq!(lm2.depends_on, vec!["LM-1".to_string()]);
        }

        #[test]
        fn deps_graph_accepts_ulid_node_label_form() {
            // Edges may use the TASK-ULID form when the node lines map
            // them to a ticket label. Strict-format.md §6 — DB-exported
            // plans use ULIDs in edges; ticket labels in node brackets.
            let md = two_task_plan(
                r#"["LM-1"]"#,
                r#"  TASK-AAAAAAAAAAAAAAAAAAAAAAAAAA["LM-1"]
  TASK-BBBBBBBBBBBBBBBBBBBBBBBBBB["LM-2"]
  TASK-BBBBBBBBBBBBBBBBBBBBBBBBBB --> TASK-AAAAAAAAAAAAAAAAAAAAAAAAAA"#,
            );
            let plan = parse_plan_strict(&md).expect("ULID-edge form must parse");
            assert_eq!(plan.units[0].tasks[1].depends_on, vec!["LM-1".to_string()]);
        }

        #[test]
        fn deps_graph_rejects_unknown_ref() {
            let md = two_task_plan(r#"[]"#, "  LM-2 --> LM-9999");
            let err = parse_plan_strict(&md).expect_err("unknown ref must reject");
            assert_eq!(err.kind, ImportErrorKind::UnknownDependsOnRef);
            assert!(
                err.hint.contains("LM-9999"),
                "hint must name the bad ref: {err:?}"
            );
        }

        #[test]
        fn deps_graph_rejects_cycle() {
            // Self-loop is the smallest cycle. We use it because the
            // bullet/graph cross-check requires consistent declaration on
            // both sides; a self-edge needs a self-claim in the bullet.
            let md = two_task_plan(r#"["LM-1", "LM-2"]"#, "  LM-2 --> LM-1\n  LM-2 --> LM-2");
            let err = parse_plan_strict(&md).expect_err("cycle must reject");
            assert_eq!(err.kind, ImportErrorKind::DependencyCycle);
            assert!(
                err.hint.contains("cycle"),
                "hint must describe the cycle: {err:?}"
            );
        }

        #[test]
        fn deps_graph_rejects_bullet_vs_graph_mismatch() {
            // Bullet claims LM-1 dependency but graph has no edge from
            // LM-2 → strict mode requires the two views to agree.
            let md = two_task_plan(r#"["LM-1"]"#, "  LM-1 --> LM-2");
            let err = parse_plan_strict(&md).expect_err("mismatch must reject");
            assert_eq!(err.kind, ImportErrorKind::DependsOnMismatch);
        }

        #[test]
        fn deps_graph_rejects_wrong_fence_language() {
            let md =
                two_task_plan(r#"["LM-1"]"#, "  LM-2 --> LM-1").replace("```mermaid", "```dot");
            let err = parse_plan_strict(&md).expect_err("non-mermaid fence must reject");
            assert_eq!(err.kind, ImportErrorKind::BadDepsGraphFence);
        }

        #[test]
        fn deps_graph_rejects_flowchart_directive() {
            let md =
                two_task_plan(r#"["LM-1"]"#, "  LM-2 --> LM-1").replace("graph LR", "flowchart LR");
            let err = parse_plan_strict(&md).expect_err("flowchart must reject");
            assert_eq!(err.kind, ImportErrorKind::BadDepsGraph);
        }

        #[test]
        fn deps_graph_omitted_when_zero_edges_is_ok() {
            // Both tasks have empty `depends_on`. Section is omitted —
            // strict-format.md §6 explicitly permits this.
            let md = two_task_plan(r#"[]"#, "  LM-2 --> LM-1");
            // Strip the entire `## Dependency Graph` block from the
            // fixture so we exercise the omission path.
            let md = md.split("## Dependency Graph").next().unwrap().to_string();
            let plan = parse_plan_strict(&md).expect("omission must parse");
            assert_eq!(plan.units[0].tasks[1].depends_on, Vec::<String>::new());
        }

        #[test]
        fn deps_graph_rejects_empty_mermaid_block() {
            // Section present but no edges → spec says omit instead.
            let md = two_task_plan(r#"[]"#, "");
            let err = parse_plan_strict(&md).expect_err("empty mermaid must reject");
            assert_eq!(err.kind, ImportErrorKind::BadDepsGraph);
        }
    }

    /// LM-263 / L1.3.b — the strict parser promotes `atomic_size_hint`
    /// and `decomposition_policy` from envelope bullets to first-class
    /// `ParsedTask` fields, validating the enum at parse time so the
    /// SQLite CHECK on `tasks.atomic_size_hint` (migration 008) never
    /// fires inside a transaction. Tests run via
    /// `cargo test import_plan::strict_tests::atomic_size_hint`.
    mod atomic_size_hint {
        use super::*;

        /// Same scaffold shape as `envelope::wrap_with_envelope`, kept
        /// local so the bullets are explicit at the call site (tests
        /// here intentionally vary `atomic_size_hint` and
        /// `decomposition_policy` independently of the rest).
        fn wrap(bullets: &str) -> String {
            format!(
                r#"# T

## Meta

- id: `PLAN-X`

## Overview

| Unit | Title | Tasks | todo | doing | done |
|------|-------|------:|-----:|------:|-----:|
| 0    | U     |     1 |    0 |     1 |    0 |

## Unit 0: U

### Task (LM-1) — _todo_

**Envelope:**

{bullets}
"#
            )
        }

        /// Explicit bullets land on `ParsedTask` verbatim so a
        /// downstream INSERT writes the spec-declared values, not the
        /// migration defaults.
        #[test]
        fn explicit_hint_and_policy_are_persisted_on_parsed_task() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `atomic_size_hint`: "medium"
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "tree(max_depth=3)""#;
            let plan = parse_plan_strict(&wrap(bullets)).expect("explicit hint/policy must parse");
            let t = &plan.units[0].tasks[0];
            assert_eq!(t.atomic_size_hint, "medium");
            assert_eq!(t.decomposition_policy, "tree(max_depth=3)");
        }

        /// Required-tier `decomposition_policy` is always present, but
        /// `atomic_size_hint` is optional. Omitting it must default to
        /// the migration-008 column default (`"small"`) rather than
        /// being left blank, which would fail the SQLite CHECK.
        #[test]
        fn missing_atomic_size_hint_defaults_to_small() {
            // Note: `atomic_size_hint` is intentionally absent. All
            // seven required-tier keys are still present.
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let plan =
                parse_plan_strict(&wrap(bullets)).expect("missing atomic_size_hint must default");
            let t = &plan.units[0].tasks[0];
            assert_eq!(t.atomic_size_hint, DEFAULT_ATOMIC_SIZE_HINT);
            assert_eq!(t.atomic_size_hint, "small");
            assert_eq!(t.decomposition_policy, "atomic");
        }

        /// Out-of-enum value rejects with `BadAtomicSizeHint` and the
        /// hint surfaces both the offending value and the allowed set
        /// so the user can fix-and-retry without reading the spec.
        #[test]
        fn invalid_atomic_size_hint_rejects() {
            let bullets = r#"- `version`: 1
- `intent`: "x"
- `target_repo`: "daemon"
- `context_refs`: []
- `atomic_size_hint`: "gigantic"
- `success_criteria`: []
- `verification_cmd`: "x"
- `decomposition_policy`: "atomic""#;
            let err = parse_plan_strict(&wrap(bullets)).expect_err("non-enum value must reject");
            assert_eq!(err.kind, ImportErrorKind::BadAtomicSizeHint);
            assert!(
                err.hint.contains("gigantic"),
                "hint must name the bad value: {err:?}"
            );
            assert!(
                err.hint.contains("small"),
                "hint must list allowed values: {err:?}"
            );
        }
    }
}
