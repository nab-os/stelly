//! The pipeline and the account, from the command line, as subcommands of
//! the server binary:
//!
//!   two-khz-server login              sign in through the browser
//!   two-khz-server crawl --max-tracks 5000
//!   two-khz-server analyse
//!   two-khz-server build-space
//!   two-khz-server layout
//!
//! The same code the pipeline view drives, so a stage started here and one
//! started from a client do the same thing. The arguments are parsed by argh,
//! from the structs below; `main` gathers them with its own into `Command`.

use crate::pipeline::{analyse, assemble, demo, layout, models, Job, Paths};
use crate::qobuz::{Credentials, QobuzClient, DEFAULT_RATE_PER_SEC};
use crate::schema::{albums, artists, layout as layout_table, tracks};
use crate::{crawl, db, login, stages, Command};
use anyhow::{bail, Context, Result};
use argh::FromArgs;
use diesel::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

// ---------------------------------------------------------------- arguments

/// Sign in through the browser; the token goes into .env.
#[derive(FromArgs)]
#[argh(subcommand, name = "login")]
pub struct Login {
    /// seconds to wait for the sign-in to finish (default 300)
    #[argh(option, default = "300")]
    timeout: u64,
    /// print the sign-in URL rather than opening a browser
    #[argh(switch)]
    no_browser: bool,
}

/// Re-scrape the app id and secrets from the web player.
#[derive(FromArgs)]
#[argh(subcommand, name = "refresh-credentials")]
pub struct RefreshCredentials {
    /// update .env in place rather than printing the values
    #[argh(switch)]
    write: bool,
}

/// Check the credentials.
#[derive(FromArgs)]
#[argh(subcommand, name = "whoami")]
pub struct Whoami {}

/// List the account's favourites.
#[derive(FromArgs)]
#[argh(subcommand, name = "favourites")]
pub struct Favourites {
    /// tracks, albums or artists (default tracks)
    #[argh(option, default = "Kind::Tracks")]
    kind: Kind,
    /// show at most this many; 0 for all (default 0)
    #[argh(option, default = "0")]
    limit: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Tracks,
    Albums,
    Artists,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Tracks => "tracks",
            Kind::Albums => "albums",
            Kind::Artists => "artists",
        }
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "tracks" => Ok(Kind::Tracks),
            "albums" => Ok(Kind::Albums),
            "artists" => Ok(Kind::Artists),
            _ => Err("--kind is tracks, albums or artists".into()),
        }
    }
}

/// Grow the catalogue from the favourites outwards, or pull in one artist or
/// album.
#[derive(FromArgs)]
#[argh(subcommand, name = "crawl")]
pub struct Crawl {
    /// stop once the catalogue holds this many tracks (default 5000)
    #[argh(option, default = "5000")]
    max_tracks: i64,
    /// how many similar-artist hops from a favourite to follow (default 2)
    #[argh(option, default = "two_khz::api::DEFAULT_MAX_DISTANCE")]
    max_distance: i64,
    /// requests per second to Qobuz (default 2)
    #[argh(option, default = "DEFAULT_RATE_PER_SEC")]
    rate: f64,
    /// skip seeding from the favourites
    #[argh(switch)]
    no_seed: bool,
    /// queue one artist's discography instead of crawling
    #[argh(option)]
    artist: Option<i64>,
    /// fetch one album's tracklist instead of crawling
    #[argh(option)]
    album: Option<String>,
}

/// Extract descriptors and CLAP embeddings for every pending track.
#[derive(FromArgs)]
#[argh(subcommand, name = "analyse")]
pub struct Analyse {
    /// analyse at most this many; 0 for all (default 0)
    #[argh(option, default = "0")]
    limit: usize,
    /// try tracks that failed before, too
    #[argh(switch)]
    retry_failed: bool,
    /// extraction workers; 0 picks one per four cores, up to four (default 0)
    #[argh(option, default = "0")]
    workers: usize,
    /// excerpts fetched at once; 0 follows the workers (default 0)
    #[argh(option, default = "0")]
    fetchers: usize,
    /// size of the excerpt cache in GB (default 20)
    #[argh(option, default = "20.0")]
    cache_gb: f64,
}

