//! Who is allowed to do what.
//!
//! A family: a few people sharing the one Qobuz account, each with their own
//! devices. Per-device tokens rather than passwords, a password could not be
//! revoked one phone at a time. A user is little more than a name the devices
//! hang off, so each person gets their own play session.
//!
//! `play` is every device; `pipeline` is hours of CPU, the shared rate limit,
//! the hidden artists and a `--purge` that deletes rows, so it is whoever looks
//! after the server. `play` still needs a token: a stream URL is minted
//! against the account, so handing those out is sharing it.

use anyhow::{bail, Result};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::BigInt;
use stelly_core::api::{Device, PairingGrant, Scope, User};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 32 bytes, hex-encoded. Long enough that guessing is not a threat model.
const TOKEN_BYTES: usize = 32;

/// Who the devices paired before there were users go to. Renamed with
/// `stelly-server user rename`.
const FIRST_USER: &str = "owner";

// Not in the shared `stelly_core::schema`: clients have no business knowing the
// tables exist, and they are created here rather than by `schema.sql`.
diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
        created_at -> Text,
    }
}

diesel::table! {
    devices (id) {
        id -> BigInt,
        name -> Text,
        scope -> Text,
        token_hash -> Text,
        created_at -> Text,
        last_seen -> Nullable<Text>,
        user_id -> Nullable<BigInt>,
    }
}

diesel::joinable!(devices -> users (user_id));
diesel::allow_tables_to_appear_in_same_query!(devices, users);

pub struct AuthStore {
    db_path: PathBuf,
}

/// A `devices` row with its owner's name. The scope is stored as its name.
#[derive(Queryable)]
struct DeviceRow {
    id: i64,
    name: String,
    scope: String,
    created_at: String,
    last_seen: Option<String>,
    user_id: i64,
    user: String,
}

impl From<DeviceRow> for Device {
    fn from(row: DeviceRow) -> Self {
        Device {
            id: row.id,
            name: row.name,
            scope: Scope::parse(&row.scope).unwrap_or(Scope::Play),
            created_at: row.created_at,
            last_seen: row.last_seen,
            user_id: row.user_id,
            user: row.user,
        }
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = users, check_for_backend(diesel::sqlite::Sqlite))]
struct UserRow {
    id: i64,
    name: String,
    created_at: String,
}

impl From<UserRow> for User {
    fn from(row: UserRow) -> Self {
        User {
            id: row.id,
            name: row.name,
            created_at: row.created_at,
        }
    }
}

