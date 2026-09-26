//! `two-khz-server`: everything but the screen.
//!
//! Holds the Qobuz token, the shared rate limit, the database, the CLAP
//! models and the whole pipeline, crawl, analyse, build-space, layout. Clients
//! get a slim catalogue and the vectors, and ask for the rest over HTTP.
//!
//! ```sh
//! two-khz-server login                                  # Qobuz, once
//! two-khz-server pair --name desktop --scope pipeline   # first device
//! two-khz-server serve                                  # 127.0.0.1:7700
//! ```
//!
//! Plain HTTP, loopback by default. The token and the signed stream URLs are
//! credentials in flight, so anything further belongs behind a VPN or TLS.

mod auth;
mod catalog;
mod cli;
mod crawl;
mod db;
mod hub;
mod login;
mod pipeline;
mod qobuz;
mod routes;
mod schema;
mod stages;
mod text;

use anyhow::{Context, Result};
use argh::FromArgs;
use auth::AuthStore;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hub::Hub;
use pipeline::Paths;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use two_khz::api::Scope;

const DEFAULT_BIND: &str = "127.0.0.1:7700";

#[derive(Clone)]
pub struct AppState {
    /// Qobuz, the block list, the crawl and the stages. The routes only
    /// translate to and from it.
    pub hub: Arc<Hub>,
    pub auth: Arc<AuthStore>,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
}

// ------------------------------------------------------------------ errors

/// A failed request, as the client can turn back into an `anyhow::Error`.
pub struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    pub fn unauthorised(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

/// Report what actually went wrong. Deliberately not sanitised: single-user
/// system behind a VPN, and a real message beats sending someone to the logs
/// on another machine.
impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{err:#}"),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(two_khz::api::ApiError {
                message: self.message,
            }),
        )
            .into_response()
    }
}

// --------------------------------------------------------------------- cli

/// two-khz-server: everything in 2kHz but the screen.
#[derive(FromArgs)]
#[argh(
    example = "{command_name} login\n{command_name} pair --name desktop --scope pipeline\n{command_name} serve",
    note = "Devices are stored in the same database as the catalogue. A token is shown
once, at pairing, and only its hash is kept.

TWO_KHZ_DATA_DIR, TWO_KHZ_MODEL_DIR and TWO_KHZ_CACHE_DIR move the corpus,
the model weights and the excerpt cache; TWO_KHZ_ENV_DIR, the .env holding
the Qobuz credentials. The environment itself wins over any .env."
)]
struct Cli {
    #[argh(subcommand)]
    command: Command,
}

/// Serving first, then the account, the pipeline and hiding; argh lists them
/// in this order. The last three groups are parsed and run by `cli`.
#[derive(FromArgs)]
#[argh(subcommand)]
pub enum Command {
    Serve(Serve),
    Pair(Pair),
    Devices(Devices),
    Revoke(Revoke),
    BuildCatalog(BuildCatalog),

    Login(cli::Login),
    RefreshCredentials(cli::RefreshCredentials),
    Whoami(cli::Whoami),
    Favourites(cli::Favourites),

    Crawl(cli::Crawl),
    Analyse(cli::Analyse),
    BuildSpace(cli::BuildSpace),
    Layout(cli::Layout),
    Models(cli::Models),
    Status(cli::Status),
    Evaluate(cli::Evaluate),
    Demo(cli::Demo),

    Block(cli::Block),
    Unblock(cli::Unblock),
    Blocked(cli::Blocked),
}

/// Run the API.
#[derive(FromArgs)]
#[argh(subcommand, name = "serve")]
pub struct Serve {
    /// address:port to listen on (default 127.0.0.1:7700)
    #[argh(option, default = "DEFAULT_BIND.to_string()")]
    bind: String,
}

/// Mint a token for a new device.
#[derive(FromArgs)]
#[argh(subcommand, name = "pair")]
pub struct Pair {
    /// what to call the device in `devices`
    #[argh(option)]
    name: String,
    /// play or pipeline (default play)
    #[argh(option, from_str_fn(scope), default = "Scope::Play")]
    scope: Scope,
}

fn scope(text: &str) -> Result<Scope, String> {
    Scope::parse(text).ok_or_else(|| format!("unknown scope “{text}”; use play or pipeline"))
}

/// List paired devices.
#[derive(FromArgs)]
#[argh(subcommand, name = "devices")]
pub struct Devices {}

/// Revoke one device.
#[derive(FromArgs)]
#[argh(subcommand, name = "revoke")]
pub struct Revoke {
    /// the device's id; see `devices`
    #[argh(positional)]
    id: i64,
}

/// Rebuild the slim catalogue clients sync.
#[derive(FromArgs)]
#[argh(subcommand, name = "build-catalog")]
pub struct BuildCatalog {}

