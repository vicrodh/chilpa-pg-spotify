use crate::{
    auth::Client,
    enrichment::{Attempt, Outcome, TimeoutStage, record},
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

const CACHE_KEY: &str = "album-label:v2";
const FOUND_TTL: u64 = 90 * 24 * 60 * 60;
const EMPTY_TTL: u64 = 7 * 24 * 60 * 60;
const REQUEST_LIMIT: usize = 20;
const TIME_LIMIT: Duration = Duration::from_secs(6);

#[derive(Deserialize, Serialize)]
struct Label {
    at: u64,
    label: Option<String>,
    source: LabelSource,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LabelSource {
    Metadata,
    Copyright,
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
            if let Some(entry) = self
                .albums
                .get(&id)
                .filter(|entry| entry.fresh(now()))
                .filter(|entry| entry.label.is_some())
            {
                item["label"] = json!(entry.label);
                item["label_source"] = json!(entry.source);
            }
        }
    }
}

fn album_id(item: &Value) -> Option<String> {
    let reference_value = if reference(text(item, "ref"), Some("track")).is_ok() {
        text(item, "album_ref")
    } else {
        text(item, "ref")
    };
    reference(reference_value, Some("album"))
        .ok()
        .map(|(_, id)| id)
}

fn parse(id: &str, bytes: &[u8]) -> Result<Label, Outcome> {
    let album = metadata::Album::parse_from_bytes(bytes).map_err(|_| Outcome::UnexpectedShape)?;
    if SpotifyId::from_raw(album.gid())
        .map(|gid| gid.to_base62())
        .ok()
        .as_deref()
        != Some(id)
    {
        return Err(Outcome::UnexpectedShape);
    }
    let direct = album.label().trim();
    let (label, source) = if !direct.is_empty() {
        (Some(direct.to_owned()), LabelSource::Metadata)
    } else {
        let label = album
            .copyright
            .iter()
            .find(|line| {
                line.type_
                    .is_none_or(|kind| kind.enum_value().ok() == Some(metadata::copyright::Type::P))
            })
            .map(|line| owner(line.text()))
            .filter(|name| !name.is_empty());
        (label, LabelSource::Copyright)
    };
    Ok(Label {
        at: now(),
        label,
        source,
    })
}

