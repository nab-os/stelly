//! Running a pipeline stage, and the counts that say which one is due.
//!
//! Stages run in this process. The crawl has its own loop in `hub`, since it
//! runs until stopped rather than until done; the other three come through
//! here, from the pipeline view and the command line alike.

use crate::pipeline::{analyse, assemble, layout, Job, Paths};
use crate::qobuz::RateLimit;
use crate::schema::{failures, features, frontier, tracks};
use anyhow::{bail, Result};
use diesel::prelude::*;
use std::path::Path;
use two_khz::api::{Corpus, Stage};

/// Run one terminating stage to completion. Analyse holds a blocking database
/// connection across awaits, so drive it on a thread of its own.
pub async fn run(stage: Stage, paths: &Paths, limiter: RateLimit, job: &Job) -> Result<()> {
    match stage {
        Stage::Crawl => bail!("the crawl runs from its own loop, not as a stage"),
        Stage::Analyse => {
            analyse::run(paths, limiter, &analyse::Options::default(), job).await?;
        }
        Stage::BuildSpace => {
            assemble::run(paths, None, job).await?;
        }
        Stage::Layout => {
            layout::run(paths, &layout::Options::default(), job)?;
        }
    }
    Ok(())
}

/// Re-read the counts that tell you what still needs running. `in_space` and
/// `on_map` stay zero: they are questions about what a client has loaded. See
/// `api::Corpus`.
pub fn corpus(db_path: &Path) -> Result<Corpus> {
    let conn = &mut crate::db::open_for_write(db_path)?;

    // Asked the way `build-space` asks it.
    let buildable = tracks::table
        .inner_join(features::table)
        .filter(features::clap_f32.is_not_null())
        .filter(crate::db::not_blocked())
        .count()
        .get_result(conn)
        .unwrap_or(0);

    Ok(Corpus {
        tracks: tracks::table.count().get_result(conn).unwrap_or(0),
        analysed: features::table.count().get_result(conn).unwrap_or(0),
        to_analyse: analyse::pending_count(conn).unwrap_or(0),
        failed: failures::table.count().get_result(conn).unwrap_or(0),
        pending: frontier::table
            .filter(frontier::state.eq("pending"))
            .count()
            .get_result(conn)
            .unwrap_or(0),
        buildable,
        in_space: 0,
        on_map: 0,
    })
}
