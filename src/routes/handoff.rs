use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::Project;
use crate::repo::{artifacts, plans, projects, questions, tasks, units};
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/handoff", get(handoff))
}

#[derive(Deserialize)]
struct HandoffQuery {
    cwd: Option<String>,
}

fn iso_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let secs = (millis / 1000) as i64;
    let ms = (millis % 1000) as u32;
    format_iso(secs, ms)
}

fn format_iso(secs: i64, ms: u32) -> String {
    // Equivalent to new Date().toISOString() — use chrono-free manual format.
    // YYYY-MM-DDTHH:MM:SS.mmmZ
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400) as u32;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (y, m, d) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hours, minutes, seconds, ms
    )
}

fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Epoch = 1970-01-01 = day 0. Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as i32, m as u32, d as u32)
}

async fn handoff(
    State(app): State<AppState>,
    Query(q): Query<HandoffQuery>,
) -> ApiResult<Json<Value>> {
    let cwd = q.cwd.as_deref().unwrap_or("");
    let conn = app.conn();

    let mut project: Option<Project> = if !cwd.is_empty() {
        projects::get_by_cwd(&conn, cwd, false)?
    } else {
        None
    };
    if project.is_none() {
        let all = projects::list(&conn)?;
        if all.len() == 1 {
            project = all.into_iter().next();
        }
    }
    let Some(project) = project else {
        return Ok(Json(json!({ "content": "# No project found" })));
    };

    let all_plans = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project.id),
            status: None,
        },
    )?;
    let active_plan = all_plans
        .into_iter()
        .find(|p| matches!(p.status.as_str(), "active" | "approved" | "draft"));

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# HANDOFF: {}", project.name));
    lines.push(format!("Generated: {}", iso_now()));
    lines.push(String::new());

    let Some(active_plan) = active_plan else {
        lines.push("No active plan.".into());
        return Ok(Json(json!({ "content": lines.join("\n") })));
    };

    let all_units = units::list(
        &conn,
        units::ListFilter {
            plan_id: Some(&active_plan.id),
            status: None,
        },
    )?;

    let mut all_tasks = Vec::new();
    for u in &all_units {
        let ts = tasks::list(
            &conn,
            tasks::ListFilter {
                unit_id: Some(&u.id),
                ..Default::default()
            },
        )?;
        all_tasks.extend(ts);
    }
    let done = all_tasks.iter().filter(|t| t.status == "done").count();
    let total = all_tasks.len();
    let pct = if total > 0 {
        (done as f64 / total as f64 * 100.0).round() as i64
    } else {
        0
    };
    lines.push(format!(
        "## Status: {}/{} tasks complete ({}%)",
        done, total, pct
    ));
    lines.push(String::new());

    let completed_units: Vec<_> = all_units.iter().filter(|u| u.status == "completed").collect();
    if !completed_units.is_empty() {
        lines.push("## Completed".into());
        for u in &completed_units {
            lines.push(format!("- [x] {}", u.title));
        }
        lines.push(String::new());
    }

    let in_progress: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.status == "in_progress")
        .collect();
    if !in_progress.is_empty() {
        lines.push("## In Progress".into());
        for t in &in_progress {
            let a = t
                .assignee
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" (@{s})"))
                .unwrap_or_default();
            lines.push(format!("- [ ] {}{}", t.title, a));
        }
        lines.push(String::new());
    }

    let blocked: Vec<_> = all_tasks.iter().filter(|t| t.status == "blocked").collect();
    if !blocked.is_empty() {
        lines.push("## Blocked".into());
        for t in &blocked {
            lines.push(format!("- [!] {}", t.title));
        }
        lines.push(String::new());
    }

    let active_units: Vec<_> = all_units
        .iter()
        .filter(|u| matches!(u.status.as_str(), "active" | "pending"))
        .collect();
    let mut next_todo: Vec<_> = Vec::new();
    for u in &active_units {
        let ts = tasks::list(
            &conn,
            tasks::ListFilter {
                unit_id: Some(&u.id),
                ..Default::default()
            },
        )?;
        for t in ts.into_iter().filter(|t| t.status == "todo") {
            next_todo.push(t);
            if next_todo.len() >= 10 {
                break;
            }
        }
        if next_todo.len() >= 10 {
            break;
        }
    }
    if !next_todo.is_empty() {
        lines.push("## Next Up".into());
        for t in &next_todo {
            lines.push(format!("- {}", t.title));
        }
        lines.push(String::new());
    }

    let open_qs = questions::list(
        &conn,
        questions::ListFilter {
            plan_id: Some(&active_plan.id),
            unit_id: None,
            task_id: None,
            pending: Some(true),
        },
    )?;
    if !open_qs.is_empty() {
        lines.push("## Open Questions".into());
        for q in &open_qs {
            lines.push(format!("- {}", q.body));
        }
        lines.push(String::new());
    }

    let decisions = artifacts::list(
        &conn,
        artifacts::ListFilter {
            plan_id: Some(&active_plan.id),
            type_: Some("decision"),
            ..Default::default()
        },
    )?;
    if !decisions.is_empty() {
        lines.push("## Design Decisions".into());
        for d in &decisions {
            let snippet: String = d.content.chars().take(120).collect();
            lines.push(format!("- **{}**: {}", d.title, snippet));
        }
        lines.push(String::new());
    }

    Ok(Json(json!({ "content": lines.join("\n") })))
}
