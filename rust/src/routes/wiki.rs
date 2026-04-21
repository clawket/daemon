use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::repo::projects;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/wiki/files", get(list_files))
        .route("/wiki/file", get(get_file))
}

#[derive(Deserialize)]
struct ListQuery {
    cwd: Option<String>,
    project_id: Option<String>,
}

#[derive(Serialize)]
struct WikiFile {
    path: String,
    name: String,
    title: String,
    size: u64,
    modified_at: u128,
    wiki_root: String,
}

const MAX_SIZE: u64 = 512 * 1024;
const MAX_DEPTH: usize = 3;

fn is_md(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx")
}

fn strip_ext(name: &str) -> String {
    match name.rfind('.') {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

fn extract_title(path: &Path, fallback: &str) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return fallback.to_string();
    };
    let head: String = String::from_utf8_lossy(&bytes)
        .chars()
        .take(500)
        .collect();
    for line in head.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    fallback.to_string()
}

fn modified_ms(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn scan_dir(
    dir: &Path,
    cwd: &Path,
    wiki_root: &str,
    depth: usize,
    out: &mut Vec<WikiFile>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
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
            scan_dir(&full, cwd, wiki_root, depth + 1, out);
        } else if is_md(&name) && meta.len() < MAX_SIZE {
            let fallback = strip_ext(&name);
            let title = extract_title(&full, &fallback);
            let rel = full
                .strip_prefix(cwd)
                .unwrap_or(&full)
                .to_string_lossy()
                .into_owned();
            out.push(WikiFile {
                path: rel,
                name: fallback,
                title,
                size: meta.len(),
                modified_at: modified_ms(&meta),
                wiki_root: wiki_root.to_string(),
            });
        }
    }
}

async fn list_files(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<WikiFile>>> {
    let cwd_str = q.cwd.unwrap_or_default();
    if cwd_str.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let cwd = PathBuf::from(&cwd_str);
    if !cwd.exists() {
        return Ok(Json(Vec::new()));
    }

    let mut results: Vec<WikiFile> = Vec::new();

    let wiki_paths: Vec<String> = if let Some(pid) = q.project_id.as_deref() {
        let conn = app.conn();
        match projects::get(&conn, pid)? {
            Some(p) if !p.wiki_paths.is_empty() => p.wiki_paths,
            _ => vec!["docs".to_string()],
        }
    } else {
        vec!["docs".to_string()]
    };

    for wp in &wiki_paths {
        let wiki_dir = if Path::new(wp).is_absolute() {
            PathBuf::from(wp)
        } else {
            cwd.join(wp)
        };
        if wiki_dir.exists() {
            scan_dir(&wiki_dir, &cwd, wp, 0, &mut results);
        }
    }

    // Root-level .md files (README, CHANGELOG, etc.)
    if let Ok(entries) = std::fs::read_dir(&cwd) {
        for entry in entries.flatten() {
            let name = match entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !is_md(&name) {
                continue;
            }
            let full = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() >= MAX_SIZE {
                continue;
            }
            let fallback = strip_ext(&name);
            let title = extract_title(&full, &fallback);
            results.push(WikiFile {
                path: name,
                name: fallback,
                title,
                size: meta.len(),
                modified_at: modified_ms(&meta),
                wiki_root: ".".to_string(),
            });
        }
    }

    Ok(Json(results))
}

#[derive(Deserialize)]
struct FileQuery {
    cwd: Option<String>,
    path: Option<String>,
    project_id: Option<String>,
}

async fn get_file(
    State(app): State<AppState>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cwd = q.cwd.unwrap_or_default();
    let rel_path = q.path.unwrap_or_default();
    if cwd.is_empty() || rel_path.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cwd and path required" })),
        ));
    }
    let cwd_path = PathBuf::from(&cwd);
    let full = cwd_path.join(&rel_path);

    let wiki_paths: Vec<String> = if let Some(pid) = q.project_id.as_deref() {
        let conn = app.conn();
        match projects::get(&conn, pid) {
            Ok(Some(p)) if !p.wiki_paths.is_empty() => p.wiki_paths,
            _ => vec!["docs".to_string()],
        }
    } else {
        vec!["docs".to_string()]
    };

    let mut allowed_roots: Vec<PathBuf> = vec![cwd_path.clone()];
    for wp in &wiki_paths {
        let root = if Path::new(wp).is_absolute() {
            PathBuf::from(wp)
        } else {
            cwd_path.join(wp)
        };
        allowed_roots.push(root);
    }

    let resolved = match full.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "file not found" })),
            ));
        }
    };

    let is_allowed = allowed_roots.iter().any(|root| {
        root.canonicalize()
            .map(|r| resolved.starts_with(&r))
            .unwrap_or(false)
    });
    if !is_allowed {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "path outside allowed wiki roots" })),
        ));
    }
    if !resolved.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file not found" })),
        ));
    }

    match std::fs::read_to_string(&resolved) {
        Ok(content) => {
            let meta = match std::fs::metadata(&resolved) {
                Ok(m) => m,
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    ));
                }
            };
            let name = strip_ext(
                Path::new(&rel_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&rel_path),
            );
            Ok(Json(json!({
                "path": rel_path,
                "name": name,
                "content": content,
                "content_format": "markdown",
                "size": meta.len(),
                "modified_at": modified_ms(&meta),
            })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}
