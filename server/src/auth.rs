//! Who is allowed to do what.
//!
//! A family: a few people sharing the one Qobuz account, each with their own
//! devices. Per-device tokens rather than passwords, a password could not be
//! revoked one phone at a time. A user is little more than a name the devices
//! hang off, so each person gets their own play session and likes.
//!
//! `play` is every device; `admin` is hours of CPU, the shared rate limit,
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
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How long a device found in the database is let in without looking
/// again. Revoking from the app clears it at once; `stelly-server revoke` is
/// another process, so this is how long a device it revokes can still get
/// in. A time rather than `PRAGMA data_version`: the devices share the file
/// with the catalogue, so every commit a stage makes would change the
/// version and empty the cache all through a crawl.
const TRUSTED_FOR: Duration = Duration::from_secs(30);

/// `last_seen` is for telling a phone in use from one in a drawer; a
/// minute is plenty fine enough.
const SEEN_EVERY: Duration = Duration::from_secs(60);

pub struct AuthStore {
    db: Arc<crate::db::Db>,
    trusted_for: Duration,
    verified: Mutex<Verified>,
    /// When each device's `last_seen` was last written, by device id.
    seen: Mutex<HashMap<i64, Instant>>,
}

/// Devices already found, by token hash.
#[derive(Default)]
struct Verified {
    devices: HashMap<String, (Device, Instant)>,
    /// Bumped by every change to devices or users, so a lookup that read
    /// the row before the change does not put it back after.
    generation: u64,
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
            db: crate::db::Db::shared(db_path),
            trusted_for: TRUSTED_FOR,
            verified: Mutex::default(),
            seen: Mutex::default(),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Forget every device found so far, after a change to who may get in
    /// or what they are called. Rare enough that being precise is not worth
    /// it.
    fn forget(&self) {
        let mut verified = self.verified.lock().unwrap();
        verified.devices.clear();
        verified.generation += 1;
    }

    /// Users, devices, likes and tunings live in the catalogue database, so one file is still
    /// the whole backup. Created here rather than in the shared `schema.sql`
    /// because the pipeline has no business knowing which phones are paired.
    fn ensure_schema(&self) -> Result<()> {
        self.db.write_here(|conn| Self::make_tables(conn))
    }

    fn make_tables(conn: &mut SqliteConnection) -> Result<()> {
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
        crate::likes::ensure_schema(conn)?;
        crate::tuning::ensure_schema(conn)?;

        let has_owner = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM pragma_table_info('devices') WHERE name = 'user_id'",
        )
        .get_result::<Found>(conn)?
        .n
            > 0;
        if !has_owner {
            conn.batch_execute("ALTER TABLE devices ADD COLUMN user_id INTEGER REFERENCES users(id)")?;
        }
        // `admin` was called `pipeline`.
        diesel::update(devices::table.filter(devices::scope.eq("pipeline")))
            .set(devices::scope.eq(Scope::Admin.as_str()))
            .execute(conn)?;

