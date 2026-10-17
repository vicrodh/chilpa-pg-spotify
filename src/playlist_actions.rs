use crate::{
    auth::Client,
    catalog::card,
    state::{self, reference, text},
    web_api::WebSource,
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

impl Client {
    pub(crate) async fn playlist_editable(&self, metadata: &Value) -> bool {
        if !self.can_edit_playlists().await {
            return false;
        }
        let account = self.account.lock().await;
        writable(metadata, &account.user)
    }

    pub async fn page_action(&self, params: &Value) -> Result<Value> {
        let action = text(params, "action");
        if !["playlist.create", "playlist.add"].contains(&action) {
            return self.favorite(params).await;
        }
        ensure!(
            self.can_edit_playlists().await,
            "Enable playlist writing in the plugin settings to grant playlist permissions."
        );
        let (path, body, target) = if action == "playlist.create" {
            ("/me/playlists".to_owned(), create_body(params)?, None)
        } else {
            let (_, id) = reference(text(params, "ref"), Some("playlist"))?;
            let body = add_body(params)?;
            (format!("/playlists/{id}/items"), body, Some(id))
        };
        let identity = self.identity().await?;
        let request = text(params, "request_id");
        ensure!(
            request.len() <= 256,
            "The playlist request identifier is too long."
        );
        let ledger_path = self.path.with_file_name("playlist-requests.json");
        let mut ledger = if request.is_empty() {
            json!({})
        } else {
            match std::fs::read(&ledger_path) {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                    .context("The playlist request record is invalid.")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
                Err(_) => bail!("Cannot read the playlist request record."),
            }
        };
        let key = format!("{:x}", Sha256::digest(format!("{identity}\0{request}")));
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(params)?));
        if !request.is_empty() {
            if let Some(previous) = ledger.get(&key) {
                ensure!(
                    previous["fingerprint"] == fingerprint,
                    "This request identifier was already used for another operation."
                );
                if let Some(result) = previous.get("result") {
                    // The host sends initial tracks separately without request IDs.
                    // I omit the create ref on replay so it cannot append them twice.
                    if action == "playlist.create" {
                        return Ok(
                            json!({"changed_refs":["section:playlists"],"warning":"This playlist request already completed. Open your library before adding more tracks."}),
                        );
                    }
                    return Ok(result.clone());
                }
                bail!(
                    "The previous playlist request may have completed. Open the playlist before trying a new request."
                );
            }
        }
        if let Some(id) = target.as_ref() {
            let metadata = self
                .get_for(WebSource::Primary, &format!("/playlists/{id}"), &[])
                .await?;
            ensure!(
                self.playlist_editable(&metadata).await,
                "This account does not have permission to add tracks to this playlist."
            );
        }
        if !request.is_empty() {
            ensure!(
                ledger.as_object().is_some_and(|v| v.len() < 10000),
                "The playlist request record is full."
            );
            ledger[&key] = json!({"fingerprint":fingerprint});
            state::save_json(&ledger_path, &ledger)?;
        }
        // I record intent before sending and never replay a write after an uncertain reply.
        let response = self
            .api_for(WebSource::Primary, Method::POST, &path, &[], Some(&body))
            .await?;
        for prefix in [
            "library:",
            "library-head-probe:",
            "web:",
            "session:rootlist",
            "discover:",
        ] {
            self.snapshots.invalidate(&identity, prefix);
        }
        let changed_id = target.as_deref().or_else(|| response["id"].as_str());
        if let Some(id) = changed_id {
            for prefix in [
                format!("session:playlist:{id}:"),
                format!("session:v2:playlist:{id}:"),
                format!("session:v2:entity:playlist:{id}"),
                format!("session:playlist-header:{id}"),
            ] {
                self.snapshots.invalidate(&identity, &prefix);
            }
        }
        let result = if action == "playlist.create" {
            let mut item = card(&response, "playlist").context("The playlist may have been created, but its reply was incomplete. Check your library before retrying.")?;
            item["editable"] = json!(self.playlist_editable(&response).await);
            json!({"item":item,"changed_refs":["section:playlists"]})
        } else {
            ensure!(
                response["snapshot_id"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty()),
                "Tracks may have been added, but the confirmation was incomplete. Open the playlist before retrying."
            );
            json!({"changed_refs":[params["ref"],"section:playlists"],"snapshot_id":response["snapshot_id"]})
        };
        if !request.is_empty() {
            ledger[&key]["result"] = result.clone();
            state::save_json(&ledger_path, &ledger)?;
        }
        Ok(result)
    }
}