/// Build the space the map and the paths are drawn in.
#[derive(FromArgs)]
#[argh(subcommand, name = "build-space")]
pub struct BuildSpace {
    /// a JSON object of block name to weight, e.g. '{"tempo": 2.0}'
    #[argh(option, from_str_fn(weights))]
    weights: Option<HashMap<String, f32>>,
}

fn weights(text: &str) -> Result<HashMap<String, f32>, String> {
    let weights: HashMap<String, f32> = serde_json::from_str(text)
        .map_err(|err| format!("--weights is a JSON object of block name to weight: {err}"))?;
    if let Some(unknown) = weights.keys().find(|k| !assemble::BLOCKS.contains(&k.as_str())) {
        return Err(format!(
            "no block called {unknown}; the blocks are {}",
            assemble::BLOCKS.join(", ")
        ));
    }
    Ok(weights)
}

/// Lay the space out in two dimensions for the map.
#[derive(FromArgs)]
#[argh(subcommand, name = "layout")]
pub struct Layout {
    /// neighbours per track in the UMAP graph (default 15)
    #[argh(option, default = "15")]
    neighbours: usize,
    /// how tightly UMAP packs points (default 0.1)
    #[argh(option, default = "0.1")]
    min_dist: f64,
    /// optimisation epochs; 0 picks by corpus size (default 0)
    #[argh(option, default = "0")]
    epochs: usize,
}

/// Fetch the CLAP weights now, not on first use.
#[derive(FromArgs)]
#[argh(subcommand, name = "models")]
pub struct Models {}

/// Row counts, and what is due.
#[derive(FromArgs)]
#[argh(subcommand, name = "status")]
pub struct Status {}

/// Do tracks from one album land together in the space?
#[derive(FromArgs)]
#[argh(subcommand, name = "evaluate")]
pub struct Evaluate {}

/// Build a synthetic corpus, no Qobuz needed.
#[derive(FromArgs)]
#[argh(subcommand, name = "demo")]
pub struct Demo {
    /// where to build it, e.g. /tmp/two-khz-demo
    #[argh(positional)]
    dir: PathBuf,
}

/// Hide an artist everywhere.
#[derive(FromArgs)]
#[argh(subcommand, name = "block")]
pub struct Block {
    /// an artist id, or part of a name
    #[argh(positional)]
    artist: String,
    /// why, for the list
    #[argh(option)]
    reason: Option<String>,
    /// block every artist the name matches
    #[argh(switch)]
    all: bool,
    /// delete their stored data too, not just hide it
    #[argh(switch)]
    purge: bool,
}

/// Lift a block.
#[derive(FromArgs)]
#[argh(subcommand, name = "unblock")]
pub struct Unblock {
    /// an artist id, or part of a name
    #[argh(positional)]
    artist: String,
}

/// List the blocked artists.
#[derive(FromArgs)]
#[argh(subcommand, name = "blocked")]
pub struct Blocked {}

// ---------------------------------------------------------------- dispatch

/// Run one of this module's commands. `main` handles the serving ones itself
/// and never passes them here.
pub fn run(command: Command, paths: &Paths) -> Result<()> {
    let block = |future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>>| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting a runtime")?
            .block_on(future)
    };

    match command {
        Command::Login(args) => block(Box::pin(login_command(args, paths))),
        Command::RefreshCredentials(args) => block(Box::pin(refresh_credentials(args, paths))),
        Command::Whoami(_) => block(Box::pin(whoami(paths))),
        Command::Favourites(args) => block(Box::pin(favourites(args, paths))),
        Command::Crawl(args) => block(Box::pin(crawl_command(args, paths))),
        Command::Analyse(args) => block(Box::pin(analyse_command(args, paths))),
        Command::BuildSpace(args) => block(Box::pin(assemble::run(paths, args.weights, &Job::stderr()))),
        Command::Layout(args) => layout_command(args, paths),
        Command::Models(_) => block(Box::pin(async {
            models::ensure_all(&paths.model_dir, &Job::stderr()).await?;
            println!("models ready in {}", paths.model_dir.display());
            Ok(())
        })),
        Command::Status(_) => status(paths),
        Command::Evaluate(_) => evaluate(paths),
        Command::Demo(args) => block(Box::pin(demo_command(args))),
        Command::Block(args) => block_command(args, paths),
        Command::Unblock(args) => unblock_command(args, paths),
        Command::Blocked(_) => blocked(paths),
        Command::Serve(_)
        | Command::Pair(_)
        | Command::Devices(_)
        | Command::Revoke(_)
        | Command::BuildCatalog(_) => unreachable!("main runs the serving commands"),
    }
}

