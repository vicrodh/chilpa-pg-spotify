use crate::{
    auth::Client,
    enrichment::{Attempt, Outcome, TimeoutStage, is_disabled_error, record, stops_requests},
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

const HASH: &str = "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f";
const CACHE_KEY: &str = "album-label:v1";
const FOUND_TTL: u64 = 90 * 24 * 60 * 60;
const EMPTY_TTL: u64 = 7 * 24 * 60 * 60;
const REQUEST_LIMIT: usize = 20;
const TIME_LIMIT: Duration = Duration::from_secs(6);

#[derive(Deserialize, Serialize)]
struct Label {
    at: u64,
    label: Option<String>,
}

impl Label {
    fn fresh(&self, current: u64) -> bool {
        let ttl = if self.label.is_some() {
            FOUND_TTL
        } else {
            EMPTY_TTL
        };
        self.at <= current && current - self.at < ttl
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    albums: HashMap<String, Label>,
    #[serde(default)]
    after: Option<String>,
}

impl Cache {
    fn apply(&self, value: &mut Value) {
        for item in value["items"].as_array_mut().into_iter().flatten() {
            if !text(item, "label").trim().is_empty() {
                continue;
            }
            let Some(id) = album_id(item) else {
                continue;
            };
            if let Some(label) = self
                .albums
                .get(&id)
                .filter(|entry| entry.fresh(now()))
                .and_then(|entry| entry.label.as_ref())
            {
                item["label"] = json!(label);
                item["label_source"] = json!("copyright");
            }
        }
    }
}

fn album_id(item: &Value) -> Option<String> {
    reference(text(item, "ref"), Some("album"))
        .ok()
        .map(|(_, id)| id)
}

fn query(id: &str) -> Value {
    json!({
        "operationName":"getAlbum",
        "variables":{"uri":format!("spotify:album:{id}"),"locale":"","offset":0,"limit":50},
        "extensions":{"persistedQuery":{"version":1,"sha256Hash":HASH}}
    })
}

fn parse(value: &Value) -> Result<Option<String>, Outcome> {
    let album = value
        .pointer("/data/albumUnion")
        .filter(|album| album["__typename"] == "Album")
        .ok_or(Outcome::UnexpectedShape)?;
    let items = album
        .pointer("/copyright/items")
        .and_then(Value::as_array)
        .ok_or(Outcome::UnexpectedShape)?;
    let mut phonographic = Vec::new();
    let mut copyright = Vec::new();
    for item in items {
        let line = item["text"]
            .as_str()
            .filter(|line| !line.trim().is_empty())
            .ok_or(Outcome::UnexpectedShape)?;
        let lines = match item["type"].as_str() {
            Some("P") => &mut phonographic,
            Some("C") => &mut copyright,
            _ => return Err(Outcome::UnexpectedShape),
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    let lines = if phonographic.is_empty() {
        copyright
    } else {
        phonographic
    };
    Ok((!lines.is_empty()).then(|| lines.join(" / ")))
}

impl Client {
    fn label_cache(&self, identity: &str) -> Cache {
        let mut saved: Cache = self
            .snapshots
            .peek(identity, CACHE_KEY)
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        saved.albums.retain(|_, label| label.fresh(now()));
        saved
    }

    pub(crate) async fn library_labels(&self, value: &mut Value) {
        self.labels_until(value, Instant::now() + TIME_LIMIT, &self.labels_disabled)
            .await;
    }

    async fn labels_until(&self, value: &mut Value, deadline: Instant, disabled: &AtomicBool) {
        let mut attempt = Attempt::start();
        for item in value["items"].as_array_mut().into_iter().flatten() {
            if item["label_source"] == "copyright" {
                if let Some(item) = item.as_object_mut() {
                    item.remove("label");
                    item.remove("label_source");
                }
            }
        }
        let Ok(Ok(identity)) = timeout_at(deadline, self.identity()).await else {
            return;
        };
        let mut seen = HashSet::new();
        let ids: Vec<_> = value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| text(item, "label").trim().is_empty())
            .filter_map(album_id)
            .filter(|id| seen.insert(id.clone()))
            .collect();
        if ids.is_empty() {
            attempt.outcome = Outcome::NoCandidates;
            let _ = record(&self.snapshots, &identity, "album_label", attempt);
            return;
        }
        let Ok(_guard) = timeout_at(deadline, self.labels_gate.lock()).await else {
            let saved = self.label_cache(&identity);
            attempt.cached = ids
                .iter()
                .filter(|id| saved.albums.contains_key(*id))
                .count();
            saved.apply(value);
            attempt.fail(Outcome::Timeout {
                stage: TimeoutStage::Gate,
            });
            let _ = record(&self.snapshots, &identity, "album_label", attempt);
            return;
        };
        let mut saved = self.label_cache(&identity);
        attempt.cached = ids
            .iter()
            .filter(|id| saved.albums.contains_key(*id))
            .count();
        saved.apply(value);
        let mut missing: Vec<_> = ids
            .into_iter()
            .filter(|id| !saved.albums.contains_key(id))
            .collect();
        if let Some(index) = missing
            .iter()
            .position(|id| Some(id) == saved.after.as_ref())
        {
            let start = (index + 1) % missing.len();
            missing.rotate_left(start);
        }
        if missing.is_empty() {
            attempt.outcome = Outcome::CacheHit;
        } else if disabled.load(Ordering::Relaxed) {
            attempt.outcome = Outcome::Disabled;
        } else {
            match self.enrichment_tokens(deadline).await {
                Err(outcome) => attempt.fail(outcome),
                Ok(tokens) => {
                    for id in missing.iter().take(REQUEST_LIMIT) {
                        if Instant::now() >= deadline {
                            attempt.fail(Outcome::Timeout {
                                stage: TimeoutStage::Request,
                            });
                            break;
                        }
                        if disabled.load(Ordering::Relaxed) {
                            attempt.fail(Outcome::Disabled);
                            break;
                        }
                        let request = async {
                            if self.partner_limits.lock().await.remaining("/album-label") > 0 {
                                return Err(Outcome::Http { status: 429 });
                            }
                            attempt.asked += 1;
                            saved.after = Some(id.clone());
                            let value = self
                                .enrichment_query(&query(id), &tokens, "/album-label")
                                .await?;
                            parse(&value)
                        };
                        let result =
                            timeout_at(deadline, request)
                                .await
                                .unwrap_or(Err(Outcome::Timeout {
                                    stage: TimeoutStage::Request,
                                }));
                        match result {
                            Ok(label) => {
                                attempt.with_data += usize::from(label.is_some());
                                saved.albums.insert(id.clone(), Label { at: now(), label });
                            }
                            Err(outcome) => {
                                if is_disabled_error(&outcome) {
                                    disabled.store(true, Ordering::Relaxed);
                                }
                                attempt.fail(outcome);
                                if stops_requests(&outcome)
                                    || matches!(outcome, Outcome::Timeout { .. })
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        saved.apply(value);
        if let Ok(value) = serde_json::to_value(saved) {
            let _ = self.snapshots.put(&identity, CACHE_KEY, value);
        }
        let _ = record(&self.snapshots, &identity, "album_label", attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::JoinHandle;
    use tiny_http::{Header, Method, Response, Server, StatusCode};

    fn id(index: usize) -> String {
        format!("{index:022}")
    }

    fn album(index: usize) -> Value {
        json!({"kind":"album","ref":format!("album:{}", id(index)),"title":"Fixture album"})
    }

    fn reply(items: Value) -> Value {
        json!({"data":{"albumUnion":{
            "__typename":"Album","copyright":{"items":items,"totalCount":items.as_array().map(Vec::len)},
            "courtesyLine":null
        }}})
    }

    fn found() -> String {
        reply(json!([{"type":"P","text":"2026 Example Records"}])).to_string()
    }

    #[tokio::test]
    async fn album_library_fills_labels_on_cached_pages() {
        let (client, _directory, worker) = fixture(vec![(200, found(), Duration::ZERO)]).await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                "library:albums:0:50:fields-v2",
                json!({"items":[album(1)],"total":1}),
            )
            .unwrap();
        let params = json!({"kind":"albums"});
        let first = client.library(&params).await.unwrap();
        assert_eq!(first["items"][0]["label"], "2026 Example Records");
        assert_eq!(first["items"][0]["label_source"], "copyright");
        assert_eq!(client.library(&params).await.unwrap(), first);
        let diagnostic = client
            .snapshots
            .peek(&identity, crate::enrichment::KEY)
            .unwrap();
        assert_eq!(diagnostic["album_label"]["outcome"]["class"], "cache_hit");
        assert_eq!(diagnostic.as_object().unwrap().len(), 3);
        assert_eq!(worker.join().unwrap().len(), 1);
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
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .expect("expected album request");
                assert_eq!(request.method(), &Method::Post);
                assert_eq!(request.url(), "/pathfinder/v2/query");
                for (name, expected) in [
                    ("Authorization", "Bearer fixture-access"),
                    ("client-token", "fixture-client"),
                    ("spotify-app-version", "896000000"),
                ] {
                    assert!(request.headers().iter().any(|header| {
                        header.field.equiv(name) && header.value.as_str() == expected
                    }));
                }
                let value: Value = serde_json::from_reader(request.as_reader()).unwrap();
                let uri = value["variables"]["uri"].as_str().unwrap();
                let album_id = uri.strip_prefix("spotify:album:").unwrap();
                assert_eq!(value, query(album_id));
                queries.push(value);
                std::thread::sleep(delay);
                let _ = request.respond(
                    Response::from_string(body)
                        .with_status_code(StatusCode(status))
                        .with_header(Header::from_bytes("Retry-After", "60").unwrap()),
                );
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none(),
                "unexpected extra album request"
            );
            queries
        });
        (client, directory, worker)
    }

    async fn diagnostic(client: &Client) -> Value {
        client
            .snapshots
            .peek(&client.identity().await.unwrap(), crate::enrichment::KEY)
            .unwrap()["album_label"]
            .clone()
    }

    async fn save(client: &Client, entries: Vec<(usize, u64, Option<&str>)>) {
        let saved = Cache {
            albums: entries
                .into_iter()
                .map(|(index, at, label)| {
                    (
                        id(index),
                        Label {
                            at,
                            label: label.map(str::to_owned),
                        },
                    )
                })
                .collect(),
            after: None,
        };
        client
            .snapshots
            .put(&client.identity().await.unwrap(), CACHE_KEY, json!(saved))
            .unwrap();
    }

    #[test]
    fn copyright_text_is_verbatim_and_never_inferred_from_courtesy() {
        assert_eq!(
            parse(&reply(json!([
                {"type":"C","text":"2025 Other owner"},
                {"type":"P","text":"2026 Example Records"},
                {"type":"P","text":"2026 Second owner"},
                {"type":"P","text":"2026 Example Records"}
            ]))),
            Ok(Some("2026 Example Records / 2026 Second owner".into()))
        );
        assert_eq!(
            parse(&reply(
                json!([{"type":"C","text":" 2026 Example Records "}])
            )),
            Ok(Some(" 2026 Example Records ".into()))
        );
        let mut empty = reply(json!([]));
        empty["data"]["albumUnion"]["courtesyLine"] = json!("Courtesy of an artist");
        assert_eq!(parse(&empty), Ok(None));
        for value in [
            json!({}),
            json!({"data":{"albumUnion":{"__typename":"NotFound"}}}),
            json!({"data":{"albumUnion":{"__typename":"Album"}}}),
            reply(json!({})),
            reply(json!([{"type":"P"}])),
            reply(json!([{"type":"P","text":" "}])),
            reply(json!([{"type":"future","text":"Example Records"}])),
        ] {
            assert_eq!(parse(&value), Err(Outcome::UnexpectedShape));
        }
    }

    #[tokio::test]
    async fn request_budget_resumes_and_cache_survives_restart_without_crossing_accounts() {
        let (client, directory, worker) =
            fixture((0..26).map(|_| (200, found(), Duration::ZERO)).collect()).await;
        let mut items: Vec<_> = (0..25).map(album).collect();
        items.push(album(0));
        let mut existing = album(100);
        existing["label"] = json!("Original label");
        items.push(existing.clone());
        let page = json!({"items":items,"total":27});
        let mut first = page.clone();
        client.library_labels(&mut first).await;
        assert_eq!(first["items"][0]["label"], "2026 Example Records");
        assert_eq!(first["items"][0]["label_source"], "copyright");
        assert!(first["items"][20].get("label").is_none());
        assert_eq!(first["items"][25]["label"], "2026 Example Records");
        assert_eq!(first["items"][26], existing);
        assert_eq!(diagnostic(&client).await["asked"], 20);
        assert_eq!(diagnostic(&client).await["with_data"], 20);
        let mut second = page.clone();
        client.library_labels(&mut second).await;
        assert_eq!(diagnostic(&client).await["asked"], 5);
        assert_eq!(diagnostic(&client).await["cached"], 20);
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.partner_fixture = client.partner_fixture.clone();
        let mut cached = page;
        restarted.library_labels(&mut cached).await;
        assert_eq!(cached, second);
        assert_eq!(
            diagnostic(&restarted).await["outcome"]["class"],
            "cache_hit"
        );
        assert_eq!(diagnostic(&restarted).await["cached"], 25);
        restarted.account.lock().await.user = "second-account".into();
        restarted
            .library_labels(&mut json!({"items":[album(0)]}))
            .await;
        assert_eq!(diagnostic(&restarted).await["asked"], 1);
        let queries = worker.join().unwrap();
        assert_eq!(queries.len(), 26);
        for (index, value) in queries[..25].iter().enumerate() {
            assert_eq!(
                value["variables"]["uri"],
                format!("spotify:album:{}", id(index))
            );
        }
    }

    #[tokio::test]
    async fn empty_answers_are_cached_and_expire_separately_from_found_labels() {
        assert_eq!(REQUEST_LIMIT, 20);
        assert_eq!(TIME_LIMIT, Duration::from_secs(6));
        for (label, ttl) in [
            (Some("Example Records".into()), FOUND_TTL),
            (None, EMPTY_TTL),
        ] {
            let entry = Label { at: 100, label };
            assert!(entry.fresh(100 + ttl - 1));
            assert!(!entry.fresh(100 + ttl));
            assert!(!entry.fresh(99));
        }
        let (client, _directory, worker) = fixture(vec![
            (200, reply(json!([])).to_string(), Duration::ZERO),
            (200, found(), Duration::ZERO),
            (200, found(), Duration::ZERO),
        ])
        .await;
        let mut page = json!({"items":[album(1)]});
        client.library_labels(&mut page).await;
        assert!(page["items"][0].get("label").is_none());
        assert_eq!(diagnostic(&client).await["with_data"], 0);
        client.library_labels(&mut page).await;
        assert_eq!(diagnostic(&client).await["outcome"]["class"], "cache_hit");
        save(&client, vec![(1, now() - EMPTY_TTL, None)]).await;
        client.library_labels(&mut page).await;
        assert_eq!(page["items"][0]["label"], "2026 Example Records");
        save(
            &client,
            vec![(1, now() - FOUND_TTL, Some("Expired copyright"))],
        )
        .await;
        client.library_labels(&mut page).await;
        assert_eq!(page["items"][0]["label"], "2026 Example Records");
        assert_eq!(diagnostic(&client).await["asked"], 1);
        assert_eq!(worker.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn failed_responses_are_diagnosed_and_never_cached_as_empty_answers() {
        let replies = [
            (503, "{}".into()),
            (401, "{}".into()),
            (403, "{}".into()),
            (200, "invalid json".into()),
            (200, "{}".into()),
            (
                200,
                json!({"errors":[{"extensions":{"code":"FORBIDDEN"},"message":"private-name"}]})
                    .to_string(),
            ),
            (200, found()),
        ];
        let (client, _directory, worker) = fixture(
            replies
                .into_iter()
                .map(|(status, body)| (status, body, Duration::ZERO))
                .collect(),
        )
        .await;
        let mut page = json!({"items":(0..7).map(album).collect::<Vec<_>>(),"total":7});
        client.library_labels(&mut page).await;
        assert!(page.get("partial").is_none());
        assert_eq!(page["items"][6]["label"], "2026 Example Records");
        assert!(
            page["items"].as_array().unwrap()[..6]
                .iter()
                .all(|item| item.get("label").is_none())
        );
        assert_eq!(
            client
                .label_cache(&client.identity().await.unwrap())
                .albums
                .len(),
            1
        );
        let saved = diagnostic(&client).await;
        assert_eq!(saved["asked"], 7);
        assert_eq!(saved["with_data"], 1);
        assert_eq!(saved["outcome"], json!({"class":"http","status":503}));
        assert!(!saved.to_string().contains("private-name"));
        assert_eq!(worker.join().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn missing_hash_disables_requests_but_cached_labels_still_apply() {
        let (client, _directory, worker) = fixture(vec![(
            200,
            json!({"errors":[{"message":"PersistedQueryNotFound"}]}).to_string(),
            Duration::ZERO,
        )])
        .await;
        save(&client, vec![(2, now(), Some("Cached copyright"))]).await;
        let original = json!({"items":(0..3).map(album).collect::<Vec<_>>()});
        let mut page = original.clone();
        client.library_labels(&mut page).await;
        assert!(client.labels_disabled.load(Ordering::Relaxed));
        assert_eq!(page["items"][2]["label"], "Cached copyright");
        assert_eq!(diagnostic(&client).await["asked"], 1);
        assert_eq!(
            diagnostic(&client).await["outcome"]["code"],
            "PersistedQueryNotFound"
        );
        client.library_labels(&mut original.clone()).await;
        assert_eq!(diagnostic(&client).await["outcome"]["class"], "disabled");
        assert_eq!(diagnostic(&client).await["asked"], 0);
        assert_eq!(diagnostic(&client).await["cached"], 1);
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deadline_keeps_earlier_answers_and_records_timeout() {
        let (client, _directory, worker) = fixture(vec![
            (200, found(), Duration::ZERO),
            (200, found(), Duration::from_millis(400)),
        ])
        .await;
        let mut page = json!({"items":(0..3).map(album).collect::<Vec<_>>()});
        let started = Instant::now();
        client
            .labels_until(
                &mut page,
                started + Duration::from_millis(200),
                &client.labels_disabled,
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(page["items"][0]["label"], "2026 Example Records");
        assert!(page["items"][1].get("label").is_none());
        assert_eq!(diagnostic(&client).await["asked"], 2);
        assert_eq!(
            diagnostic(&client).await["outcome"],
            json!({"class":"timeout","stage":"request"})
        );
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn no_session_gate_timeout_and_throttle_preserve_cached_labels() {
        let (client, _directory, worker) = crate::fixtures::client(vec![]).await;
        save(&client, vec![(0, now(), Some("Cached copyright"))]).await;
        let page = json!({"items":[album(0),album(1)]});
        let mut missing_session = page.clone();
        client.library_labels(&mut missing_session).await;
        assert_eq!(missing_session["items"][0]["label"], "Cached copyright");
        assert_eq!(diagnostic(&client).await["outcome"]["class"], "no_session");
        assert_eq!(diagnostic(&client).await["asked"], 0);
        let guard = client.labels_gate.lock().await;
        let mut gate = page;
        client
            .labels_until(
                &mut gate,
                Instant::now() + Duration::from_millis(10),
                &client.labels_disabled,
            )
            .await;
        assert_eq!(gate["items"][0]["label"], "Cached copyright");
        assert_eq!(
            diagnostic(&client).await["outcome"],
            json!({"class":"timeout","stage":"gate"})
        );
        drop(guard);
        assert!(worker.join().unwrap().is_empty());
        let (client, _directory, worker) = fixture(vec![]).await;
        client
            .partner_limits
            .lock()
            .await
            .record("/album-label", 60, false);
        client
            .library_labels(&mut json!({"items":[album(0)]}))
            .await;
        assert_eq!(
            diagnostic(&client).await["outcome"],
            json!({"class":"http","status":429})
        );
        assert_eq!(diagnostic(&client).await["asked"], 0);
        assert!(worker.join().unwrap().is_empty());
    }
}
