use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::import_plan::{
    import_parsed_plan, import_plan_file, parse_plan_strict, ImportOptions, ImportResult,
};
use crate::repo::{knowledge, projects};
use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans/import", post(plans_import))
        .route("/plans/import/strict", post(plans_import_strict))
        .route("/knowledge/import", post(knowledge_import))
        .route("/knowledge/export", post(knowledge_export))
}

#[derive(Deserialize)]
struct PlanImportBody {
    file: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default, rename = "dryRun")]
    dry_run: bool,
}

async fn plans_import(
    State(app): State<AppState>,
    Json(body): Json<PlanImportBody>,
) -> ApiResult<Json<ImportResult>> {
    let source = body.source.as_deref().unwrap_or("import");
    let mut conn = app.conn();
    let result = import_plan_file(
        &mut conn,
        &body.file,
        ImportOptions {
            project_name: body.project.as_deref(),
            cwd: body.cwd.as_deref(),
            source,
            source_path: None,
            dry_run: body.dry_run,
        },
    )?;
    Ok(Json(result))
}

// LM-260 / L1.1.a — strict-mode import gate.
//
// The hook (`adapters/claude/plan-sync.cjs`) shells through this route to
// validate Plan Mode markdown. On 400 it surfaces line/column/kind/hint to
// the user; on 200 with `dry_run=true` it follows up with a real import.
//
// Why a separate route: `/plans/import` reads from a file path on disk.
// The hook receives markdown inline (or in `~/.claude/plans/`) and we do not
// want every hook invocation to round-trip through the filesystem.

#[derive(Deserialize)]
struct StrictImportBody {
    content: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "dryRun")]
    dry_run: bool,
}

async fn plans_import_strict(
    State(app): State<AppState>,
    Json(body): Json<StrictImportBody>,
) -> ApiResult<Json<ImportResult>> {
    if body.content.trim().is_empty() {
        return Err(ApiError::bad_request("content required"));
    }
    let parsed = match parse_plan_strict(&body.content) {
        Ok(p) => p,
        Err(e) => {
            // Surface as 400 with structured body so the hook can format
            // a helpful message. The route never panics on parse error.
            return Err(ApiError::bad_request_with_details(
                "strict_format_violation",
                json!({
                    "line": e.line,
                    "column": e.column,
                    "kind": format!("{:?}", e.kind),
                    "hint": e.hint,
                }),
            ));
        }
    };

    // LM-264 / L1.2.d — wire the strict path through the same
    // persistence as `/plans/import`. `source` is "strict-import" so
    // the audit trail distinguishes hook-driven imports from the
    // legacy file-based flow; `source_path` is `None` because the
    // payload arrived inline.
    let mut conn = app.conn();
    let result = import_parsed_plan(
        &mut conn,
        parsed,
        ImportOptions {
            project_name: body.project.as_deref(),
            cwd: body.cwd.as_deref(),
            source: "strict-import",
            source_path: None,
            dry_run: body.dry_run,
        },
    )?;
    Ok(Json(result))
}

const MD_MAX_SIZE: u64 = 512 * 1024;
const MD_MAX_DEPTH: usize = 3;

fn is_md(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx")
}

fn ext_of(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".mdx") {
        "mdx"
    } else {
        "md"
    }
}

fn strip_ext(name: &str) -> String {
    match name.rfind('.') {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

fn extract_title(content: &str, fallback: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    fallback.to_string()
}

#[derive(Deserialize)]
struct KnowledgeImportBody {
    cwd: String,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    unit_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
struct KnowledgeImportItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    path: String,
    title: String,
}

#[derive(Serialize)]
struct SkippedItem {
    path: String,
    title: String,
    reason: String,
}

#[derive(Serialize)]
struct KnowledgeImportResult {
    imported: usize,
    skipped: usize,
    items: Vec<KnowledgeImportItem>,
    #[serde(rename = "skippedItems")]
    skipped_items: Vec<SkippedItem>,
    dry_run: bool,
}

struct ImportScanCtx<'a> {
    cwd: &'a Path,
    existing: &'a mut HashSet<String>,
    imported: &'a mut Vec<KnowledgeImportItem>,
    skipped: &'a mut Vec<SkippedItem>,
    plan_id: Option<&'a str>,
    unit_id: Option<&'a str>,
    dry_run: bool,
}

fn scan_import_dir(
    conn: &rusqlite::Connection,
    dir: &Path,
    depth: usize,
    ctx: &mut ImportScanCtx<'_>,
) -> ApiResult<()> {
    if depth > MD_MAX_DEPTH {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let full = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            scan_import_dir(conn, &full, depth + 1, ctx)?;
            continue;
        }
        if !is_md(&name) || meta.len() >= MD_MAX_SIZE {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&full) else {
            continue;
        };
        let fallback = strip_ext(&name);
        let title = extract_title(&content, &fallback);
        let rel = full
            .strip_prefix(ctx.cwd)
            .unwrap_or(&full)
            .to_string_lossy()
            .into_owned();

        if ctx.existing.contains(&title) {
            ctx.skipped.push(SkippedItem {
                path: rel,
                title,
                reason: "duplicate".into(),
            });
            continue;
        }

        if ctx.dry_run {
            ctx.imported.push(KnowledgeImportItem {
                id: None,
                path: rel,
                title: title.clone(),
            });
        } else {
            let fmt = ext_of(&name);
            let art = knowledge::create(
                conn,
                knowledge::CreateInput {
                    task_id: None,
                    unit_id: ctx.unit_id,
                    plan_id: ctx.plan_id,
                    type_: "document",
                    title: &title,
                    content: Some(&content),
                    content_format: Some(fmt),
                    parent_id: None,
                },
            )?;
            if let Some(a) = art {
                ctx.imported.push(KnowledgeImportItem {
                    id: Some(a.id),
                    path: rel,
                    title: title.clone(),
                });
            }
        }
        ctx.existing.insert(title);
    }
    Ok(())
}