/// "℗ 2017 JKP GmbH & Co. KG" as "JKP GmbH & Co. KG".
fn owner(line: &str) -> String {
    let mut rest = line.trim();
    for mark in ["℗", "©", "(P)", "(p)", "(C)", "(c)"] {
        if let Some(after) = rest.strip_prefix(mark) {
            rest = after.trim_start();
        }
    }
    while rest.len() >= 4
        && rest.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && rest
            .as_bytes()
            .get(4)
            .is_none_or(|next| !next.is_ascii_digit())
    {
        rest = rest[4..].trim_start_matches(|c: char| {
            c == '-' || c == '\u{2013}' || c == ',' || c == '/' || c.is_whitespace()
        });
    }
    rest.trim().to_string()
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
        self.labels_until(value, Instant::now() + TIME_LIMIT).await;
    }

    async fn labels_until(&self, value: &mut Value, deadline: Instant) {
        let mut attempt = Attempt::start();
        for item in value["items"].as_array_mut().into_iter().flatten() {
            if matches!(
                item["label_source"].as_str(),
                Some("copyright" | "metadata")
            ) {
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
        } else {
            let selected: Vec<_> = missing.iter().take(REQUEST_LIMIT).collect();
            let uris: Vec<_> = selected
                .iter()
                .map(|id| format!("spotify:album:{id}"))
                .collect();
            match self
                .enrichment_metadata(
                    &identity,
                    ExtensionKind::ALBUM_V4,
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
                            Ok(label) => {
                                attempt.with_data += usize::from(label.label.is_some());
                                saved.albums.insert((*id).clone(), label);
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
    use crate::fixtures::{
        album_metadata, enrichment_diagnostic as diagnostic, id, metadata_client, metadata_reply,
    };

    fn album(index: usize) -> Value {
        json!({"kind":"album","ref":format!("album:{}",id(index))})
    }

    fn reply(indices: impl IntoIterator<Item = usize>) -> Vec<u8> {
        metadata_reply(
            ExtensionKind::ALBUM_V4,
            indices
                .into_iter()
                .map(|i| (i, 200, album_metadata(i, "Example Records", &[])))
                .collect(),
        )
    }

    #[test]
    fn protobuf_label_precedes_the_first_phonographic_owner() {
        let lines = [
            ("2025 Other copyright", false),
            ("℗ 2017 JKP GmbH & Co. KG", true),
            ("2026 Second owner", true),
        ];
        let direct = parse(&id(1), &album_metadata(1, "  Label  ", &lines)).unwrap();
        assert_eq!(direct.label.as_deref(), Some("Label"));
        assert!(matches!(direct.source, LabelSource::Metadata));
        let fallback = parse(&id(1), &album_metadata(1, "  ", &lines)).unwrap();
        assert_eq!(fallback.label.as_deref(), Some("JKP GmbH & Co. KG"));
        assert!(matches!(fallback.source, LabelSource::Copyright));
        for lines in [
            vec![],
            vec![("2026 Copyright only", false)],
            vec![("2017", true), ("2026 Second owner", true)],
        ] {
            assert!(
                parse(&id(1), &album_metadata(1, "", &lines))
                    .unwrap()
                    .label
                    .is_none()
            );
        }
        for (line, expected) in [
            ("(P) 2010-2012 Example", "Example"),
            ("© 2019, 2020 4AD Ltd", "4AD Ltd"),
            ("2017", ""),
        ] {
            assert_eq!(owner(line), expected);
        }
        // Proto2 defaults an omitted copyright type to P.
        let mut album = metadata::Album::parse_from_bytes(&album_metadata(
            1,
            "",
            &[("2026 Default owner", true)],
        ))
        .unwrap();
        album.copyright[0].clear_type_();
        assert_eq!(
            parse(&id(1), &album.write_to_bytes().unwrap())
                .unwrap()
                .label
                .as_deref(),
            Some("Default owner")
        );
        album.copyright[0].type_ = Some(protobuf::EnumOrUnknown::from_i32(99));
        assert!(
            parse(&id(1), &album.write_to_bytes().unwrap())
                .unwrap()
                .label
                .is_none()
        );
        for bytes in [
            vec![],
            b"invalid protobuf".to_vec(),
            album_metadata(2, "Wrong identity", &[]),
        ] {
            assert!(matches!(
                parse(&id(1), &bytes),
                Err(Outcome::UnexpectedShape)
            ));
        }
    }

    #[tokio::test]
    async fn albums_and_tracks_share_the_budget_cache_and_restart_answers() {
        let (client, directory, worker) = metadata_client(vec![
            (200, reply(0..20), Duration::ZERO),
            (200, reply(20..25), Duration::ZERO),
            (200, reply([0]), Duration::ZERO),
        ])
        .await;
        let mut items: Vec<_> = (0..25).map(album).collect();
        items.push(json!({"kind":"track","ref":format!("track:{}",id(90)),"album_ref":format!("album:{}",id(0))}));
        let original = json!({"kind":"album","ref":format!("album:{}",id(99)),"label":"Original"});
        items.push(original.clone());
        let page = json!({"items":items});
        let mut first = page.clone();
        client.library_labels(&mut first).await;
        assert_eq!(first["items"][25]["label"], "Example Records");
        assert_eq!(first["items"][0]["label_source"], "metadata");
        assert!(first["items"][20].get("label").is_none());
        assert_eq!(first["items"][26], original);
        assert_eq!(diagnostic(&client, "album_label").await["asked"], 20);
        client.library_labels(&mut first).await;
        assert_eq!(diagnostic(&client, "album_label").await["asked"], 5);
        assert_eq!(diagnostic(&client, "album_label").await["cached"], 20);
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        *restarted.account.lock().await = client.account.lock().await.clone();
        restarted.metadata_fixture = client.metadata_fixture.clone();
        let mut cached = page;
        restarted.library_labels(&mut cached).await;
        assert_eq!(cached, first);
        assert_eq!(
            diagnostic(&restarted, "album_label").await["outcome"]["class"],
            "cache_hit"
        );
        restarted.account.lock().await.user = "other".into();
        restarted
            .library_labels(&mut json!({"items":[album(0)]}))
            .await;
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].entity_request[0].entity_uri,
            format!("spotify:album:{}", id(20))
        );
    }

    #[tokio::test]
    async fn old_empty_answers_are_replaced_and_new_answers_expire() {
        assert_eq!(REQUEST_LIMIT, 20);
        assert_eq!(TIME_LIMIT, Duration::from_secs(6));
        assert_eq!(FOUND_TTL, 7_776_000);
        assert_eq!(EMPTY_TTL, 604_800);
        for (label, ttl) in [(Some("Label".into()), FOUND_TTL), (None, EMPTY_TTL)] {
            let entry = Label {
                at: 100,
                label,
                source: LabelSource::Metadata,
            };
            assert!(entry.fresh(100 + ttl - 1));
            assert!(!entry.fresh(100 + ttl));
            assert!(!entry.fresh(99));
        }
        let empty = metadata_reply(
            ExtensionKind::ALBUM_V4,
            vec![(1, 200, album_metadata(1, "", &[]))],
        );
        let fallback = metadata_reply(
            ExtensionKind::ALBUM_V4,
            vec![(1, 200, album_metadata(1, "", &[("(P) 2026 Owner", true)]))],
        );
        let (client, _directory, worker) = metadata_client(vec![
            (200, empty, Duration::ZERO),
            (200, fallback, Duration::ZERO),
            (200, reply([1]), Duration::ZERO),
        ])
        .await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                "album-label:v1",
                json!({"albums":{id(1):{"at":now(),"label":null}}}),
            )
            .unwrap();
        let mut page = json!({"items":[album(1)]});
        client.library_labels(&mut page).await;
        assert_eq!(diagnostic(&client, "album_label").await["asked"], 1);
        client.library_labels(&mut page).await;
        assert_eq!(
            diagnostic(&client, "album_label").await["outcome"]["class"],
            "cache_hit"
        );
        for ttl in [EMPTY_TTL, FOUND_TTL] {
            let mut cache = client.snapshots.peek(&identity, CACHE_KEY).unwrap();
            cache["albums"][id(1)]["at"] = json!(now() - ttl);
            client.snapshots.put(&identity, CACHE_KEY, cache).unwrap();
            client.library_labels(&mut page).await;
            assert_eq!(
                page["items"][0]["label"],
                if ttl == EMPTY_TTL {
                    "Owner"
                } else {
                    "Example Records"
                }
            );
        }
        assert_eq!(worker.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn library_tracks_are_enriched_from_their_album() {
        let (client, _directory, worker) =
            metadata_client(vec![(200, reply([2]), Duration::ZERO)]).await;
        let identity = client.identity().await.unwrap();
        client.snapshots.put(&identity, "library:tracks:0:50:fields-v2", json!({"items":[{"ref":format!("track:{}",id(1)),"album_ref":format!("album:{}",id(2)),"genres":["ambient"]}]})).unwrap();
        let page = client.library(&json!({"kind":"tracks"})).await.unwrap();
        assert_eq!(page["items"][0]["label"], "Example Records");
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_session_gate_and_request_deadlines_keep_fresh_cached_labels() {
        let (client, _directory, worker) = crate::fixtures::client(vec![]).await;
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                CACHE_KEY,
                json!({"albums":{id(0):{"at":now(),"label":"Cached","source":"metadata"}}}),
            )
            .unwrap();
        let mut page = json!({"items":[album(0),album(1)]});
        client.library_labels(&mut page).await;
        assert_eq!(page["items"][0]["label"], "Cached");
        assert_eq!(
            diagnostic(&client, "album_label").await["outcome"]["class"],
            "no_session"
        );
        let _guard = client.labels_gate.lock().await;
        client
            .labels_until(&mut page, Instant::now() + Duration::from_millis(10))
            .await;
        assert_eq!(
            diagnostic(&client, "album_label").await["outcome"],
            json!({"class":"timeout","stage":"gate"})
        );
        assert!(worker.join().unwrap().is_empty());
        let (client, _directory, worker) =
            metadata_client(vec![(200, reply([1]), Duration::from_millis(200))]).await;
        let mut page = json!({"items":[album(1)]});
        let started = Instant::now();
        client
            .labels_until(&mut page, started + Duration::from_millis(50))
            .await;
        assert!(started.elapsed() < Duration::from_millis(180));
        assert_eq!(
            diagnostic(&client, "album_label").await["outcome"],
            json!({"class":"timeout","stage":"request"})
        );
        assert!(page["items"][0].get("label").is_none());
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
