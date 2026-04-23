use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::embeddings;
use crate::models::Task;
use crate::repo::{comments, cycles, plans, projects, tasks, units};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::{norm_opt, value_to_opt_string};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(create))
        .route("/tasks/search", get(search))
        .route("/tasks/bulk-update", post(bulk_update))
        .route(
            "/tasks/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/tasks/{id}/body", post(append_body))
        .route("/tasks/{id}/similar", get(similar))
}

#[derive(Deserialize)]
struct ListQuery {
    unit_id: Option<String>,
    plan_id: Option<String>,
    status: Option<String>,
    cycle_id: Option<String>,
    assignee: Option<String>,
    agent_id: Option<String>,
    parent_task_id: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Task>>> {
    let parent = q
        .parent_task_id
        .as_deref()
        .map(|s| if s == "null" { None } else { Some(s) });
    let (cycle_id_filter, no_cycle) = match q.cycle_id.as_deref() {
        Some("null") | Some("") => (None, true),
        other => (other, false),
    };
    Ok(Json(tasks::list(
        &app.conn(),
        tasks::ListFilter {
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            status: q.status.as_deref(),
            cycle_id: cycle_id_filter,
            no_cycle,
            assignee: q.assignee.as_deref(),
            agent_id: q.agent_id.as_deref(),
            parent_task_id: parent,
        },
    )?))
}

#[derive(Deserialize, Default)]
struct CreateBody {
    unit_id: Option<String>,
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    idx: Option<i64>,
    #[serde(default)]
    depends_on: Vec<String>,
    parent_task_id: Option<String>,
    priority: Option<String>,
    complexity: Option<String>,
    estimated_edits: Option<i64>,
    cycle_id: Option<String>,
    reporter: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    cwd: Option<String>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Task>> {
    let mut conn = app.conn();
    let assignee = norm_opt(body.assignee);
    let parent_task_id = norm_opt(body.parent_task_id);
    let priority = norm_opt(body.priority);
    let complexity = norm_opt(body.complexity);
    let reporter = norm_opt(body.reporter);
    let type_ = norm_opt(body.type_);
    let body_text = norm_opt(body.body);

    let cwd = norm_opt(body.cwd);
    let mut unit_id = norm_opt(body.unit_id);
    let mut cycle_id = norm_opt(body.cycle_id);

    if let Some(cwd_str) = cwd.as_deref() {
        if let Some(project) = projects::get_by_cwd(&conn, cwd_str, false)? {
            if unit_id.is_none() {
                let plan_list = plans::list(
                    &conn,
                    plans::ListFilter {
                        project_id: Some(&project.id),
                        status: Some("active"),
                    },
                )?;
                let plan = plan_list.into_iter().next().or_else(|| {
                    plans::list(
                        &conn,
                        plans::ListFilter {
                            project_id: Some(&project.id),
                            status: None,
                        },
                    )
                    .ok()
                    .and_then(|v| v.into_iter().next())
                });
                if let Some(plan) = plan {
                    let unit_list = units::list(
                        &conn,
                        units::ListFilter {
                            plan_id: Some(&plan.id),
                            status: None,
                        },
                    )?;
                    let unit = unit_list
                        .iter()
                        .find(|u| u.status != "completed")
                        .or_else(|| unit_list.first())
                        .cloned();
                    if let Some(u) = unit {
                        unit_id = Some(u.id);
                    }
                }
            }

            if cycle_id.is_none() {
                let cycle_list = cycles::list(
                    &conn,
                    cycles::ListFilter {
                        project_id: Some(&project.id),
                        status: None,
                    },
                )?;
                let cycle = cycle_list
                    .iter()
                    .find(|c| c.status == "active")
                    .or_else(|| cycle_list.iter().find(|c| c.status != "completed"))
                    .cloned();
                if let Some(c) = cycle {
                    cycle_id = Some(c.id);
                }
            }
        }
    }

    let unit_id = unit_id.ok_or_else(|| ApiError::bad_request("unit_id required (or supply cwd for auto-infer)"))?;

    let created = tasks::create(
        &mut conn,
        tasks::CreateInput {
            unit_id: &unit_id,
            title: &body.title,
            body: body_text.as_deref(),
            assignee: assignee.as_deref(),
            idx: body.idx,
            depends_on: body.depends_on,
            parent_task_id: parent_task_id.as_deref(),
            priority: priority.as_deref(),
            complexity: complexity.as_deref(),
            estimated_edits: body.estimated_edits,
            cycle_id: cycle_id.as_deref(),
            reporter: reporter.as_deref(),
            type_: type_.as_deref(),
        },
    )?;
    drop(conn);
    if let Some(t) = &created {
        app.emit("task:created", serde_json::json!({ "id": t.id }));
        schedule_task_embed(app.clone(), t);
    }
    json_or_404(created)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Task>> {
    json_or_404(tasks::get(&app.conn(), &id)?)
}

#[derive(Deserialize, Default)]
struct DeleteBody {
    reason: Option<String>,
}

// Node v2.2.1 parity: hard-delete only for `todo` tasks under a draft plan;
// otherwise soft-delete by flipping status to `cancelled` and attaching a
// `[Cancelled] …` system comment so the audit trail is preserved.
async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<DeleteBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found("Task not found"))?;
    let canonical = task.id.clone();

