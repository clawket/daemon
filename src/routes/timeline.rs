use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::repo::timeline;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/projects/{id}/timeline", get(list))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    types: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<timeline::TimelineEvent>>> {
    let events = timeline::list(
        &app.conn(),
        timeline::ListFilter {
            project_id: &id,
            limit: q.limit.unwrap_or(100),
            offset: q.offset.unwrap_or(0),
            types: q.types.as_deref(),
        },
    )?;
    Ok(Json(events))
}