async fn knowledge_import(
    State(app): State<AppState>,
    Json(body): Json<KnowledgeImportBody>,
) -> ApiResult<Json<KnowledgeImportResult>> {
    let cwd = PathBuf::from(&body.cwd);
    if body.cwd.is_empty() || !cwd.exists() {
        return Err(ApiError::bad_request("cwd required"));
    }
    let conn = app.conn();

    let mut existing: HashSet<String> = knowledge::list(
        &conn,
        knowledge::ListFilter {
            plan_id: body.plan_id.as_deref(),
            unit_id: body.unit_id.as_deref(),
            ..Default::default()
        },
    )?
    .into_iter()
    .map(|a| a.title)
    .collect();

    let mut imported: Vec<KnowledgeImportItem> = Vec::new();
    let mut skipped: Vec<SkippedItem> = Vec::new();

    let wiki_paths: Vec<String> = if let Some(pid) = body.project_id.as_deref() {
        match projects::get(&conn, pid)? {
            Some(p) if !p.wiki_paths.is_empty() => p.wiki_paths,
            _ => vec!["docs".into()],
        }
    } else {
        vec!["docs".into()]
    };

    for wp in &wiki_paths {
        let wiki_dir = if Path::new(wp).is_absolute() {
            PathBuf::from(wp)
        } else {
            cwd.join(wp)
        };
        if !wiki_dir.exists() {
            continue;
        }
        let mut ctx = ImportScanCtx {
            cwd: &cwd,
            existing: &mut existing,
            imported: &mut imported,
            skipped: &mut skipped,
            plan_id: body.plan_id.as_deref(),
            unit_id: body.unit_id.as_deref(),
            dry_run: body.dry_run,
        };
        scan_import_dir(&conn, &wiki_dir, 0, &mut ctx)?;
    }

    Ok(Json(KnowledgeImportResult {
        imported: imported.len(),
        skipped: skipped.len(),
        items: imported,
        skipped_items: skipped,
        dry_run: body.dry_run,
    }))
}

#[derive(Deserialize)]
struct KnowledgeExportBody {
    cwd: String,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    unit_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == ' ' || ('가'..='힣').contains(&ch) {
            out.push(ch);
        }
    }
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join("-");
    collapsed.to_ascii_lowercase()
}

async fn knowledge_export(
    State(app): State<AppState>,
    Json(body): Json<KnowledgeExportBody>,
) -> ApiResult<Json<Value>> {
    if body.cwd.is_empty() {
        return Err(ApiError::bad_request("cwd required"));
    }
    let cwd = PathBuf::from(&body.cwd);
    let conn = app.conn();

    let export_path: String = if let Some(pid) = body.project_id.as_deref() {
        match projects::get(&conn, pid)? {
            Some(p) => p
                .wiki_paths
                .first()
                .cloned()
                .unwrap_or_else(|| "docs".into()),
            None => "docs".into(),
        }
    } else {
        "docs".into()
    };
    let docs_dir = if Path::new(&export_path).is_absolute() {
        PathBuf::from(&export_path)
    } else {
        cwd.join(&export_path)
    };
    std::fs::create_dir_all(&docs_dir)
        .map_err(|e| ApiError::internal(format!("create docs dir: {e}")))?;

    let list = knowledge::list(
        &conn,
        knowledge::ListFilter {
            plan_id: body.plan_id.as_deref(),
            unit_id: body.unit_id.as_deref(),
            ..Default::default()
        },
    )?;

    let mut items: Vec<Value> = Vec::new();
    for art in list {
        if art.content.is_empty() {
            continue;
        }
        let slug = slugify(&art.title);
        let ext = match art.content_format.as_str() {
            "json" => ".json",
            "yaml" => ".yaml",
            _ => ".md",
        };
        let file_path = docs_dir.join(format!("{slug}{ext}"));
        if let Err(e) = std::fs::write(&file_path, &art.content) {
            return Err(ApiError::internal(format!("write: {e}")));
        }
        let rel = file_path
            .strip_prefix(&cwd)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .into_owned();
        items.push(json!({
            "id": art.id,
            "title": art.title,
            "path": rel,
        }));
    }

    Ok(Json(json!({
        "exported": items.len(),
        "items": items,
    })))
}
