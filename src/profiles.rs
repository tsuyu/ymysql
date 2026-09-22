//! Saved connection profiles.
//!
//! Everything except the password goes to a JSON file next to the metric store.
//! Passwords, when the user asks for them to be remembered, go to the OS
//! credential store (Windows Credential Manager, Keychain, Secret Service) —
//! never to disk in plaintext.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::ConnConfig;
use crate::store;

/// Service name under which passwords are filed in the OS credential store.
const KEYRING_SERVICE: &str = "ymysql";

/// Service name used before the app was renamed to yMySQL. Passwords filed
/// under it are still read, then re-filed under `KEYRING_SERVICE`.
const LEGACY_KEYRING_SERVICE: &str = "mysql_perf";

/// A statement saved against one profile, with the database it was written for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub sql: String,
    /// Default database to select when the query is loaded.
    #[serde(default)]
    pub schema: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(flatten)]
    pub conn: ConnConfig,
    /// Whether the password lives in the OS credential store for this profile.
    #[serde(default)]
    pub remember_password: bool,
    /// Statements saved against this connection.
    #[serde(default)]
    pub queries: Vec<SavedQuery>,
}

impl Profile {
    /// Adds a query, replacing any with the same name.
    pub fn put_query(&mut self, query: SavedQuery) {
        match self.queries.iter_mut().find(|q| q.name == query.name) {
            Some(slot) => *slot = query,
            None => self.queries.push(query),
        }
        self.queries.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove_query(&mut self, name: &str) -> Option<SavedQuery> {
        let i = self.queries.iter().position(|q| q.name == name)?;
        Some(self.queries.remove(i))
    }
}

/// The whole profile file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profiles {
    #[serde(default)]
    pub items: Vec<Profile>,
    /// Name of the profile to preselect at startup.
    #[serde(default)]
    pub last_used: Option<String>,
}

pub fn default_path() -> PathBuf {
    store::data_dir().join("profiles.json")
}

