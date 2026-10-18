//! The pipeline and the account, from the command line, as subcommands of
//! the server binary. Signing in to Qobuz is the gateway's: `stelly-qobuz
//! login`.
//!
//!   stelly-server crawl --max-tracks 5000
//!   stelly-server analyse --album ID
//!   stelly-server build-space
//!   stelly-server layout
//!
//! The same code the pipeline view drives, so a stage started here and one
//! started from a client do the same thing. The arguments are parsed by argh,
//! from the structs below; `main` gathers them with its own into `Command`.

use crate::pipeline::{analyse, assemble, demo, layout, models, Job, Paths};
use crate::provider::{Provider, RemoteAlbum, RemoteArtist, RemoteTrack};
use crate::schema::{albums, artists, layout as layout_table, tracks};
use crate::{crawl, db, stages, Command};
use anyhow::{bail, Context, Result};
use argh::FromArgs;
use diesel::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use stelly_core::api::Target;

// ---------------------------------------------------------------- arguments

/// Check that the music provider answers, and who it is signed in as.
#[derive(FromArgs)]
#[argh(subcommand, name = "whoami")]
pub struct Whoami {}

/// Have the first music provider convert what a Stelly before providers
/// stored in its own shapes. `serve` does this at start; this does it now.
#[derive(FromArgs)]
#[argh(subcommand, name = "convert-legacy")]
pub struct ConvertLegacy {}

/// Make a provider account's likes one person's: its liked tracks, albums
/// and artists, as if they had been liked in Stelly, when they were liked
/// there. The crawl seeds from them at its next run.
#[derive(FromArgs)]
#[argh(subcommand, name = "import")]
pub struct Import {
    /// the provider whose account's likes they are, by name; see whoami
    #[argh(option)]
    provider: String,
    /// whose likes they become
    #[argh(option)]
    user: String,
    /// only these, comma-separated: tracks, albums, artists (default all)
    #[argh(option, default = "String::from(\"tracks,albums,artists\")")]
    kinds: String,
    /// say what would be imported, and import nothing
    #[argh(switch)]
    dry_run: bool,
}

/// List a provider account's own favourites.
#[derive(FromArgs)]
#[argh(subcommand, name = "favourites")]
pub struct Favourites {
    /// the provider, by name; see whoami
    #[argh(option)]
    provider: String,
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

/// Analyse whole tracks for mixing: beats, bars, phrases, phrase embeddings.
#[derive(FromArgs)]
#[argh(subcommand, name = "mix-analyse")]
pub struct MixAnalyse {
    /// this track
    #[argh(option)]
    track: Option<i64>,
    /// a Qobuz playlist's tracks
    #[argh(option)]
    playlist: Option<i64>,
    /// the family's liked tracks and albums
    #[argh(switch)]
    favourites: bool,
    /// the tracks in the space nearest this description, --count of them
    #[argh(option)]
    mood: Option<String>,
    /// this album, catalogued first if need be
    #[argh(option)]
    album: Option<String>,
    /// this artist's releases, catalogued first if need be
    #[argh(option)]
    artist: Option<i64>,
    /// this track and its nearest in the space
    #[argh(option)]
    around: Option<i64>,
    /// every track in the space credited to this name, and their nearest
    #[argh(option)]
    artist_name: Option<String>,
    /// the space's tracks of these genres, comma-separated
    #[argh(option)]
    genre: Option<String>,
    /// how many nearest to each with --around or --artist-name, or in all
    /// with --mood (default 20)
    #[argh(option, default = "crate::mix::AROUND")]
    count: usize,
    /// with --genre, at most this many; 0 for all (default 0)
    #[argh(option, default = "0")]
    limit: usize,
    /// analysis workers; 0 picks from the cores (default 0)
    #[argh(option, default = "0")]
    workers: usize,
    /// try tracks that failed before, too
    #[argh(switch)]
    retry_failed: bool,
}

/// Grow the catalogue from the family's likes outwards.
#[derive(FromArgs)]
#[argh(subcommand, name = "crawl")]
pub struct Crawl {
    /// stop once the catalogue holds this many tracks (default 5000)
    #[argh(option, default = "5000")]
    max_tracks: i64,
    /// how many similar-artist hops from a like to follow (default 2)
    #[argh(option, default = "stelly_core::api::DEFAULT_MAX_DISTANCE")]
    max_distance: i64,
    /// skip seeding from the likes
    #[argh(switch)]
    no_seed: bool,
}

/// Extract descriptors and CLAP embeddings for every pending track, or for one
/// track, album or artist, catalogued first if need be.
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
    /// just this track
    #[argh(option)]
    track: Option<i64>,
    /// just this album
    #[argh(option)]
    album: Option<String>,
    /// just this artist's own releases
    #[argh(option)]
    artist: Option<i64>,
}

