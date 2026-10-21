use crate::{
    auth::{ApiFailure, Client, WEB_CLIENT, apply_token},
    state::{self, Account, now},
};
use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebSource {
    Primary,
    Shared,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Standard,
    Catalogue(u8),
    Enrichment,
}

impl Client {
    async fn effective_source(&self, source: WebSource) -> WebSource {
        if self.account.lock().await.client_id == WEB_CLIENT {
            WebSource::Primary
        } else {
            source
        }
    }
    pub async fn has_shared_web(&self) -> bool {
        let a = self.account.lock().await;
        !a.refresh_token.is_empty() && (a.client_id == WEB_CLIENT || a.shared.is_some())
    }
    pub async fn primary_is_personal(&self) -> bool {
        let a = self.account.lock().await;
        !a.client_id.is_empty() && a.client_id != WEB_CLIENT
    }
    pub async fn can_edit_playlists(&self) -> bool {
        let a = self.account.lock().await;
        ["playlist-modify-private", "playlist-modify-public"]
            .iter()
            .all(|scope| a.scopes.iter().any(|s| s == scope))
    }
    pub(crate) async fn refresh_for(
        &self,
        source: WebSource,
        rejected: Option<&str>,
    ) -> Result<String> {
        let source = self.effective_source(source).await;
        let _guard = if source == WebSource::Primary {
            self.primary_refresh.lock().await
        } else {
            self.shared_refresh.lock().await
        };
        let snapshot = self.account.lock().await.clone();
        let mut grant = if source == WebSource::Primary {
            snapshot.clone()
        } else {
            let Some(shared) = &snapshot.shared else {
                bail!("Enable shared search in the plugin settings.");
            };
            Account {
                client_id: WEB_CLIENT.into(),
                access_token: shared.access_token.clone(),
                refresh_token: shared.refresh_token.clone(),
                expires: shared.expires,
                scopes: shared.scopes.clone(),
                ..Default::default()
            }
        };
        if grant.refresh_token.is_empty() {
            return Err(ApiFailure {
                status: 401,
                wait: 0,
            }
            .into());
        }
        if grant.expires >= now().saturating_add(90)
            && rejected != Some(grant.access_token.as_str())
        {
            return Ok(grant.access_token);
        }
        let response = self
            .http
            .post(&self.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", grant.refresh_token.as_str()),
                ("client_id", grant.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(|_| ApiFailure {
                status: 503,
                wait: 0,
            })?;
        if matches!(response.status().as_u16(), 400 | 401) {
            if source == WebSource::Primary {
                self.library_reauth.store(true, Ordering::Relaxed);
            }
            return Err(ApiFailure {
                status: 401,
                wait: 0,
            }
            .into());
        }
        if !response.status().is_success() {
            return Err(ApiFailure {
                status: response.status().as_u16(),
                wait: if response.status().as_u16() == 429 {
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(60)
                        .max(1)
                } else {
                    0
                },
            }
            .into());
        }
        apply_token(&mut grant, &Self::checked(response).await?)?;
        let mut current = self.account.lock().await;
        if current.user != snapshot.user
            || current.client_id != snapshot.client_id
            || (source == WebSource::Primary && current.refresh_token != snapshot.refresh_token)
            || (source == WebSource::Shared
                && current.shared.as_ref().map(|g| &g.refresh_token)
                    != snapshot.shared.as_ref().map(|g| &g.refresh_token))
        {
            return Err(ApiFailure {
                status: 401,
                wait: 0,
            }
            .into());
        }
        if source == WebSource::Primary {
            current.access_token = grant.access_token.clone();
            current.refresh_token = grant.refresh_token;
            current.expires = grant.expires;
            current.scopes = grant.scopes;
            self.library_reauth.store(false, Ordering::Relaxed);
        } else {
            current.shared = Some(state::WebGrant::from_account(&grant));
        }
        state::save(&self.path, &current)?;
        Ok(grant.access_token)
    }
    pub async fn api(&self, method: Method, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.api_for(WebSource::Primary, method, path, query, None)
            .await
    }
    pub async fn api_for(
        &self,
        source: WebSource,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        self.request_for(source, method, path, query, body, RequestKind::Standard)
            .await
    }
    pub(crate) async fn enrichment_get(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        self.request_for(
            WebSource::Primary,
            Method::GET,
            path,
            query,
            None,
            RequestKind::Enrichment,
        )
        .await
    }
    async fn request_for(
        &self,
        source: WebSource,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        kind: RequestKind,
    ) -> Result<Value> {
        if !path.starts_with('/') || path.contains("..") || path.contains(['?', '#']) {
            bail!("Invalid Spotify API path.");
        }
        let source = self.effective_source(source).await;
        let _permit = self.api_slots.acquire().await?;
        let limiter = if source == WebSource::Primary {
            &self.limits
        } else {
            &self.shared_limits
        };
        let catalogue = match kind {
            RequestKind::Catalogue(catalogue) => catalogue,
            _ => 0,
        };
        let probe = {
            let mut limits = limiter.lock().await;
            let probe =
                catalogue > 0 && path == "/search" && limits.begin_search_probe(catalogue == 2)?;
            let wait = if catalogue > 0 && path == "/search" {
                limits.remaining_search(catalogue == 2)
            } else {
                limits.remaining(path)
            };
            if wait > 0 && !probe {
                return Err(ApiFailure { status: 429, wait }.into());
            }
            probe
        };
        let result = async {
            let token = self.refresh_for(source, None).await?;
            let request = |token: String| {
                let mut request = self
                    .http
                    .request(method.clone(), format!("{}{path}", self.api_base))
                    .bearer_auth(token)
                    .query(query);
                if let Some(body) = body {
                    request = request.json(body);
                } else if matches!(method, Method::PUT | Method::POST | Method::DELETE) {
                    request = request.header(reqwest::header::CONTENT_LENGTH, "0");
                }
                request.send()
            };
            let mut response = request(token.clone()).await.map_err(|_| ApiFailure {
                status: 503,
                wait: 0,
            })?;
            if response.status().as_u16() == 401 && method == Method::GET {
                drop(response);
                response = request(self.refresh_for(source, Some(&token)).await?)
                    .await
                    .map_err(|_| ApiFailure {
                        status: 503,
                        wait: 0,
                    })?;
            }
            let status = response.status().as_u16();
            if status == 401 && source == WebSource::Primary {
                self.library_reauth.store(true, Ordering::Relaxed);
            }
            if status == 429 {
                let seconds = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60)
                    .max(1);
                let mut bytes = Vec::new();
                while let Ok(Some(chunk)) = response.chunk().await {
                    if bytes.len() + chunk.len() > 65536 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let quota = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .is_some_and(|v| {
                        v.pointer("/error/reason").and_then(Value::as_str) == Some("QUOTA_EXCEEDED")
                    });
                limiter.lock().await.record(
                    path,
                    seconds,
                    quota || kind == RequestKind::Enrichment,
                );
                return Err(ApiFailure {
                    status,
                    wait: seconds,
                }
                .into());
            }
            if !(200..300).contains(&status) {
                return Err(ApiFailure { status, wait: 0 }.into());
            }
            Self::checked(response).await
        }
        .await;
        if probe {
            limiter
                .lock()
                .await
                .finish_search_probe(catalogue == 2, result.is_ok())?;
        }
        result
    }
    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.get_for(WebSource::Primary, path, query).await
    }
    pub async fn get_for(
        &self,
        source: WebSource,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        self.cached_get(source, path, query, 0).await
    }
    pub async fn get_for_catalogue(
        &self,
        source: WebSource,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        if path != "/search"
            || !query.iter().any(|(key, value)| {
                *key == "type"
                    && !value.is_empty()
                    && value
                        .split(',')
                        .all(|kind| matches!(kind, "track" | "album" | "artist"))
            })
        {
            bail!("Invalid catalogue search request.");
        }
        self.cached_get(source, path, query, 1).await
    }
    pub async fn get_for_playlist_search(
        &self,
        source: WebSource,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        if path != "/search"
            || !query
                .iter()
                .any(|(key, value)| *key == "type" && value == "playlist")
        {
            bail!("Invalid playlist search request.");
        }
        self.cached_get(source, path, query, 2).await
    }
    async fn cached_get(
        &self,
        source: WebSource,
        path: &str,
        query: &[(&str, String)],
        catalogue: u8,
    ) -> Result<Value> {
        let source = self.effective_source(source).await;
        if path == "/me" {
            return self.api_for(source, Method::GET, path, query, None).await;
        }
        self.cached(
            &format!(
                "web:{source:?}:{catalogue}:{path}:{}",
                serde_json::to_string(query)?
            ),
            if path.starts_with("/me/") {
                21600
            } else {
                3600
            },
            false,
            || {
                self.request_for(
                    source,
                    Method::GET,
                    path,
                    query,
                    None,
                    RequestKind::Catalogue(catalogue),
                )
            },
        )
        .await
    }
}
pub fn error_data(error: &anyhow::Error) -> Value {
    match error.downcast_ref::<ApiFailure>() {
        Some(e) => {
            json!({"status":e.status,"retry_after_ms":e.wait.saturating_mul(1000),"terminal": !matches!(e.status,429|500..=599)})
        }
        None => json!({"status":500,"retry_after_ms":0,"terminal":false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn favorites_send_an_explicit_empty_length_and_keep_json_writes_intact() {
        use tiny_http::{Response, Server, StatusCode};
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            for kind in ["track", "album", "artist", "playlist"] {
                for method in ["PUT", "DELETE"] {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap()
                        .expect("expected favorite write");
                    let length = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Content-Length"))
                        .map(|h| h.value.as_str());
                    let valid = length == Some("0");
                    assert_eq!(request.method().as_str(), method);
                    let url = url::Url::parse(&format!("http://fixture{}", request.url())).unwrap();
                    assert_eq!(url.path(), "/me/library");
                    assert_eq!(
                        url.query_pairs().find(|(key, _)| key == "uris").unwrap().1,
                        format!("spotify:{kind}:1234567890123456789012")
                    );
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    assert!(body.is_empty());
                    request
                        .respond(Response::empty(StatusCode(if valid { 204 } else { 411 })))
                        .unwrap();
                }
            }
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap()
                .expect("expected JSON write");
            let length = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Length"))
                .unwrap()
                .value
                .as_str()
                .parse::<usize>()
                .unwrap();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            assert_eq!(length, body.len());
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                json!({"name":"Saved"})
            );
            request.respond(Response::from_string("{}")).unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut client = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        client.api_base = address;
        *client.account.lock().await = Account {
            user: "test".into(),
            access_token: "test-only".into(),
            refresh_token: "test-only".into(),
            expires: u64::MAX,
            ..Default::default()
        };
        for kind in ["track", "album", "artist", "playlist"] {
            for on in [true, false] {
                client
                    .favorite(&json!({"ref":format!("{kind}:1234567890123456789012"),"on":on}))
                    .await
                    .unwrap();
            }
        }
        client
            .api_for(
                WebSource::Primary,
                Method::POST,
                "/me/playlists",
                &[],
                Some(&json!({"name":"Saved"})),
            )
            .await
            .unwrap();
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn enrichment_429_only_pauses_its_route_and_survives_restart() {
        for body in ["{}", "invalid", r#"{"error":{"reason":"QUOTA_EXCEEDED"}}"#] {
            let (client, dir, worker) = crate::fixtures::client(vec![
                (429, body.into()),
                (200, "{}".into()),
                (200, "{}".into()),
                (200, "{}".into()),
                (200, "{}".into()),
            ])
            .await;
            let error = client.enrichment_get("/artists", &[]).await.unwrap_err();
            assert_eq!(error_data(&error)["status"], 429);
            assert_eq!(error_data(&error)["retry_after_ms"], 60000);
            let routes = [
                "/me/albums",
                "/me/tracks",
                "/albums/0000000000000000000001",
                "/search",
            ];
            {
                let limits = client.limits.lock().await;
                let restarted =
                    crate::limits::Limits::load(&dir.path().join("data/rate-limit.json"), "");
                assert!((58..=60).contains(&limits.remaining("/artists")));
                assert!((58..=60).contains(&restarted.remaining("/artists")));
                for route in routes {
                    assert_eq!(limits.remaining(route), 0);
                    assert_eq!(restarted.remaining(route), 0);
                }
                assert_eq!(limits.remaining_search(false), 0);
                assert_eq!(limits.remaining_search(true), 0);
            }
            assert!(client.enrichment_get("/artists", &[]).await.is_err());
            for route in routes {
                client.api(Method::GET, route, &[]).await.unwrap();
            }
            assert_eq!(
                worker.join().unwrap(),
                [vec!["/artists"], routes.to_vec()].concat()
            );
        }
    }

    #[tokio::test]
    async fn ordinary_429_still_pauses_the_grant_including_enrichment() {
        let (client, _dir, worker) = crate::fixtures::client(vec![(429, "{}".into())]).await;
        assert!(client.api(Method::GET, "/me/tracks", &[]).await.is_err());
        assert!(client.limits.lock().await.remaining("/me/albums") > 0);
        assert!(client.enrichment_get("/artists", &[]).await.is_err());
        assert_eq!(worker.join().unwrap(), vec!["/me/tracks"]);
    }

    #[tokio::test]
    async fn concurrent_catalogue_recovery_uses_one_persisted_probe() {
        let (client, _dir, worker) = crate::fixtures::client(vec![(200, "{}".into())]).await;
        let path = client.storage_path().join("legacy-test.json");
        state::save_json(&path, &json!({"client_id":"test","until":now()+90000})).unwrap();
        *client.limits.lock().await = crate::limits::Limits::load(&path, "test");
        let query = [("q", "test".into()), ("type", "track,album,artist".into())];
        let (a, b) = tokio::join!(
            client.get_for_catalogue(WebSource::Primary, "/search", &query),
            client.get_for_catalogue(WebSource::Primary, "/search", &query)
        );
        assert!(a.is_ok() || b.is_ok());
        assert!(client.get("/me", &[]).await.is_err());
        assert_eq!(
            worker.join().unwrap(),
            vec!["/search?q=test&type=track%2Calbum%2Cartist"]
        );
    }
    #[test]
    fn rpc_failure_retains_saturating_provider_wait() {
        let error: anyhow::Error = ApiFailure {
            status: 429,
            wait: u64::MAX,
        }
        .into();
        let data = error_data(&error);
        assert_eq!(data["retry_after_ms"], u64::MAX);
        assert_eq!(data["terminal"], false);
    }
}
