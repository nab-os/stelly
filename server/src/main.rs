//! `stelly-server`: everything but the screen.
//!
//! Holds the Qobuz token, the shared rate limit, the database, the CLAP
//! models, the whole pipeline, crawl, analyse, build-space, layout, and the
//! loaded space every client navigates. Clients ask for all of it over HTTP.
//!
//! Sized for a family: a few people on the one Qobuz account, each with their
//! own devices and play session, sharing the favourites and the space.
//!
//! ```sh
//! stelly-server login                                              # Qobuz, once
//! stelly-server pair --name desktop --scope pipeline --user sasha  # first device
//! stelly-server pair --name phone --user sam                       # and Sam's
//! stelly-server serve                                              # 127.0.0.1:7700
//! ```
//!
//! Plain HTTP, loopback by default. The token and the signed stream URLs are
//! credentials in flight, so anything further belongs behind a VPN or TLS.

mod auth;
mod cache;
mod cli;
mod crawl;
mod db;
mod hub;
mod likes;
mod login;
mod pipeline;
mod playback;
mod qobuz;
mod routes;
mod space;
use stelly_core::schema;
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
use std::sync::Arc;
use stelly_core::api::Scope;

const DEFAULT_BIND: &str = "127.0.0.1:7700";

#[derive(Clone)]
pub struct AppState {
    /// Qobuz, the block list, the crawl and the stages. The routes only
    /// translate to and from it.
    pub hub: Arc<Hub>,
    pub auth: Arc<AuthStore>,
    /// A queue and an output per user, which their devices share.
    pub playback: Arc<playback::Sessions>,
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

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

/// Report what actually went wrong. Deliberately not sanitised: a family's
/// server behind a VPN, and a real message beats sending someone to the logs
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
            axum::Json(stelly_core::api::ApiError {
                message: self.message,
            }),
        )
            .into_response()
    }
}

// --------------------------------------------------------------------- cli

/// stelly-server: everything in Stelly but the screen.
#[derive(FromArgs)]
#[argh(
    example = "{command_name} login\n{command_name} pair --name desktop --scope pipeline --user sasha\n{command_name} serve",
    note = "Users and devices are stored in the same database as the catalogue. A token
is shown once, at pairing, and only its hash is kept. Pairing for a name
nobody has yet adds that person.

STELLY_DATA_DIR, STELLY_MODEL_DIR and STELLY_CACHE_DIR move the corpus,
the model weights and the excerpt cache; STELLY_ENV_DIR, the .env holding
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
    User(UserCommand),
    Pair(Pair),
    Devices(Devices),
    Revoke(Revoke),

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

/// Add, list, rename or remove the people sharing this server.
#[derive(FromArgs)]
#[argh(subcommand, name = "user")]
pub struct UserCommand {
    #[argh(subcommand)]
    action: UserAction,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum UserAction {
    Add(UserAdd),
    List(UserList),
    Rename(UserRename),
    Remove(UserRemove),
}

/// Add someone. Pairing a device for a new name does this too.
#[derive(FromArgs)]
#[argh(subcommand, name = "add")]
struct UserAdd {
    #[argh(positional)]
    name: String,
}

/// List everyone, with how many devices each has.
#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
struct UserList {}

/// Rename someone; their devices and likes follow.
#[derive(FromArgs)]
#[argh(subcommand, name = "rename")]
struct UserRename {
    #[argh(positional)]
    from: String,
    #[argh(positional)]
    to: String,
}

/// Remove someone whose devices are all revoked.
#[derive(FromArgs)]
#[argh(subcommand, name = "remove")]
struct UserRemove {
    #[argh(positional)]
    name: String,
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
    /// whose device it is, added if new (default: the only user there is)
    #[argh(option)]
    user: Option<String>,
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

    match Cli::from_args(&["stelly-server"], &args) {
        Ok(cli) => cli,
        Err(argh::EarlyExit { output, status }) => match status {
            Ok(()) => {
                println!("{output}");
                std::process::exit(0);
            }
            Err(()) => {
                eprintln!("{output}\nRun stelly-server --help for more information.");
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
        Command::User(args) => user(args.action, &store()?),
        Command::Pair(args) => {
            let store = store()?;
            let user = store.user_for_pairing(args.user.as_deref())?;
            let grant = store.issue(&args.name, args.scope, &user)?;
            println!(
                "Paired “{}” for {} with scope {}.\n\nSet this on the device, it is not shown again:\n\n  \
                 export STELLY_SERVER=http://<this-host>:7700\n  \
                 export STELLY_TOKEN={}\n",
                grant.device.name,
                user.name,
                args.scope.as_str(),
                grant.token
            );
            Ok(())
        }
        Command::Devices(_) => {
            let devices = store()?.list(None)?;
            if devices.is_empty() {
                println!("No devices paired. Start with:\n  stelly-server pair --name desktop --scope pipeline");
            }
            for device in devices {
                println!(
                    "{:>4}  {:<24} {:<16} {:<9} last seen {}",
                    device.id,
                    device.name,
                    device.user,
                    device.scope.as_str(),
                    device.last_seen.as_deref().unwrap_or("never")
                );
            }
            Ok(())
        }
        Command::Revoke(args) => {
            store()?.revoke(args.id, None)?;
            println!("Revoked device {}.", args.id);
            Ok(())
        }
        pipeline => cli::run(pipeline, &paths),
    }
}

fn user(action: UserAction, store: &AuthStore) -> Result<()> {
    match action {
        UserAction::Add(args) => {
            let user = store.add_user(&args.name)?;
            println!(
                "Added {}. Pair their first device with:\n  stelly-server pair --name phone --user {}",
                user.name, user.name
            );
        }
        UserAction::List(_) => {
            let devices = store.list(None)?;
            for user in store.users()? {
                let paired = devices.iter().filter(|device| device.user_id == user.id).count();
                println!("{:>4}  {:<16} {paired} device(s), since {}", user.id, user.name, user.created_at);
            }
        }
        UserAction::Rename(args) => {
            store.rename_user(&args.from, &args.to)?;
            println!("Renamed {} to {}.", args.from, args.to);
        }
        UserAction::Remove(args) => {
            store.remove_user(&args.name)?;
            println!("Removed {}.", args.name);
        }
    }
    Ok(())
}

fn serve(bind: String, paths: Paths, store: AuthStore) -> Result<()> {
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
             stelly-server pair --name desktop --scope pipeline\n"
        );
    }

    let hub = Arc::new(Hub::new(paths));

    let state = AppState {
        hub,
        auth: Arc::new(store),
        playback: Arc::default(),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let served = runtime.block_on(async move {
        // Behind the listener rather than before it: until it lands the
        // space answers as not built, and everything else already works.
        let space = state.hub.space().clone();
        tokio::task::spawn_blocking(move || {
            if let Err(err) = space.reload() {
                eprintln!("no space loaded yet: {err:#}");
            }
        });

        let listener = tokio::net::TcpListener::bind(address).await?;
        println!("stelly-server listening on http://{address}");

        axum::serve(listener, routes::router(state))
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok::<(), anyhow::Error>(())
    });

    // Dropping the runtime would wait on its blocking threads, and a stage
    // runs on one of those until it finishes.
    runtime.shutdown_background();
    served
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down; a running stage stops with the process");

    // Graceful shutdown still waits on requests in flight; a second ctrl-c
    // stops waiting.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    });
}
