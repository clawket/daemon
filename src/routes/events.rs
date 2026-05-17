// FIX-DAEMON-004: Unit approval lifecycle removed — units are pure grouping entities.
// /units/{id}/events (approval SSE) is deprecated and returns an empty stream.
// FIX-DAEMON-102: structured event payload (entity_type/change_type), entity_types filter,
//                 SSE "id:" field for Last-Event-ID replay.
// FIX-DAEMON-111: /replay endpoint — replay historical events from audit_log as SSE
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
use futures::stream::{self, Stream};
use serde::Deserialize;
use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::repo::audit_log;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", get(events_stream))
        // FIX-DAEMON-111: replay historical events from audit_log
        .route("/events/replay", get(replay_stream))
    // NOTE: /units/{id}/events removed in v3.0 (FIX-DAEMON-004).
    // Units are pure grouping entities; they have no approval lifecycle.
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Comma-separated entity types to include, e.g. "task,cycle".
    /// When absent all entity types are forwarded.
    entity_types: Option<String>,
}

async fn events_stream(
    State(app): State<AppState>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // FIX-DAEMON-102: entity_types filter
    let filter: Option<HashSet<String>> = q.entity_types.map(|s| {
        s.split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    });

    // FIX-DAEMON-102: Last-Event-ID — clients send this header to resume
    // from a known event id. We cannot replay from memory here (broadcast channel
    // drops old events), but we record it for potential future replay endpoint.
    let _last_event_id: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let rx = app.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Ok(ev) => {
            // Apply entity_types filter
            if let Some(ref allowed) = filter {
                if !allowed.contains(ev.entity_type) {
                    return None;
                }
            }
            // FIX-DAEMON-102: set SSE "id:" field for Last-Event-ID tracking
            let sse_event = Event::default()
                .id(ev.id.to_string())
                .event(ev.event)
                .data(ev.data.to_string());
            Some(Ok(sse_event))
        }
        Err(_) => None,
    });
    // SSE keepalive — must include a `data:` field. Per the WHATWG SSE spec,
    // events with no data line are silently dropped by EventSource (the data
    // buffer remains empty → dispatchMessage returns without firing). Without
    // `data: ok`, the daemon would still keep the TCP connection alive but
    // the client's `addEventListener('ping', …)` would never fire, leaving
    // the header lag clock stuck and the badge flapping to "Lagging".
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .event(Event::default().event("ping").data("ok")),
    )
}

/// FIX-DAEMON-111: GET /events/replay?entity_type=&entity_id=&limit=
/// Replay historical audit_log entries as SSE events (non-streaming, batch).
/// Returns a finite SSE stream of past entries then closes.
#[derive(Deserialize)]
struct ReplayQuery {
    entity_type: Option<String>,
    entity_id: Option<String>,
    limit: Option<i64>,
}

async fn replay_stream(
    State(app): State<AppState>,
    Query(q): Query<ReplayQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let conn = app.conn();
    let entries = audit_log::list(
        &conn,
        audit_log::ListFilter {
            entity_type: q.entity_type.as_deref(),
            entity_id: q.entity_id.as_deref(),
            actor: None,
            op_type: None,
            limit: q.limit.or(Some(200)),
        },
    )
    .unwrap_or_default();
    drop(conn);

    let events: Vec<Result<Event, Infallible>> = entries
        .into_iter()
        .map(|e| {
            let data = serde_json::json!({
                "entity_type": e.entity_type,
                "entity_id": e.entity_id,
                "op_type": e.op_type,
                "field": e.field,
                "old_value": e.old_value,
                "new_value": e.new_value,
                "actor": e.actor,
                "at": e.at,
            });
            Ok(Event::default()
                .id(e.id.clone())
                .event(format!("{}:{}", e.entity_type, e.op_type))
                .data(data.to_string()))
        })
        .collect();

    Sse::new(stream::iter(events))
}
