use crate::{
    auth::Client,
    enrichment::{self, Attempt, DisabledReason, Outcome},
    state::{now, reference, text},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

const CACHE_KEY: &str = "track-credits:v2";
const ALBUM_KEY: &str = "album-credit-tracks:v1";
const FOUND_TTL: u64 = 90 * 24 * 60 * 60;
const EMPTY_TTL: u64 = 7 * 24 * 60 * 60;
const TIME_LIMIT: Duration = Duration::from_secs(6);

#[derive(Deserialize, Serialize)]
struct Credit {
    at: u64,
    composers: Vec<String>,
}

impl Credit {
    fn fresh(&self, current: u64) -> bool {
        let ttl = if self.composers.is_empty() {
            EMPTY_TTL
        } else {
            FOUND_TTL
        };
        self.at <= current && current - self.at < ttl
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    tracks: HashMap<String, Credit>,
    albums: HashMap<String, Vec<String>>,
}

impl Cache {
    fn candidates(&self, kind: &str, value: &Value) -> Vec<String> {
        let mut seen = HashSet::new();
        value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|item| {
                if kind == "tracks" {
                    item_id(item, "track").into_iter().collect::<Vec<_>>()
                } else {
                    item_id(item, "album")
                        .and_then(|id| self.albums.get(&id))
                        .cloned()
                        .unwrap_or_default()
                }
            })
            .filter(|id| reference(&format!("track:{id}"), Some("track")).is_ok())
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    fn remember(&mut self, album: &str, tracks: impl IntoIterator<Item = String>) {
        let known = self.albums.entry(album.into()).or_default();
        for track in tracks {
            if !known.contains(&track) {
                known.push(track);
            }
        }
    }

    fn apply(&self, kind: &str, value: &mut Value) {
        let Some(items) = value["items"].as_array_mut() else {
            return;
        };
        for item in items {
            // Enrichment is projected afresh; expired answers never survive a page cache.
            if let Some(item) = item.as_object_mut() {
                item.remove("composers");
            }
            let Some(id) = item_id(item, kind.trim_end_matches('s')) else {
                continue;
            };
            let tracks = if kind == "tracks" {
                vec![id]
            } else {
                self.albums.get(&id).cloned().unwrap_or_default()
            };
            let mut names = Vec::new();
            for track in tracks {
                if let Some(credit) = self.tracks.get(&track).filter(|c| c.fresh(now())) {
                    for name in &credit.composers {
                        if !names.contains(name) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            if !names.is_empty() {
                item["composers"] = json!(names);
            }
        }
    }
}

fn item_id(item: &Value, kind: &str) -> Option<String> {
    reference(text(item, "ref"), Some(kind))
        .ok()
        .map(|(_, id)| id)
}

fn raw_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|track| {
            reference(&format!("track:{}", text(track, "id")), Some("track"))
                .ok()
                .map(|(_, id)| id)
        })
        .collect()
}

impl Client {
    pub(crate) async fn remember_album_tracks(&self, batch: &[Value]) {
        let Ok(identity) = self.identity().await else {
            return;
        };
        for saved in batch {
            let album = &saved["album"];
            let Ok((_, id)) = reference(&format!("album:{}", text(album, "id")), Some("album"))
            else {
                continue;
            };
            if let Some(tracks) = album.pointer("/tracks/items").filter(|v| v.is_array()) {
                let _ = self.snapshots.put(
                    &identity,
                    &format!("{ALBUM_KEY}:{id}"),
                    json!(raw_ids(tracks)),
                );
            }
        }
    }

    pub(crate) async fn library_composers(&self, kind: &str, value: &mut Value) {
        self.composers_until(kind, value, Instant::now() + TIME_LIMIT)
            .await;
    }

    async fn composers_until(&self, kind: &str, value: &mut Value, deadline: Instant) {
        let Ok(Ok(identity)) = timeout_at(deadline, self.identity()).await else {
            return;
        };
        let mut attempt = Attempt::start();
        let mut saved: Cache = self
            .snapshots
            .peek(&identity, CACHE_KEY)
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        saved.tracks.retain(|_, credit| credit.fresh(now()));
        for item in value["items"].as_array().into_iter().flatten() {
            if kind == "tracks" {
                if let (Some(track), Ok((_, album))) = (
                    item_id(item, "track"),
                    reference(text(item, "album_ref"), Some("album")),
                ) {
                    saved.remember(&album, [track]);
                }
            } else if let Some(album) = item_id(item, "album") {
                if let Some(tracks) = self
                    .snapshots
                    .peek(&identity, &format!("{ALBUM_KEY}:{album}"))
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                {
                    saved.remember(&album, tracks);
                }
                if let Some(page) = self
                    .snapshots
                    .peek(&identity, &format!("session:v2:album:{album}:0"))
                {
                    saved.remember(
                        &album,
                        page["tracks"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|t| item_id(t, "track")),
                    );
                }
                for source in ["Primary", "Shared"] {
                    if let Some(raw) = self
                        .snapshots
                        .peek(&identity, &format!("web:{source}:0:/albums/{album}:[]"))
                    {
                        saved.remember(&album, raw_ids(&raw["tracks"]["items"]));
                    }
                }
            }
        }
        saved.apply(kind, value);
        let candidates = saved.candidates(kind, value);
        attempt.cached = candidates
            .iter()
            .filter(|id| saved.tracks.contains_key(*id))
            .count();
        // Dedicated credits kinds have no payload schema in this vendor.
        attempt.outcome = Outcome::Disabled {
            remaining: 0,
            reason: Some(DisabledReason::UnsupportedCreditsSchema),
        };
        let _ = enrichment::record(&self.snapshots, &identity, "track_credits", attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{enrichment_diagnostic as diagnostic, id, metadata_client};

    fn track(index: usize) -> Value {
        json!({"ref":format!("track:{}",id(index)), "label":"Existing",
            "composer":"Work header", "composer_ref":"work-header", "work":"Suite"})
    }

    async fn assert_disabled(client: &Client, cached: usize) {
        let entry = diagnostic(client, "track_credits").await;
        assert_eq!(
            entry["outcome"],
            json!({
                "class":"disabled","remaining":0,"reason":"unsupported_credits_schema"
            })
        );
        assert_eq!(entry["asked"], 0);
        assert_eq!(entry["with_data"], 0);
        assert_eq!(entry["cached"], cached);
    }

    #[tokio::test]
    async fn credits_are_disabled_without_requests_or_new_negative_cache_entries() {
        let (client, directory, worker) = metadata_client(vec![]).await;
        let mut tracks = json!({"items":(0..25).map(track).collect::<Vec<_>>()});
        let original = tracks.clone();
        for _ in 0..2 {
            client.library_composers("tracks", &mut tracks).await;
            assert_eq!(tracks, original);
            assert_disabled(&client, 0).await;
        }
        client
            .library_composers(
                "albums",
                &mut json!({
                    "items":[{"ref":format!("album:{}",id(100))}]
                }),
            )
            .await;
        assert_disabled(&client, 0).await;
        let identity = client.identity().await.unwrap();
        assert!(client.snapshots.peek(&identity, CACHE_KEY).is_none());
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.metadata_fixture = client.metadata_fixture.clone();
        restarted.library_composers("tracks", &mut tracks).await;
        assert_disabled(&restarted, 0).await;
        assert!(worker.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_credits_preserve_fresh_cache_and_expire_old_answers_per_account() {
        let (client, _directory, worker) = metadata_client(vec![]).await;
        let identity = client.identity().await.unwrap();
        let cache = json!({"tracks":{
            id(0):{"at":now(),"composers":["Cached"]},
            id(1):{"at":now(),"composers":[]},
            id(2):{"at":now()-FOUND_TTL,"composers":["Expired"]},
            id(3):{"at":now()-EMPTY_TTL,"composers":[]},
            id(4):{"at":now()+1000,"composers":["Future"]}
        },"albums":{},"after":id(4)});
        client
            .snapshots
            .put(&identity, CACHE_KEY, cache.clone())
            .unwrap();
        let mut tracks = json!({"items":(0..6).map(track).collect::<Vec<_>>()});
        tracks["items"][2]["composers"] = json!(["Stale page"]);
        client.library_composers("tracks", &mut tracks).await;
        assert_eq!(tracks["items"][0]["composers"], json!(["Cached"]));
        for index in 1..6 {
            assert!(tracks["items"][index].get("composers").is_none());
        }
        for index in 0..6 {
            assert_eq!(tracks["items"][index]["composer"], "Work header");
        }
        assert_disabled(&client, 2).await;
        assert_eq!(client.snapshots.peek(&identity, CACHE_KEY).unwrap(), cache);
        client.account.lock().await.user = "other-account".into();
        client.library_composers("tracks", &mut tracks).await;
        assert!(tracks["items"][0].get("composers").is_none());
        assert_disabled(&client, 0).await;
        assert!(worker.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_album_credits_use_known_membership_and_deduplicate_cached_names() {
        let (client, _directory, worker) = metadata_client(vec![]).await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                CACHE_KEY,
                json!({"tracks":{
            id(0):{"at":now(),"composers":["First","Shared"]},
            id(1):{"at":now(),"composers":["Shared","Second"]}
        },"albums":{id(100):[id(0)]}}),
            )
            .unwrap();
        client
            .remember_album_tracks(&[
                json!({"album":{"id":id(100),"tracks":{"items":[{"id":id(1)},{"id":id(2)}]}}}),
            ])
            .await;
        let mut page = json!({"items":[
            {"ref":format!("album:{}",id(100))}, {"ref":format!("album:{}",id(101))}
        ]});
        client.library_composers("albums", &mut page).await;
        assert_eq!(
            page["items"][0]["composers"],
            json!(["First", "Shared", "Second"])
        );
        assert!(page["items"][1].get("composers").is_none());
        assert_disabled(&client, 2).await;
        assert!(worker.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn diagnostic_write_failures_do_not_fail_disabled_credits() {
        let (client, directory, worker) = metadata_client(vec![]).await;
        let path = directory.path().join("cache/snapshots");
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "blocked writes").unwrap();
        let mut page = json!({"items":[track(0)]});
        client.library_composers("tracks", &mut page).await;
        assert_eq!(page["items"][0], track(0));
        assert!(worker.join().unwrap().is_empty());
    }
}
