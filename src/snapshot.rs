// I persist account snapshots and coalesce reads, following Spotifast's account_cache.
use crate::state::{now, save_json};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;

tokio::task_local! {
    static INCOMPLETE: Arc<AtomicBool>;
}

pub(crate) fn mark_incomplete() {
    let _ = INCOMPLETE.try_with(|state| state.store(true, Ordering::Relaxed));
}

pub(crate) fn is_incomplete() -> bool {
    INCOMPLETE
        .try_with(|state| state.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn incomplete(value: &Value) -> bool {
    value["cached"] == true || value["partial"] == true
}

fn stale(mut value: Value, at: u64) -> Value {
    mark_incomplete();
    if value.is_object() {
        value["cached"] = Value::Bool(true);
        value["cache_saved_at"] = Value::from(at);
        value["cache_notice"] =
            Value::String("Showing the last saved copy; it could not be updated.".into());
    }
    value
}

pub struct Store {
    root: PathBuf,
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ApiFailure;
    #[tokio::test]
    async fn nested_stale_reads_keep_the_complete_outer_snapshot_and_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().to_owned()).unwrap();
        store
            .put("a", "outer", serde_json::json!({"items":["complete"]}))
            .unwrap();
        store
            .put("a", "inner", serde_json::json!({"items":["old"]}))
            .unwrap();
        let before = std::fs::read(store.path("a", "outer")).unwrap();
        let result = store
            .read("a", "outer", 0, false, || async {
                let nested = store
                    .read("a", "inner", 0, false, || async {
                        Err(ApiFailure {
                            status: 503,
                            wait: 0,
                        }
                        .into())
                    })
                    .await?;
                Ok(serde_json::json!({"items":nested["items"],"new":"mixed"}))
            })
            .await
            .unwrap();
        assert_eq!(result["items"][0], "complete");
        assert_eq!(result["cached"], true);
        assert_eq!(before, std::fs::read(store.path("a", "outer")).unwrap());
    }

    #[tokio::test]
    async fn legacy_snapshots_are_fallbacks_instead_of_fresh_hits() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().to_owned()).unwrap();
        let entry = Entry {
            version: 1,
            identity: "a".into(),
            key: "page".into(),
            at: now(),
            value: serde_json::json!({"old":true}),
        };
        save_json(
            &store.path("a", "page"),
            &serde_json::to_value(entry).unwrap(),
        )
        .unwrap();
        let result = store
            .read("a", "page", 3600, false, || async {
                Err(ApiFailure {
                    status: 503,
                    wait: 0,
                }
                .into())
            })
            .await
            .unwrap();
        assert_eq!(result["cached"], true);
        assert_eq!(result["old"], true);
    }

    #[tokio::test]
    async fn stale_snapshots_survive_quota_failures_but_never_a_permission_denial_or_another_account()
     {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("snapshots")).unwrap();
        store
            .put("alice", "page", serde_json::json!({"items":["saved"]}))
            .unwrap();
        let stale = store
            .read("alice", "page", 0, false, || async {
                Err(ApiFailure {
                    status: 429,
                    wait: 100,
                }
                .into())
            })
            .await
            .unwrap();
        assert_eq!(stale["items"][0], "saved");
        assert_eq!(stale["cached"], true);
        assert!(
            store
                .read("alice", "page", 0, false, || async {
                    Err(ApiFailure {
                        status: 403,
                        wait: 0,
                    }
                    .into())
                })
                .await
                .is_err()
        );
        assert!(
            store
                .read("bob", "page", 0, false, || async {
                    Err(ApiFailure {
                        status: 429,
                        wait: 100,
                    }
                    .into())
                })
                .await
                .is_err()
        );
        let restarted = Store::new(dir.path().join("snapshots")).unwrap();
        assert_eq!(
            restarted
                .read("alice", "page", 3600, false, || async {
                    panic!("a restart must reuse the snapshot")
                })
                .await
                .unwrap()["items"][0],
            "saved"
        );
        restarted.invalidate("alice", "");
        assert!(restarted.peek("alice", "page").is_none());
    }
}