        // Devices from before there were users all belonged to the one there
        // was.
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
        let rows = self
            .db
            .read_here(|conn| Ok(users::table.order(users::id).select(UserRow::as_select()).load(conn)?))?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    pub fn add_user(&self, name: &str) -> Result<User> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a user needs a name");
        }
        self.db.write_here(|conn| {
            if self.find_user(conn, name)?.is_some() {
                bail!("there is already a user called “{name}”");
            }
            insert_user(conn, name)
        })
    }

    /// By name, which is what the CLI and the app both speak.
    pub fn user(&self, name: &str) -> Result<User> {
        self.db
            .read_here(|conn| self.find_user(conn, name))?
            .ok_or_else(|| anyhow::anyhow!("no user called “{name}”; see `stelly-server user list`"))
    }

    /// Who a new device goes to: someone already added, so a typo is
    /// refused rather than quietly starting a new person. Without a name, the
    /// only user there is, or the first, on a server that has none.
    pub fn user_for_pairing(&self, name: Option<&str>) -> Result<User> {
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            return self.user(name);
        }
        self.db.write_here(|conn| {
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
        })
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
        self.db.write_here(|conn| {
            if self.find_user(conn, to)?.is_some() {
                bail!("there is already a user called “{to}”");
            }
            diesel::update(users::table.find(user.id))
                .set(users::name.eq(to))
                .execute(conn)?;
            Ok(())
        })?;
        self.forget();
        Ok(())
    }

    /// Only once their devices are revoked, so removing someone never quietly
    /// locks a phone out.
    pub fn remove_user(&self, name: &str) -> Result<()> {
        let user = self.user(name)?;
        self.db.write_here(|conn| {
            let paired: i64 = devices::table
                .filter(devices::user_id.eq(user.id))
                .count()
                .get_result(conn)?;
            if paired > 0 {
                bail!("“{name}” still has {paired} paired device(s); revoke them first");
            }
            crate::tuning::forget(conn, user.id)?;
            diesel::delete(users::table.find(user.id)).execute(conn)?;
            Ok(())
        })?;
        self.forget();
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

        let id = self.db.write_here(|conn| {
            Ok(diesel::insert_into(devices::table)
                .values((
                    devices::name.eq(name),
                    devices::scope.eq(scope.as_str()),
                    devices::token_hash.eq(hash(&token)),
                    devices::created_at.eq(&now),
                    devices::user_id.eq(user.id),
                ))
                .returning(devices::id)
                .get_result(conn)?)
        })?;

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

    /// Resolve a presented token, and note that it was used.
    ///
    /// From memory when it was found in the last `TRUSTED_FOR`, else from
    /// the database off the runtime. `last_seen` is written at most every
    /// `SEEN_EVERY`, and never waited for: a stage holding the write lock
    /// would otherwise hold up every request behind it.
    ///
    /// `None` only for a token that is not there. A database that cannot be
    /// read is an error, not a revoked device: the app takes the one for the
    /// other and drops back to pairing.
    pub async fn check(self: &Arc<Self>, token: &str) -> Result<Option<Device>> {
        let token_hash = hash(token);
        let device = match self.remembered(&token_hash) {
            Some(device) => device,
            None => {
                let store = Arc::clone(self);
                match tokio::task::spawn_blocking(move || store.verify(token_hash)).await?? {
                    Some(device) => device,
                    None => return Ok(None),
                }
            }
        };
        if self.seen_due(device.id) {
            let store = Arc::clone(self);
            let id = device.id;
            tokio::task::spawn_blocking(move || store.record_seen(id));
        }
        Ok(Some(device))
    }

    fn remembered(&self, token_hash: &str) -> Option<Device> {
        let verified = self.verified.lock().unwrap();
        let (device, found) = verified.devices.get(token_hash)?;
        (found.elapsed() < self.trusted_for).then(|| device.clone())
    }

    /// Looked up by hash, so the comparison is SQLite's and not constant-time.
    /// Fine for a 256-bit random token, which cannot be probed a byte at a
    /// time; it would not be for a password.
    fn verify(&self, token_hash: String) -> Result<Option<Device>> {
        let generation = self.verified.lock().unwrap().generation;
        let found = self.db.read_here(|conn| {
            Ok(device_rows!()
                .filter(devices::token_hash.eq(&token_hash))
                .first::<DeviceRow>(conn)
                .optional()?)
        })?;
        let Some(row) = found else {
            return Ok(None);
        };
        let device = Device::from(row);

        let mut verified = self.verified.lock().unwrap();
        if verified.generation == generation {
            verified.devices.insert(token_hash, (device.clone(), Instant::now()));
        }
        Ok(Some(device))
    }

    /// Whether it is time to write this device's `last_seen`, counting it
    /// as written if so.
    fn seen_due(&self, device_id: i64) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.get(&device_id).is_some_and(|at| at.elapsed() < SEEN_EVERY) {
            return false;
        }
        seen.insert(device_id, Instant::now());
        true
    }

    /// A failure is only logged; the next one due records it.
    fn record_seen(&self, device_id: i64) {
        let written = self.db.write_here(|conn| {
            Ok(diesel::update(devices::table.find(device_id))
                .set(devices::last_seen.eq(crate::db::utc_now()))
                .execute(conn)?)
        });
        if let Err(err) = written {
            eprintln!("recording when device {device_id} was last seen failed: {err:#}");
        }
    }

    /// Everyone's, or one user's.
    pub fn list(&self, user_id: Option<i64>) -> Result<Vec<Device>> {
        let mut query = device_rows!().order((users::name, devices::id)).into_boxed();
        if let Some(user_id) = user_id {
            query = query.filter(devices::user_id.eq(user_id));
        }
        let rows = self.db.read_here(|conn| Ok(query.load::<DeviceRow>(conn)?))?;
        Ok(rows.into_iter().map(Device::from).collect())
    }

    /// Any device, or with `user_id` only one of theirs.
    pub fn revoke(&self, device_id: i64, user_id: Option<i64>) -> Result<()> {
        let mut target = diesel::delete(devices::table.find(device_id)).into_boxed();
        if let Some(user_id) = user_id {
            target = target.filter(devices::user_id.eq(user_id));
        }
        if self.db.write_here(|conn| Ok(target.execute(conn)?))? == 0 {
            bail!("no device with id {device_id}");
        }
        self.forget();
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        self.db.read_here(|conn| Ok(devices::table.count().get_result(conn)?))
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
/// `AdminAuth`, so there is no unauthed path by construction.
pub struct PlayAuth(pub Device);

/// A request from a device trusted with the expensive, destructive half.
pub struct AdminAuth;

fn bearer(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|token| token.trim().to_string())
}