fn writable(metadata: &Value, user: &str) -> bool {
    !user.is_empty() && metadata["owner"]["id"].as_str() == Some(user)
}
fn create_body(params: &Value) -> Result<Value> {
    let title = text(params, "title").trim();
    let description = text(params, "description");
    ensure!(
        !title.is_empty() && title.chars().count() <= 100,
        "Use a playlist name between 1 and 100 characters."
    );
    ensure!(
        description.chars().count() <= 300,
        "Use a playlist description of at most 300 characters."
    );
    let public = match params.get("public") {
        None => false,
        Some(v) => v
            .as_bool()
            .context("Playlist visibility must be true or false.")?,
    };
    Ok(json!({"name":title,"description":description,"public":public}))
}
fn add_body(params: &Value) -> Result<Value> {
    let tracks = params["tracks"]
        .as_array()
        .context("Choose tracks to add.")?;
    ensure!(
        !tracks.is_empty() && tracks.len() <= 100,
        "Add between 1 and 100 tracks at a time."
    );
    let uris: Result<Vec<_>> = tracks
        .iter()
        .map(|track| {
            let (_, id) = reference(
                track.as_str().context("Invalid track reference.")?,
                Some("track"),
            )?;
            Ok(format!("spotify:track:{id}"))
        })
        .collect();
    Ok(json!({"uris":uris?}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bodies_preserve_order_duplicates_and_validate_limits() {
        let a = "track:1234567890123456789012";
        let b = "track:abcdefghijklmnopqrstuv@entry:2";
        assert_eq!(
            add_body(&json!({"tracks":[a,b,a]})).unwrap()["uris"],
            json!([
                "spotify:track:1234567890123456789012",
                "spotify:track:abcdefghijklmnopqrstuv",
                "spotify:track:1234567890123456789012"
            ])
        );
        assert!(add_body(&json!({"tracks":[]})).is_err());
        assert!(add_body(&json!({"tracks":["playlist:1234567890123456789012"]})).is_err());
        assert!(add_body(&json!({"tracks":vec![a;101]})).is_err());
        assert_eq!(
            create_body(&json!({"title":" Mix "})).unwrap(),
            json!({"name":"Mix","description":"","public":false})
        );
    }
    #[test]
    fn caller_editable_and_collaborative_flags_are_not_permissions() {
        assert!(!writable(
            &json!({"editable":true,"collaborative":true,"owner":{"id":"other"}}),
            "test"
        ));
        assert!(!writable(&json!({"owner":{"id":""}}), ""));
        assert!(writable(&json!({"owner":{"id":"test"}}), "test"));
    }
    #[tokio::test]
    async fn missing_permissions_and_foreign_owner_never_write() {
        let (client, _directory, worker) = crate::fixtures::client(vec![]).await;
        assert!(
            client
                .page_action(&json!({"action":"playlist.create","title":"Mix"}))
                .await
                .is_err()
        );
        assert!(worker.join().unwrap().is_empty());
        let (client, _directory, worker) = crate::fixtures::client(vec![(
            200,
            json!({"owner":{"id":"other"},"editable":true}).to_string(),
        )])
        .await;
        client.account.lock().await.scopes = vec![
            "playlist-modify-private".into(),
            "playlist-modify-public".into(),
        ];
        assert!(client.page_action(&json!({"action":"playlist.add","ref":"playlist:1234567890123456789012","tracks":["track:1234567890123456789012"]})).await.is_err());
        assert_eq!(
            worker.join().unwrap(),
            ["/playlists/1234567890123456789012"]
        );
    }
    #[tokio::test]
    async fn an_ambiguous_create_is_never_replayed_with_the_same_request_id() {
        let (client, _directory, worker) = crate::fixtures::client(vec![(500, "{}".into())]).await;
        client.account.lock().await.scopes = vec![
            "playlist-modify-private".into(),
            "playlist-modify-public".into(),
        ];
        let request = json!({"action":"playlist.create","title":"Mix","request_id":"request-1"});
        assert!(client.page_action(&request).await.is_err());
        assert!(
            client
                .page_action(&request)
                .await
                .unwrap_err()
                .to_string()
                .contains("may have completed")
        );
        assert_eq!(worker.join().unwrap(), ["/me/playlists"]);
    }
    #[tokio::test]
    async fn confirmed_create_replay_cannot_append_initial_tracks_again() {
        let reply = json!({"id":"1234567890123456789012","name":"Mix","owner":{"id":"test"}});
        let (client, _directory, worker) =
            crate::fixtures::client(vec![(201, reply.to_string())]).await;
        client.account.lock().await.scopes = vec![
            "playlist-modify-private".into(),
            "playlist-modify-public".into(),
        ];
        let request = json!({"action":"playlist.create","title":"Mix","request_id":"request-1"});
        let result = client.page_action(&request).await.unwrap();
        assert_eq!(result["item"]["ref"], "playlist:1234567890123456789012");
        let replay = client.page_action(&request).await.unwrap();
        assert!(replay["item"]["ref"].is_null());
        assert!(replay["ref"].is_null());
        assert!(replay["warning"].is_string());
        assert_eq!(worker.join().unwrap(), ["/me/playlists"]);
    }
    #[tokio::test]
    async fn writes_send_json_order_and_invalidate_current_snapshot_namespaces() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            let mut captured = vec![];
            for reply in [
                json!({"id":"1234567890123456789012","name":"Mix","owner":{"id":"test"}}),
                json!({"owner":{"id":"test"}}),
                json!({"snapshot_id":"next"}),
            ] {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                captured.push((
                    request.method().as_str().to_owned(),
                    request.url().to_owned(),
                    request
                        .headers()
                        .iter()
                        .map(|h| (h.field.to_string(), h.value.as_str().to_owned()))
                        .collect::<Vec<_>>(),
                    body,
                ));
                request
                    .respond(tiny_http::Response::from_string(reply.to_string()))
                    .unwrap();
            }
            captured
        });
        let directory = tempfile::tempdir().unwrap();
        let mut client = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        client.api_base = address;
        *client.account.lock().await = crate::state::Account {
            user: "test".into(),
            client_id: "personal".into(),
            access_token: "fixture-token".into(),
            refresh_token: "fixture-token".into(),
            expires: u64::MAX,
            scopes: vec![
                "playlist-modify-private".into(),
                "playlist-modify-public".into(),
            ],
            ..Default::default()
        };
        client.page_action(&json!({"action":"playlist.create","title":"Mix","description":"Description","public":false})).await.unwrap();
        let identity = client.identity().await.unwrap();
        let keys = [
            "web:Primary:0:/me/playlists:[]",
            "web:Shared:0:/playlists/1234567890123456789012:[]",
            "library:playlists:0",
            "library-head-probe:playlists",
            "session:rootlist",
            "session:playlist-header:1234567890123456789012",
            "session:v2:playlist:1234567890123456789012:0",
            "session:v2:entity:playlist:1234567890123456789012",
            "discover:home",
        ];
        for key in keys {
            client
                .snapshots
                .put(&identity, key, json!({"old":true}))
                .unwrap();
        }
        let request = json!({"action":"playlist.add","ref":"playlist:1234567890123456789012","request_id":"addition","tracks":["track:1234567890123456789012","track:0VjIjW4GlUZAMYd2vXMi3b","track:1234567890123456789012"]});
        let result = client.page_action(&request).await.unwrap();
        for key in keys {
            assert!(client.snapshots.peek(&identity, key).is_none(), "{key}");
        }
        // A confirmed add replay needs neither a metadata read nor another write.
        assert_eq!(client.page_action(&request).await.unwrap(), result);
        let captured = worker.join().unwrap();
        assert_eq!(
            captured.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["POST", "GET", "POST"]
        );
        assert_eq!(captured[0].1, "/me/playlists");
        assert_eq!(
            serde_json::from_str::<Value>(&captured[0].3).unwrap(),
            json!({"name":"Mix","description":"Description","public":false})
        );
        assert_eq!(captured[2].1, "/playlists/1234567890123456789012/items");
        assert_eq!(
            serde_json::from_str::<Value>(&captured[2].3).unwrap()["uris"],
            json!([
                "spotify:track:1234567890123456789012",
                "spotify:track:0VjIjW4GlUZAMYd2vXMi3b",
                "spotify:track:1234567890123456789012"
            ])
        );
        for row in [&captured[0], &captured[2]] {
            assert!(
                row.2
                    .iter()
                    .any(|(key, value)| key.eq_ignore_ascii_case("content-type")
                        && value == "application/json")
            );
            assert!(
                row.2
                    .iter()
                    .any(|(key, value)| key.eq_ignore_ascii_case("authorization")
                        && value == "Bearer fixture-token")
            );
        }
    }
}