#[derive(QueryableByName)]
struct Found {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Devices joined to their owners, in the shape `DeviceRow` reads.
macro_rules! device_rows {
    () => {
        devices::table.inner_join(users::table).select((
            devices::id,
            devices::name,
            devices::scope,
            devices::created_at,
            devices::last_seen,
            users::id,
            users::name,
        ))
    };
}

impl AuthStore {
    pub fn new(db_path: &Path) -> Result<Self> {
        let store = Self {
            db_path: db_path.to_path_buf(),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn open(&self) -> Result<SqliteConnection> {
        crate::db::connect(&self.db_path, Duration::from_secs(5))
    }

    /// Users, devices and likes live in the catalogue database, so one file is still
    /// the whole backup. Created here rather than in the shared `schema.sql`
    /// because the pipeline has no business knowing which phones are paired.
    fn ensure_schema(&self) -> Result<()> {
        let conn = &mut self.open()?;
        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS users (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS devices (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 scope      TEXT NOT NULL,
                 token_hash TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL,
                 last_seen  TEXT
             );
             CREATE INDEX IF NOT EXISTS devices_token ON devices(token_hash);",
        )?;
        conn.batch_execute(crate::likes::SCHEMA)?;

        let has_owner = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM pragma_table_info('devices') WHERE name = 'user_id'",
        )
        .get_result::<Found>(conn)?
        .n
            > 0;
        if !has_owner {
            conn.batch_execute("ALTER TABLE devices ADD COLUMN user_id INTEGER REFERENCES users(id)")?;
        }

        // Devices from before there were users all belonged to the one there
        // was, and so does everything already in the favourites.
        let orphans: i64 = devices::table
            .filter(devices::user_id.is_null())
            .count()
            .get_result(conn)?;
        if orphans > 0 {
            let first = match users::table.order(users::id).select(users::id).first::<i64>(conn) {
                Ok(id) => id,
                Err(diesel::result::Error::NotFound) => {
                    eprintln!(
                        "Paired devices now belong to a user, “{FIRST_USER}” for the ones already \
                         here. Name them with:\n  stelly-server user rename {FIRST_USER} <name>\n"
                    );
                    insert_user(conn, FIRST_USER)?.id
                }
                Err(err) => return Err(err.into()),
            };
            diesel::update(devices::table.filter(devices::user_id.is_null()))
                .set(devices::user_id.eq(first))
                .execute(conn)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- users

    pub fn users(&self) -> Result<Vec<User>> {
        let rows = users::table
            .order(users::id)
            .select(UserRow::as_select())
            .load(&mut self.open()?)?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    pub fn add_user(&self, name: &str) -> Result<User> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a user needs a name");
        }
        let conn = &mut self.open()?;
        if self.find_user(conn, name)?.is_some() {
            bail!("there is already a user called “{name}”");
        }
        insert_user(conn, name)
    }

    /// By name, which is what the CLI and the app both speak.
    pub fn user(&self, name: &str) -> Result<User> {
        self.find_user(&mut self.open()?, name)?
            .ok_or_else(|| anyhow::anyhow!("no user called “{name}”; see `stelly-server user list`"))
    }

    /// Who a new device goes to. Pairing for someone new is how they join, so
    /// a name nobody has yet is created. Without one, the only user there is,
    /// or the first, on a server that has none.
    pub fn user_for_pairing(&self, name: Option<&str>) -> Result<User> {
        let conn = &mut self.open()?;
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            return match self.find_user(conn, name)? {
                Some(user) => Ok(user),
                None => insert_user(conn, name),
            };
        }
        let mut everyone = users::table
            .order(users::id)
            .select(UserRow::as_select())
            .load(conn)?;
        match everyone.len() {
            0 => insert_user(conn, FIRST_USER),
            1 => Ok(everyone.remove(0).into()),
            _ => {
                let names: Vec<String> = everyone.into_iter().map(|user| user.name).collect();
                bail!("whose device is it? pass --user, one of: {}", names.join(", "))
            }
        }
    }

    fn find_user(&self, conn: &mut SqliteConnection, name: &str) -> Result<Option<User>> {
        Ok(users::table
            .filter(users::name.eq(name))
            .select(UserRow::as_select())
            .first(conn)
            .optional()?
            .map(User::from))
    }

    pub fn rename_user(&self, from: &str, to: &str) -> Result<()> {
        let to = to.trim();
        if to.is_empty() {
            bail!("a user needs a name");
        }
        let user = self.user(from)?;
        let conn = &mut self.open()?;
        if self.find_user(conn, to)?.is_some() {
            bail!("there is already a user called “{to}”");
        }
        diesel::update(users::table.find(user.id))
            .set(users::name.eq(to))
            .execute(conn)?;
        Ok(())
    }

    /// Only once their devices are revoked, so removing someone never quietly
    /// locks a phone out.
    pub fn remove_user(&self, name: &str) -> Result<()> {
        let user = self.user(name)?;
        let conn = &mut self.open()?;
        let paired: i64 = devices::table
            .filter(devices::user_id.eq(user.id))
            .count()
            .get_result(conn)?;
        if paired > 0 {
            bail!("“{name}” still has {paired} paired device(s); revoke them first");
        }
        diesel::delete(users::table.find(user.id)).execute(conn)?;
        Ok(())
    }

    // ----------------------------------------------------------- devices

    /// Mint a token for a new device. Returned once and never again, only
    /// the hash is kept, so a lost token means re-pairing.
    pub fn issue(&self, name: &str, scope: Scope, user: &User) -> Result<PairingGrant> {
        let mut raw = [0u8; TOKEN_BYTES];
        rand::rng().fill(&mut raw);
        let token = hex(&raw);
        let now = crate::db::utc_now();

        let id = diesel::insert_into(devices::table)
            .values((
                devices::name.eq(name),
                devices::scope.eq(scope.as_str()),
                devices::token_hash.eq(hash(&token)),
                devices::created_at.eq(&now),
                devices::user_id.eq(user.id),
            ))
            .returning(devices::id)
            .get_result(&mut self.open()?)?;

        Ok(PairingGrant {
            device: Device {
                id,
                name: name.to_string(),
                scope,
                created_at: now,
                last_seen: None,
                user_id: user.id,
                user: user.name.clone(),
            },
            token,
        })
    }

    /// Resolve a presented token, and record that it was used.
    ///
    /// Looked up by hash, so the comparison is SQLite's and not constant-time.
    /// Fine for a 256-bit random token, which cannot be probed a byte at a
    /// time; it would not be for a password.
    pub fn verify(&self, token: &str) -> Option<Device> {
        let conn = &mut self.open().ok()?;

        let device: Device = device_rows!()
            .filter(devices::token_hash.eq(hash(token)))
            .first::<DeviceRow>(conn)
            .ok()?
            .into();

        let _ = diesel::update(devices::table.find(device.id))
            .set(devices::last_seen.eq(crate::db::utc_now()))
            .execute(conn);

        Some(device)
    }

    /// Everyone's, or one user's.
    pub fn list(&self, user_id: Option<i64>) -> Result<Vec<Device>> {
        let mut query = device_rows!().order((users::name, devices::id)).into_boxed();
        if let Some(user_id) = user_id {
            query = query.filter(devices::user_id.eq(user_id));
        }
        let rows = query.load::<DeviceRow>(&mut self.open()?)?;
        Ok(rows.into_iter().map(Device::from).collect())
    }

    /// Any device, or with `user_id` only one of theirs.
    pub fn revoke(&self, device_id: i64, user_id: Option<i64>) -> Result<()> {
        let mut target = diesel::delete(devices::table.find(device_id)).into_boxed();
        if let Some(user_id) = user_id {
            target = target.filter(devices::user_id.eq(user_id));
        }
        if target.execute(&mut self.open()?)? == 0 {
            bail!("no device with id {device_id}");
        }
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        Ok(devices::table.count().get_result(&mut self.open()?)?)
    }
}

fn insert_user(conn: &mut SqliteConnection, name: &str) -> Result<User> {
    let created_at = crate::db::utc_now();
    let id = diesel::insert_into(users::table)
        .values((users::name.eq(name), users::created_at.eq(&created_at)))
        .returning(users::id)
        .get_result(conn)?;
    Ok(User {
        id,
        name: name.to_string(),
        created_at,
    })
}

fn hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ------------------------------------------------------------- extractors

/// A request that carried a valid token. Every route takes one of these or
/// `PipelineAuth`, so there is no unauthed path by construction.
pub struct PlayAuth(pub Device);

/// A request from a device trusted with the expensive, destructive half.
pub struct PipelineAuth(pub Device);

fn bearer(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|token| token.trim().to_string())
}

fn authorise(parts: &Parts, state: &crate::AppState, needed: Scope) -> Result<Device, crate::Failure> {
    let Some(token) = bearer(parts) else {
        return Err(crate::Failure::unauthorised(
            "no bearer token; pair this device with `stelly-server pair`",
        ));
    };
    let Some(device) = state.auth.verify(&token) else {
        return Err(crate::Failure::unauthorised(
            "unrecognised token; it may have been revoked",
        ));
    };
    if !device.scope.covers(needed) {
        return Err(crate::Failure::forbidden(format!(
            "device “{}” is paired for {} only; this needs {}",
            device.name,
            device.scope.as_str(),
            needed.as_str()
        )));
    }
    Ok(device)
}

impl FromRequestParts<crate::AppState> for PlayAuth {
    type Rejection = crate::Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        authorise(parts, state, Scope::Play).map(PlayAuth)
    }
}

impl FromRequestParts<crate::AppState> for PipelineAuth {
    type Rejection = crate::Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        authorise(parts, state, Scope::Pipeline).map(PipelineAuth)
    }
}