async fn authorise(parts: &Parts, state: &crate::AppState, needed: Scope) -> Result<Device, crate::Failure> {
    let Some(token) = bearer(parts) else {
        return Err(crate::Failure::unauthorised(
            "no bearer token; pair this device with `stelly-server pair`",
        ));
    };
    let device = match state.auth.check(&token).await {
        Ok(Some(device)) => device,
        Ok(None) => {
            return Err(crate::Failure::unauthorised(
                "unrecognised token; it may have been revoked",
            ))
        }
        Err(err) => {
            return Err(crate::Failure::unavailable(format!(
                "could not check the token, try again: {err:#}"
            )))
        }
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
        authorise(parts, state, Scope::Play).await.map(PlayAuth)
    }
}

impl FromRequestParts<crate::AppState> for AdminAuth {
    type Rejection = crate::Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        authorise(parts, state, Scope::Admin).await.map(|_| AdminAuth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::connection::SimpleConnection;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stelly-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("stelly.db")
    }

    fn paired(name: &str) -> (PathBuf, Arc<AuthStore>, PairingGrant) {
        let db_path = scratch(name);
        let store = Arc::new(AuthStore::new(&db_path).unwrap());
        let user = store.user_for_pairing(None).unwrap();
        let grant = store.issue("phone", Scope::Play, &user).unwrap();
        (db_path, store, grant)
    }

    /// Only a token that is not there is unknown; one the database cannot
    /// check is an error. A held write lock holds up neither the lookup nor,
    /// with `last_seen` written aside, the answer.
    #[tokio::test]
    async fn unknown_tokens_apart_from_a_database_in_the_way() {
        let (db_path, store, grant) = paired("verify");
        assert!(store.check("not a token").await.unwrap().is_none());

        // Another connection holding the write lock, as a stage does.
        let mut writer = crate::db::connect(&db_path, Duration::from_secs(5)).unwrap();
        writer.batch_execute("PRAGMA journal_mode = WAL; BEGIN IMMEDIATE;").unwrap();
        let asked = Instant::now();
        assert_eq!(store.check(&grant.token).await.unwrap().unwrap().id, grant.device.id);
        assert!(asked.elapsed() < Duration::from_secs(1), "waited {:?}", asked.elapsed());
        writer.batch_execute("ROLLBACK;").unwrap();

        // No devices table to read is a failure, not an unknown token.
        writer.batch_execute("DROP TABLE devices;").unwrap();
        store.forget();
        assert!(store.check(&grant.token).await.is_err());
    }

    /// Revoked here, refused on the next request; renamed here, the next
    /// request has the new name.
    #[tokio::test]
    async fn changes_made_here_apply_at_once() {
        let (_, store, grant) = paired("forget");
        assert_eq!(store.check(&grant.token).await.unwrap().unwrap().user, "owner");

        store.rename_user("owner", "ana").unwrap();
        assert_eq!(store.check(&grant.token).await.unwrap().unwrap().user, "ana");

        store.revoke(grant.device.id, None).unwrap();
        assert!(store.check(&grant.token).await.unwrap().is_none());
    }

    /// `stelly-server revoke` is another process with its own store: the
    /// device gets in until what was found expires, then no longer.
    #[tokio::test]
    async fn changes_made_elsewhere_apply_within_the_bound() {
        let (db_path, mut store, grant) = paired("elsewhere");
        Arc::get_mut(&mut store).unwrap().trusted_for = Duration::from_millis(200);
        assert!(store.check(&grant.token).await.unwrap().is_some());

        AuthStore::new(&db_path).unwrap().revoke(grant.device.id, None).unwrap();
        assert!(store.check(&grant.token).await.unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(store.check(&grant.token).await.unwrap().is_none());
    }
}
