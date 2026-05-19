// FIX-DAEMON-108: TCP auth middleware — X-Clawket-Token validation.
//
// Only applied to the TCP listener. Unix socket connections are exempt
// (local = trusted). The token is regenerated on every daemon start
// (rotation on restart) and written to `~/.cache/clawket/clawketd.token` (0600).
//
// LM-10833: the browser SPA can't read the token file the way the CLI can,
// so this middleware now accepts the token via either the `X-Clawket-Token`
// header (CLI / external tools) or the `clawket_session` HttpOnly cookie
// (browser SPA, issued by the SPA index handler). For mutating methods
// (POST/PUT/PATCH/DELETE) we additionally require the request's Origin or
// Referer to point at the daemon's own origin — same-site cookies plus the
// origin guard close the CSRF surface that comes with cookie-based auth.
//
// If CLAWKET_TCP_AUTH=0 the middleware is a no-op (e.g. for integration tests
// running without the token file).

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

const SESSION_COOKIE: &str = "clawket_session";

/// axum `from_fn_with_state`-compatible handler for TCP auth.
///
/// Usage in main.rs:
/// ```ignore
/// use axum::middleware;
/// let tcp_app = base_router.layer(middleware::from_fn_with_state(token.clone(), tcp_auth_layer));
/// ```
pub async fn tcp_auth_layer(
    axum::extract::State(token): axum::extract::State<String>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // CLAWKET_TCP_AUTH=0 disables auth (for tests / local dev without token file)
    let enabled = std::env::var("CLAWKET_TCP_AUTH")
        .ok()
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);

    if !enabled {
        return next.run(req).await;
    }

    // Auth-exempt paths:
    // - `/health` — CLI status check before token is known to the caller
    // - `/` (SPA index), `/assets/*`, `/favicon.svg`, `/icons.svg` — LM-10833:
    //   the SPA index response is the channel that *issues* the bootstrap
    //   cookie, so it can't itself require the cookie. Static assets carry
    //   no secrets (they're built JS/CSS) and the data API remains protected.
    let path = req.uri().path();
    if path == "/health"
        || path == "/"
        || path == "/web"
        || path == "/favicon.svg"
        || path == "/icons.svg"
        || path.starts_with("/assets/")
    {
        return next.run(req).await;
    }

    // 1) Extract candidate tokens from header AND cookie. Either channel
    //    matching is sufficient — using OR (not header-preferred) means a
    //    stale localStorage value the SPA might still send doesn't shadow a
    //    fresh bootstrap cookie. The header channel remains the only path the
    //    CLI uses, so CLI-only flows are unaffected.
    let header_token = req
        .headers()
        .get("x-clawket-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let cookie_token = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| extract_cookie(raw, SESSION_COOKIE));

    let header_ok = matches!(header_token, Some(ref t) if t == &token);
    let cookie_ok = matches!(cookie_token, Some(ref t) if t == &token);
    let token_ok = header_ok || cookie_ok;
    if !token_ok {
        tracing::warn!(
            path = %req.uri().path(),
            "TCP auth failed: missing or invalid token (header and cookie)"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "TCP auth required: provide X-Clawket-Token header or clawket_session cookie. Read token from ~/.cache/clawket/clawketd.token",
                "code": "TOKEN_REQUIRED"
            })),
        )
            .into_response();
    }

    // 2) Origin guard for state-changing methods. The cookie channel is
    //    cross-site-readable in principle, so we require the request to come
    //    from our own origin. SameSite=Strict already blocks the cross-site
    //    case in modern browsers; this is defense in depth (and also catches
    //    older browsers / non-browser clients that forward cookies).
    let is_mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if is_mutating {
        let origin = req
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        let referer = req
            .headers()
            .get(axum::http::header::REFERER)
            .and_then(|v| v.to_str().ok());
        let host = req
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok());

        // CLI / curl typically sends neither Origin nor Referer. Header-token
        // requests don't need the origin guard (they prove possession of the
        // file-system token, which is itself a same-machine claim). The check
        // is `header_ok`, not "any header present" — a stale header that
        // failed validation must NOT bypass the origin guard.
        if !header_ok && !origin_or_referer_matches_host(origin, referer, host) {
            tracing::warn!(
                path = %req.uri().path(),
                method = %req.method(),
                origin = ?origin,
                referer = ?referer,
                host = ?host,
                "TCP auth: origin guard rejected mutating cookie-only request"
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "Origin/Referer mismatch on cookie-authenticated mutating request",
                    "code": "ORIGIN_MISMATCH"
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

/// Extract a single cookie value from a `Cookie:` header line. Returns `None`
/// if the cookie isn't present. We avoid pulling in a cookie-parsing crate
/// because the format we need is trivial (`name=value; name2=value2; ...`).
fn extract_cookie(raw: &str, name: &str) -> Option<String> {
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let (k, v) = part.split_at(eq);
            if k == name {
                return Some(v[1..].to_string());
            }
        }
    }
    None
}

/// True if the Origin (or, when Origin is absent, the Referer) header refers
/// to the same scheme://authority pair the request was sent to. The Host
/// header gives us the authority part; we compare authority only, ignoring
/// path/query.
fn origin_or_referer_matches_host(
    origin: Option<&str>,
    referer: Option<&str>,
    host: Option<&str>,
) -> bool {
    let host = match host {
        Some(h) => h,
        None => return false,
    };

    if let Some(o) = origin {
        return authority_of(o).map(|a| a == host).unwrap_or(false);
    }
    if let Some(r) = referer {
        return authority_of(r).map(|a| a == host).unwrap_or(false);
    }
    // Neither Origin nor Referer set. This is what curl / CLI does, but we
    // already short-circuit cookie-only mutating requests before reaching
    // here only when the header token wasn't used. So a request reaching
    // this point with no Origin and no Referer is suspicious — reject.
    false
}

fn authority_of(url: &str) -> Option<&str> {
    // Accept "scheme://authority[/path...]". For an exact host:port match we
    // strip the scheme and any path/fragment.
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        None
    } else {
        Some(authority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_cookie_finds_named_value() {
        let raw = "foo=bar; clawket_session=abc123; baz=qux";
        assert_eq!(
            extract_cookie(raw, "clawket_session"),
            Some("abc123".into())
        );
        assert_eq!(extract_cookie(raw, "foo"), Some("bar".into()));
        assert_eq!(extract_cookie(raw, "missing"), None);
    }

    #[test]
    fn extract_cookie_handles_single_pair() {
        assert_eq!(
            extract_cookie("clawket_session=xyz", "clawket_session"),
            Some("xyz".into())
        );
    }

    #[test]
    fn authority_of_strips_scheme_and_path() {
        assert_eq!(
            authority_of("http://localhost:19400/foo"),
            Some("localhost:19400")
        );
        assert_eq!(authority_of("https://example.com"), Some("example.com"));
        assert_eq!(
            authority_of("http://127.0.0.1:5174/?x=1"),
            Some("127.0.0.1:5174")
        );
    }

    #[test]
    fn origin_match_requires_authority_equality() {
        assert!(origin_or_referer_matches_host(
            Some("http://localhost:19400"),
            None,
            Some("localhost:19400"),
        ));
        assert!(!origin_or_referer_matches_host(
            Some("http://evil.example"),
            None,
            Some("localhost:19400"),
        ));
        assert!(!origin_or_referer_matches_host(
            None,
            None,
            Some("localhost:19400")
        ));
    }
}
