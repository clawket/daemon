use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::{Plan, Project, Task, Unit};
use crate::repo::{cycles, plans, projects, questions, runs, tasks, units};
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/dashboard", get(dashboard))
}

#[derive(Deserialize)]
struct DashboardQuery {
    cwd: Option<String>,
    show: Option<String>,
}

async fn dashboard(
    State(app): State<AppState>,
    Query(q): Query<DashboardQuery>,
) -> ApiResult<Json<Value>> {
    let cwd = q.cwd.as_deref().unwrap_or("");
    let show = q.show.as_deref().unwrap_or("all");
    let conn = app.conn();

    // Resolve project by cwd (enabled only) or single-enabled-project fallback
    let mut project: Option<Project> = if !cwd.is_empty() {
        projects::get_by_cwd(&conn, cwd, true)?
    } else {
        None
    };
    if project.is_none() {
        let all: Vec<Project> = projects::list(&conn)?
            .into_iter()
            .filter(|p| p.enabled != 0)
            .collect();
        if all.len() == 1 {
            project = all.into_iter().next();
        }
    }
    let Some(project) = project else {
        return Ok(Json(json!({ "context": "", "project": null })));
    };

    // Visible plans: non-completed + completed-with-in_progress-tasks
    let all_plans = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project.id),
            status: None,
        },
    )?;
    let visible_plans: Vec<Plan> = all_plans
        .into_iter()
        .filter(|p| {
            matches!(p.status.as_str(), "active" | "approved" | "draft") || {
                if p.status == "completed" {
                    let pu = units::list(
                        &conn,
                        units::ListFilter {
                            plan_id: Some(&p.id),
                        },
                    )
                    .unwrap_or_default();
                    pu.iter().any(|u| {
                        tasks::list(
                            &conn,
                            tasks::ListFilter {
                                unit_id: Some(&u.id),
                                ..Default::default()
                            },
                        )
                        .map(|ts| ts.iter().any(|t| t.status == "in_progress"))
                        .unwrap_or(false)
                    })
                } else {
                    false
                }
            }
        })
        .collect();

    if visible_plans.is_empty() {
        return Ok(Json(json!({
            "context": format!("# Clawket: {}\nNo active plan.", project.name),
            "project": project.id,
        })));
    }

    let mut lines: Vec<String> = Vec::new();
    let plural = if visible_plans.len() > 1 { "s" } else { "" };
    lines.push(format!(
        "# Clawket: {} ({} plan{})",
        project.name,
        visible_plans.len(),
        plural
    ));
    lines.push(String::new());

    // R3 DOGFOOD-009 fix: surface active-plan-count warning in dashboard render
    // (the discover-loop:start emit covers /discover-loop/start path; dashboard
    // is the CLI surface PDD §X1 cares about). Threshold: >1 active plan triggers
    // warning, >2 triggers stronger warning. Also emit tracing::warn! for daemon log.
    let active_plan_count = visible_plans
        .iter()
        .filter(|p| p.status == "active")
        .count();
    if active_plan_count > 1 {
        let level = if active_plan_count > 2 {
            "ERROR"
        } else {
            "WARN"
        };
        let line = format!(
            "[{level}] active plan {} (>1) — PDD recommends ≤ 1 (≤ 2 in transition). Consider closing or completing extra plans.",
            active_plan_count
        );
        tracing::warn!(
            project_id = %project.id,
            active_plan_count,
            recommended_max = 1,
            transition_max = 2,
            "dashboard: {}", line
        );
        lines.push(line);
        lines.push(String::new());
    }

    let active_plan = visible_plans
        .iter()
        .find(|p| p.status == "active")
        .cloned()
        .unwrap_or_else(|| visible_plans[0].clone());

    // Active cycle (project-scoped). Surfaced before plan list because cycle is
    // the time-boxed execution container the user is currently inside.
    let active_cycle = cycles::list(
        &conn,
        cycles::ListFilter {
            project_id: Some(&project.id),
            status: Some("active"),
            unit_id: None,
        },
    )?
    .into_iter()
    .next();
    if let Some(cycle) = &active_cycle {
        lines.push(format!("## Active Cycle: {} ({})", cycle.title, cycle.id));
        if let Some(goal) = cycle.goal.as_deref().filter(|s| !s.is_empty()) {
            let snippet: String = goal.chars().take(200).collect();
            let suffix = if goal.chars().count() > 200 {
                "…"
            } else {
                ""
            };
            lines.push(format!("  Goal: {snippet}{suffix}"));
        }
        lines.push(String::new());
    }

    for plan in &visible_plans {
        let is_active = plan.id == active_plan.id;
        lines.push(format!(
            "## Plan: {} ({}) [{}]{}",
            plan.title,
            plan.id,
            plan.status,
            if is_active { " ← active" } else { "" }
        ));

        let all_units = units::list(
            &conn,
            units::ListFilter {
                plan_id: Some(&plan.id),
            },
        )?;

        let visible_units: Vec<&Unit> = match show {
            // Units are pure grouping entities (no status field since FIX-DAEMON-004).
            // Interpret show=active as "units containing in_progress tasks".
            "active" | "next" => all_units
                .iter()
                .filter(|u| {
                    tasks::list(
                        &conn,
                        tasks::ListFilter {
                            unit_id: Some(&u.id),
                            ..Default::default()
                        },
                    )
                    .map(|ts| ts.iter().any(|t| t.status == "in_progress"))
                    .unwrap_or(false)
                })
                .collect(),
            _ => all_units.iter().collect(),
        };

        for unit in &visible_units {
            lines.push(format!("## {} ({})", unit.title, unit.id));

            let all_tasks = tasks::list(
                &conn,
                tasks::ListFilter {
                    unit_id: Some(&unit.id),
                    ..Default::default()
                },
            )?;
            let done_count = all_tasks.iter().filter(|t| t.status == "done").count();
            if !all_tasks.is_empty() {
                lines.push(format!("  Progress: {}/{}", done_count, all_tasks.len()));
            }

            let tasks_to_show: Vec<&Task> =
                all_tasks.iter().filter(|t| t.status != "done").collect();

            for task in &tasks_to_show {
                let icon = match task.status.as_str() {
                    "todo" => "[ ]",
                    "in_progress" => "[>]",
                    "blocked" => "[!]",
                    "cancelled" => "[-]",
                    "done" => "[x]",
                    _ => "[ ]",
                };
                let assignee = task
                    .assignee
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" @{s}"))
                    .unwrap_or_default();
                let reference = task
                    .ticket_number
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&task.id);
                lines.push(format!(
                    "  {} {} ({}){}",
                    icon, task.title, reference, assignee
                ));
            }
            lines.push(String::new());
        }

        if visible_units.len() < all_units.len() {
            let hidden = all_units.len() - visible_units.len();
            lines.push(format!(
                "({} more units hidden — use show=all to see all)",
                hidden
            ));
            lines.push(String::new());
        }
    }

    // Recent runs
    let recent_runs = runs::list(
        &conn,
        runs::ListFilter {
            project_id: Some(&project.id),
            ..Default::default()
        },
    )?;
    let recent_runs: Vec<_> = recent_runs.into_iter().take(5).collect();
    if !recent_runs.is_empty() {
        lines.push("## Recent Activity".into());
        for r in &recent_runs {
            let status = if r.ended_at.is_some() {
                format!("done ({})", r.result.as_deref().unwrap_or("ok"))
            } else {
                "running".into()
            };
            let task = tasks::get(&conn, &r.task_id).ok().flatten();
            let task_title = task
                .as_ref()
                .map(|t| t.title.clone())
                .unwrap_or_else(|| r.task_id.clone());
            let ticket = task
                .as_ref()
                .and_then(|t| t.ticket_number.clone())
                .filter(|s| !s.is_empty())
                .map(|s| format!("{s} "))
                .unwrap_or_default();
            let notes = r
                .notes
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" — {}", &s.chars().take(60).collect::<String>()))
                .unwrap_or_default();
            let agent = &r.agent;
            lines.push(format!(
                "  @{} → {}{} [{}]{}",
                agent, ticket, task_title, status, notes
            ));
        }
        lines.push(String::new());
    }

    // In-progress carry-over across all visible plans
    let mut in_progress: Vec<Task> = Vec::new();
    for p in &visible_plans {
        let pu = units::list(
            &conn,
            units::ListFilter {
                plan_id: Some(&p.id),
            },
        )?;
        for u in &pu {
            let ts = tasks::list(
                &conn,
                tasks::ListFilter {
                    unit_id: Some(&u.id),
                    ..Default::default()
                },
            )?;
            in_progress.extend(ts.into_iter().filter(|t| t.status == "in_progress"));
        }
    }
    if !in_progress.is_empty() {
        lines.push("## In Progress (carry-over)".into());
        for t in &in_progress {
            let assignee = t
                .assignee
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" @{s}"))
                .unwrap_or_default();
            lines.push(format!("  [>] {} ({}){}", t.title, t.id, assignee));
        }
        lines.push(String::new());
    }

    // R3 DOGFOOD-040 fix: discover-loop round summary block. When any visible
    // plan title matches "<Domain> Round <N>", emit a per-round snapshot:
    // "Round N: D defect, S scenario_error, decision=<continue|converged|regression>".
    // The decision string is derived from the same rules as
    // /discover-loop/status (defect>0 ∧ defect>prev → regression; defect=0 ∧
    // scenario_error=0 → converged; else continue). Surfacing this on the
    // dashboard satisfies qa-flow §7 hand-off visibility.
    {
        // Collect all "Round N" plans for the project (active or completed).
        let all_round_plans: Vec<Plan> = plans::list(
            &conn,
            plans::ListFilter {
                project_id: Some(&project.id),
                status: None,
            },
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            let title = p.title.as_str();
            // Cheap regex-free check: ends with " Round <digits>"
            title.rfind(" Round ").is_some_and(|idx| {
                title[idx + 7..].chars().all(|c| c.is_ascii_digit()) && idx + 7 < title.len()
            })
        })
        .collect();

        if !all_round_plans.is_empty() {
            // Sort by round number ascending.
            let mut indexed: Vec<(u32, &Plan)> = all_round_plans
                .iter()
                .filter_map(|p| {
                    let title = p.title.as_str();
                    let idx = title.rfind(" Round ")?;
                    let num: u32 = title[idx + 7..].parse().ok()?;
                    Some((num, p))
                })
                .collect();
            indexed.sort_by_key(|(n, _)| *n);

            lines.push("## Discover-Loop Rounds".into());
            let mut prev_defect: Option<i64> = None;
            for (round_num, plan) in &indexed {
                // Tally task qa_status.
                let plan_units = units::list(
                    &conn,
                    units::ListFilter {
                        plan_id: Some(&plan.id),
                    },
                )
                .unwrap_or_default();
                let mut defect = 0i64;
                let mut scenario_error = 0i64;
                let mut pass = 0i64;
                for u in &plan_units {
                    let ts = tasks::list(
                        &conn,
                        tasks::ListFilter {
                            unit_id: Some(&u.id),
                            ..Default::default()
                        },
                    )
                    .unwrap_or_default();
                    for t in &ts {
                        match t.qa_status.as_deref() {
                            Some("defect") => defect += 1,
                            Some("scenario_error") => scenario_error += 1,
                            Some("pass") => pass += 1,
                            _ => {}
                        }
                    }
                }
                let decision = if defect == 0 && scenario_error == 0 {
                    "converged"
                } else if defect > 0 && prev_defect.map(|prev| defect > prev).unwrap_or(false) {
                    "regression"
                } else {
                    "continue"
                };
                lines.push(format!(
                    "  Round {}: {} defect, {} scenario_error, {} pass, decision={}",
                    round_num, defect, scenario_error, pass, decision
                ));
                prev_defect = Some(defect);
            }
            lines.push(String::new());
        }
    }

    // Pending questions (active plan only)
    let pending_qs = questions::list(
        &conn,
        questions::ListFilter {
            plan_id: Some(&active_plan.id),
            unit_id: None,
            task_id: None,
            pending: Some(true),
        },
    )?;
    if !pending_qs.is_empty() {
        lines.push(format!("## Pending Questions ({})", pending_qs.len()));
        for q in &pending_qs {
            lines.push(format!("  ? {} ({})", q.body, q.id));
        }
        lines.push(String::new());
    }

    lines.push(
        "Commands: clawket task view <ID> | clawket task update <ID> --status <s> | clawket unit approve <ID>"
            .into(),
    );

    let mut response = json!({
        "context": lines.join("\n"),
        "project": project.id,
        "plan": active_plan.id,
    });
    // R3 DOGFOOD-009 fix: expose machine-readable active-plan warning so CLI
    // can mirror it to stderr (separate from the human-readable context body).
    if active_plan_count > 1 {
        response["active_plan_warning"] = json!({
            "active_plan_count": active_plan_count,
            "recommended_max": 1,
            "transition_max": 2,
            "level": if active_plan_count > 2 { "error" } else { "warn" },
        });
    }
    Ok(Json(response))
}
