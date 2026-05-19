// Routes for the knowledge surface.
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::embeddings;
use crate::models::{Knowledge, KnowledgeVersion};
use crate::repo::knowledge;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::norm_opt;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/knowledge", get(list).post(create))
        .route("/knowledge/search", get(search))
        .route(
            "/knowledge/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route(
            "/knowledge/{id}/versions",
            get(list_versions).post(snapshot_version),
        )
        .route("/wiki/tree", get(wiki_tree))
}

#[derive(Deserialize)]
struct ListQuery {
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    /// Filter by project — accepts `project` (canonical) or `project_id` (legacy alias).
    project: Option<String>,
    project_id: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Knowledge>>> {
    let mut results = knowledge::list(
        &app.conn(),
        knowledge::ListFilter {
            task_id: q.task_id.as_deref(),
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            type_: q.type_.as_deref(),
        },
    )?;

    let project_filter = q.project.as_deref().or(q.project_id.as_deref());
    if let Some(pid) = project_filter {
        results.retain(|a| project_id_matches(a, Some(pid), &app));
    }

    Ok(Json(results))
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
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Knowledge>> {
    let task_id = norm_opt(body.task_id);
    let unit_id = norm_opt(body.unit_id);
    let plan_id = norm_opt(body.plan_id);
    let content = norm_opt(body.content);
    let content_format = norm_opt(body.content_format);
    let parent_id = norm_opt(body.parent_id);

    let created = knowledge::create(
        &app.conn(),
        knowledge::CreateInput {
            task_id: task_id.as_deref(),
            unit_id: unit_id.as_deref(),
            plan_id: plan_id.as_deref(),
            type_: &body.type_,
            title: &body.title,
            content: content.as_deref(),
            content_format: content_format.as_deref(),
            parent_id: parent_id.as_deref(),
        },
    )?;
    // Synchronous embed with rollback semantics. The embedding INSERT must
    // succeed atomically with knowledge creation. If the embedder fails,
    // hard-delete the row so the caller observes a clean failure
    // (read-your-write consistency for /search).
    if let Some(a) = &created {
        if !a.content.is_empty() {
            match embed_knowledge_atomic(app.clone(), a).await {
                Ok(_) => {}
                Err(e) => {
                    let _ = knowledge::delete(&app.conn(), &a.id);
                    return Err(ApiError::internal(format!(
                        "EMBED_FAILED: knowledge rolled back: {}",
                        e
                    )));
                }
            }
        }
        app.emit("knowledge:created", serde_json::json!({ "id": a.id }));
    }
    json_or_404(created)
}

async fn get_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Knowledge>> {
    json_or_404(knowledge::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let art = knowledge::get(&app.conn(), &id)?;
    match art {
        Some(_) => {
            knowledge::soft_delete(&app.conn(), &id)?;
            app.emit("knowledge:deleted", serde_json::json!({ "id": id }));
            Ok(Json(
                serde_json::json!({ "ok": true, "deleted": id, "soft": true }),
            ))
        }
        None => Err(ApiError::not_found("knowledge not found")),
    }
}

#[derive(Deserialize)]
struct UpdateBody {
    title: Option<String>,
    content: Option<String>,
    content_format: Option<String>,
    created_by: Option<String>,
    wiki_idx: Option<i64>,
    /// Explicit parent reparenting; JSON `null` clears the parent, a string
    /// sets it, missing key leaves it unchanged.
    parent_id: Option<serde_json::Value>,
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<Knowledge>> {
    let title_changed = body.title.is_some();
    let content_touched = body.content.is_some();
    let parent_id_field: Option<Option<String>> = match body.parent_id {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(s)) => Some(Some(s)),
        _ => None,
    };
    let f = knowledge::UpdateFields {
        title: norm_opt(body.title),
        content: norm_opt(body.content),
        content_format: norm_opt(body.content_format),
        created_by: norm_opt(body.created_by),
        parent_id: parent_id_field,
    };
    let updated = knowledge::update(&app.conn(), &id, f)?
        .ok_or_else(|| ApiError::not_found("knowledge not found"))?;

    // Re-embed when title or content changes (synchronous).
    if content_touched || title_changed {
        embed_knowledge_sync(app.clone(), &updated).await;
    }

    // Update wiki_idx if provided.
    if let Some(idx) = body.wiki_idx {
        app.conn()
            .execute(
                "UPDATE knowledge SET wiki_idx = ?1 WHERE id = ?2",
                rusqlite::params![idx, id],
            )
            .ok();
    }

    app.emit("knowledge:updated", serde_json::json!({ "id": updated.id }));
    Ok(Json(updated))
}

async fn list_versions(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<KnowledgeVersion>>> {
    Ok(Json(knowledge::list_versions(&app.conn(), &id)?))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
    mode: Option<String>,
    project_id: Option<String>,
}

/// Search hit with three score fields plus optional truncation flag.
#[derive(Serialize)]
struct KnowledgeHit {
    #[serde(flatten)]
    kn: Knowledge,
    #[serde(skip_serializing_if = "Option::is_none")]
    bm25_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hybrid_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
}

#[derive(Serialize)]
struct SearchResponse {
    hits: Vec<KnowledgeHit>,
    total_returned: usize,
    limit: i64,
    truncated: bool,
}

async fn search(
    State(app): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<SearchResponse>> {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let mode = q.mode.as_deref().unwrap_or("hybrid");
    let project_id = q.project_id.as_deref();

    // Fetch extra results when a project filter narrows the set post-hoc.
    let fetch_limit = if project_id.is_some() {
        limit * 3
    } else {
        limit
    };

    if mode == "semantic" || mode == "hybrid" {
        if let Ok(Some(vec)) = embeddings::embed(&query).await {
            let vec_hits = knowledge::vector_search(&app.conn(), &vec, fetch_limit)?;
            if mode == "semantic" {
                let hits: Vec<KnowledgeHit> = vec_hits
                    .into_iter()
                    .filter(|(a, _)| project_id_matches(a, project_id, &app))
                    .take(limit as usize)
                    .map(|(a, d)| KnowledgeHit {
                        kn: a,
                        bm25_score: None,
                        vector_score: Some(d),
                        hybrid_score: None,
                        truncated: None,
                    })
                    .collect();
                let total = hits.len();
                return Ok(Json(SearchResponse {
                    truncated: false,
                    total_returned: total,
                    limit,
                    hits,
                }));
            }

            // Hybrid: merge BM25 + vector.
            let fts = knowledge::keyword_search(&app.conn(), &query, fetch_limit)?;

            let mut bm25_map: std::collections::HashMap<String, f32> =
                std::collections::HashMap::new();
            let n_fts = fts.len() as f32;
            for (rank, a) in fts.iter().enumerate() {
                let score = 1.0 - (rank as f32 / n_fts.max(1.0));
                bm25_map.insert(a.id.clone(), score);
            }

            let mut vec_map: std::collections::HashMap<String, f32> =
                std::collections::HashMap::new();
            let mut knowledge_map: std::collections::HashMap<String, Knowledge> =
                std::collections::HashMap::new();
            for (a, distance) in vec_hits.iter() {
                let score = 1.0 - (distance / 2.0).clamp(0.0, 1.0);
                vec_map.insert(a.id.clone(), score);
                knowledge_map.insert(a.id.clone(), a.clone());
            }
            for a in &fts {
                knowledge_map
                    .entry(a.id.clone())
                    .or_insert_with(|| a.clone());
            }

            let mut merged: Vec<KnowledgeHit> = knowledge_map
                .into_iter()
                .filter(|(_, a)| project_id_matches(a, project_id, &app))
                .map(|(id, kn)| {
                    let bm = bm25_map.get(&id).copied();
                    let vec = vec_map.get(&id).copied();
                    // Hybrid weighting: 0.3 BM25 + 0.7 vector. Vector recall is the
                    // primary signal for semantic search; BM25 contributes lexical
                    // pinning. When only one signal is present, the same weight is
                    // applied so cross-result ranking stays monotonic with hybrid.
                    let hybrid = match (bm, vec) {
                        (Some(b), Some(v)) => Some(0.3 * b + 0.7 * v),
                        (Some(b), None) => Some(b * 0.3),
                        (None, Some(v)) => Some(v * 0.7),
                        (None, None) => None,
                    };
                    KnowledgeHit {
                        kn,
                        bm25_score: bm,
                        vector_score: vec,
                        hybrid_score: hybrid,
                        truncated: None,
                    }
                })
                .collect();

            merged.sort_by(|a, b| {
                b.hybrid_score
                    .unwrap_or(0.0)
                    .partial_cmp(&a.hybrid_score.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let was_truncated = merged.len() > limit as usize;
            merged.truncate(limit as usize);
            let total = merged.len();

            return Ok(Json(SearchResponse {
                hits: merged,
                total_returned: total,
                limit,
                truncated: was_truncated,
            }));
        }
    }

    // Keyword-only fallback.
    let results = knowledge::keyword_search(&app.conn(), &query, fetch_limit)?;
    let hits: Vec<KnowledgeHit> = results
        .into_iter()
        .filter(|a| project_id_matches(a, project_id, &app))
        .take(limit as usize)
        .enumerate()
        .map(|(rank, a)| {
            let n = limit as f32;
            let bm = 1.0 - (rank as f32 / n.max(1.0));
            KnowledgeHit {
                kn: a,
                bm25_score: Some(bm),
                vector_score: None,
                hybrid_score: Some(bm * 0.3),
                truncated: None,
            }
        })
        .collect();

    let total = hits.len();
    Ok(Json(SearchResponse {
        hits,
        total_returned: total,
        limit,
        truncated: false,
    }))
}

/// Match a knowledge entry against a project filter. Orphan knowledge entries
/// (no plan/unit/task attachment) match any project — they are user-scoped, not
/// project-bound.
fn project_id_matches(kn: &Knowledge, project_id: Option<&str>, app: &AppState) -> bool {
    let Some(pid) = project_id else { return true };
    if kn.plan_id.is_none() && kn.unit_id.is_none() && kn.task_id.is_none() {
        return true;
    }
    let conn = app.conn();
    if let Some(plan_id) = &kn.plan_id {
        let ok: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM plans WHERE id = ?1 AND project_id = ?2",
                rusqlite::params![plan_id, pid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if ok {
            return true;
        }
    }
    if let Some(unit_id) = &kn.unit_id {
        let ok: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM plans pl JOIN units u ON u.plan_id = pl.id
             WHERE u.id = ?1 AND pl.project_id = ?2",
                rusqlite::params![unit_id, pid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if ok {
            return true;
        }
    }
    if let Some(task_id) = &kn.task_id {
        let ok: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM plans pl JOIN units u ON u.plan_id = pl.id
             JOIN tasks t ON t.unit_id = u.id
             WHERE t.id = ?1 AND pl.project_id = ?2",
                rusqlite::params![task_id, pid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if ok {
            return true;
        }
    }
    false
}

/// Synchronous embed — awaits completion. Best-effort: failures are logged
/// but do not abort the caller. Used after updates and version snapshots.
async fn embed_knowledge_sync(app: AppState, art: &Knowledge) {
    if art.content.is_empty() {
        return;
    }
    let id = art.id.clone();
    let source = format!("{}\n{}", art.title, art.content);
    match embeddings::embed(&source).await {
        Ok(Some(vec)) => {
            let _ = knowledge::store_embedding(&app.conn(), &id, &vec);
        }
        Ok(None) => {
            tracing::debug!(knowledge_id = %id, "embed skipped: empty source");
        }
        Err(err) => {
            tracing::warn!(knowledge_id = %id, "embed failed, knowledge may lack vector: {err:#}");
        }
    }
}

/// Atomic embed — bubble up the error so the caller can roll the knowledge
/// row back. Distinct from `embed_knowledge_sync` (which is best-effort).
async fn embed_knowledge_atomic(app: AppState, art: &Knowledge) -> anyhow::Result<()> {
    let source = format!("{}\n{}", art.title, art.content);
    match embeddings::embed(&source).await {
        Ok(Some(vec)) => {
            knowledge::store_embedding(&app.conn(), &art.id, &vec)?;
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("embed failed: {}", e)),
    }
}

async fn snapshot_version(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let version = knowledge::snapshot_current(&app.conn(), &id, None)?;
    if version.is_some() {
        let art_opt = knowledge::get(&app.conn(), &id)?;
        if let Some(art) = art_opt {
            embed_knowledge_sync(app.clone(), &art).await;
        }
        app.emit("knowledge:updated", serde_json::json!({ "id": id }));
    }
    match version {
        Some(v) => Ok(Json(
            serde_json::to_value(v).map_err(|e| ApiError::internal(e.to_string()))?,
        )),
        None => Err(ApiError::not_found("knowledge not found")),
    }
}

#[derive(Deserialize)]
struct WikiTreeQuery {
    root_id: Option<String>,
    plan_id: Option<String>,
}

async fn wiki_tree(
    State(app): State<AppState>,
    Query(q): Query<WikiTreeQuery>,
) -> ApiResult<Json<Vec<Knowledge>>> {
    Ok(Json(knowledge::wiki_tree(
        &app.conn(),
        q.root_id.as_deref(),
        q.plan_id.as_deref(),
    )?))
}
