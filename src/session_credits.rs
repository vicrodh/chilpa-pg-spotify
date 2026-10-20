use crate::{
    auth::Client,
    state::{now, reference, text},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

const HASH: &str = "f135fb9be58a72d041ab5d214d817021a272405d883860468e2627afb01a3ca9";
const CACHE_KEY: &str = "track-credits:v1";
const ALBUM_KEY: &str = "album-credit-tracks:v1";
const FOUND_TTL: u64 = 90 * 24 * 60 * 60;
const EMPTY_TTL: u64 = 7 * 24 * 60 * 60;
const REQUEST_LIMIT: usize = 20;
const TIME_LIMIT: Duration = Duration::from_secs(6);
static DISABLED: AtomicBool = AtomicBool::new(false);

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

fn query(id: &str) -> Value {
    json!({
        "variables":{"trackUri":format!("spotify:track:{id}"),"contributorsLimit":100,"contributorsOffset":0},
        "operationName":"queryTrackCreditsGroupedModal",
        "extensions":{"persistedQuery":{"version":1,"sha256Hash":HASH}}
    })
}

#[derive(Debug, PartialEq)]
enum Reply {
    Credits(Vec<String>),
    Failed,
    Stop,
}

fn parse(value: &Value, disabled: &AtomicBool) -> Reply {
    if let Some(errors) = value
        .get("errors")
        .filter(|e| !e.is_null() && e.as_array().is_none_or(|errors| !errors.is_empty()))
    {
        if errors.to_string().contains("PersistedQueryNotFound") {
            disabled.store(true, Ordering::Relaxed);
            return Reply::Stop;
        }
        return Reply::Failed;
    }
    let Some(items) = value
        .pointer("/data/trackUnion/creditsTrait/contributors/items")
        .and_then(Value::as_array)
    else {
        return Reply::Failed;
    };
    let mut names = Vec::new();
    for item in items {
        let (Some(name), Some(role), Some(group)) = (
            item["name"].as_str(),
            item["role"].as_str(),
            item.pointer("/roleGroup/name").and_then(Value::as_str),
        ) else {
            return Reply::Failed;
        };
        if name.trim().is_empty() {
            return Reply::Failed;
        }
        let role = role.to_ascii_lowercase();
        let group = group.to_ascii_lowercase();
        if (role.contains("compos")
            || role.contains("songwrit")
            || role == "writer"
            || group == "writers"
            || group == "written by")
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    Reply::Credits(names)
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
        self.composers_until(kind, value, Instant::now() + TIME_LIMIT, &DISABLED)
            .await;
    }

    async fn composers_until(
        &self,
        kind: &str,
        value: &mut Value,
        deadline: Instant,
        disabled: &AtomicBool,
    ) {
        let Ok(Ok(identity)) = timeout_at(deadline, self.identity()).await else {
            return;
        };
        let Ok(_guard) = timeout_at(deadline, self.credits_gate.lock()).await else {
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
        let mut seen = HashSet::new();
        let mut missing: Vec<_> = value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item_id(item, "track"))
            .filter(|id| !saved.tracks.contains_key(id) && seen.insert(id.clone()))
            .collect();
        if let Some(index) = missing
            .iter()
            .position(|id| Some(id) == saved.after.as_ref())
        {
            let start = (index + 1) % missing.len();
            missing.rotate_left(start);
        }
        if kind == "tracks" && !missing.is_empty() && !disabled.load(Ordering::Relaxed) {
            // Session setup, token reads and response bodies share the same deadline.
            if let Ok(Ok(tokens)) = timeout_at(deadline, self.partner_tokens()).await {
                for id in missing.iter().take(REQUEST_LIMIT) {
                    if Instant::now() >= deadline || disabled.load(Ordering::Relaxed) {
                        break;
                    }
                    saved.after = Some(id.clone());
                    match timeout_at(deadline, self.track_credit(id, &tokens, disabled)).await {
                        Ok(Reply::Credits(composers)) => {
                            saved.tracks.insert(
                                id.clone(),
                                Credit {
                                    at: now(),
                                    composers,
                                },
                            );
                        }
                        Ok(Reply::Failed) => {}
                        Ok(Reply::Stop) | Err(_) => break,
                    }
                }
            }
        }
        saved.apply(kind, value);
        if let Ok(cache) = serde_json::to_value(&saved) {
            let _ = self.snapshots.put(&identity, CACHE_KEY, cache);
        }
    }

    async fn track_credit(
        &self,
        id: &str,
        tokens: &(String, String),
        disabled: &AtomicBool,
    ) -> Reply {
        if self.partner_limits.lock().await.remaining("/credits") > 0 {
            return Reply::Stop;
        }
        let Ok(response) = self.partner_request(tokens).json(&query(id)).send().await else {
            return Reply::Failed;
        };
        if response.status().as_u16() == 429 {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            self.partner_limits
                .lock()
                .await
                .record("/credits", seconds, false);
            return Reply::Stop;
        }
        let success = response.status().is_success();
        match Self::response_json(response).await {
            Ok(value) => match parse(&value, disabled) {
                Reply::Stop => Reply::Stop,
                reply if success => reply,
                _ => Reply::Failed,
            },
            Err(_) => Reply::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::JoinHandle;
    use tiny_http::{Header, Method, Response, Server, StatusCode};

    fn credits(items: Value) -> Value {
        json!({"data":{"trackUnion":{"creditsTrait":{"contributors":{"items":items}}}}})
    }

    fn writer(name: &str) -> Value {
        json!({"name":name,"role":"Composer","roleGroup":{"name":"Writers"}})
    }

    fn reply(name: &str) -> String {
        credits(json!([writer(name)])).to_string()
    }

    fn id(index: usize) -> String {
        format!("{index:022}")
    }

    fn track(index: usize) -> Value {
        json!({
            "kind":"track","ref":format!("track:{}", id(index)),
            "title":format!("Song {index}"),"album_ref":format!("album:{}", id(100)),
            "duration_ms":180000,"year":2000,"genres":["ambient"],"label":"Example Records",
            "added_at":1704164645_i64,"release_type":"album","quality":{"codec":"vorbis"},
            "composer":"Work header","composer_ref":"work-header","work":"Suite"
        })
    }

    async fn seed_page(client: &Client, kind: &str, items: Vec<Value>) {
        client
            .snapshots
            .put(
                &client.identity().await.unwrap(),
                &format!("library:{kind}:0:50:fields-v2"),
                json!({"total":items.len(),"items":items}),
            )
            .unwrap();
    }

    async fn save_cache(client: &Client, entries: Vec<(usize, u64, Vec<&str>)>) {
        let mut saved = Cache::default();
        for (index, at, composers) in entries {
            saved.tracks.insert(
                id(index),
                Credit {
                    at,
                    composers: composers.into_iter().map(str::to_owned).collect(),
                },
            );
        }
        client
            .snapshots
            .put(
                &client.identity().await.unwrap(),
                CACHE_KEY,
                serde_json::to_value(saved).unwrap(),
            )
            .unwrap();
    }

    async fn fixture(
        replies: Vec<(u16, String, Duration)>,
    ) -> (Client, tempfile::TempDir, JoinHandle<Vec<Value>>) {
        let (mut client, directory, worker) = crate::fixtures::client(vec![]).await;
        assert!(worker.join().unwrap().is_empty());
        let server = Server::http("127.0.0.1:0").unwrap();
        client.partner_fixture = Some(format!(
            "http://{}/pathfinder/v2/query",
            server.server_addr()
        ));
        let worker = std::thread::spawn(move || {
            let mut queries = Vec::new();
            for (status, body, delay) in replies {
                let mut request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .expect("expected credit request");
                assert_eq!(request.method(), &Method::Post);
                assert_eq!(request.url(), "/pathfinder/v2/query");
                for (name, expected) in [
                    ("Authorization", "Bearer fixture-access"),
                    ("client-token", "fixture-client"),
                ] {
                    assert!(
                        request
                            .headers()
                            .iter()
                            .any(|h| { h.field.equiv(name) && h.value.as_str() == expected })
                    );
                }
                let body_value: Value = serde_json::from_reader(request.as_reader()).unwrap();
                assert_eq!(body_value["operationName"], "queryTrackCreditsGroupedModal");
                assert_eq!(
                    body_value["extensions"]["persistedQuery"],
                    json!({"version":1,"sha256Hash":HASH})
                );
                assert_eq!(body_value["variables"]["contributorsLimit"], 100);
                assert_eq!(body_value["variables"]["contributorsOffset"], 0);
                assert_eq!(body_value["variables"].as_object().unwrap().len(), 3);
                queries.push(body_value);
                std::thread::sleep(delay);
                let _ = request.respond(
                    Response::from_string(body)
                        .with_status_code(StatusCode(status))
                        .with_header(Header::from_bytes("Retry-After", "60").unwrap()),
                );
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none(),
                "unexpected extra credit request"
            );
            queries
        });
        (client, directory, worker)
    }

    fn immediate(status: u16, body: String) -> (u16, String, Duration) {
        (status, body, Duration::ZERO)
    }

    #[test]
    fn roles_keep_writers_in_order_and_drop_duplicates() {
        let contributors = [
            ("Performer", "Vocals", "Performers"),
            ("First", "cOmPoSeR", "Other"),
            ("Second", "SongWriting", "Other"),
            ("Third", "WRITER", "Other"),
            ("Fourth", "Lyricist", "WrItErS"),
            ("Fifth", "Lyricist", "Written BY"),
            ("First", "Composition", "Writers"),
            ("Producer", "Producer", "Production"),
            ("Copyist", "Copywriter", "Other"),
        ].map(|(name, role, group)| json!({"name":name,"role":role,"roleGroup":{"name":group},"uri":"optional"}));
        let disabled = AtomicBool::new(false);
        assert_eq!(
            parse(&credits(json!(contributors)), &disabled),
            Reply::Credits(
                ["First", "Second", "Third", "Fourth", "Fifth"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
        assert_eq!(
            parse(&credits(json!([])), &disabled),
            Reply::Credits(vec![])
        );
        assert_eq!(
            parse(
                &credits(
                    json!([{"name":"Singer","role":"Vocals","roleGroup":{"name":"Performers"}}])
                ),
                &disabled
            ),
            Reply::Credits(vec![])
        );
        for value in [
            json!({}),
            json!({"data":null}),
            credits(json!({})),
            credits(json!([null])),
            credits(json!([{"name":"Incomplete","role":"Composer"}])),
            credits(json!([{"name":" ","role":"Composer","roleGroup":{"name":"Writers"}}])),
            json!({"errors":[{"message":"Denied"}]}),
        ] {
            assert_eq!(parse(&value, &disabled), Reply::Failed);
        }
        assert!(!disabled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn request_budget_resumes_cached_pages_and_survives_restart() {
        let (client, directory, worker) = fixture(
            (0..25)
                .map(|i| immediate(200, reply(&format!("Writer {i}"))))
                .collect(),
        )
        .await;
        let mut items: Vec<_> = (0..25).map(track).collect();
        items.push(track(0));
        items.push(track(99));
        seed_page(&client, "tracks", items.clone()).await;
        save_cache(&client, vec![(99, now(), vec!["Cached writer"])]).await;
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert_eq!(first["total"], 27);
        assert_eq!(
            first["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v.get("composers").is_some())
                .count(),
            22
        );
        assert!(first["items"][20].get("composers").is_none());
        assert_eq!(first["items"][26]["composers"], json!(["Cached writer"]));
        for (original, enriched) in items.iter().zip(first["items"].as_array().unwrap()) {
            let mut unchanged = enriched.clone();
            unchanged.as_object_mut().unwrap().remove("composers");
            assert_eq!(&unchanged, original);
        }
        let second = client.library(&params).await.unwrap();
        assert!(
            second["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["composers"].is_array())
        );
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.api_base = client.api_base.clone();
        restarted.partner_fixture = client.partner_fixture.clone();
        assert_eq!(restarted.library(&params).await.unwrap(), second);
        let queries = worker.join().unwrap();
        assert_eq!(queries.len(), 25);
        for (i, query) in queries.iter().enumerate() {
            assert_eq!(
                query["variables"]["trackUri"],
                format!("spotify:track:{}", id(i))
            );
        }
    }

    #[test]
    fn ttl_distinguishes_found_empty_expired_and_future_answers() {
        assert_eq!(REQUEST_LIMIT, 20);
        assert_eq!(TIME_LIMIT, Duration::from_secs(6));
        assert_eq!(FOUND_TTL, 7_776_000);
        assert_eq!(EMPTY_TTL, 604_800);
        for (composers, ttl) in [(vec!["Writer".into()], FOUND_TTL), (vec![], EMPTY_TTL)] {
            let credit = Credit { at: 100, composers };
            assert!(credit.fresh(100 + ttl - 1));
            assert!(!credit.fresh(100 + ttl));
            assert!(!credit.fresh(99));
        }
    }

    #[tokio::test]
    async fn empty_answers_are_cached_expire_and_stay_account_scoped() {
        let (client, _directory, worker) = fixture(vec![
            immediate(200, credits(json!([])).to_string()),
            immediate(200, reply("After expiry")),
            immediate(200, reply("Other account")),
        ])
        .await;
        seed_page(&client, "tracks", vec![track(1)]).await;
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert!(first["items"][0].get("composers").is_none());
        assert_eq!(client.library(&params).await.unwrap(), first);
        let identity = client.identity().await.unwrap();
        let cached = client.snapshots.peek(&identity, CACHE_KEY).unwrap();
        assert_eq!(cached["tracks"][id(1)]["composers"], json!([]));
        save_cache(&client, vec![(1, now() - EMPTY_TTL, vec![])]).await;
        assert_eq!(
            client.library(&params).await.unwrap()["items"][0]["composers"],
            json!(["After expiry"])
        );
        client.account.lock().await.user = "other".into();
        seed_page(&client, "tracks", vec![track(1)]).await;
        assert_eq!(
            client.library(&params).await.unwrap()["items"][0]["composers"],
            json!(["Other account"])
        );
        assert_eq!(
            client.snapshots.peek(&identity, CACHE_KEY).unwrap()["tracks"][id(1)]["composers"],
            json!(["After expiry"])
        );
        assert_eq!(worker.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn failures_do_not_fail_or_mark_the_listing_partial_and_retry_later() {
        let mut replies = vec![
            immediate(503, "{}".into()),
            immediate(401, "{}".into()),
            immediate(403, "{}".into()),
            immediate(200, "invalid json".into()),
            immediate(200, "{}".into()),
            immediate(200, json!({"errors":[{"message":"Denied"}]}).to_string()),
            immediate(200, reply("Last writer")),
        ];
        replies.extend((0..6).map(|_| immediate(200, reply("Recovered"))));
        let (client, _directory, worker) = fixture(replies).await;
        seed_page(&client, "tracks", (0..7).map(track).collect()).await;
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert_eq!(first["total"], 7);
        assert!(first.get("partial").is_none());
        assert!(first.get("cached").is_none());
        assert!(
            first["items"].as_array().unwrap()[..6]
                .iter()
                .all(|v| v.get("composers").is_none())
        );
        assert_eq!(first["items"][6]["composers"], json!(["Last writer"]));
        let cached = client
            .snapshots
            .peek(&client.identity().await.unwrap(), CACHE_KEY)
            .unwrap();
        assert_eq!(cached["tracks"].as_object().unwrap().len(), 1);
        let second = client.library(&params).await.unwrap();
        assert!(
            second["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["composers"].is_array())
        );
        assert_eq!(worker.join().unwrap().len(), 13);
    }

    #[tokio::test]
    async fn repeated_failures_do_not_starve_tracks_after_the_request_budget() {
        let mut replies: Vec<_> = (0..20).map(|_| immediate(503, "{}".into())).collect();
        replies.extend((0..20).map(|_| immediate(200, reply("Recovered"))));
        let (client, _directory, worker) = fixture(replies).await;
        seed_page(&client, "tracks", (0..25).map(track).collect()).await;
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert!(
            first["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v.get("composers").is_none())
        );
        let second = client.library(&params).await.unwrap();
        assert!(
            second["items"].as_array().unwrap()[20..]
                .iter()
                .all(|v| v["composers"].is_array())
        );
        let queries = worker.join().unwrap();
        assert_eq!(queries.len(), 40);
        assert_eq!(
            queries[20]["variables"]["trackUri"],
            format!("spotify:track:{}", id(20))
        );
        assert_eq!(
            queries[25]["variables"]["trackUri"],
            format!("spotify:track:{}", id(0))
        );
    }

    #[tokio::test]
    async fn concurrent_listings_share_successful_credit_requests() {
        let (client, _directory, worker) =
            fixture(vec![(200, reply("Writer"), Duration::from_millis(100))]).await;
        seed_page(&client, "tracks", vec![track(1)]).await;
        let params = json!({"kind":"tracks"});
        let (first, second) = tokio::join!(client.library(&params), client.library(&params));
        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limit_stops_requests_but_keeps_cached_and_completed_credits() {
        let (client, _directory, worker) = fixture(vec![
            immediate(200, reply("First")),
            immediate(429, "{}".into()),
        ])
        .await;
        seed_page(&client, "tracks", (0..4).map(track).collect()).await;
        save_cache(&client, vec![(3, now(), vec!["Cached"])]).await;
        let params = json!({"kind":"tracks"});
        let first = client.library(&params).await.unwrap();
        assert_eq!(first["items"][0]["composers"], json!(["First"]));
        assert!(first["items"][1].get("composers").is_none());
        assert!(first["items"][2].get("composers").is_none());
        assert_eq!(first["items"][3]["composers"], json!(["Cached"]));
        assert_eq!(client.library(&params).await.unwrap(), first);
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn one_wall_deadline_cancels_slow_requests_and_keeps_earlier_answers() {
        let (client, _directory, worker) = fixture(vec![
            (200, reply("First"), Duration::from_millis(300)),
            (200, reply("Too late"), Duration::from_secs(6)),
        ])
        .await;
        seed_page(&client, "tracks", (0..4).map(track).collect()).await;
        save_cache(&client, vec![(3, now(), vec!["Cached"])]).await;
        let started = Instant::now();
        let value = client.library(&json!({"kind":"tracks"})).await.unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= TIME_LIMIT);
        assert!(
            elapsed < TIME_LIMIT + Duration::from_millis(250),
            "{elapsed:?}"
        );
        assert_eq!(value["items"][0]["composers"], json!(["First"]));
        assert!(value["items"][1].get("composers").is_none());
        assert!(value["items"][2].get("composers").is_none());
        assert_eq!(value["items"][3]["composers"], json!(["Cached"]));
        let saved = client
            .snapshots
            .peek(&client.identity().await.unwrap(), CACHE_KEY)
            .unwrap();
        assert_eq!(saved["tracks"][id(0)]["composers"], json!(["First"]));
        assert!(saved["tracks"].get(id(1)).is_none());
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn missing_hash_disables_later_calls_and_clients_but_not_cache_hits() {
        let (client, directory, worker) = fixture(vec![immediate(
            200,
            json!({"errors":[{"message":"PersistedQueryNotFound"}]}).to_string(),
        )])
        .await;
        save_cache(&client, vec![(3, now(), vec!["Cached"])]).await;
        let disabled = AtomicBool::new(false);
        let mut value = json!({"items":[track(0),track(1),track(3)]});
        client
            .composers_until("tracks", &mut value, Instant::now() + TIME_LIMIT, &disabled)
            .await;
        assert!(disabled.load(Ordering::Relaxed));
        let mut replacement = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *replacement.account.lock().await = client.account.lock().await.clone();
        replacement.partner_fixture = client.partner_fixture.clone();
        replacement
            .composers_until("tracks", &mut value, Instant::now() + TIME_LIMIT, &disabled)
            .await;
        assert!(value["items"][0].get("composers").is_none());
        assert!(value["items"][1].get("composers").is_none());
        assert_eq!(value["items"][2]["composers"], json!(["Cached"]));
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_hash_in_a_graphql_http_error_also_disables_requests() {
        let (client, _directory, worker) = fixture(vec![immediate(
            400,
            json!({"errors":[{
                "message":"Unknown operation","extensions":{"code":"PersistedQueryNotFound"}
            }]})
            .to_string(),
        )])
        .await;
        let disabled = AtomicBool::new(false);
        let mut value = json!({"items":[track(0),track(1)]});
        client
            .composers_until("tracks", &mut value, Instant::now() + TIME_LIMIT, &disabled)
            .await;
        assert!(disabled.load(Ordering::Relaxed));
        assert!(
            value["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v.get("composers").is_none())
        );
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cache_write_failure_keeps_successful_credits_in_the_listing() {
        let (client, directory, worker) = fixture(vec![immediate(200, reply("Writer"))]).await;
        let snapshots = directory.path().join("cache/snapshots");
        std::fs::remove_dir(&snapshots).unwrap();
        std::fs::write(&snapshots, "unwritable cache directory").unwrap();
        let mut value = json!({"items":[track(1)]});
        client.library_composers("tracks", &mut value).await;
        assert_eq!(value["items"][0]["composers"], json!(["Writer"]));
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_session_and_expired_credits_leave_fields_unchanged() {
        let (client, _directory, worker) = crate::fixtures::client(vec![]).await;
        seed_page(&client, "tracks", vec![track(1), track(2)]).await;
        save_cache(
            &client,
            vec![
                (1, now() - FOUND_TTL, vec!["Expired"]),
                (2, now(), vec!["Fresh"]),
            ],
        )
        .await;
        let value = client.library(&json!({"kind":"tracks"})).await.unwrap();
        assert_eq!(value["items"][0], track(1));
        assert_eq!(value["items"][1]["composers"], json!(["Fresh"]));
        assert_eq!(value["items"][1]["composer"], "Work header");
        assert_eq!(value["items"][1]["composer_ref"], "work-header");
        assert_eq!(value["items"][1]["work"], "Suite");
        assert!(worker.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn albums_union_known_tracks_without_requests_including_after_restart() {
        let (client, directory, worker) = fixture(vec![
            immediate(
                200,
                credits(json!([writer("First"), writer("Shared")])).to_string(),
            ),
            immediate(
                200,
                credits(json!([writer("Shared"), writer("Last")])).to_string(),
            ),
        ])
        .await;
        seed_page(&client, "tracks", vec![track(1), track(2)]).await;
        client.library(&json!({"kind":"tracks"})).await.unwrap();
        seed_page(
            &client,
            "albums",
            vec![
                json!({"kind":"album","ref":format!("album:{}", id(100))}),
                json!({"kind":"album","ref":format!("album:{}", id(101))}),
            ],
        )
        .await;
        let value = client.library(&json!({"kind":"albums"})).await.unwrap();
        assert_eq!(
            value["items"][0]["composers"],
            json!(["First", "Shared", "Last"])
        );
        assert!(value["items"][1].get("composers").is_none());
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.partner_fixture = client.partner_fixture.clone();
        assert_eq!(
            restarted.library(&json!({"kind":"albums"})).await.unwrap(),
            value
        );
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn albums_use_embedded_and_previously_cached_record_tracks() {
        let (client, _directory, worker) = fixture(vec![]).await;
        save_cache(
            &client,
            vec![
                (1, now(), vec!["First", "Shared"]),
                (2, now(), vec!["Shared", "Last"]),
                (3, now() - FOUND_TTL, vec!["Expired"]),
            ],
        )
        .await;
        let identity = client.identity().await.unwrap();
        let album = |index| {
            json!({"album":{
                "id":id(index),"tracks":{"items":[{"id":id(2)},{"id":id(1)},{"id":id(3)}]}
            }})
        };
        let query = [("limit", "50"), ("offset", "0")];
        client
            .snapshots
            .put(
                &identity,
                &format!(
                    "web:Primary:0:/me/albums:{}",
                    serde_json::to_string(&query).unwrap()
                ),
                json!({"items":[album(100)],"total":1,"next":null}),
            )
            .unwrap();
        // Old cards are rebuilt from the cached service response to retain membership.
        client
            .snapshots
            .put(
                &identity,
                "library:albums:0:50:fields-v1",
                json!({"items":[{"kind":"album","ref":format!("album:{}", id(100))}],"total":1}),
            )
            .unwrap();
        let value = client.library(&json!({"kind":"albums"})).await.unwrap();
        assert_eq!(
            value["items"][0]["composers"],
            json!(["Shared", "Last", "First"])
        );
        client
            .snapshots
            .put(
                &identity,
                &format!("session:v2:album:{}:0", id(101)),
                json!({"tracks":[track(1),track(2)]}),
            )
            .unwrap();
        client
            .snapshots
            .put(
                &identity,
                &format!("web:Primary:0:/albums/{}:[]", id(102)),
                album(102)["album"].clone(),
            )
            .unwrap();
        seed_page(
            &client,
            "albums",
            (101..103)
                .map(|index| {
                    json!({
                        "kind":"album","ref":format!("album:{}", id(index))
                    })
                })
                .collect(),
        )
        .await;
        let value = client.library(&json!({"kind":"albums"})).await.unwrap();
        assert_eq!(
            value["items"][0]["composers"],
            json!(["First", "Shared", "Last"])
        );
        assert_eq!(
            value["items"][1]["composers"],
            json!(["Shared", "Last", "First"])
        );
        assert!(worker.join().unwrap().is_empty());
    }
}