    if task.status == "todo" {
        let plan_is_draft = match units::get(&conn, &task.unit_id)? {
            Some(u) => match plans::get(&conn, &u.plan_id)? {
                Some(p) => p.status == "draft",
                None => false,
            },
            None => false,
        };
        if plan_is_draft {
            tasks::delete(&conn, &canonical)?;
            app.emit("task:deleted", serde_json::json!({ "id": canonical }));
            return Ok(Json(
                serde_json::json!({ "ok": true, "deleted": canonical }),
            ));
        }
    }

    drop(conn);
    let reason = body
        .and_then(|b| b.0.reason)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Cancelled via delete".into());

    let mut conn = app.conn();
    let updated = tasks::update(
        &mut conn,
        &canonical,
        tasks::UpdateFields {
            status: Some("cancelled".into()),
            ..Default::default()
        },
    )?;
    comments::create(
        &conn,
        &canonical,
        "system",
        &format!("[Cancelled] {reason}"),
    )?;
    app.emit("task:updated", serde_json::json!({ "id": canonical }));
    let payload = updated
        .map(|t| serde_json::to_value(t).unwrap_or_else(|_| serde_json::json!({})))
        .unwrap_or_else(|| serde_json::json!({ "ok": true, "deleted": canonical }));
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct AppendBody {
    text: String,
}

async fn append_body(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AppendBody>,
) -> ApiResult<Json<Task>> {
    json_or_404(tasks::append_body(&app.conn(), &id, &body.text)?)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Task>> {
    let mut conn = app.conn();
    let title_or_body_touched = body
        .as_object()
        .map(|o| o.contains_key("title") || o.contains_key("body"))
        .unwrap_or(false);
    // Sidecar: `_comment` attaches a comment in the same request for audit-trail parity.
    // Author precedence: explicit `_author` > `assignee` in the PATCH body > `_agent` hook marker > "main".
    let sidecar_comment = body
        .get("_comment")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let sidecar_author = body
        .get("_author")
        .and_then(Value::as_str)
        .or_else(|| body.get("assignee").and_then(Value::as_str))
        .or_else(|| body.get("_agent").and_then(Value::as_str))
        .unwrap_or("main")
        .to_owned();
    let updated = tasks::update(&mut conn, &id, parse_update(&body)?)?;
    if let (Some(text), Some(task)) = (sidecar_comment.as_deref(), updated.as_ref()) {
        comments::create(&conn, &task.id, &sidecar_author, text)?;
    }
    drop(conn);
    if let Some(t) = &updated {
        app.emit("task:updated", serde_json::json!({ "id": t.id }));
        if title_or_body_touched {
            schedule_task_embed(app.clone(), t);
        }
    }
    json_or_404(updated)
}

#[derive(Deserialize)]
struct BulkBody {
    ids: Vec<String>,
    fields: Value,
}

async fn bulk_update(
    State(app): State<AppState>,
    Json(body): Json<BulkBody>,
) -> ApiResult<Json<Vec<Task>>> {
    let mut conn = app.conn();
    let mut out = Vec::new();
    for id in &body.ids {
        if let Some(t) = tasks::update(&mut conn, id, parse_update(&body.fields)?)? {
            out.push(t);
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
    mode: Option<String>,
}

#[derive(Serialize)]
struct TaskHit {
    #[serde(flatten)]
    task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    _distance: Option<f32>,
}

async fn search(
    State(app): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<TaskHit>>> {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let mode = q.mode.unwrap_or_else(|| "keyword".into());

    if mode == "semantic" || mode == "hybrid" {
        if let Ok(Some(vec)) = embeddings::embed(&query).await {
            let vec_hits = tasks::vector_search(&app.conn(), &vec, limit)?;
            if mode == "semantic" {
                return Ok(Json(
                    vec_hits
                        .into_iter()
                        .map(|(t, d)| TaskHit {
                            task: t,
                            _distance: Some(d),
                        })
                        .collect(),
                ));
            }
            let fts = tasks::keyword_search(&app.conn(), &query, limit)?;
            let mut seen = std::collections::HashSet::new();
            let mut merged: Vec<TaskHit> = Vec::new();
            for t in fts {
                if seen.insert(t.id.clone()) {
                    merged.push(TaskHit {
                        task: t,
                        _distance: None,
                    });
                }
            }
            for (t, d) in vec_hits {
                if seen.insert(t.id.clone()) {
                    merged.push(TaskHit {
                        task: t,
                        _distance: Some(d),
                    });
                }
            }
            merged.truncate(limit as usize);
            return Ok(Json(merged));
        }
    }

    let results = tasks::keyword_search(&app.conn(), &query, limit)?;
    Ok(Json(
        results
            .into_iter()
            .map(|t| TaskHit {
                task: t,
                _distance: None,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct SimilarQuery {
    limit: Option<i64>,
    status: Option<String>,
}

async fn similar(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SimilarQuery>,
) -> ApiResult<Json<Vec<TaskHit>>> {
    let limit = q.limit.unwrap_or(10).clamp(1, 30);
    let task = tasks::get(&app.conn(), &id)?
        .ok_or_else(|| ApiError::not_found("Task not found"))?;
    let source = format!("{}\n{}", task.title, task.body);
    let vec = match embeddings::embed(&source)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        Some(v) => v,
        None => return Ok(Json(Vec::new())),
    };
    let raw = tasks::vector_search(&app.conn(), &vec, limit + 5)?;
    let out: Vec<TaskHit> = raw
        .into_iter()
        .filter(|(t, _)| t.id != task.id)
        .filter(|(t, _)| match &q.status {
            Some(s) => t.status == *s,
            None => true,
        })
        .take(limit as usize)
        .map(|(t, d)| TaskHit {
            task: t,
            _distance: Some(d),
        })
        .collect();
    Ok(Json(out))
}

// Fire-and-forget embed so HTTP responses stay snappy; mirrors Node v2.2.1.
fn schedule_task_embed(app: AppState, task: &Task) {
    let id = task.id.clone();
    let source = format!("{}\n{}", task.title, task.body);
    tokio::spawn(async move {
        if let Ok(Some(vec)) = embeddings::embed(&source).await {
            let _ = tasks::store_embedding(&app.conn(), &id, &vec);
        }
    });
}

fn parse_update(v: &Value) -> ApiResult<tasks::UpdateFields> {
    let obj = v
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = tasks::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("body") {
        f.body = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        f.status = Some(s.into());
    }
    if let Some(v) = obj.get("assignee") {
        f.assignee = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("priority").and_then(Value::as_str) {
        f.priority = Some(s.into());
    }
    if let Some(v) = obj.get("complexity") {
        f.complexity = Some(value_to_opt_string(v));
    }
    if let Some(v) = obj.get("estimated_edits") {
        f.estimated_edits = Some(v.as_i64());
    }
    if let Some(v) = obj.get("parent_task_id") {
        f.parent_task_id = Some(value_to_opt_string(v));
    }
    if let Some(v) = obj.get("cycle_id") {
        f.cycle_id = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("unit_id").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        f.unit_id = Some(s.into());
    }
    if let Some(v) = obj.get("reporter") {
        f.reporter = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("type").and_then(Value::as_str) {
        f.type_ = Some(s.into());
    }
    if let Some(v) = obj.get("agent_id") {
        f.agent_id = Some(value_to_opt_string(v));
    }
    Ok(f)
}