/// Spellings argh would otherwise refuse. Only the subcommand is rewritten,
/// never an argument that happens to match.
const ALIASES: &[(&str, &str)] = &[("favorites", "favourites"), ("analyze", "analyse")];

fn parse_args() -> Cli {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Some(first) = args.first_mut() {
        if let Some((_, canonical)) = ALIASES.iter().find(|(alias, _)| alias == first) {
            *first = canonical;
        }
    }

    match Cli::from_args(&["two-khz-server"], &args) {
        Ok(cli) => cli,
        Err(argh::EarlyExit { output, status }) => match status {
            Ok(()) => {
                println!("{output}");
                std::process::exit(0);
            }
            Err(()) => {
                eprintln!("{output}\nRun two-khz-server --help for more information.");
                std::process::exit(2);
            }
        },
    }
}

fn main() -> Result<()> {
    let Cli { command } = parse_args();
    let paths = Paths::from_env();

    let data_dir = paths.data_dir.clone();
    let db_path = paths.db_path.clone();
    let store = || -> Result<AuthStore> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        AuthStore::new(&db_path)
    };

    match command {
        Command::Serve(args) => serve(args.bind, paths, store()?),
        Command::Pair(args) => {
            let grant = store()?.issue(&args.name, args.scope)?;
            println!(
                "Paired “{}” with scope {}.\n\nSet this on the device, it is not shown again:\n\n  \
                 export TWO_KHZ_SERVER=http://<this-host>:7700\n  \
                 export TWO_KHZ_TOKEN={}\n",
                grant.device.name,
                args.scope.as_str(),
                grant.token
            );
            Ok(())
        }
        Command::Devices(_) => {
            let devices = store()?.list()?;
            if devices.is_empty() {
                println!("No devices paired. Start with:\n  two-khz-server pair --name desktop --scope pipeline");
            }
            for device in devices {
                println!(
                    "{:>4}  {:<24} {:<9} last seen {}",
                    device.id,
                    device.name,
                    device.scope.as_str(),
                    device.last_seen.as_deref().unwrap_or("never")
                );
            }
            Ok(())
        }
        Command::Revoke(args) => {
            store()?.revoke(args.id)?;
            println!("Revoked device {}.", args.id);
            Ok(())
        }
        Command::BuildCatalog(_) => {
            store()?;
            let target = data_dir.join("catalog.db");
            let bytes = catalog::build(&db_path, &target)?;
            println!(
                "Wrote {} ({:.1} MB) from {}.",
                target.display(),
                bytes as f64 / 1_048_576.0,
                db_path.display()
            );
            Ok(())
        }
        pipeline => cli::run(pipeline, &paths),
    }
}

fn serve(bind: String, paths: Paths, store: AuthStore) -> Result<()> {
    let (data_dir, db_path) = (paths.data_dir.clone(), paths.db_path.clone());
    let address: SocketAddr = bind
        .parse()
        .with_context(|| format!("“{bind}” is not an address:port"))?;

    if !address.ip().is_loopback() {
        eprintln!(
            "WARNING: binding to {address}, which is not loopback.\n\
             This speaks plain HTTP. The device tokens and the signed stream URLs\n\
             it hands out are both credentials, so put it behind WireGuard/Tailscale\n\
             or a TLS proxy, do not expose it directly.\n"
        );
    }

    if store.count()? == 0 {
        eprintln!(
            "No devices are paired, so every request will be refused. Mint one with:\n  \
             two-khz-server pair --name desktop --scope pipeline\n"
        );
    }

    let hub = Arc::new(Hub::new(paths));

    let state = AppState {
        hub: hub.clone(),
        auth: Arc::new(store),
        data_dir: data_dir.clone(),
        db_path: db_path.clone(),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // The slim catalogue has to follow the space: a client syncing new
        // vectors against an old catalogue draws the right points with the
        // wrong labels.
        tokio::spawn(watch_generation(hub, db_path, data_dir));

        let listener = tokio::net::TcpListener::bind(address).await?;
        println!("two-khz-server listening on http://{address}");

        axum::serve(listener, routes::router(state))
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Rebuild `catalog.db` whenever a stage has rewritten the space.
async fn watch_generation(hub: Arc<Hub>, db_path: PathBuf, data_dir: PathBuf) {
    let mut seen = u64::MAX;

    loop {
        if let Ok(status) = hub.pipeline_status().await {
            if status.generation != seen {
                // Including the first pass: a catalogue left by an older
                // server may not match the schema clients now read.
                let target = data_dir.join("catalog.db");
                match catalog::build(&db_path, &target) {
                    Ok(bytes) => println!(
                        "rebuilt {} ({:.1} MB)",
                        target.display(),
                        bytes as f64 / 1_048_576.0
                    ),
                    Err(err) => eprintln!("could not rebuild the slim catalogue: {err:#}"),
                }
                seen = status.generation;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down; a running stage stops with the process");
}
