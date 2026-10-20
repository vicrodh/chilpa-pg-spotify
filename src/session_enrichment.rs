use crate::{
    auth::Client,
    enrichment::{Attempt, Outcome, TimeoutStage},
};
use librespot_core::{Error, http_client::HttpClientError};
use librespot_protocol::{
    extended_metadata::{
        BatchedEntityRequest, BatchedExtensionResponse, EntityRequest, ExtensionQuery,
    },
    extension_kind::ExtensionKind,
};
use protobuf::EnumOrUnknown;
#[cfg(test)]
use protobuf::Message;
use std::collections::HashMap;
use tokio::time::{Instant, timeout_at};

pub(crate) const ROUTE: &str = "session_metadata";
#[cfg(test)]
pub(crate) const ENDPOINT: &str = "/extended-metadata/v0/extended-metadata";
pub(crate) type Answers = HashMap<String, Result<Vec<u8>, Outcome>>;

fn session_error(error: Error) -> Outcome {
    if let Some(HttpClientError::StatusCode(status)) = error.error.downcast_ref::<HttpClientError>()
    {
        Outcome::Http {
            status: status.as_u16(),
        }
    } else if error.error.downcast_ref::<protobuf::Error>().is_some() {
        Outcome::UnexpectedShape
    } else {
        Outcome::Transport
    }
}

fn answers(
    uris: &[String],
    kind: ExtensionKind,
    response: BatchedExtensionResponse,
) -> Result<Answers, Outcome> {
    let mut found = HashMap::new();
    for array in response.extended_metadata {
        if array.extension_kind.enum_value().ok() != Some(kind) {
            continue;
        }
        let status = array.header.provider_error_status;
        if !matches!(status, 0 | 200) {
            return Err(status_outcome(status));
        }
        for data in array.extension_data {
            if !uris.contains(&data.entity_uri) || found.contains_key(&data.entity_uri) {
                return Err(Outcome::UnexpectedShape);
            }
            let row = match data.header.status_code {
                0 | 200 => data
                    .extension_data
                    .into_option()
                    .map(|data| data.value)
                    .ok_or(Outcome::UnexpectedShape),
                status => Err(status_outcome(status)),
            };
            found.insert(data.entity_uri, row);
        }
    }
    for uri in uris {
        found
            .entry(uri.clone())
            .or_insert(Err(Outcome::UnexpectedShape));
    }
    Ok(found)
}

fn status_outcome(status: i32) -> Outcome {
    match status {
        100..=599 => Outcome::Http {
            status: status as u16,
        },
        _ => Outcome::UnexpectedShape,
    }
}

