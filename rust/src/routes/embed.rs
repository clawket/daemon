use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::embeddings;
use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/embed", post(embed))
}

#[derive(Deserialize)]
struct EmbedBody {
    text: String,
}

#[derive(Serialize)]
struct EmbedResponse {
    dim: usize,
    embedding: Vec<f32>,
}

async fn embed(Json(body): Json<EmbedBody>) -> ApiResult<Json<EmbedResponse>> {
    let v = embeddings::embed(&body.text)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::bad_request("text is empty"))?;
    Ok(Json(EmbedResponse {
        dim: v.len(),
        embedding: v,
    }))
}