#[derive(Deserialize, Serialize)]
struct Entry {
    version: u32,
    identity: String,
    key: String,
    at: u64,
    value: Value,
}

impl Store {
    fn path(&self, identity: &str, key: &str) -> PathBuf {
        self.root.join(format!(
            "{:x}.json",
            Sha256::digest(format!("{identity}\0{key}"))
        ))
    }
    pub fn peek(&self, identity: &str, key: &str) -> Option<Value> {
        std::fs::read(self.path(identity, key))
            .ok()
            .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
            .filter(|e| matches!(e.version, 1 | 2) && e.identity == identity && e.key == key)
            .map(|e| e.value)
    }
    pub fn put(&self, identity: &str, key: &str, value: Value) -> Result<()> {
        save_json(
            &self.path(identity, key),
            &serde_json::to_value(Entry {
                version: 2,
                identity: identity.into(),
                key: key.into(),
                at: now(),
                value,
            })?,
        )
    }
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root,
            gates: Mutex::new(HashMap::new()),
        })
    }

    pub async fn read<F, Fut>(
        &self,
        identity: &str,
        key: &str,
        age: u64,
        refresh: bool,
        fetch: F,
    ) -> Result<Value>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value>>,
    {
        if INCOMPLETE.try_with(|_| ()).is_ok() {
            self.read_inner(identity, key, age, refresh, fetch).await
        } else {
            INCOMPLETE
                .scope(
                    Arc::new(AtomicBool::new(false)),
                    self.read_inner(identity, key, age, refresh, fetch),
                )
                .await
        }
    }

    async fn read_inner<F, Fut>(
        &self,
        identity: &str,
        key: &str,
        age: u64,
        refresh: bool,
        fetch: F,
    ) -> Result<Value>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value>>,
    {
        let digest = format!("{:x}", Sha256::digest(format!("{identity}\0{key}")));
        let gate = {
            let mut gates = self.gates.lock().await;
            if gates.len() > 1024 {
                gates.retain(|_, gate| Arc::strong_count(gate) > 1);
            }
            gates.entry(digest.clone()).or_default().clone()
        };
        let _guard = gate.lock().await;
        let path = self.root.join(format!("{digest}.json"));
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).ok())
            .filter(|entry| {
                matches!(entry.version, 1 | 2)
                    && entry.identity == identity
                    && entry.key == key
                    && entry.at <= now()
            });
        // Repeated host polling and concurrent Refresh clicks share at least one minute.
        let age = if refresh { age.min(60) } else { age };
        if let Some(entry) = &saved {
            if entry.version == 2 && now().saturating_sub(entry.at) < age {
                return Ok(entry.value.clone());
            }
        }
        match fetch().await {
            Ok(value) => {
                if incomplete(&value) || is_incomplete() {
                    mark_incomplete();
                    return Ok(match saved {
                        Some(entry) => stale(entry.value, entry.at),
                        None => stale(value, 0),
                    });
                }
                let entry = Entry {
                    version: 2,
                    identity: identity.into(),
                    key: key.into(),
                    at: now(),
                    value: value.clone(),
                };
                save_json(&path, &serde_json::to_value(entry)?)?;
                self.prune();
                Ok(value)
            }
            Err(error) if crate::auth::retryable(&error) && saved.is_some() => {
                let entry = saved.unwrap();
                Ok(stale(entry.value, entry.at))
            }
            Err(error) => Err(error),
        }
    }

    pub fn invalidate(&self, identity: &str, prefix: &str) {
        let Ok(files) = std::fs::read_dir(&self.root) else {
            return;
        };
        for file in files.flatten() {
            if let Some(entry) = std::fs::read(file.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
            {
                if entry.identity == identity && entry.key.starts_with(prefix) {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
    }

    fn prune(&self) {
        let Ok(files) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut files: Vec<_> = files
            .flatten()
            .filter_map(|f| Some((f.metadata().ok()?.modified().ok()?, f.path())))
            .collect();
        if files.len() <= 512 {
            return;
        }
        files.sort();
        for (_, path) in files.iter().take(files.len() - 512) {
            let _ = std::fs::remove_file(path);
        }
    }
}