impl Client {
    pub(crate) async fn enrichment_metadata(
        &self,
        identity: &str,
        kind: ExtensionKind,
        uris: &[String],
        deadline: Instant,
        attempt: &mut Attempt,
    ) -> Result<Answers, Outcome> {
        // Library enrichment fits within the catalog's 50-entity batches.
        if uris.is_empty() || uris.len() > 20 {
            return Err(Outcome::UnexpectedShape);
        }
        let mut stage = TimeoutStage::Gate;
        let work = async {
            let _guard = self.metadata_gate.lock().await;
            if let Some(outcome) = self.enrichment_pause(identity, ROUTE) {
                return Err(outcome);
            }
            let request = BatchedEntityRequest {
                entity_request: uris
                    .iter()
                    .map(|uri| EntityRequest {
                        entity_uri: uri.clone(),
                        query: vec![ExtensionQuery {
                            extension_kind: EnumOrUnknown::new(kind),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            #[cfg(test)]
            if let Some(base) = &self.metadata_fixture {
                stage = TimeoutStage::Request;
                attempt.asked += uris.len();
                let response = self
                    .http
                    .post(format!("{base}{ENDPOINT}"))
                    .header("content-type", "application/x-protobuf")
                    .body(
                        request
                            .write_to_bytes()
                            .map_err(|_| Outcome::UnexpectedShape)?,
                    )
                    .send()
                    .await
                    .map_err(|_| Outcome::Transport)?;
                let status = response.status().as_u16();
                if !(200..300).contains(&status) {
                    let outcome = Outcome::Http { status };
                    self.pause_enrichment(identity, ROUTE, outcome);
                    return Err(outcome);
                }
                let bytes = response.bytes().await.map_err(|_| Outcome::Transport)?;
                let response = BatchedExtensionResponse::parse_from_bytes(&bytes)
                    .map_err(|_| Outcome::UnexpectedShape)?;
                return self.enrichment_answers(identity, uris, kind, response);
            }
            stage = TimeoutStage::Token;
            if !self.has_session().await {
                return Err(Outcome::NoSession);
            }
            let session = self.session().await.map_err(|_| Outcome::NoToken)?;
            stage = TimeoutStage::Request;
            attempt.asked += uris.len();
            match session.spclient().get_extended_metadata(request).await {
                Ok(response) => self.enrichment_answers(identity, uris, kind, response),
                Err(error) => {
                    let outcome = session_error(error);
                    self.pause_enrichment(identity, ROUTE, outcome);
                    Err(outcome)
                }
            }
        };
        timeout_at(deadline, work)
            .await
            .unwrap_or_else(|_| Err(Outcome::Timeout { stage }))
    }

    fn enrichment_answers(
        &self,
        identity: &str,
        uris: &[String],
        kind: ExtensionKind,
        response: BatchedExtensionResponse,
    ) -> Result<Answers, Outcome> {
        let result = answers(uris, kind, response);
        match &result {
            Ok(rows) => {
                // A refusal applies to this shared endpoint, including other extension kinds.
                for status in [429, 403] {
                    if rows
                        .values()
                        .any(|row| row.as_ref().err() == Some(&Outcome::Http { status }))
                    {
                        self.pause_enrichment(identity, ROUTE, Outcome::Http { status });
                    }
                }
            }
            Err(outcome) => self.pause_enrichment(identity, ROUTE, *outcome),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixtures::{
            album_metadata, enrichment_diagnostic as diagnostic, id, metadata_client,
            metadata_reply,
        },
        state::now,
    };
    use librespot_protocol::extended_metadata::EntityExtensionDataArrayHeader;
    use protobuf::MessageField;
    use serde_json::{Value, json};
    use std::time::Duration;

    fn page() -> Value {
        json!({"items":(0..25).map(|i|json!({"ref":format!("album:{}",id(i))})).collect::<Vec<_>>()})
    }

    #[test]
    fn native_errors_keep_only_the_numeric_http_status() {
        for status in [401, 403, 404, 429, 503] {
            let error =
                HttpClientError::StatusCode(reqwest::StatusCode::from_u16(status).unwrap()).into();
            assert_eq!(session_error(error), Outcome::Http { status });
        }
        assert_eq!(
            session_error(Error::unavailable("private-token")),
            Outcome::Transport
        );
        let error = BatchedExtensionResponse::parse_from_bytes(b"bad protobuf").unwrap_err();
        assert_eq!(session_error(error.into()), Outcome::UnexpectedShape);
    }

    #[tokio::test]
    async fn forbidden_transport_provider_and_rows_disable_labels_for_an_hour() {
        let kind = ExtensionKind::ALBUM_V4;
        for level in ["http", "provider", "row"] {
            let rows = metadata_reply(kind, (0..20).map(|i| (i, 403, vec![])).collect());
            let (status, body) = match level {
                "http" => (403, b"private-response".to_vec()),
                "provider" => {
                    let mut response = BatchedExtensionResponse::parse_from_bytes(&rows).unwrap();
                    response.extended_metadata[0].header =
                        MessageField::some(EntityExtensionDataArrayHeader {
                            provider_error_status: 403,
                            ..Default::default()
                        });
                    (200, response.write_to_bytes().unwrap())
                }
                _ => (200, rows),
            };
            let success = metadata_reply(
                kind,
                (0..20)
                    .map(|i| (i, 200, album_metadata(i, "Label", &[])))
                    .collect(),
            );
            let (client, directory, worker) = metadata_client(vec![
                (status, body, Duration::ZERO),
                (200, success.clone(), Duration::ZERO),
                (200, success, Duration::ZERO),
            ])
            .await;
            client.library_labels(&mut page()).await;
            let entry = diagnostic(&client, "album_label").await;
            assert_eq!(entry["outcome"], json!({"class":"http","status":403}));
            assert_eq!(entry["asked"], 20);
            let identity = client.identity().await.unwrap();
            assert!(
                client.snapshots.peek(&identity, "album-label:v2").unwrap()["albums"]
                    .as_object()
                    .unwrap()
                    .is_empty()
            );
            let pause_key = format!("capability:enrichment:{ROUTE}");
            let until = client.snapshots.peek(&identity, &pause_key).unwrap()["until"]
                .as_u64()
                .unwrap();
            assert!((3598..=3600).contains(&until.saturating_sub(now())));
            let mut restarted = Client::new(
                directory.path().join("data"),
                directory.path().join("cache"),
            )
            .unwrap();
            *restarted.account.lock().await = client.account.lock().await.clone();
            restarted.metadata_fixture = client.metadata_fixture.clone();
            restarted.library_labels(&mut page()).await;
            let paused = diagnostic(&restarted, "album_label").await;
            assert_eq!(paused["asked"], 0);
            assert_eq!(paused["outcome"]["class"], "disabled");
            assert!((3598..=3600).contains(&paused["outcome"]["remaining"].as_u64().unwrap()));
            restarted.account.lock().await.user = "other-account".into();
            restarted.library_labels(&mut page()).await;
            assert_eq!(diagnostic(&restarted, "album_label").await["with_data"], 20);
            restarted.account.lock().await.user = "test".into();
            restarted.enrichment_pauses.lock().unwrap().clear();
            restarted
                .snapshots
                .put(&identity, &pause_key, json!({"until":now(),"status":403}))
                .unwrap();
            restarted
                .snapshots
                .put(&identity, "album-label:v2", json!({"albums":{}}))
                .unwrap();
            restarted.library_labels(&mut page()).await;
            assert_eq!(diagnostic(&restarted, "album_label").await["with_data"], 20);
            assert_eq!(worker.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn paused_routes_still_project_cached_data_and_survive_write_failure() {
        let (client, directory, worker) =
            metadata_client(vec![(403, vec![], Duration::ZERO)]).await;
        let path = directory.path().join("cache/snapshots");
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "blocked writes").unwrap();
        client.library_labels(&mut page()).await;
        client.library_labels(&mut page()).await;
        assert!(matches!(
            client.enrichment_pause(&client.identity().await.unwrap(), ROUTE),
            Some(Outcome::Disabled {
                remaining: 1..=3600,
                ..
            })
        ));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let identity = client.identity().await.unwrap();
        client
            .snapshots
            .put(
                &identity,
                "album-label:v2",
                json!({"albums":{id(0):{"at":now(),"label":"Cached","source":"metadata"}}}),
            )
            .unwrap();
        let mut value = page();
        client.library_labels(&mut value).await;
        assert_eq!(value["items"][0]["label"], "Cached");
        assert_eq!(diagnostic(&client, "album_label").await["cached"], 1);
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limits_pause_and_malformed_responses_remain_uncached() {
        for status in [200, 429] {
            let (client, _directory, worker) =
                metadata_client(vec![(status, b"invalid protobuf".to_vec(), Duration::ZERO)]).await;
            client.library_labels(&mut page()).await;
            let outcome = diagnostic(&client, "album_label").await["outcome"].clone();
            if status == 429 {
                assert_eq!(outcome, json!({"class":"http","status":429}));
                client.library_labels(&mut page()).await;
                assert_eq!(diagnostic(&client, "album_label").await["asked"], 0);
                assert_eq!(client.limits.lock().await.remaining("/me/albums"), 0);
            } else {
                assert_eq!(outcome, json!({"class":"unexpected_shape"}));
            }
            assert_eq!(worker.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn disabled_credits_do_not_interfere_with_label_requests() {
        let (client, _directory, worker) =
            metadata_client(vec![(403, vec![], Duration::from_millis(30))]).await;
        let mut albums = page();
        let mut tracks = json!({"items":[{"ref":format!("track:{}",id(0))}]});
        tokio::join!(
            client.library_labels(&mut albums),
            client.library_composers("tracks", &mut tracks)
        );
        let label = diagnostic(&client, "album_label").await;
        let credit = diagnostic(&client, "track_credits").await;
        assert_eq!(label["outcome"], json!({"class":"http","status":403}));
        assert_eq!(label["asked"], 20);
        assert_eq!(credit["outcome"]["class"], "disabled");
        assert_eq!(credit["outcome"]["reason"], "unsupported_credits_schema");
        assert_eq!(credit["asked"], 0);
        assert_eq!(worker.join().unwrap().len(), 1);
    }

    #[test]
    fn envelope_validation_rejects_missing_duplicate_wrong_kind_and_wrong_uri_rows() {
        let kind = ExtensionKind::ALBUM_V4;
        let uris = [format!("spotify:album:{}", id(1))];
        let response = metadata_reply(kind, vec![(1, 200, album_metadata(1, "Label", &[]))]);
        let response = BatchedExtensionResponse::parse_from_bytes(&response).unwrap();
        assert!(answers(&uris, kind, response.clone()).unwrap()[&uris[0]].is_ok());
        let mut wrong = response.clone();
        wrong.extended_metadata[0].extension_kind = EnumOrUnknown::new(ExtensionKind::TRACK_V4);
        assert_eq!(
            answers(&uris, kind, wrong).unwrap()[&uris[0]],
            Err(Outcome::UnexpectedShape)
        );
        let mut duplicate = response.clone();
        let row = duplicate.extended_metadata[0].extension_data[0].clone();
        duplicate.extended_metadata[0].extension_data.push(row);
        assert_eq!(
            answers(&uris, kind, duplicate),
            Err(Outcome::UnexpectedShape)
        );
        let mut wrong = response.clone();
        wrong.extended_metadata[0].extension_data[0].entity_uri = "spotify:track:wrong".into();
        assert_eq!(answers(&uris, kind, wrong), Err(Outcome::UnexpectedShape));
        let mut missing = response;
        missing.extended_metadata[0].extension_data[0]
            .extension_data
            .clear();
        assert_eq!(
            answers(&uris, kind, missing).unwrap()[&uris[0]],
            Err(Outcome::UnexpectedShape)
        );
    }
}
