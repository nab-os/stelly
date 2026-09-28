//! Running a pipeline stage, and the counts that say which one is due.
//!
//! Stages run in this process. The crawl has its own loop in `hub`, since it
//! runs until stopped rather than until done; the other three come through
//! here, from the pipeline view and the command line alike.
//!
//! Analyse can be pointed at one track, album or artist, which it catalogues
//! first, so a single album reaches the space without the backlog.

use crate::pipeline::{analyse, assemble, layout, Job, Paths};
use crate::qobuz::{Credentials, QobuzClient, RateLimit};
use crate::schema::{failures, features, frontier, tracks};
use crate::{crawl, db};
use anyhow::{bail, Result};
use diesel::prelude::*;
use std::path::Path;
use two_khz::api::{Corpus, Stage, Target};

/// Run one terminating stage to completion. Analyse holds a blocking database
/// connection across awaits, so drive it on a thread of its own.
pub async fn run(stage: Stage, target: Option<&Target>, paths: &Paths, limiter: RateLimit, job: &Job) -> Result<()> {
    match (stage, target) {
        (Stage::Crawl, _) => bail!("the crawl runs from its own loop, not as a stage"),
        (Stage::Analyse, Some(target)) => {
            analyse_target(target, paths, limiter, analyse::Options::default(), job).await?;
        }
        (Stage::Analyse, None) => {
            analyse::run(paths, limiter, &analyse::Options::default(), job).await?;
        }
        (Stage::BuildSpace, _) => {
            assemble::run(paths, None, job).await?;
        }
        (Stage::Layout, _) => {
            layout::run(paths, &layout::Options::default(), job)?;
        }
    }
    Ok(())
}

/// Catalogue one target, then analyse its tracks and nothing else. Past
/// failures are retried: asking by name is asking again. An error when none
/// of it could be analysed, so the caller does not wait for it on the map.
pub async fn analyse_target(
    target: &Target,
    paths: &Paths,
    limiter: RateLimit,
    options: analyse::Options,
    job: &Job,
) -> Result<analyse::Stats> {
    job.log(format!("cataloguing {target}"));
    let ids = {
        let conn = &mut db::open_for_write(&paths.db_path)?;
        let credentials = Credentials::from_env(&paths.env_dir)?;
        let mut client = QobuzClient::sharing(credentials, limiter.clone());
        crawl::catalogue_target(conn, &mut client, target, &|line| job.log(line)).await?
    };
    if ids.is_empty() {
        bail!("{target} has no tracks");
    }

    let options = analyse::Options {
        only: Some(ids),
        retry_failed: true,
        ..options
    };
    let stats = analyse::run(paths, limiter, &options, job).await?;
    if stats.total > 0 && stats.done == 0 && !job.cancelled() {
        bail!("none of {target} could be analysed");
    }
    Ok(stats)
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
