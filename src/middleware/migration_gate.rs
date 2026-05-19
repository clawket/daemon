//! US-CKT-SCHEMA-037: Migration-in-progress gate.
//!
//! When the daemon's `AppState::is_migrating()` returns true, all mutating
//! HTTP requests (POST/PATCH/PUT/DELETE) return HTTP 503 with code
//! `MIGRATION_IN_PROGRESS` and message `"schema migration in progress"`.
//!
//! Read methods (GET/HEAD/OPTIONS) pass through so dashboards can still
//! observe progress while a migration runs. SSE endpoints likewise pass
//! through (they only read).
//!
//! In the current architecture startup migrations finish before the HTTP
//! listener binds, so this middleware is dormant on cold start. It exists
//! so that any future runtime/online migration code (e.g. a CLI-triggered
//! `clawket admin migrate` path) can flip the flag and the contract holds
//! uniformly across all migration entrypoints.

use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::routes::error::ApiError;
use crate::state::AppState;

pub async fn migration_gate(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method();
    let mutating = matches!(
        method,
        &Method::POST | &Method::PATCH | &Method::PUT | &Method::DELETE
    );
    if mutating && app.is_migrating() {
        // Single source of truth for the 503 contract — see ApiError::migration_in_progress
        // (US-CKT-SCHEMA-037). The Retry-After header is added on the response after
        // the IntoResponse conversion since ApiError doesn't carry response headers.
        let mut resp = ApiError::migration_in_progress().into_response();
        resp.headers_mut()
            .insert("Retry-After", "1".parse().expect("static header value"));
        return resp;
    }
    next.run(req).await
}
