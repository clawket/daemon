use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/agents", get(list))
}

async fn list(State(app): State<AppState>) -> ApiResult<Json<Vec<String>>> {
    let conn = app.conn();
    let mut stmt = conn.prepare("SELECT DISTINCT agent FROM runs WHERE agent IS NOT NULL ORDER BY agent")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(Json(out))
}