// ------------------------------------------------------------------ account

fn client(paths: &Paths, rate: f64) -> Result<QobuzClient> {
    Ok(QobuzClient::new(Credentials::from_env(&paths.env_dir)?).with_rate(rate))
}

async fn whoami(paths: &Paths) -> Result<()> {
    let user = client(paths, DEFAULT_RATE_PER_SEC)?.user().await?;
    let field = |key: &str| user.get(key).and_then(|v| v.as_str()).unwrap_or("?").to_string();
    let credential = user.get("credential").cloned().unwrap_or_default();
    let subscription = credential
        .get("label")
        .or_else(|| credential.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("user id     : {}", user.get("id").map(|v| v.to_string()).unwrap_or_default());
    println!("email       : {}", field("email"));
    println!("display name: {}", field("display_name"));
    println!("subscription: {subscription}");
    Ok(())
}

async fn login_command(args: Login, paths: &Paths) -> Result<()> {
    eprintln!("fetching the Qobuz web player bundle…");
    let bundle = login::fetch_bundle().await?;
    let timeout = Duration::from_secs(args.timeout);
    let token = login::browser_login(&bundle, timeout, !args.no_browser).await?;
    login::update_env(
        &paths.env_dir,
        &[
            ("QOBUZ_APP_ID", bundle.app_id.clone()),
            ("QOBUZ_APP_SECRETS", bundle.secrets.join(",")),
            ("QOBUZ_USER_AUTH_TOKEN", token),
        ],
    )?;
    println!("logged in; token saved to {}", paths.env_dir.join(".env").display());
    whoami(paths).await
}

async fn refresh_credentials(args: RefreshCredentials, paths: &Paths) -> Result<()> {
    eprintln!("fetching the Qobuz web player bundle…");
    let bundle = login::fetch_bundle().await?;
    let joined = bundle.secrets.join(",");
    if !args.write {
        println!("\nAdd these to your .env:\n");
        println!("QOBUZ_APP_ID={}", bundle.app_id);
        println!("QOBUZ_APP_SECRETS={joined}");
        println!("\n(re-run with --write to update the file in place)");
        return Ok(());
    }
    login::update_env(
        &paths.env_dir,
        &[("QOBUZ_APP_ID", bundle.app_id.clone()), ("QOBUZ_APP_SECRETS", joined)],
    )?;
    println!("updated .env with the app id and {} candidate secrets", bundle.secrets.len());
    Ok(())
}

async fn favourites(args: Favourites, paths: &Paths) -> Result<()> {
    let kind = args.kind;
    let cap = if args.limit == 0 { usize::MAX } else { args.limit };
    let items = client(paths, DEFAULT_RATE_PER_SEC)?.favorites_raw(kind.as_str(), cap).await?;

    let name = |v: &serde_json::Value, key: &str, field: &str| -> String {
        v.get(key)
            .and_then(|o| o.get(field))
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let text = |v: &serde_json::Value, key: &str| v.get(key).and_then(|s| s.as_str()).unwrap_or("?").to_string();
    for item in &items {
        let id = item.get("id").map(|v| v.to_string()).unwrap_or_default();
        match kind {
            Kind::Tracks => println!(
                "{id:>12}  {} - {}  [{}]",
                name(item, "performer", "name"),
                text(item, "title"),
                name(item, "album", "title")
            ),
            Kind::Albums => println!("{id:>12}  {} - {}", name(item, "artist", "name"), text(item, "title")),
            Kind::Artists => println!("{id:>12}  {}", text(item, "name")),
        }
    }
    eprintln!("\n{} {} shown", items.len(), kind.as_str());
    Ok(())
}

// ----------------------------------------------------------------- pipeline

async fn crawl_command(args: Crawl, paths: &Paths) -> Result<()> {
    let conn = &mut db::open_for_write(&paths.db_path)?;
    let mut client = client(paths, args.rate)?;
    client.login().await.context("signing in to Qobuz")?;

    if let Some(artist) = args.artist {
        let queued = crawl::discover_artist(conn, &mut client, artist).await?;
        println!("artist {artist}: {queued} albums queued");
        return Ok(());
    }
    if let Some(album) = &args.album {
        let written = crawl::crawl_one_album(conn, &mut client, album).await?;
        println!("album {album}: {written} tracks");
        return Ok(());
    }

    if !args.no_seed {
        eprintln!("seeding from favourites…");
        let seeded = crawl::seed(conn, &mut client, 5000).await?;
        eprintln!(
            "  seeded {} tracks, {} albums, {} artists",
            seeded.tracks_added, seeded.albums_expanded, seeded.artists_expanded
        );
    }

    let (max_tracks, max_distance) = (args.max_tracks, args.max_distance);
    eprintln!("crawling to {max_tracks} tracks, max {max_distance} hops");
    let report = |stats: &crawl::Stats, tracks: i64| {
        eprintln!(
            "  artists={} albums={} tracks={tracks} errors={}",
            stats.artists_expanded, stats.albums_expanded, stats.errors
        );
    };
    let stats = crawl::crawl(conn, &mut client, max_tracks, max_distance, Some(&report)).await?;
    println!(
        "expanded {} artists and {} albums, {} tracks added, {} errors, {} blocked",
        stats.artists_expanded,
        stats.albums_expanded,
        stats.tracks_added,
        stats.errors,
        stats.blocked_skipped
    );
    Ok(())
}

async fn analyse_command(args: Analyse, paths: &Paths) -> Result<()> {
    let options = analyse::Options {
        limit: args.limit,
        retry_failed: args.retry_failed,
        cache_gb: args.cache_gb,
        workers: args.workers,
        fetchers: args.fetchers,
    };
    let limiter = client(paths, DEFAULT_RATE_PER_SEC)?.rate_limit();
    analyse::run(paths, limiter, &options, &Job::stderr()).await?;
    Ok(())
}

fn layout_command(args: Layout, paths: &Paths) -> Result<()> {
    let options = layout::Options {
        neighbours: args.neighbours,
        min_dist: args.min_dist,
        epochs: args.epochs,
    };
    layout::run(paths, &options, &Job::stderr())?;
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let corpus = stages::corpus(&paths.db_path)?;
    let conn = &mut db::open_for_write(&paths.db_path)?;
    for (label, n) in [
        ("artists", artists::table.count().get_result(conn).unwrap_or(0)),
        ("albums", albums::table.count().get_result(conn).unwrap_or(0)),
        ("tracks", corpus.tracks),
        ("features", corpus.analysed),
        ("layout", layout_table::table.count().get_result(conn).unwrap_or(0)),
        ("failures", corpus.failed),
        ("frontier_pending", corpus.pending),
        ("to_analyse", corpus.to_analyse),
    ] {
        println!("{label:<18} {n:>8}");
    }
    if let Ok(space) = two_khz::space::Space::load(&paths.data_dir) {
        let m = &space.manifest;
        println!("{:<18} {} x {} (built {})", "space", m.n_tracks, m.n_dims, m.built_at);
    }
    Ok(())
}

/// Space sanity: do tracks from one album land near each other? If not, the
/// space is noise and no path logic will rescue it. Worth watching whenever
/// the weights or labels change.
fn evaluate(paths: &Paths) -> Result<()> {
    let space = two_khz::space::Space::load(&paths.data_dir)?;
    let weighted = space.weighted(&space.default_weights());
    let conn = &mut db::open_for_write(&paths.db_path)?;
    let albums: HashMap<i64, String> = tracks::table
        .filter(tracks::album_id.is_not_null())
        .select((tracks::id, tracks::album_id.assume_not_null()))
        .load::<(i64, String)>(conn)?
        .into_iter()
        .collect();

    let mut by_album: HashMap<&str, Vec<usize>> = HashMap::new();
    for (row, id) in space.manifest.track_ids.iter().enumerate() {
        if let Some(album) = albums.get(id) {
            by_album.entry(album).or_default().push(row);
        }
    }
    by_album.retain(|_, rows| rows.len() > 1);
    if by_album.is_empty() {
        bail!("not enough multi-track albums to evaluate");
    }

    let mut ranks = Vec::new();
    for rows in by_album.values() {
        for &row in rows {
            let mut scores = weighted.similarities(weighted.row(row));
            scores[row] = f32::NEG_INFINITY;
            let order = two_khz::space::argsort_desc(&scores);
            let rank = order
                .iter()
                .position(|j| *j != row && rows.contains(j))
                .map_or(order.len(), |p| p + 1);
            ranks.push(rank);
        }
    }
    ranks.sort_unstable();
    let top1 = ranks.iter().filter(|&&r| r == 1).count();
    let median = ranks[ranks.len() / 2];
    let mean = ranks.iter().sum::<usize>() as f64 / ranks.len() as f64;
    println!("albums evaluated      : {}", by_album.len());
    println!("tracks evaluated      : {}", ranks.len());
    println!(
        "same-album is top-1   : {top1}/{} ({:.1}%)",
        ranks.len(),
        100.0 * top1 as f64 / ranks.len() as f64
    );
    println!("median rank           : {median}");
    println!("mean rank             : {mean:.1}  (of {} tracks)", weighted.n_tracks);
    Ok(())
}

async fn demo_command(args: Demo) -> Result<()> {
    let root = std::path::absolute(&args.dir)?;
    let paths = demo::build(&root, &Job::stderr()).await?;
    println!(
        "\ndemo corpus ready in {}\nserve it with:\n  TWO_KHZ_DATA_DIR={} two-khz-server serve",
        paths.data_dir.display(),
        paths.data_dir.display()
    );
    Ok(())
}

// ------------------------------------------------------------------- hiding

fn block_command(args: Block, paths: &Paths) -> Result<()> {
    let needle = &args.artist;
    let conn = &mut db::open_for_write(&paths.db_path)?;
    let matches = db::resolve_artist(conn, needle)?;
    if matches.is_empty() {
        bail!("no artist matching {needle:?} in the catalogue; block by numeric id if they have not been crawled yet");
    }
    // Blocking the wrong person is worse than an extra keystroke.
    if matches.len() > 1 && !args.all {
        eprintln!("{} artists match {needle:?}:", matches.len());
        for m in &matches {
            eprintln!("  {:>10}  {}  ({} tracks)", m.id, m.name, m.tracks);
        }
        bail!("re-run with an id, or --all to block every match");
    }

    for m in &matches {
        db::block_artist(&paths.db_path, m.id, &m.name, args.reason.as_deref())?;
        println!("blocked {} ({})", m.name, m.id);
        if args.purge {
            let purged = db::purge(conn, m.id)?;
            println!(
                "  purged {} tracks, {} features, {} albums",
                purged.tracks, purged.features, purged.albums
            );
        }
    }
    if args.purge {
        println!("\nRe-run build-space and layout to drop them from the map.");
    } else {
        println!("\nHidden everywhere from now on; no rebuild needed.");
    }
    Ok(())
}

fn unblock_command(args: Unblock, paths: &Paths) -> Result<()> {
    let needle = &args.artist;
    let blocked = db::blocked_artists(&paths.db_path)?;
    let lifted: Vec<_> = blocked
        .iter()
        .filter(|b| {
            b.artist_id.to_string() == *needle
                || b.name.to_lowercase().contains(&needle.to_lowercase())
        })
        .collect();
    if lifted.is_empty() {
        bail!("no block found for {needle:?}");
    }
    for b in lifted {
        db::unblock_artist(&paths.db_path, b.artist_id)?;
        println!("unblocked {} ({})", b.name, b.artist_id);
    }
    println!("Purged data does not come back; re-crawl to restore it.");
    Ok(())
}

fn blocked(paths: &Paths) -> Result<()> {
    let rows = db::blocked_artists(&paths.db_path)?;
    if rows.is_empty() {
        println!("no artists blocked");
    }
    for b in rows {
        let reason = b.reason.map(|r| format!("  - {r}")).unwrap_or_default();
        println!("{:>10}  {}{reason}", b.artist_id, b.name);
    }
    Ok(())
}
