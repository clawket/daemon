pub mod activity;
pub mod admin;
pub mod agents;
pub mod audit;
pub mod backlog;
pub mod comments;
pub mod continuation;
pub mod cycles;
pub mod dashboard;
pub mod discover;
pub mod embed;
pub mod error;
pub mod events;
pub mod handoff;
pub mod import_export;
pub mod knowledge;
pub mod labels;
pub mod locks;
pub mod metrics;
pub mod plans;
pub mod projects;
pub mod questions;
pub mod relations;
pub mod runs;
pub mod static_files;
pub mod tasks;
pub mod timeline;
pub mod units;
pub mod usage;
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
        .merge(knowledge::router())
        .merge(runs::router())
        .merge(questions::router())
        .merge(activity::router())
        .merge(admin::router())
        .merge(audit::router())
        .merge(backlog::router())
        .merge(embed::router())
        .merge(events::router())
        .merge(timeline::router())
        .merge(agents::router())
        .merge(dashboard::router())
        .merge(discover::router())
        .merge(wiki::router())
        .merge(handoff::router())
        .merge(continuation::router())
        .merge(import_export::router())
        .merge(locks::router())
        .merge(usage::router())
        .merge(metrics::router())
        .merge(static_files::router())
}
