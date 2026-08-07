//! `POST /backup` and `POST /restore`.
//!
//! The CLI has called these two paths since v3.0.0 (`cli/src/main.rs`, the
//! `Backup` / `Restore` arms); the daemon never served them, so both commands
//! returned 404. The request bodies here are the ones the CLI already sends and
//! are not open to change.
//!
//! Archive format, restore atomicity and the merge refusal are documented in
//! `repo::backup`.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::id::now_ms;
use crate::repo::backup;
use crate::routes::error::ApiResult;
use crate::routes::util::resolve_project_ref_opt;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/backup", post(create_backup))
        .route("/restore", post(restore_backup))
}

#[derive(Deserialize)]
struct BackupBody {
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

async fn create_backup(
    State(app): State<AppState>,
    Json(body): Json<BackupBody>,
) -> ApiResult<Json<Value>> {
    let now = now_ms();

    // `--project` is validated so a typo fails loudly instead of being recorded
    // as a meaningless header value. It does NOT subset the payload: the archive
    // is a whole-database snapshot. A per-project export would have to walk the
    // FK closure of ~30 tables and emit rows rather than a database file, which
    // is a different artifact with a different restore path — not something to
    // smuggle in behind the same flag. The resolved id is carried in the header
    // so `restore` can report what the archive was taken for.
    let project_id = {
        let conn = app.conn();
        resolve_project_ref_opt(&conn, body.project_id.as_deref())?
    };

    let output = backup::resolve_output_path(body.output.as_deref(), now)?;
    let db_path = app.paths().db.clone();
    let schema_version = app.schema_version();

    // VACUUM INTO + gzip of a multi-MB database is blocking work; keep it off
    // the async runtime's worker threads.
    let result = tokio::task::spawn_blocking(move || {
        backup::backup(
            &db_path,
            &output,
            project_id.as_deref(),
            schema_version,
            now,
        )
    })
    .await
    .map_err(|e| crate::routes::error::ApiError::internal(format!("backup task join: {e}")))??;

    let mut value = serde_json::to_value(&result)
        .map_err(|e| crate::routes::error::ApiError::internal(e.to_string()))?;
    if let Value::Object(ref mut map) = value {
        // The payload is always the whole store; say so rather than letting the
        // presence of `project_id` imply a subset.
        map.insert("scope".into(), json!("full-database"));
        // A CLI at v0.6.1 or older documents --project as "back up this project
        // only". It never reached a daemon (/backup did not exist), so no
        // working behaviour changes here — but a user reading that help would
        // take this archive for a single-project export and restore it
        // expecting the rest of their data to survive. `scope` alone is easy to
        // miss in a JSON blob, so when the flag is actually passed, say it.
        if let Some(pid) = map.get("project_id").and_then(Value::as_str) {
            map.insert(
                "note".into(),
                json!(format!(
                    "project_id {pid} is recorded in the archive header for provenance, but the \
                     archive holds the whole database — a per-project export is not implemented. \
                     Restoring this replaces every project, not just {pid}."
                )),
            );
        }
    }
    Ok(Json(value))
}

#[derive(Deserialize)]
struct RestoreBody {
    input: String,
    #[serde(default)]
    merge: bool,
    #[serde(default)]
    dry_run: bool,
}

async fn restore_backup(
    State(app): State<AppState>,
    Json(body): Json<RestoreBody>,
) -> ApiResult<Json<Value>> {
    let input = backup::resolve_input_path(&body.input)?;
    let db_path = app.paths().db.clone();
    let current_schema_version = app.schema_version();
    let merge = body.merge;
    let dry_run = body.dry_run;

    let result = tokio::task::spawn_blocking(move || {
        backup::restore(&db_path, &input, merge, dry_run, current_schema_version)
    })
    .await
    .map_err(|e| crate::routes::error::ApiError::internal(format!("restore task join: {e}")))??;

    // The file under the pool has been replaced. Every pooled connection still
    // points at the old inode, so a write from here on commits into a file
    // nothing will reopen — acknowledged, then gone. Latch the state so those
    // writes are refused with RESTART_REQUIRED instead of being lost silently.
    // Only on a real swap: a dry run changed nothing.
    if result.restored {
        app.mark_restore_pending();
    }

    let value = serde_json::to_value(&result)
        .map_err(|e| crate::routes::error::ApiError::internal(e.to_string()))?;
    Ok(Json(value))
}