/// Merge tracks that are one recording: newly analysed tracks whose CLAP
/// embeddings all but coincide with another's, at about the same length.
#[derive(FromArgs)]
#[argh(subcommand, name = "dedupe")]
pub struct Dedupe {
    /// how alike two embeddings must be, cosine similarity, when either
    /// track has no ISRC (default 0.995)
    #[argh(option, default = "crate::dedupe::MIN_SIMILARITY")]
    min_similarity: f32,
    /// the same, between tracks with different ISRCs (default 0.999)
    #[argh(option, default = "crate::dedupe::MIN_SIMILARITY_OTHER_ISRC")]
    min_similarity_other_isrc: f32,
    /// say what would be merged, and merge nothing
    #[argh(switch)]
    dry_run: bool,
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
    /// where to build it, e.g. /tmp/stelly-demo
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
        Command::Whoami(_) => block(Box::pin(whoami(paths))),
        Command::ConvertLegacy(_) => block(Box::pin(async {
            let report = |line: String| eprintln!("{line}");
            let provider = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();
            let done = crate::legacy::run(&paths.db_path, &provider, &report).await?;
            println!("{} stored items converted, {} dropped", done.converted, done.dropped);
            Ok(())
        })),
        Command::Import(args) => block(Box::pin(import_likes(args, paths))),
        Command::Favourites(args) => block(Box::pin(favourites(args, paths))),
        Command::Crawl(args) => block(Box::pin(crawl_command(args, paths))),
        Command::Analyse(args) => block(Box::pin(analyse_command(args, paths))),
        Command::MixAnalyse(args) => block(Box::pin(mix_analyse_command(args, paths))),
        Command::Dedupe(args) => {
            let options = crate::dedupe::Options {
                min_similarity: args.min_similarity,
                min_similarity_other_isrc: args.min_similarity_other_isrc,
                dry_run: args.dry_run,
            };
            crate::dedupe::run(paths, &options, &Job::stderr()).map(|_| ())
        }
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
        | Command::User(_)
        | Command::Pair(_)
        | Command::Devices(_)
        | Command::Revoke(_) => unreachable!("main runs the serving commands"),
    }
}

// ------------------------------------------------------------------ account

async fn whoami(paths: &Paths) -> Result<()> {
    let provider = Provider::from_env(&paths.env_dir, &paths.db_path);
    let field = |value: Option<String>| value.unwrap_or_else(|| "?".into());
    for (n, (what, url, account)) in provider.accounts().await.into_iter().enumerate() {
        if n > 0 {
            println!();
        }
        println!("provider    : {what}, at {url}");
        match account {
            Ok(account) => {
                println!("account id  : {}", field(account.id));
                println!("email       : {}", field(account.email));
                println!("display name: {}", field(account.name));
                println!("subscription: {}", field(account.subscription));
            }
            Err(err) => println!("account     : {err:#}"),
        }
    }
    Ok(())
}

async fn import_likes(args: Import, paths: &Paths) -> Result<()> {
    use crate::likes::{self, Liked};
    let user = crate::auth::AuthStore::new(&paths.db_path)?.user(&args.user)?;
    let provider = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();
    let now = likes::now();
    for kind in args.kinds.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        // Each with when it was liked there, if the provider says.
        let found: Vec<(String, Liked, Option<i64>)> = match kind {
            "tracks" => provider
                .favourite_tracks(&args.provider, usize::MAX)
                .await?
                .into_iter()
                .map(|t| (t.id.to_string(), t.liked_at, Liked::Track(RemoteTrack { liked_at: None, ..t })))
                .map(|(id, at, liked)| (id, liked, at))
                .collect(),
            "albums" => provider
                .favourite_albums(&args.provider, usize::MAX)
                .await?
                .into_iter()
                .map(|a| (a.id.clone(), a.liked_at, Liked::Album(RemoteAlbum { liked_at: None, ..a })))
                .map(|(id, at, liked)| (id, liked, at))
                .collect(),
            "artists" => provider
                .favourite_artists(&args.provider, usize::MAX)
                .await?
                .into_iter()
                .map(|a| (a.id.to_string(), a.liked_at, Liked::Artist(RemoteArtist { liked_at: None, ..a })))
                .map(|(id, at, liked)| (id, liked, at))
                .collect(),
            other => anyhow::bail!("{other:?} is not tracks, albums or artists"),
        };
        if args.dry_run {
            println!("{}: {} {kind} to import for {}", args.provider, found.len(), user.name);
            continue;
        }
        let mut added = 0;
        for (id, liked, at) in &found {
            added += likes::record(&paths.db_path, user.id, id, liked, at.unwrap_or(now))?;
        }
        println!("{}: {added} new liked {kind} for {}, of {}", args.provider, user.name, found.len());
    }
    Ok(())
}

