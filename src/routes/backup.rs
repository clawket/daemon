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

    let value = serde_json::to_value(&result)
        .map_err(|e| crate::routes::error::ApiError::internal(e.to_string()))?;
    Ok(Json(value))
}
