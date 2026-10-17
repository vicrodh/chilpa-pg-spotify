// Spotify does not publish bucket membership. I pause only the observed route
// family for QUOTA_EXCEEDED; an ordinary rolling rate limit pauses the grant.
use crate::state::{now, save_json};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Serialize)]
pub struct Limits {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    version: u8,
    #[serde(default)]
    account: String,
    #[serde(default)]
    legacy_until: u64,
    #[serde(default)]
    catalogue_verified: bool,
    #[serde(default)]
    playlist_verified: bool,
    #[serde(default)]
    playlist_probe_after: u64,
    #[serde(default)]
    probe_after: u64,
    #[serde(default)]
    until: u64,
    #[serde(default)]
    routes: HashMap<String, u64>,
    #[serde(skip)]
    path: PathBuf,
}
impl Limits {
    pub fn load(path: &Path, client: &str) -> Self {
        let value: serde_json::Value = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let legacy = value.get("routes").is_none() && value.get("version").is_none();
        let mut limits: Self = serde_json::from_value(value).unwrap_or_default();
        if legacy {
            limits.legacy_until = limits.until;
            limits.until = 0;
        }
        limits.version = 1;
        if limits.client_id != client {
            limits = Self::default();
        }
        // An absent routes field identifies the old unknown endpoint pause.
        limits.client_id = client.into();
        limits.path = path.into();
        limits
    }
    pub fn load_account(path: &Path, client: &str, account: &str) -> Self {
        let mut limits = Self::load(path, client);
        if !limits.account.is_empty() && limits.account != account {
            limits = Self {
                client_id: client.into(),
                path: path.into(),
                version: 1,
                ..Default::default()
            };
        }
        limits.account = account.into();
        limits
    }
    pub fn remaining(&self, route: &str) -> u64 {
        self.until
            .max(self.legacy_until)
            .max(*self.routes.get(&family(route)).unwrap_or(&0))
            .saturating_sub(now())
    }
    pub fn maximum(&self) -> u64 {
        self.routes
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .max(self.until)
            .max(self.legacy_until)
            .saturating_sub(now())
    }
    fn persist(&self) -> anyhow::Result<()> {
        save_json(&self.path, &serde_json::to_value(self)?)
    }
    // I persist a lease before sending the check so a crash cannot create a probe loop.
    pub fn begin_catalogue_probe(&mut self) -> anyhow::Result<bool> {
        self.begin_search_probe(false)
    }
    pub fn remaining_search(&self, playlist: bool) -> u64 {
        self.until
            .max(self.routes.get("search").copied().unwrap_or(0))
            .max(
                if if playlist {
                    self.playlist_verified
                } else {
                    self.catalogue_verified
                } {
                    0
                } else {
                    self.legacy_until
                },
            )
            .saturating_sub(now())
    }
    pub fn begin_search_probe(&mut self, playlist: bool) -> anyhow::Result<bool> {
        let current = now();
        let (verified, after) = if playlist {
            (self.playlist_verified, self.playlist_probe_after)
        } else {
            (self.catalogue_verified, self.probe_after)
        };
        if self.legacy_until <= current
            || verified
            || self.until > current
            || self.routes.get("search").copied().unwrap_or(0) > current
            || after > current
        {
            return Ok(false);
        }
        if playlist {
            self.playlist_probe_after = current.saturating_add(300);
        } else {
            self.probe_after = current.saturating_add(300);
        }
        self.persist()?;
        Ok(true)
    }
    pub fn finish_catalogue_probe(&mut self, success: bool) -> anyhow::Result<()> {
        self.finish_search_probe(false, success)
    }
    pub fn finish_search_probe(&mut self, playlist: bool, success: bool) -> anyhow::Result<()> {
        if success
            && self.until <= now()
            && self.routes.get("search").copied().unwrap_or(0) <= now()
        {
            if playlist {
                self.playlist_verified = true;
            } else {
                self.catalogue_verified = true;
            }
        }
        self.persist()
    }
    pub fn record(&mut self, route: &str, seconds: u64, quota: bool) {
        let deadline = now().saturating_add(seconds.max(1));
        if family(route) == "search" {
            self.catalogue_verified = false;
            self.playlist_verified = false;
        }
        self.routes.retain(|_, until| *until > now());
        if quota {
            let until = self.routes.entry(family(route)).or_default();
            *until = (*until).max(deadline);
        } else {
            self.until = self.until.max(deadline);
        }
        let _ = save_json(
            &self.path,
            &serde_json::to_value(&*self).unwrap_or_default(),
        );
    }
}
fn family(route: &str) -> String {
    let mut pieces: Vec<_> = route.split('/').filter(|p| !p.is_empty()).collect();
    for piece in &mut pieces {
        if piece.len() == 22 && piece.bytes().all(|b| b.is_ascii_alphanumeric()) {
            *piece = "*";
        }
    }
    pieces.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_is_scoped_and_rolling_limits_and_legacy_pauses_remain_global() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        let mut limits = Limits::load(&path, "app");
        limits.record("/me/top/tracks", 90000, true);
        assert!(limits.remaining("/me/top/tracks") > 86400);
        assert_eq!(limits.remaining("/search"), 0);
        assert!(Limits::load(&path, "app").remaining("/me/top/tracks") > 0);
        assert_eq!(Limits::load(&path, "other").maximum(), 0);
        limits.record("/search", 60, false);
        assert!(limits.remaining("/me/albums") > 0);
        save_json(
            &path,
            &serde_json::json!({"client_id":"app","until":now()+100}),
        )
        .unwrap();
        assert!(Limits::load(&path, "app").remaining("/me") > 0);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_absent_routes_allows_a_persisted_catalogue_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        for routes in [None, Some(json!({}))] {
            let mut value = json!({"client_id":"app","until":now()+90000});
            if let Some(routes) = &routes {
                value["routes"] = routes.clone();
            }
            save_json(&path, &value).unwrap();
            let mut limits = Limits::load_account(&path, "app", "user");
            assert_eq!(limits.begin_catalogue_probe().unwrap(), routes.is_none());
            assert!(!limits.begin_catalogue_probe().unwrap());
            assert!(
                !Limits::load_account(&path, "app", "user")
                    .begin_catalogue_probe()
                    .unwrap()
            );
            if routes.is_none() {
                limits.finish_catalogue_probe(true).unwrap();
                let mut restarted = Limits::load_account(&path, "app", "user");
                assert_eq!(restarted.remaining_search(false), 0);
                assert!(restarted.remaining_search(true) > 86400);
                assert!(restarted.remaining("/search") > 86400);
                assert!(restarted.remaining("/me") > 86400);
                restarted.record("/search", u64::MAX, true);
                assert!(restarted.remaining("/search") > 86400);
                assert!(!restarted.begin_catalogue_probe().unwrap());
                assert_eq!(
                    Limits::load_account(&path, "app", "other-user").maximum(),
                    0
                );
                assert_eq!(
                    Limits::load_account(&path, "other-app", "user").maximum(),
                    0
                );
            } else {
                assert!(limits.remaining("/search") > 86400);
            }
        }
    }
    #[test]
    fn transient_failure_keeps_unknown_pause_and_the_probe_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        save_json(&path, &json!({"client_id":"app","until":now()+90000})).unwrap();
        let mut limits = Limits::load(&path, "app");
        assert!(limits.begin_catalogue_probe().unwrap());
        limits.finish_catalogue_probe(false).unwrap();
        assert!(limits.remaining("/search") > 86400);
        assert!(!Limits::load(&path, "app").begin_catalogue_probe().unwrap());
    }
}
