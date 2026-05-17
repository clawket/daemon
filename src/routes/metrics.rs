// FIX-DAEMON-022: /metrics endpoint — Prometheus text format (exposition format 0.0.4).
//
// Enabled when the daemon is started with `--metrics` flag (or CLAWKETD_METRICS=1).
// Returns Content-Type: text/plain; version=0.0.4 with basic operational counters.
//
// Metrics exposed:
//   clawket_schema_version        — current DB schema version (gauge)
//   clawket_uptime_ms             — milliseconds since daemon start (gauge)
//   clawket_tasks_total           — count by status label (gauge)
//   clawket_plans_total           — count by status label (gauge)
//   clawket_vec_enabled           — 1 if sqlite-vec is active, else 0 (gauge)

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics_handler))
}

async fn metrics_handler(State(app): State<AppState>) -> impl IntoResponse {
    let conn = app.conn();

    let mut lines = Vec::<String>::new();

    // --- clawket_schema_version ---
    lines.push("# HELP clawket_schema_version Current SQLite schema version".to_string());
    lines.push("# TYPE clawket_schema_version gauge".to_string());
    lines.push(format!(
        "clawket_schema_version {}",
        app.schema_version()
    ));

    // --- clawket_uptime_ms ---
    lines.push("# HELP clawket_uptime_ms Milliseconds since daemon started".to_string());
    lines.push("# TYPE clawket_uptime_ms gauge".to_string());
    lines.push(format!("clawket_uptime_ms {}", app.uptime_ms()));

    // --- clawket_vec_enabled ---
    lines.push("# HELP clawket_vec_enabled 1 if sqlite-vec vector search is active".to_string());
    lines.push("# TYPE clawket_vec_enabled gauge".to_string());
    lines.push(format!(
        "clawket_vec_enabled {}",
        if app.vec_enabled() { 1 } else { 0 }
    ));

    // --- clawket_tasks_total ---
    lines.push("# HELP clawket_tasks_total Number of tasks by status".to_string());
    lines.push("# TYPE clawket_tasks_total gauge".to_string());
    if let Ok(rows) = task_counts_by_status(&conn) {
        for (status, count) in rows {
            lines.push(format!(
                r#"clawket_tasks_total{{status="{status}"}} {count}"#
            ));
        }
    }

    // --- clawket_plans_total ---
    lines.push("# HELP clawket_plans_total Number of plans by status".to_string());
    lines.push("# TYPE clawket_plans_total gauge".to_string());
    if let Ok(rows) = plan_counts_by_status(&conn) {
        for (status, count) in rows {
            lines.push(format!(
                r#"clawket_plans_total{{status="{status}"}} {count}"#
            ));
        }
    }

    // Prometheus expects a trailing newline.
    lines.push(String::new());
    let body = lines.join("\n");

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

fn task_counts_by_status(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM tasks GROUP BY status ORDER BY status",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn plan_counts_by_status(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM plans GROUP BY status ORDER BY status",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
