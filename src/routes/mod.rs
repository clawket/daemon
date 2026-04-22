pub mod activity;
pub mod agents;
pub mod artifacts;
pub mod backlog;
pub mod comments;
pub mod cycles;
pub mod dashboard;
pub mod embed;
pub mod error;
pub mod events;
pub mod handoff;
pub mod import_export;
pub mod labels;
pub mod plans;
pub mod projects;
pub mod questions;
pub mod relations;
pub mod runs;
pub mod static_files;
pub mod tasks;
pub mod timeline;
pub mod units;
pub mod util;
pub mod wiki;

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
        .merge(embed::router())
        .merge(events::router())
        .merge(timeline::router())
        .merge(agents::router())
        .merge(dashboard::router())
        .merge(wiki::router())
        .merge(handoff::router())
        .merge(import_export::router())
        .merge(static_files::router())
}
