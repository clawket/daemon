use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::path::{Path, PathBuf};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(spa_index))
        .route("/web", get(web_redirect))
        .route("/favicon.svg", get(favicon))
        .route("/icons.svg", get(icons))
        .route("/assets/{*path}", get(asset))
}

const DASHBOARD_NOT_BUILT_HTML: &str = r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Clawket</title>
<style>body{font-family:-apple-system,BlinkMacSystemFont,sans-serif;background:#0d1117;color:#e6edf3;padding:40px;max-width:640px;margin:0 auto;line-height:1.6}h1{color:#58a6ff;font-size:20px;margin-bottom:16px}code{background:#161b22;padding:2px 6px;border-radius:4px;font-size:13px}pre{background:#161b22;padding:12px;border-radius:6px;overflow-x:auto;font-size:13px}</style>
</head><body>
<h1>Clawket dashboard not built</h1>
<p>The daemon is running but <code>web/dist/index.html</code> was not found.</p>
<p>Build the React dashboard from the repo root:</p>
<pre>cd web &amp;&amp; pnpm install &amp;&amp; pnpm build</pre>
<p>API is available at <code>/projects</code>, <code>/plans</code>, <code>/units</code>, <code>/tasks</code>, <code>/cycles</code>.</p>
</body></html>"##;

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn serve_file(path: &Path) -> Option<Response<Body>> {
    let bytes = std::fs::read(path).ok()?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = mime_for(&ext);
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Some(resp)
}

async fn web_redirect() -> Response<Body> {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::MOVED_PERMANENTLY;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    resp
}

fn not_found() -> Response<Body> {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn dashboard_not_built() -> Response<Body> {
    let mut resp = Response::new(Body::from(DASHBOARD_NOT_BUILT_HTML));
    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

async fn spa_index(State(app): State<AppState>) -> Response<Body> {
    let Some(web_dir) = app.paths().web_dir.clone() else {
        return dashboard_not_built();
    };
    let index = web_dir.join("index.html");
    let Ok(html) = std::fs::read(&index) else {
        return dashboard_not_built();
    };
    let mut resp = Response::new(Body::from(html));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

async fn favicon(State(app): State<AppState>) -> Response<Body> {
    let Some(web_dir) = app.paths().web_dir.clone() else {
        return not_found();
    };
    serve_file(&web_dir.join("favicon.svg")).unwrap_or_else(not_found)
}

async fn icons(State(app): State<AppState>) -> Response<Body> {
    let Some(web_dir) = app.paths().web_dir.clone() else {
        return not_found();
    };
    serve_file(&web_dir.join("icons.svg")).unwrap_or_else(not_found)
}

async fn asset(
    State(app): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Response<Body> {
    let Some(web_dir) = app.paths().web_dir.clone() else {
        return not_found();
    };
    let assets_root = web_dir.join("assets");
    let requested: PathBuf = assets_root.join(&path);
    let Ok(resolved) = requested.canonicalize() else {
        return not_found();
    };
    let Ok(root_canon) = assets_root.canonicalize() else {
        return not_found();
    };
    if !resolved.starts_with(&root_canon) {
        return not_found();
    }
    serve_file(&resolved).unwrap_or_else(not_found)
}
