use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

use crate::models::{Plan, Project, Task, Unit};
use crate::repo::{plans, projects, tasks, units};
use rusqlite::Connection;

#[derive(Debug, Default, Clone)]
pub struct ParsedTask {
    pub idx: i64,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedUnit {
    pub idx: i64,
    pub title: String,
    pub body: String,
    pub tasks: Vec<ParsedTask>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedPlan {
    pub title: Option<String>,
    pub description: String,
    pub units: Vec<ParsedUnit>,
}

static UNIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^##\s+Unit\s*(\d+)?\s*[:.]?\s*(.*)$").unwrap()
});
static H3_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^###\s+(.+)$").unwrap());
static NUMBERED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*\d+\.\s+\*?\*?([^*]+)\*?\*?(.*)$").unwrap());
static HEADING_ANY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^##?#?\s+").unwrap());
static NUMBERED_START_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\d+\.\s+").unwrap());
static TITLE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^#\s+").unwrap());

pub fn parse_plan_markdown(md: &str) -> ParsedPlan {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut plan = ParsedPlan::default();

    if let Some(t) = lines.iter().find(|l| TITLE_RE.is_match(l)) {
        let stripped = TITLE_RE.replace(t, "").trim().to_string();
        plan.title = Some(stripped);
    }

    let mut unit_markers: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = UNIT_RE.captures(line) {
            let num = caps.get(1).map(|m| m.as_str().to_string());
            let name = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            let title = name.unwrap_or_else(|| {
                format!("Unit {}", num.unwrap_or_else(|| (unit_markers.len() + 1).to_string()))
            });
            unit_markers.push((i, title));
        }
    }

    if unit_markers.is_empty() {
        plan.units.push(ParsedUnit {
            idx: 0,
            title: "Unit 1".to_string(),
            body: lines.join("\n").trim().to_string(),
            tasks: extract_tasks_from_body(&lines, 0, lines.len()),
        });
    } else {
        plan.description = lines[..unit_markers[0].0].join("\n").trim().to_string();
        for (p, (line_idx, title)) in unit_markers.iter().enumerate() {
            let end = unit_markers
                .get(p + 1)
                .map(|(i, _)| *i)
                .unwrap_or(lines.len());
            plan.units.push(ParsedUnit {
                idx: p as i64,
                title: title.clone(),
                body: lines[*line_idx..end].join("\n").trim().to_string(),
                tasks: extract_tasks_from_body(&lines, *line_idx + 1, end),
            });
        }
    }

    plan
}

fn extract_tasks_from_body(lines: &[&str], start: usize, end: usize) -> Vec<ParsedTask> {
    let mut h3: Vec<(usize, String)> = Vec::new();
    for i in start..end {
        if let Some(caps) = H3_RE.captures(lines[i]) {
            h3.push((i, caps[1].trim().to_string()));
        }
    }
    if !h3.is_empty() {
        let mut out = Vec::new();
        for (j, (s, title)) in h3.iter().enumerate() {
            let e = h3.get(j + 1).map(|(i, _)| *i).unwrap_or(end);
            out.push(ParsedTask {
                idx: j as i64,
                title: title.clone(),
                body: lines[s + 1..e].join("\n").trim().to_string(),
            });
        }
        return out;
    }

    let mut out: Vec<ParsedTask> = Vec::new();
    for i in start..end {
        if let Some(caps) = NUMBERED_RE.captures(lines[i]) {
            let title = caps
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let tail = caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let body = format!("{}\n{}", tail, gather_continuation(lines, i + 1, end))
                .trim()
                .to_string();
            out.push(ParsedTask {
                idx: out.len() as i64,
                title,
                body,
            });
        }
    }
    out
}

fn gather_continuation(lines: &[&str], from: usize, end: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for i in from..end {
        if HEADING_ANY_RE.is_match(lines[i]) || NUMBERED_START_RE.is_match(lines[i]) {
            break;
        }
        out.push(lines[i]);
    }
    out.join("\n")
}

pub struct ImportOptions<'a> {
    pub project_name: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub source: &'a str,
    pub dry_run: bool,
}

