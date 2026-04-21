use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::repo::units;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", get(events_stream))
        .route("/units/{id}/events", get(unit_events_stream))
}

async fn events_stream(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => Some(Ok(Event::default()
            .event(ev.event)
            .data(ev.data.to_string()))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .event(Event::default().event("ping")),
    )
}

#[derive(Deserialize)]
struct UnitEventsQuery {
    timeout: Option<i64>,
    interval: Option<i64>,
}

async fn unit_events_stream(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<UnitEventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let timeout_sec = q.timeout.unwrap_or(600).max(1);
    let interval_ms = q.interval.unwrap_or(1000).max(100);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_sec as u64);

    let stream = async_stream::stream! {
        let initial = units::get(&app.conn(), &id).ok().flatten();
        let Some(initial) = initial else {
            yield Ok(Event::default()
                .event("error")
                .data(json!({"error": "not found", "id": id}).to_string()));
            return;
        };
        if initial.approved_at.is_some() {
            yield Ok(Event::default()
                .event("approved")
                .data(json!({
                    "id": id,
                    "approved_by": initial.approved_by,
                    "approved_at": initial.approved_at,
                }).to_string()));
            return;
        }
        yield Ok(Event::default()
            .event("waiting")
            .data(json!({"id": id, "timeout_sec": timeout_sec}).to_string()));

        loop {
            if std::time::Instant::now() >= deadline {
                yield Ok(Event::default()
                    .event("timeout")
                    .data(json!({"id": id}).to_string()));
                return;
            }
            tokio::time::sleep(Duration::from_millis(interval_ms as u64)).await;
            let current = units::get(&app.conn(), &id).ok().flatten();
            if let Some(u) = current {
                if u.approved_at.is_some() {
                    yield Ok(Event::default()
                        .event("approved")
                        .data(json!({
                            "id": id,
                            "approved_by": u.approved_by,
                            "approved_at": u.approved_at,
                        }).to_string()));
                    return;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