async fn favourites(args: Favourites, paths: &Paths) -> Result<()> {
    let kind = args.kind;
    let cap = if args.limit == 0 { usize::MAX } else { args.limit };
    let provider = Provider::from_env(&paths.env_dir, &paths.db_path);
    let name = args.provider.as_str();
    let lines: Vec<String> = match kind {
        Kind::Tracks => provider
            .favourite_tracks(name, cap)
            .await?
            .iter()
            .map(|t| format!("{:>12}  {} - {}  [{}]", t.id, t.artist, t.title, t.album))
            .collect(),
        Kind::Albums => provider
            .favourite_albums(name, cap)
            .await?
            .iter()
            .map(|a| format!("{:>12}  {} - {}", a.id, a.artist, a.title))
            .collect(),
        Kind::Artists => provider
            .favourite_artists(name, cap)
            .await?
            .iter()
            .map(|a| format!("{:>12}  {}", a.id, a.name))
            .collect(),
    };
    for line in &lines {
        println!("{line}");
    }
    eprintln!("\n{} {} shown", lines.len(), kind.as_str());
    Ok(())
}

// ----------------------------------------------------------------- pipeline

async fn crawl_command(args: Crawl, paths: &Paths) -> Result<()> {
    let conn = &mut db::open_for_write(&paths.db_path)?;
    // How fast is the gateway's to say, for the whole account.
    let mut client = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();

    if !args.no_seed {
        eprintln!("seeding from the family's likes…");
        let seeded = crawl::seed(conn)?;
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
        only: None,
    };
    let targets: Vec<Target> = [
        args.track.map(Target::Track),
        args.album.map(Target::Album),
        args.artist.map(Target::Artist),
    ]
    .into_iter()
    .flatten()
    .collect();
    match targets.as_slice() {
        [] => {
            analyse::run(paths, &options, &Job::stderr()).await?;
        }
        [target] => {
            stages::analyse_target(target, paths, options, &Job::stderr()).await?;
        }
        _ => bail!("one of --track, --album and --artist at a time"),
    }
    Ok(())
}

async fn mix_analyse_command(args: MixAnalyse, paths: &Paths) -> Result<()> {
    use stelly_mix::api::Selection;
    let mut picks: Vec<Selection> = Vec::new();
    picks.extend(args.track.map(|id| Selection::Track { id }));
    picks.extend(args.album.map(|id| Selection::Album { id }));
    picks.extend(args.artist.map(|id| Selection::Artist { id }));
    picks.extend(args.playlist.map(|id| Selection::Playlist { id }));
    picks.extend(args.favourites.then_some(Selection::Favourites));
    picks.extend(args.mood.map(|phrase| Selection::Mood { phrase, count: args.count }));
    picks.extend(args.around.map(|seed| Selection::Around { seed, count: args.count }));
    picks.extend(args.artist_name.map(|name| Selection::ArtistName { name, count: args.count }));
    picks.extend(args.genre.map(|genres| Selection::Space {
        genres: genres.split(',').map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect(),
        limit: args.limit,
    }));
    let [selection] = picks.as_slice() else {
        bail!("one of --track, --album, --artist, --playlist, --favourites, --mood, --around, --artist-name and --genre at a time");
    };
    let job = Job::stderr();
    job.log(format!("choosing {selection}"));
    let ids = crate::mix::select(paths, selection, &job).await?;
    let options = crate::mix::Options {
        workers: args.workers,
        retry_failed: args.retry_failed,
    };
    let paths = paths.clone();
    // Its workers block; off the runtime it was asked from.
    let stats = tokio::task::spawn_blocking(move || crate::mix::run(&paths, &ids, &options, &job)).await??;
    println!("{} mix-analysed, {} failed, of {}", stats.done, stats.failed, stats.total);
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
    if let Ok(space) = stelly_core::space::Space::load(&paths.data_dir) {
        let m = &space.manifest;
        println!("{:<18} {} x {} (built {})", "space", m.n_tracks, m.n_dims, m.built_at);
    }
    Ok(())
}

/// Space sanity: do tracks from one album land near each other? If not, the
/// space is noise and no path logic will rescue it. Worth watching whenever
/// the weights or labels change.
fn evaluate(paths: &Paths) -> Result<()> {
    let space = stelly_core::space::Space::load(&paths.data_dir)?;
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
            let order = stelly_core::space::argsort_desc(&scores);
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
        "\ndemo corpus ready in {}\nserve it with:\n  STELLY_DATA_DIR={} stelly-server serve",
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