#[derive(serde::Serialize)]
pub struct DryRunUnit {
    pub title: String,
    pub tasks: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum ImportResult {
    DryRun {
        dry_run: bool,
        project: Project,
        plan_title: String,
        unit_count: usize,
        task_count: usize,
        units: Vec<DryRunUnit>,
    },
    Created {
        project: Project,
        plan: Plan,
        units: Vec<CreatedUnit>,
        summary: Summary,
    },
}

#[derive(serde::Serialize)]
pub struct CreatedUnit {
    pub unit: Unit,
    pub tasks: Vec<Task>,
}

#[derive(serde::Serialize)]
pub struct Summary {
    pub unit_count: usize,
    pub task_count: usize,
}

pub fn import_plan_file(
    conn: &mut Connection,
    file_path: &str,
    opts: ImportOptions<'_>,
) -> Result<ImportResult> {
    let md = std::fs::read_to_string(file_path)?;
    let parsed = parse_plan_markdown(&md);
    let Some(title) = parsed.title.clone() else {
        bail!("no plan title (first # heading) found");
    };

    let effective_cwd = opts
        .cwd
        .map(String::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });

    let project = if let Some(name) = opts.project_name {
        match projects::get_by_name(conn, name)? {
            Some(p) => p,
            None => projects::create(
                conn,
                projects::CreateInput {
                    name,
                    description: None,
                    cwd: Some(&effective_cwd),
                    key: None,
                },
            )?
            .ok_or_else(|| anyhow::anyhow!("project create failed"))?,
        }
    } else {
        match projects::get_by_cwd(conn, &effective_cwd, false)? {
            Some(p) => p,
            None => {
                let fallback = Path::new(file_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("plan")
                    .chars()
                    .take(40)
                    .collect::<String>();
                projects::create(
                    conn,
                    projects::CreateInput {
                        name: &fallback,
                        description: None,
                        cwd: Some(&effective_cwd),
                        key: None,
                    },
                )?
                .ok_or_else(|| anyhow::anyhow!("project create failed"))?
            }
        }
    };

    if opts.dry_run {
        let unit_count = parsed.units.len();
        let task_count: usize = parsed.units.iter().map(|u| u.tasks.len()).sum();
        let units_out: Vec<DryRunUnit> = parsed
            .units
            .iter()
            .map(|u| DryRunUnit {
                title: u.title.clone(),
                tasks: u.tasks.iter().map(|t| t.title.clone()).collect(),
            })
            .collect();
        return Ok(ImportResult::DryRun {
            dry_run: true,
            project,
            plan_title: title,
            unit_count,
            task_count,
            units: units_out,
        });
    }

    let description = if parsed.description.is_empty() {
        None
    } else {
        Some(parsed.description.as_str())
    };
    let plan = plans::create(
        conn,
        plans::CreateInput {
            project_id: &project.id,
            title: &title,
            description,
            source: Some(opts.source),
            source_path: Some(file_path),
        },
    )?
    .ok_or_else(|| anyhow::anyhow!("plan create failed"))?;

    let mut created_units: Vec<CreatedUnit> = Vec::new();
    for pu in &parsed.units {
        let unit = units::create(
            conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: &pu.title,
                goal: None,
                idx: Some(pu.idx),
                approval_required: false,
                execution_mode: None,
            },
        )?
        .ok_or_else(|| anyhow::anyhow!("unit create failed"))?;
        let mut created_tasks = Vec::new();
        for pt in &pu.tasks {
            let task = tasks::create(
                conn,
                tasks::CreateInput {
                    unit_id: &unit.id,
                    title: &pt.title,
                    body: Some(&pt.body),
                    assignee: None,
                    idx: Some(pt.idx),
                    depends_on: Vec::new(),
                    parent_task_id: None,
                    priority: None,
                    complexity: None,
                    estimated_edits: None,
                    cycle_id: None,
                    reporter: None,
                    type_: None,
                },
            )?
            .ok_or_else(|| anyhow::anyhow!("task create failed"))?;
            created_tasks.push(task);
        }
        created_units.push(CreatedUnit {
            unit,
            tasks: created_tasks,
        });
    }

    let unit_count = created_units.len();
    let task_count: usize = created_units.iter().map(|u| u.tasks.len()).sum();
    Ok(ImportResult::Created {
        project,
        plan,
        units: created_units,
        summary: Summary {
            unit_count,
            task_count,
        },
    })
}
