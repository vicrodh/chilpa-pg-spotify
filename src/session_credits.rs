use crate::{
    auth::Client,
    enrichment::{self, Attempt, Outcome, TimeoutStage},
    state::{now, reference, text},
};
use librespot_core::SpotifyId;
use librespot_protocol::{extension_kind::ExtensionKind, metadata};
use protobuf::Message;
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
const REQUEST_LIMIT: usize = 20;
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
    #[serde(default)]
    after: Option<String>,
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

fn parse(id: &str, bytes: &[u8]) -> Result<Vec<String>, Outcome> {
    let track = metadata::Track::parse_from_bytes(bytes).map_err(|_| Outcome::UnexpectedShape)?;
    if SpotifyId::from_raw(track.gid())
        .map(|gid| gid.to_base62())
        .ok()
        .as_deref()
        != Some(id)
    {
        return Err(Outcome::UnexpectedShape);
    }
    let mut names = Vec::new();
    for artist in track.artist_with_role {
        if artist.role.and_then(|role| role.enum_value().ok())
            != Some(metadata::artist_with_role::ArtistRole::ARTIST_ROLE_COMPOSER)
        {
            continue;
        }
        let name = artist.artist_name().trim();
        if name.is_empty() {
            return Err(Outcome::UnexpectedShape);
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
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
        let Ok(_guard) = timeout_at(deadline, self.credits_gate.lock()).await else {
            let saved: Cache = self
                .snapshots
                .peek(&identity, CACHE_KEY)
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            attempt.cached = saved
                .candidates(kind, value)
                .iter()
                .filter(|id| {
                    saved
                        .tracks
                        .get(*id)
                        .is_some_and(|credit| credit.fresh(now()))
                })
                .count();
            saved.apply(kind, value);
            attempt.fail(Outcome::Timeout {
                stage: TimeoutStage::Gate,
            });
            let _ = enrichment::record(&self.snapshots, &identity, "track_credits", attempt);
            return;
        };
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
        let mut missing: Vec<_> = candidates
            .iter()
            .filter(|id| !saved.tracks.contains_key(*id))
            .cloned()
            .collect();
        attempt.cached = candidates.len() - missing.len();
        if missing.is_empty() {
            attempt.outcome = if candidates.is_empty() {
                Outcome::NoCandidates
            } else {
                Outcome::CacheHit
            };
        }
        if let Some(index) = missing
            .iter()
            .position(|id| Some(id) == saved.after.as_ref())
        {
            let start = (index + 1) % missing.len();
            missing.rotate_left(start);
        }
        if !missing.is_empty() {
            let selected: Vec<_> = missing.iter().take(REQUEST_LIMIT).collect();
            let uris: Vec<_> = selected
                .iter()
                .map(|id| format!("spotify:track:{id}"))
                .collect();
            match self
                .enrichment_metadata(
                    &identity,
                    ExtensionKind::TRACK_V4,
                    &uris,
                    deadline,
                    &mut attempt,
                )
                .await
            {
                Ok(mut rows) => {
                    for (id, uri) in selected.iter().zip(&uris) {
                        match rows
                            .remove(uri)
                            .unwrap_or(Err(Outcome::UnexpectedShape))
                            .and_then(|bytes| parse(id, &bytes))
                        {
                            Ok(composers) => {
                                attempt.with_data += usize::from(!composers.is_empty());
                                saved.tracks.insert(
                                    (*id).clone(),
                                    Credit {
                                        at: now(),
                                        composers,
                                    },
                                );
                            }
                            Err(outcome) => attempt.fail(outcome),
                        }
                    }
                }
                Err(outcome) => attempt.fail(outcome),
            }
            if attempt.asked > 0 {
                saved.after = selected.last().map(|id| (*id).clone());
            }
        }
        saved.apply(kind, value);
        if let Ok(cache) = serde_json::to_value(&saved) {
            let _ = self.snapshots.put(&identity, CACHE_KEY, cache);
        }
        let _ = enrichment::record(&self.snapshots, &identity, "track_credits", attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{
        enrichment_diagnostic as diagnostic, id, metadata_client, metadata_reply, track_metadata,
    };

    fn track(index: usize) -> Value {
        json!({"kind":"track","ref":format!("track:{}",id(index)),"album_ref":format!("album:{}",id(100)),"label":"Existing","composer":"Work header","composer_ref":"work-header","work":"Suite"})
    }

    fn reply(indices: impl IntoIterator<Item = usize>) -> Vec<u8> {
        metadata_reply(
            ExtensionKind::TRACK_V4,
            indices
                .into_iter()
                .map(|i| (i, 200, track_metadata(i, &[&format!("Composer {i}")])))
                .collect(),
        )
    }

    async fn seed(client: &Client, entries: Vec<(usize, u64, Vec<&str>)>) {
        let mut cache = Cache::default();
        for (i, at, names) in entries {
            cache.tracks.insert(
                id(i),
                Credit {
                    at,
                    composers: names.into_iter().map(str::to_owned).collect(),
                },
            );
        }
        client
            .snapshots
            .put(&client.identity().await.unwrap(), CACHE_KEY, json!(cache))
            .unwrap();
    }

    #[test]
    fn only_explicit_composer_roles_are_used_in_response_order() {
        use librespot_protocol::metadata::{ArtistWithRole, artist_with_role::ArtistRole};
        let bytes = track_metadata(1, &[" First ", "Second", "First"]);
        let mut track = metadata::Track::parse_from_bytes(&bytes).unwrap();
        for role in [
            Some(ArtistRole::ARTIST_ROLE_MAIN_ARTIST),
            Some(ArtistRole::ARTIST_ROLE_CONDUCTOR),
            None,
        ] {
            let mut artist = ArtistWithRole::new();
            artist.set_artist_name("Not a composer".into());
            if let Some(role) = role {
                artist.set_role(role);
            }
            track.artist_with_role.push(artist);
        }
        assert_eq!(
            parse(&id(1), &track.write_to_bytes().unwrap()),
            Ok(vec!["First".into(), "Second".into()])
        );
        assert_eq!(parse(&id(1), &track_metadata(1, &[])), Ok(vec![]));
        for bytes in [
            vec![],
            b"invalid protobuf".to_vec(),
            track_metadata(2, &["Wrong identity"]),
            track_metadata(1, &[" "]),
        ] {
            assert_eq!(parse(&id(1), &bytes), Err(Outcome::UnexpectedShape));
        }
    }

    #[tokio::test]
    async fn budget_resumes_on_cached_pages_and_survives_restart() {
        let (client, directory, worker) = metadata_client(vec![
            (200, reply(0..20), Duration::ZERO),
            (200, reply(20..25), Duration::ZERO),
        ])
        .await;
        let mut items: Vec<_> = (0..25).map(track).collect();
        items.push(track(0));
        items.push(track(99));
        seed(&client, vec![(99, now(), vec!["Cached"])]).await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                "library:tracks:0:50:fields-v2",
                json!({"items":items,"total":27}),
            )
            .unwrap();
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert!(first["items"][20].get("composers").is_none());
        assert_eq!(first["items"][25]["composers"], json!(["Composer 0"]));
        assert_eq!(first["items"][26]["composers"], json!(["Cached"]));
        assert_eq!(diagnostic(&client, "track_credits").await["asked"], 20);
        for (original, enriched) in items.iter().zip(first["items"].as_array().unwrap()) {
            let mut unchanged = enriched.clone();
            unchanged.as_object_mut().unwrap().remove("composers");
            assert_eq!(*original, unchanged);
        }
        let second = client.library(&params).await.unwrap();
        assert_eq!(diagnostic(&client, "track_credits").await["asked"], 5);
        assert_eq!(diagnostic(&client, "track_credits").await["cached"], 21);
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.metadata_fixture = client.metadata_fixture.clone();
        assert_eq!(restarted.library(&params).await.unwrap(), second);
        assert_eq!(
            diagnostic(&restarted, "track_credits").await["outcome"]["class"],
            "cache_hit"
        );
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].entity_request[0].entity_uri,
            format!("spotify:track:{}", id(20))
        );
    }

    #[tokio::test]
    async fn album_only_listings_fetch_known_tracks_and_union_composers() {
        let first = metadata_reply(
            ExtensionKind::TRACK_V4,
            vec![
                (1, 200, track_metadata(1, &["First", "Shared"])),
                (2, 200, track_metadata(2, &["Shared", "Last"])),
            ],
        );
        let (client, _directory, worker) =
            metadata_client(vec![(200, first, Duration::ZERO)]).await;
        client
            .remember_album_tracks(&[
                json!({"album":{"id":id(100),"tracks":{"items":[{"id":id(1)},{"id":id(2)}]}}}),
            ])
            .await;
        let mut page = json!({"items":[{"ref":format!("album:{}",id(100)),"label":"Existing"},{"ref":format!("album:{}",id(101))}]});
        client.library_composers("albums", &mut page).await;
        assert_eq!(
            page["items"][0]["composers"],
            json!(["First", "Shared", "Last"])
        );
        assert!(page["items"][1].get("composers").is_none());
        client.library_composers("albums", &mut page).await;
        assert_eq!(
            diagnostic(&client, "track_credits").await["outcome"]["class"],
            "cache_hit"
        );
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_answers_expire_and_remain_account_scoped() {
        assert_eq!(REQUEST_LIMIT, 20);
        assert_eq!(TIME_LIMIT, Duration::from_secs(6));
        for (composers, ttl) in [(vec!["Name".into()], FOUND_TTL), (vec![], EMPTY_TTL)] {
            let credit = Credit { at: 100, composers };
            assert!(credit.fresh(100 + ttl - 1));
            assert!(!credit.fresh(100 + ttl));
            assert!(!credit.fresh(99));
        }
        let empty = metadata_reply(
            ExtensionKind::TRACK_V4,
            vec![(1, 200, track_metadata(1, &[]))],
        );
        let (client, _directory, worker) = metadata_client(vec![
            (200, empty, Duration::ZERO),
            (200, reply([1]), Duration::ZERO),
            (200, reply([1]), Duration::ZERO),
        ])
        .await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                "track-credits:v1",
                json!({"tracks":{id(1):{"at":now(),"composers":[]}},"albums":{}}),
            )
            .unwrap();
        let mut page = json!({"items":[track(1)]});
        client.library_composers("tracks", &mut page).await;
        assert_eq!(diagnostic(&client, "track_credits").await["asked"], 1);
        client.library_composers("tracks", &mut page).await;
        assert_eq!(
            diagnostic(&client, "track_credits").await["outcome"]["class"],
            "cache_hit"
        );
        seed(&client, vec![(1, now() - EMPTY_TTL, vec![])]).await;
        client.library_composers("tracks", &mut page).await;
        assert_eq!(page["items"][0]["composers"], json!(["Composer 1"]));
        client.account.lock().await.user = "other".into();
        client.library_composers("tracks", &mut page).await;
        assert_eq!(diagnostic(&client, "track_credits").await["asked"], 1);
        assert_eq!(worker.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn failed_rows_are_not_empty_answers_and_later_rows_are_preserved() {
        let rows = metadata_reply(
            ExtensionKind::TRACK_V4,
            vec![
                (0, 503, vec![]),
                (1, 200, vec![]),
                (2, 404, vec![]),
                (3, 200, track_metadata(3, &["Found"])),
            ],
        );
        let (client, _directory, worker) = metadata_client(vec![
            (200, rows, Duration::ZERO),
            (200, reply(0..3), Duration::ZERO),
        ])
        .await;
        let mut page = json!({"items":(0..4).map(track).collect::<Vec<_>>()});
        client.library_composers("tracks", &mut page).await;
        assert_eq!(page["items"][3]["composers"], json!(["Found"]));
        assert!(page["items"][0].get("composers").is_none());
        assert!(page.get("partial").is_none());
        let cache = client
            .snapshots
            .peek(&client.identity().await.unwrap(), CACHE_KEY)
            .unwrap();
        assert_eq!(cache["tracks"].as_object().unwrap().len(), 1);
        client.library_composers("tracks", &mut page).await;
        assert_eq!(diagnostic(&client, "track_credits").await["asked"], 3);
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_batches_rotate_the_budget_and_concurrent_calls_coalesce() {
        let selected = (20..25).chain(0..15);
        let (client, _directory, worker) = metadata_client(vec![
            (503, vec![], Duration::ZERO),
            (200, reply(selected), Duration::from_millis(30)),
        ])
        .await;
        let page = json!({"items":(0..25).map(track).collect::<Vec<_>>()});
        client.library_composers("tracks", &mut page.clone()).await;
        let mut second = page.clone();
        client.library_composers("tracks", &mut second).await;
        assert!(second["items"][24]["composers"].is_array());
        let requests = worker.join().unwrap();
        assert_eq!(
            requests[1].entity_request[0].entity_uri,
            format!("spotify:track:{}", id(20))
        );
        let (client, _directory, worker) =
            metadata_client(vec![(200, reply([1]), Duration::from_millis(30))]).await;
        let mut first = json!({"items":[track(1)]});
        let mut second = first.clone();
        tokio::join!(
            client.library_composers("tracks", &mut first),
            client.library_composers("tracks", &mut second)
        );
        assert_eq!(first, second);
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cache_write_failures_keep_fetched_composers() {
        let (client, directory, worker) =
            metadata_client(vec![(200, reply([1]), Duration::ZERO)]).await;
        let path = directory.path().join("cache/snapshots");
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "blocked writes").unwrap();
        let mut page = json!({"items":[track(1)]});
        client.library_composers("tracks", &mut page).await;
        assert_eq!(page["items"][0]["composers"], json!(["Composer 1"]));
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_session_and_deadlines_keep_only_fresh_credits() {
        let (client, _directory, worker) = crate::fixtures::client(vec![]).await;
        seed(
            &client,
            vec![
                (0, now() - FOUND_TTL, vec!["Expired"]),
                (1, now(), vec!["Fresh"]),
            ],
        )
        .await;
        let mut page = json!({"items":[track(0),track(1)]});
        client.library_composers("tracks", &mut page).await;
        assert_eq!(page["items"][0], track(0));
        assert_eq!(page["items"][1]["composers"], json!(["Fresh"]));
        assert_eq!(
            diagnostic(&client, "track_credits").await["outcome"]["class"],
            "no_session"
        );
        let _guard = client.credits_gate.lock().await;
        client
            .composers_until(
                "tracks",
                &mut page,
                Instant::now() + Duration::from_millis(10),
            )
            .await;
        assert_eq!(
            diagnostic(&client, "track_credits").await["outcome"],
            json!({"class":"timeout","stage":"gate"})
        );
        assert_eq!(page["items"][1]["composers"], json!(["Fresh"]));
        assert!(worker.join().unwrap().is_empty());
        let (client, _directory, worker) =
            metadata_client(vec![(200, reply([0]), Duration::from_millis(200))]).await;
        seed(&client, vec![(1, now(), vec!["Fresh"])]).await;
        let started = Instant::now();
        client
            .composers_until("tracks", &mut page, started + Duration::from_millis(50))
            .await;
        assert!(started.elapsed() < Duration::from_millis(180));
        assert_eq!(page["items"][1]["composers"], json!(["Fresh"]));
        assert_eq!(
            diagnostic(&client, "track_credits").await["outcome"],
            json!({"class":"timeout","stage":"request"})
        );
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