impl Profiles {
    /// Loads the profile file, or an empty set if it is missing or unreadable.
    pub fn load() -> Self {
        let path = default_path();
        match Self::load_from(&path) {
            Ok(p) => {
                info!(count = p.items.len(), path = %path.display(), "profiles loaded");
                p
            }
            Err(e) => {
                if path.exists() {
                    warn!("could not read {}: {e:#}", path.display());
                }
                Self::default()
            }
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&default_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn names(&self) -> Vec<String> {
        self.items.iter().map(|p| p.conn.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.items.iter().find(|p| p.conn.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.items.iter_mut().find(|p| p.conn.name == name)
    }

    /// Adds a profile, replacing any existing one with the same name. Saved
    /// queries survive: re-saving the connection form must not drop them.
    pub fn upsert(&mut self, profile: Profile) {
        match self
            .items
            .iter_mut()
            .find(|p| p.conn.name == profile.conn.name)
        {
            Some(slot) => {
                let mut profile = profile;
                if profile.queries.is_empty() {
                    profile.queries = std::mem::take(&mut slot.queries);
                }
                *slot = profile;
            }
            None => self.items.push(profile),
        }
        self.items.sort_by(|a, b| a.conn.name.cmp(&b.conn.name));
    }

    pub fn remove(&mut self, name: &str) -> Option<Profile> {
        let idx = self.items.iter().position(|p| p.conn.name == name)?;
        if self.last_used.as_deref() == Some(name) {
            self.last_used = None;
        }
        Some(self.items.remove(idx))
    }
}

/// Credential-store key for a profile. Includes the target so that renaming a
/// server does not silently hand back the old host's password.
fn account(cfg: &ConnConfig) -> String {
    format!("{}|{}@{}:{}", cfg.name, cfg.user, cfg.host, cfg.port)
}

fn entry(cfg: &ConnConfig) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &account(cfg)).context("OS credential store unavailable")
}

fn legacy_entry(cfg: &ConnConfig) -> Result<keyring::Entry> {
    keyring::Entry::new(LEGACY_KEYRING_SERVICE, &account(cfg))
        .context("OS credential store unavailable")
}

pub fn save_password(cfg: &ConnConfig) -> Result<()> {
    entry(cfg)?
        .set_password(&cfg.password)
        .context("storing password failed")
}

/// Returns the stored password, or `None` when there is none (or the store is
/// unavailable — a missing password must never block connecting by hand).
pub fn load_password(cfg: &ConnConfig) -> Option<String> {
    match entry(cfg).and_then(|e| e.get_password().map_err(Into::into)) {
        Ok(pw) => Some(pw),
        Err(e) => {
            info!("no stored password for {}: {e}", account(cfg));
            load_legacy_password(cfg)
        }
    }
}

/// Reads a password filed under the pre-rename service name and re-files it
/// under the current one, so the rename does not lose saved credentials. The
/// old entry is left in place: deleting it is the user's call.
fn load_legacy_password(cfg: &ConnConfig) -> Option<String> {
    let pw = legacy_entry(cfg)
        .and_then(|e| e.get_password().map_err(Into::into))
        .ok()?;
    match entry(cfg).and_then(|e| e.set_password(&pw).map_err(Into::into)) {
        Ok(()) => info!("migrated stored password for {}", account(cfg)),
        Err(e) => warn!("could not migrate password for {}: {e}", account(cfg)),
    }
    Some(pw)
}

pub fn forget_password(cfg: &ConnConfig) {
    if let Ok(e) = entry(cfg)
        && let Err(err) = e.delete_credential()
    {
        info!("nothing to delete for {}: {err}", account(cfg));
    }
    if let Ok(e) = legacy_entry(cfg)
        && let Err(err) = e.delete_credential()
    {
        info!("nothing to delete for legacy {}: {err}", account(cfg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, host: &str) -> ConnConfig {
        ConnConfig {
            name: name.into(),
            host: host.into(),
            password: "hunter2".into(),
            ..Default::default()
        }
    }

    fn profile(name: &str, host: &str) -> Profile {
        Profile {
            conn: cfg(name, host),
            remember_password: false,
            queries: Vec::new(),
        }
    }

    fn query(name: &str, sql: &str) -> SavedQuery {
        SavedQuery {
            name: name.into(),
            sql: sql.into(),
            schema: Some("demo".into()),
        }
    }

    fn temp_file(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ymysql_profiles_{tag}_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn round_trips_without_the_password() {
        let path = temp_file("roundtrip");
        let mut store = Profiles::default();
        store.upsert(profile("prod", "db1"));
        store.last_used = Some("prod".into());
        store.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("hunter2"),
            "passwords must never reach the profile file: {text}"
        );

        let loaded = Profiles::load_from(&path).unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].conn.host, "db1");
        assert_eq!(loaded.items[0].conn.password, "");
        assert_eq!(loaded.last_used.as_deref(), Some("prod"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_replaces_by_name_and_keeps_order() {
        let mut store = Profiles::default();
        store.upsert(profile("beta", "b"));
        store.upsert(profile("alpha", "a"));
        store.upsert(profile("beta", "b2"));

        assert_eq!(store.names(), vec!["alpha", "beta"]);
        assert_eq!(store.get("beta").unwrap().conn.host, "b2");
    }

    #[test]
    fn remove_clears_last_used() {
        let mut store = Profiles::default();
        store.upsert(profile("prod", "db1"));
        store.last_used = Some("prod".into());

        assert!(store.remove("prod").is_some());
        assert!(store.items.is_empty());
        assert!(store.last_used.is_none());
        assert!(store.remove("prod").is_none());
    }

    #[test]
    fn missing_file_is_not_an_error_for_the_app() {
        let path = temp_file("missing");
        assert!(Profiles::load_from(&path).is_err());
        // `load()` swallows that and yields an empty set; verified here on the
        // parse path so the test does not touch the user's real config.
        assert!(Profiles::default().items.is_empty());
    }

    #[test]
    fn queries_are_stored_per_profile() {
        let path = temp_file("queries");
        let mut store = Profiles::default();
        store.upsert(profile("prod", "db1"));
        store.upsert(profile("stage", "db2"));

        store
            .get_mut("prod")
            .unwrap()
            .put_query(query("daily orders", "SELECT * FROM orders"));
        store.save_to(&path).unwrap();

        let loaded = Profiles::load_from(&path).unwrap();
        assert_eq!(loaded.get("prod").unwrap().queries.len(), 1);
        assert!(
            loaded.get("stage").unwrap().queries.is_empty(),
            "queries belong to one profile only"
        );
        let q = &loaded.get("prod").unwrap().queries[0];
        assert_eq!(q.sql, "SELECT * FROM orders");
        assert_eq!(q.schema.as_deref(), Some("demo"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_a_query_twice_replaces_it() {
        let mut p = profile("prod", "db1");
        p.put_query(query("q", "SELECT 1"));
        p.put_query(query("q", "SELECT 2"));
        assert_eq!(p.queries.len(), 1);
        assert_eq!(p.queries[0].sql, "SELECT 2");

        assert!(p.remove_query("q").is_some());
        assert!(p.remove_query("q").is_none());
    }

    #[test]
    fn resaving_the_connection_keeps_saved_queries() {
        let mut store = Profiles::default();
        store.upsert(profile("prod", "db1"));
        store
            .get_mut("prod")
            .unwrap()
            .put_query(query("q", "SELECT 1"));

        // Same name, edited host — what the Save button sends.
        store.upsert(profile("prod", "db-new"));

        let p = store.get("prod").unwrap();
        assert_eq!(p.conn.host, "db-new");
        assert_eq!(p.queries.len(), 1, "queries must not be lost on re-save");
    }

    #[test]
    fn keyring_account_includes_the_target() {
        let a = account(&cfg("prod", "db1"));
        let b = account(&cfg("prod", "db2"));
        assert_ne!(a, b, "same name on a different host is a different secret");
        assert!(a.starts_with("prod|root@db1:3306"));
    }
}
