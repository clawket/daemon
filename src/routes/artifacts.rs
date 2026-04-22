use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::embeddings;
use crate::models::{Artifact, ArtifactVersion};
use crate::repo::artifacts;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::norm_opt;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/artifacts", get(list).post(create))
        .route("/artifacts/search", get(search))
        .route(
            "/artifacts/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route(
            "/artifacts/{id}/versions",
            get(list_versions).post(snapshot_version),
        )
}

#[derive(Deserialize)]
struct ListQuery {
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    scope: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Artifact>>> {
    Ok(Json(artifacts::list(
        &app.conn(),
        artifacts::ListFilter {
            task_id: q.task_id.as_deref(),
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            type_: q.type_.as_deref(),
            scope: q.scope.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    #[serde(rename = "type")]
    type_: String,
    title: String,
    content: Option<String>,
    content_format: Option<String>,
    parent_id: Option<String>,
    scope: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Artifact>> {
    let task_id = norm_opt(body.task_id);
    let unit_id = norm_opt(body.unit_id);
    let plan_id = norm_opt(body.plan_id);
    let content = norm_opt(body.content);
    let content_format = norm_opt(body.content_format);
    let parent_id = norm_opt(body.parent_id);
    let scope = norm_opt(body.scope);
    let created = artifacts::create(
        &app.conn(),
        artifacts::CreateInput {
            task_id: task_id.as_deref(),
            unit_id: unit_id.as_deref(),
            plan_id: plan_id.as_deref(),
            type_: &body.type_,
            title: &body.title,
            content: content.as_deref(),
            content_format: content_format.as_deref(),
            parent_id: parent_id.as_deref(),
            scope: scope.as_deref(),
        },
    )?;
    if let Some(a) = &created {
        schedule_embed(app.clone(), a);
    }
    json_or_404(created)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Artifact>> {
    json_or_404(artifacts::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    artifacts::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize)]
struct UpdateBody {
    title: Option<String>,
    content: Option<String>,
    content_format: Option<String>,
    scope: Option<String>,
    created_by: Option<String>,
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<Artifact>> {
    let content_touched = body.content.is_some();
    let scope_touched = body.scope.is_some();
    let f = artifacts::UpdateFields {
        title: norm_opt(body.title),
        content: norm_opt(body.content),
        content_format: norm_opt(body.content_format),
        scope: norm_opt(body.scope),
        created_by: norm_opt(body.created_by),
    };
    let updated = artifacts::update(&app.conn(), &id, f)?
        .ok_or_else(|| ApiError::not_found("artifact not found"))?;
    if updated.scope == "rag" && (content_touched || scope_touched) {
        schedule_embed(app.clone(), &updated);
    }
    Ok(Json(updated))
}

async fn list_versions(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ArtifactVersion>>> {
    Ok(Json(artifacts::list_versions(&app.conn(), &id)?))
}

#[derive(Deserialize, Default)]
struct SnapshotBody {
    created_by: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
    scope: Option<String>,
    mode: Option<String>,
}

#[derive(Serialize)]
struct ArtifactHit {
    #[serde(flatten)]
    artifact: Artifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    _distance: Option<f32>,
}

async fn search(
    State(app): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<ArtifactHit>>> {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let scope = q.scope.as_deref().or(Some("rag"));
    let mode = q.mode.as_deref().unwrap_or("hybrid");

    if mode == "semantic" || mode == "hybrid" {
        if let Ok(Some(vec)) = embeddings::embed(&query).await {
            let vec_hits = artifacts::vector_search(&app.conn(), &vec, limit, scope)?;
            if mode == "semantic" {
                return Ok(Json(
                    vec_hits
                        .into_iter()
                        .map(|(a, d)| ArtifactHit {
                            artifact: a,
                            _distance: Some(d),
                        })
                        .collect(),
                ));
            }
            let fts = artifacts::keyword_search(&app.conn(), &query, limit, scope)?;
            let mut seen = std::collections::HashSet::new();
            let mut merged: Vec<ArtifactHit> = Vec::new();
            for a in fts {
                if seen.insert(a.id.clone()) {
                    merged.push(ArtifactHit {
                        artifact: a,
                        _distance: None,
                    });
                }
            }
            for (a, d) in vec_hits {
                if seen.insert(a.id.clone()) {
                    merged.push(ArtifactHit {
                        artifact: a,
                        _distance: Some(d),
                    });
                }
            }
            merged.truncate(limit as usize);
            return Ok(Json(merged));
        }
    }

    let results = artifacts::keyword_search(&app.conn(), &query, limit, scope)?;
    Ok(Json(
        results
            .into_iter()
            .map(|a| ArtifactHit {
                artifact: a,
                _distance: None,
            })
            .collect(),
    ))
}

// Fire-and-forget embed for rag-scoped artifacts. Node v2.2.1 runs this as a
// detached Promise so the HTTP response returns before the embedding completes;
// we mirror that by spawning a tokio task that re-acquires the connection lock
// when it finishes.
fn schedule_embed(app: AppState, art: &Artifact) {
    if art.scope != "rag" || art.content.is_empty() {
        return;
    }
    let id = art.id.clone();
    let source = format!("{}\n{}", art.title, art.content);
    tokio::spawn(async move {
        match embeddings::embed(&source).await {
            Ok(Some(vec)) => {
                let _ = artifacts::store_embedding(&app.conn(), &id, &vec);
            }
            _ => {}
        }
    });
}

async fn snapshot_version(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SnapshotBody>>,
) -> ApiResult<Json<ArtifactVersion>> {
    let by = body.and_then(|b| b.0.created_by);
    let version = artifacts::snapshot_current(&app.conn(), &id, by.as_deref())?;
    if version.is_some() {
        if let Some(art) = artifacts::get(&app.conn(), &id)? {
            schedule_embed(app.clone(), &art);
        }
    }
    json_or_404(version)
}
