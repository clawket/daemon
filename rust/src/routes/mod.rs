pub mod activity;
pub mod artifacts;
pub mod backlog;
pub mod comments;
pub mod cycles;
pub mod error;
pub mod labels;
pub mod plans;
pub mod projects;
pub mod questions;
pub mod relations;
pub mod runs;
pub mod tasks;
pub mod units;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(projects::router())
        .merge(plans::router())
        .merge(units::router())
        .merge(cycles::router())
        .merge(tasks::router())
        .merge(comments::router())
        .merge(labels::router())
        .merge(relations::router())
        .merge(artifacts::router())
        .merge(runs::router())
        .merge(questions::router())
        .merge(activity::router())
        .merge(backlog::router())
}
