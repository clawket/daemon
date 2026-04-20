use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub key: Option<String>,
    pub enabled: i64,
    pub wiki_paths: Vec<String>,
    pub cwds: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: Option<String>,
    pub source: String,
    pub source_path: Option<String>,
    pub created_at: i64,
    pub approved_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    pub plan_id: String,
    pub idx: i64,
    pub title: String,
    pub goal: Option<String>,
    pub execution_mode: String,
    pub approval_required: i64,
    pub approved_at: Option<i64>,
    pub approved_by: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cycle {
    pub id: String,
    pub project_id: String,
    pub idx: i64,
    pub title: String,
    pub goal: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub unit_id: String,
    pub cycle_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub ticket_number: Option<String>,
    pub idx: i64,
    pub title: String,
    pub body: String,
    pub priority: String,
    pub complexity: Option<String>,
    pub estimated_edits: Option<i64>,
    #[serde(rename = "type")]
    pub type_: String,
    pub reporter: Option<String>,
    pub assignee: Option<String>,
    pub agent_id: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub status: String,
}
