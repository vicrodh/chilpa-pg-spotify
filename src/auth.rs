// I adapt Spotifast's separate PKCE grants; see licenses/Spotifast-MIT.txt.
use crate::{
    callback,
    state::{self, Account, now, text},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use librespot_core::{Session, authentication::Credentials, config::SessionConfig};
use librespot_protocol::authentication::AuthenticationType;
use rand::RngCore;
use reqwest::{Client as Http, Method};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};
use url::Url;

pub const WEB_CLIENT: &str = "d420a117a32841c2b3474932e49fb54b";
const PLAYBACK_CLIENT: &str = "65b708073fc0480ea92a077233ca87bd";
const WEB_SCOPES: &str = "playlist-modify-private playlist-modify-public user-read-private user-library-read user-library-modify user-follow-read playlist-read-private playlist-read-collaborative user-top-read user-read-recently-played";

pub fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

struct Flow {
    verifier: String,
    state: String,
    client: String,
    redirect: String,
    url: String,
    playback: bool,
    shared: bool,
    expires: u64,
    capture: Option<callback::Capture>,
    returned_address: String,
    callback_error: Option<String>,
}

impl Flow {
    fn new(client: String, playback: bool) -> Self {
        let verifier = random();
        let state = random();
        let redirect = format!(
            "http://127.0.0.1:{}/login",
            if playback { 8898 } else { 8989 }
        );
        let mut url = Url::parse("https://accounts.spotify.com/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", client.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                URL_SAFE_NO_PAD
                    .encode(Sha256::digest(verifier.as_bytes()))
                    .as_str(),
            ),
            ("state", state.as_str()),
            ("scope", if playback { "streaming" } else { WEB_SCOPES }),
        ]);
        if playback {
            url.query_pairs_mut().append_pair("show_dialog", "true");
        }
        Self {
            verifier,
            state,
            client,
            redirect,
            url: url.into(),
            playback,
            shared: false,
            expires: now() + 600,
            capture: None,
            returned_address: String::new(),
            callback_error: None,
        }
    }

    fn code(&self, address: &str) -> Result<String> {
        callback::code(&self.redirect, &self.state, self.expires, address)
    }
}

pub struct Client {
    pub http: Http,
    pub account: Mutex<Account>,
    pending: Mutex<Option<Flow>>,
    session: Mutex<Option<Session>>,
    pub(crate) path: PathBuf,
    cache: PathBuf,
    revision: Mutex<u64>,
    pub(crate) api_base: String,
    pub(crate) token_endpoint: String,
    pub(crate) api_slots: Semaphore,
    pub(crate) limits: Mutex<crate::limits::Limits>,
    pub(crate) partner_limits: Mutex<crate::limits::Limits>,
    rate_path: PathBuf,
    pub(crate) shared_limits: Mutex<crate::limits::Limits>,
    pub(crate) primary_refresh: Mutex<()>,
    pub(crate) shared_refresh: Mutex<()>,
    advanced: Mutex<bool>,
    configure: Mutex<bool>,
    search_notice: Mutex<Option<String>>,
    pub(crate) snapshots: crate::snapshot::Store,
    pub(crate) library_reauth: AtomicBool,
}

impl Client {
    pub fn new(data: PathBuf, cache: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&cache)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = data.join("account.json");
        let account = state::load(&path)?;
        let rate_path = data.join("rate-limit.json");
        let limits =
            crate::limits::Limits::load_account(&rate_path, &account.client_id, &account.user);
        let partner_limits =
            crate::limits::Limits::load(&data.join("session-home-limit.json"), &account.user);
        let snapshots = crate::snapshot::Store::new(cache.join("snapshots"))?;
        let shared_identity = format!("{}:{}", WEB_CLIENT, account.user);
        Ok(Self {
            http: Http::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("Chilpa-Spotify/", env!("CARGO_PKG_VERSION")))
                .build()?,
            account: Mutex::new(account),
            pending: Mutex::new(None),
            session: Mutex::new(None),
            path,
            cache,
            revision: Mutex::new(1),
            api_base: "https://api.spotify.com/v1".into(),
            token_endpoint: "https://accounts.spotify.com/api/token".into(),
            api_slots: Semaphore::new(4),
            limits: Mutex::new(limits),
            partner_limits: Mutex::new(partner_limits),
            shared_limits: Mutex::new(crate::limits::Limits::load(
                &data.join("shared-rate-limit.json"),
                &shared_identity,
            )),
            primary_refresh: Mutex::new(()),
            shared_refresh: Mutex::new(()),
            rate_path,
            advanced: Mutex::new(false),
            configure: Mutex::new(false),
            search_notice: Mutex::new(None),
            snapshots,
            library_reauth: AtomicBool::new(false),
        })
    }

    pub async fn checked(mut response: reqwest::Response) -> Result<Value> {
        let status = response.status().as_u16();
        if status == 429 {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            bail!("Spotify rate limit reached. Retry after {seconds} seconds.");
        }
        match status {
            200..=299 => {
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| anyhow::anyhow!("Spotify response was interrupted."))?
                {
                    if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                        bail!("Spotify response exceeds the size limit.");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if bytes.is_empty() {
                    return Ok(json!({}));
                }
                serde_json::from_slice(&bytes).context("Spotify returned an invalid response.")
            }
            401 => bail!("Spotify authorization expired or was rejected. Sign in again."),
            403 => bail!(
                "Spotify denied this operation. Check the account, app access and subscription."
            ),
            404 => bail!("Spotify did not return this item."),
            _ => bail!("Spotify request failed (HTTP {status})."),
        }
    }

    pub async fn token(&self) -> Result<String> {
        self.refresh_token(None).await
    }

    pub(crate) async fn refresh_token(&self, rejected: Option<&str>) -> Result<String> {
        self.refresh_for(crate::web_api::WebSource::Primary, rejected)
            .await
    }

    pub async fn identity(&self) -> Result<String> {
        let account = self.account.lock().await;
        if account.user.is_empty() {
            bail!("Connect your account in Spotify plugin settings.");
        }
        Ok(format!("{}:{}", account.client_id, account.user))
    }

    pub async fn cached<F, Fut>(
        &self,
        key: &str,
        age: u64,
        refresh: bool,
        fetch: F,
    ) -> Result<Value>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        self.snapshots
            .read(&self.identity().await?, key, age, refresh, fetch)
            .await
    }

    pub async fn has_session(&self) -> bool {
        self.account.lock().await.playback.is_some()
    }

    pub async fn spanish(&self) -> bool {
        self.account.lock().await.language == "es"
    }

    pub async fn status(&self) -> Value {
        let account = self.account.lock().await;
        json!({"signed_in": !account.refresh_token.is_empty(), "name": account.name,
            "playback_ready": account.playback.is_some(),
            "shared_search_ready": account.client_id == WEB_CLIENT || account.shared.is_some(),
            "playlist_writing_ready": (["playlist-modify-private","playlist-modify-public"].iter().all(|scope|account.scopes.iter().any(|s|s==scope))),
            "library_reauth_required":self.library_reauth.load(Ordering::Relaxed)})
    }

    async fn cancel_flow(&self) {
        let previous = self.pending.lock().await.take();
        if let Some(mut flow) = previous {
            if let Some(capture) = flow.capture.take() {
                capture.close().await;
            }
        }
    }

    async fn start_flow(&self, client: String, playback: bool) -> Result<()> {
        self.cancel_flow().await;
        let mut flow = Flow::new(client, playback);
        let capture = callback::Capture::bind(&flow.redirect, &flow.state, flow.expires).await?;
        flow.redirect.clone_from(&capture.redirect);
        flow.capture = Some(capture);
        *self.pending.lock().await = Some(flow);
        Ok(())
    }

    async fn complete_callback(&self) {
        let address = {
            let mut pending = self.pending.lock().await;
            let Some(flow) = pending.as_mut() else {
                return;
            };
            match flow.capture.as_ref().and_then(|capture| capture.take()) {
                Some(Ok(address)) => {
                    flow.returned_address = address.clone();
                    Some(address)
                }
                Some(Err(error)) => {
                    flow.callback_error = Some(error);
                    None
                }
                None => None,
            }
        };
        if let Some(address) = address {
            if let Err(error) = self.finish(&address).await {
                if let Some(flow) = self.pending.lock().await.as_mut() {
                    flow.callback_error = Some(error.to_string());
                }
            }
        }
    }

    pub async fn callback_received(&self) -> bool {
        self.pending
            .lock()
            .await
            .as_ref()
            .and_then(|flow| flow.capture.as_ref())
            .is_some_and(|capture| capture.received())
    }

    pub(crate) fn storage_path(&self) -> &std::path::Path {
        self.path.parent().expect("account directory")
    }

    pub async fn set_search_notice(&self, notice: Option<String>) {
        *self.search_notice.lock().await = notice;
    }

    pub async fn settings(&self, notice: Option<&str>) -> Value {
        self.complete_callback().await;
        let quota = self.limits.lock().await.maximum();
        let quota_message = (quota > 0).then(|| quota_notice(quota));
        let account = self.account.lock().await;
        let pending = self.pending.lock().await;
        let mut form = crate::wizard::render(crate::wizard::View {
            revision: *self.revision.lock().await,
            account_name: &account.name,
            client_id: if account.client_id == WEB_CLIENT {
                ""
            } else {
                &account.client_id
            },
            signed_in: !account.user.is_empty(),
            playback_ready: account.playback.is_some(),
            shared_ready: account.client_id == WEB_CLIENT || account.shared.is_some(),
            playlist_writing: account
                .scopes
                .iter()
                .any(|s| s == "playlist-modify-private")
                && account.scopes.iter().any(|s| s == "playlist-modify-public"),
            configure: *self.configure.lock().await,
            advanced: *self.advanced.lock().await,
            pending: pending.as_ref().map(|flow| crate::wizard::Pending {
                playback: flow.playback,
                shared: flow.shared,
                redirect: &flow.redirect,
                authorization: &flow.url,
                returned_address: &flow.returned_address,
                error: flow.callback_error.as_deref(),
                expired: now() >= flow.expires,
            }),
            notice: notice.or(quota_message.as_deref()),
            library_reauth: self.library_reauth.load(Ordering::Relaxed),
        });
        if let Some(notice) = self.search_notice.lock().await.as_ref() {
            form["notices"].as_array_mut().unwrap().push(json!(notice));
        }
        form["form"]["actions"].as_array_mut().unwrap().push(json!({"key":"language","label":if account.language == "es" {"Use English"} else {"Use Spanish"},"fields":[]}));
        form
    }

    pub async fn submit(&self, params: &Value) -> Result<Value> {
        {
            let mut revision = self.revision.lock().await;
            if params.get("revision").and_then(Value::as_u64) != Some(*revision) {
                bail!("Settings changed. Open them again before submitting.");
            }
            *revision += 1;
        }
        match text(params, "action") {
            "language" => {
                let mut account = self.account.lock().await;
                account.language = if account.language == "es" { "en" } else { "es" }.into();
                state::save(&self.path, &account)?;
            }
            "cancel" => {
                let library = self
                    .pending
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|flow| !flow.playback && !flow.shared);
                self.cancel_flow().await;
                *self.configure.lock().await = library;
            }
            "sign_out" => {
                self.logout().await?;
            }
            "sign_in" | "use_personal" | "use_shared" => {
                let action = text(params, "action");
                let submitted = text(&params["values"], "client_id").trim();
                let saved = self.account.lock().await.client_id.clone();
                if action == "use_personal" && submitted.is_empty() {
                    bail!("Paste your Client ID or choose shared access explicitly.");
                }
                let id = if action == "use_shared" {
                    WEB_CLIENT
                } else if !submitted.is_empty() {
                    submitted
                } else {
                    &saved
                };
                let id = if id.is_empty() { WEB_CLIENT } else { id };
                if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
                    bail!("Invalid Web API Client ID.");
                }
                self.start_flow(id.into(), false).await?;
                *self.configure.lock().await = false;
            }
            "enable_shared" => {
                if self.account.lock().await.user.is_empty() {
                    bail!("Connect the library first.");
                }
                self.start_flow(WEB_CLIENT.into(), false).await?;
                if let Some(flow) = self.pending.lock().await.as_mut() {
                    flow.shared = true;
                }
            }
            "enable_playlist_writing" => {
                let client = self.account.lock().await.client_id.clone();
                if client.is_empty() {
                    bail!("Connect the library first.");
                }
                self.start_flow(client, false).await?;
            }
            "playback" => {
                if self.account.lock().await.user.is_empty() {
                    bail!("Connect the library first.");
                }
                self.start_flow(PLAYBACK_CLIENT.into(), true).await?;
            }
            "finish" => {
                self.finish(text(&params["values"], "redirect_url")).await?;
            }
            "advanced" => {
                let mut advanced = self.advanced.lock().await;
                *advanced = !*advanced;
            }
            "configure" => {
                self.cancel_flow().await;
                *self.configure.lock().await = true;
            }
            "check_library" => {
                self.get("/me", &[]).await?;
                self.library_reauth.store(false, Ordering::Relaxed);
                return Ok(self.settings(Some("Spotify confirmed library access. If a playlist does not open, check its error message; its permissions may differ.")).await);
            }
            "retry" => {
                let (client, playback, shared) = {
                    let pending = self.pending.lock().await;
                    let flow = pending
                        .as_ref()
                        .context("Return to the previous step to start sign-in.")?;
                    (flow.client.clone(), flow.playback, flow.shared)
                };
                self.start_flow(client, playback).await?;
                if let Some(flow) = self.pending.lock().await.as_mut() {
                    flow.shared = shared;
                }
            }
            _ => bail!("Unknown settings action."),
        }
        Ok(self.settings(None).await)
    }

    async fn finish(&self, address: &str) -> Result<()> {
        let mut pending = self.pending.lock().await;
        let flow = pending.as_ref().context("Start sign-in again.")?;
        let address = if address.trim().is_empty() {
            &flow.returned_address
        } else {
            address
        };
        let code = flow.code(address)?;
        let response = self
            .http
            .post(&self.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", flow.client.as_str()),
                ("redirect_uri", flow.redirect.as_str()),
                ("code_verifier", flow.verifier.as_str()),
                ("code", code.as_str()),
            ])
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Cannot complete Spotify sign-in. Start again."))?;
        let token = Self::checked(response).await?;
        if flow.playback {
            let session = self
                .connect(Credentials::with_access_token(text(&token, "access_token")))
                .await?;
            let mut account = self.account.lock().await;
            if session.username() != account.user {
                session.shutdown();
                bail!("Authorize playback with the same Spotify account as the library.");
            }
            account.playback = Some(Credentials {
                username: Some(session.username()),
                auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
                auth_data: session.auth_data(),
            });
            state::save(&self.path, &account)?;
            if let Some(old) = self.session.lock().await.replace(session) {
                old.shutdown();
            }
        } else {
            let access = text(&token, "access_token");
            if access.is_empty() {
                bail!("Spotify did not return an access token.");
            }
            let profile = Self::checked(
                self.http
                    .get(format!("{}/me", self.api_base))
                    .bearer_auth(access)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Cannot read the Spotify account."))?,
            )
            .await?;
            if text(&profile, "id").is_empty() {
                bail!("Spotify did not identify the account.");
            }
            if flow.shared {
                let mut previous = self.account.lock().await;
                if previous.user != text(&profile, "id") {
                    bail!("Enable shared search with the same account as the library.");
                }
                let mut grant = Account {
                    client_id: WEB_CLIENT.into(),
                    ..Default::default()
                };
                apply_token(&mut grant, &token)?;
                if grant.refresh_token.is_empty() {
                    bail!("Spotify did not return a renewable authorization.");
                }
                if token.get("scope").is_none() {
                    grant.scopes = WEB_SCOPES.split_whitespace().map(str::to_owned).collect();
                }
                self.snapshots
                    .invalidate(&format!("{}:{}", previous.client_id, previous.user), "");
                previous.shared = Some(state::WebGrant::from_account(&grant));
                state::save(&self.path, &previous)?;
                *pending = None;
                return Ok(());
            }
            let mut account = Account {
                client_id: flow.client.clone(),
                user: text(&profile, "id").into(),
                name: text(&profile, "display_name").into(),
                ..Default::default()
            };
            apply_token(&mut account, &token)?;
            if token.get("scope").is_none() {
                account.scopes = WEB_SCOPES.split_whitespace().map(str::to_owned).collect();
            }
            if account.refresh_token.is_empty() {
                bail!("Spotify did not return a renewable authorization.");
            }
            let mut previous = self.account.lock().await;
            account.language.clone_from(&previous.language);
            let same_user = previous.user == account.user;
            if !previous.user.is_empty() {
                self.snapshots
                    .invalidate(&format!("{}:{}", previous.client_id, previous.user), "");
            }
            if same_user {
                account.playback = previous.playback.clone();
                if previous.client_id == account.client_id {
                    account.shared = previous.shared.clone();
                }
            } else if !previous.user.is_empty() {
                self.snapshots
                    .invalidate(&format!("{}:{}", previous.client_id, previous.user), "");
            }
            state::save(&self.path, &account)?;
            *self.limits.lock().await = crate::limits::Limits::load_account(
                &self.rate_path,
                &account.client_id,
                &account.user,
            );
            *self.partner_limits.lock().await = crate::limits::Limits::load(
                &self.path.with_file_name("session-home-limit.json"),
                &account.user,
            );
            *self.shared_limits.lock().await = crate::limits::Limits::load(
                &self.path.with_file_name("shared-rate-limit.json"),
                &format!("{}:{}", WEB_CLIENT, account.user),
            );
            *previous = account;
            drop(previous);
            self.library_reauth.store(false, Ordering::Relaxed);
            if !same_user {
                if let Some(old) = self.session.lock().await.take() {
                    old.shutdown();
                }
            }
        }
        *pending = None;
        Ok(())
    }

    async fn connect(&self, credentials: Credentials) -> Result<Session> {
        let config = SessionConfig {
            client_id: PLAYBACK_CLIENT.into(),
            tmp_dir: self.cache.clone(),
            ..Default::default()
        };
        let session = Session::new(config, None);
        let result =
            tokio::time::timeout(Duration::from_secs(30), session.connect(credentials, false))
                .await;
        if !matches!(result, Ok(Ok(()))) {
            session.shutdown();
            bail!("Cannot start Spotify playback. Check Premium and authorize playback again.");
        }
        Ok(session)
    }

    pub async fn session(&self) -> Result<Session> {
        let mut active = self.session.lock().await;
        if let Some(session) = active.as_ref().filter(|s| !s.is_invalid()) {
            return Ok(session.clone());
        }
        let credentials = self
            .account
            .lock()
            .await
            .playback
            .clone()
            .context("Authorize playback in the Spotify plugin settings.")?;
        let session = self.connect(credentials).await?;
        *active = Some(session.clone());
        Ok(session)
    }

    pub async fn logout(&self) -> Result<()> {
        if let Ok(identity) = self.identity().await {
            self.snapshots.invalidate(&identity, "");
        }
        let language = self.account.lock().await.language.clone();
        let account = Account {
            language,
            ..Default::default()
        };
        state::save(&self.path, &account)?;
        *self.account.lock().await = account;
        self.library_reauth.store(false, Ordering::Relaxed);
        *self.shared_limits.lock().await =
            crate::limits::Limits::load(&self.path.with_file_name("shared-rate-limit.json"), "");
        *self.limits.lock().await = crate::limits::Limits::load(&self.rate_path, "");
        *self.search_notice.lock().await = None;
        self.cancel_flow().await;
        if let Some(session) = self.session.lock().await.take() {
            session.shutdown();
        }
        *self.revision.lock().await += 1;
        Ok(())
    }
}

fn quota_notice(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    let wait = if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    };
    format!(
        "Spotify paused Web API requests. Retry in {wait} (HTTP 429). Home, playlists, albums, artists, playback and lyrics can use the playback session. Signing in again does not reset the quota."
    )
}

#[derive(Debug)]
pub(crate) struct ApiFailure {
    pub status: u16,
    pub wait: u64,
}
impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 429 {
            return f.write_str(&quota_notice(self.wait));
        }
        write!(
            f,
            "Spotify could not answer this request (HTTP {}).",
            self.status
        )
    }
}
impl std::error::Error for ApiFailure {}
pub(crate) fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .map_or(true, |e| e.status == 429 || e.status >= 500)
}

pub(crate) fn apply_token(account: &mut Account, token: &Value) -> Result<()> {
    let access = text(token, "access_token");
    if access.is_empty() {
        bail!("Spotify returned an incomplete authorization.");
    }
    account.access_token = access.into();
    if let Some(refresh) = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        account.refresh_token = refresh.into();
    }
    if let Some(scopes) = token.get("scope").and_then(Value::as_str) {
        account.scopes = scopes.split_whitespace().map(str::to_owned).collect();
    }
    account.expires = now().saturating_add(
        token
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_callback_is_bound_to_origin_state_and_expiry() {
        let mut flow = Flow::new(WEB_CLIENT.into(), false);
        let valid = format!("{}?state={}&code=test", flow.redirect, flow.state);
        assert_eq!(flow.code(&valid).unwrap(), "test");
        assert!(
            flow.code(&valid.replace("127.0.0.1", "evil.example"))
                .is_err()
        );
        assert!(flow.code(&format!("{valid}&code=other")).is_err());
        assert!(flow.code(&format!("{valid}&state=other")).is_err());
        flow.expires = 0;
        assert!(flow.code(&valid).is_err());
    }
    #[test]
    fn refresh_keeps_the_previous_refresh_token() {
        let mut account = Account {
            refresh_token: "old".into(),
            ..Default::default()
        };
        apply_token(&mut account, &json!({"access_token":"new","expires_in":10})).unwrap();
        assert_eq!(account.refresh_token, "old");
        assert!(apply_token(&mut account, &json!({})).is_err());
    }

    #[tokio::test]
    async fn settings_complete_a_captured_callback_without_pasting_or_api_keys() {
        let token =
            json!({"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600});
        let profile = json!({"id":"test-user","display_name":"Test account"});
        let (mut client, _directory, worker) =
            crate::fixtures::client(vec![(200, token.to_string()), (200, profile.to_string())])
                .await;
        *client.account.lock().await = Account::default();
        client.token_endpoint = format!("{}/token", client.api_base);
        let initial = client.settings(None).await;
        assert_eq!(
            initial["values"]["callback_url"],
            "http://127.0.0.1:8989/login"
        );
        assert_eq!(initial["values"]["client_id"], "");
        let mut flow = Flow::new(WEB_CLIENT.into(), false);
        let capture =
            callback::Capture::bind("http://127.0.0.1:0/login", &flow.state, flow.expires)
                .await
                .unwrap();
        flow.redirect = capture.redirect.clone();
        let address = format!("{}?state={}&code=test-code", flow.redirect, flow.state);
        flow.capture = Some(capture);
        *client.pending.lock().await = Some(flow);
        reqwest::get(address).await.unwrap();
        for _ in 0..100 {
            if client.callback_received().await {
                break;
            }
            tokio::task::yield_now().await;
        }
        let completed = client.settings(None).await;
        assert_eq!(completed["status"]["phase"], "ready");
        assert!(completed["links"].as_array().unwrap().is_empty());
        assert_eq!(client.status().await["signed_in"], true);
        assert_eq!(client.status().await["name"], "Test account");
        assert_eq!(worker.join().unwrap(), vec!["/token", "/me"]);
    }

    #[tokio::test]
    async fn permission_denial_and_malformed_replies_remain_errors_without_private_bodies() {
        let (mut client, _directory, worker) = crate::fixtures::client(vec![
            (403, "private-error-token".into()),
            (200, "{broken".into()),
            (401, "private-error-token".into()),
            (400, "private-error-token".into()),
        ])
        .await;
        client.token_endpoint = format!("{}/token", client.api_base);
        for _ in 0..3 {
            let error = client.get("/me", &[]).await.unwrap_err().to_string();
            assert!(!error.contains("private-error-token"));
        }
        assert!(
            client.status().await["library_reauth_required"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(worker.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_rejected_read_refreshes_once_and_a_rejected_write_is_not_replayed() {
        let token = json!({"access_token":"renewed","expires_in":3600});
        let (mut client, _directory, worker) = crate::fixtures::client(vec![
            (401, "private-body".into()),
            (200, token.to_string()),
            (200, "{}".into()),
            (401, "private-body".into()),
        ])
        .await;
        client.token_endpoint = format!("{}/token", client.api_base);
        assert!(client.get("/me", &[]).await.is_ok());
        assert_eq!(client.account.lock().await.refresh_token, "test-only");
        assert_eq!(client.account.lock().await.access_token, "renewed");
        assert!(client.api(Method::PUT, "/me/library", &[]).await.is_err());
        assert_eq!(
            worker.join().unwrap(),
            vec!["/me", "/token", "/me", "/me/library"]
        );
    }

    #[tokio::test]
    async fn concurrent_expired_tokens_share_a_single_refresh() {
        let token = json!({"access_token":"renewed","expires_in":3600});
        let (mut client, _directory, worker) =
            crate::fixtures::client(vec![(200, token.to_string())]).await;
        client.token_endpoint = format!("{}/token", client.api_base);
        client.account.lock().await.expires = 0;
        let (a, b, c, d) = tokio::join!(
            client.token(),
            client.token(),
            client.token(),
            client.token()
        );
        for result in [a, b, c, d] {
            assert_eq!(result.unwrap(), "renewed");
        }
        assert_eq!(worker.join().unwrap(), vec!["/token"]);
    }

    #[tokio::test]
    async fn reconnecting_the_same_library_keeps_playback_but_switching_accounts_does_not() {
        for same_user in [true, false] {
            let token =
                json!({"access_token":"renewed","refresh_token":"new-refresh","expires_in":3600});
            let profile = json!({"id":if same_user {"test"} else {"other"},"display_name":"Test"});
            let (mut client, _directory, worker) =
                crate::fixtures::client(vec![(200, token.to_string()), (200, profile.to_string())])
                    .await;
            client.token_endpoint = format!("{}/token", client.api_base);
            client.account.lock().await.playback =
                Some(Credentials::with_access_token("test-only"));
            let flow = Flow::new(WEB_CLIENT.into(), false);
            let callback = format!("{}?state={}&code=test-only", flow.redirect, flow.state);
            *client.pending.lock().await = Some(flow);
            client.finish(&callback).await.unwrap();
            assert_eq!(client.account.lock().await.playback.is_some(), same_user);
            assert_eq!(worker.join().unwrap(), vec!["/token", "/me"]);
        }
    }

    #[tokio::test]
    async fn development_quota_does_not_block_unrelated_web_routes() {
        let quota =
            json!({"error":{"status":429,"reason":"QUOTA_EXCEEDED","message":"private-body"}});
        let (client, _directory, worker) =
            crate::fixtures::client(vec![(429, quota.to_string()), (200, "{}".into())]).await;
        assert!(client.get("/me/top/tracks", &[]).await.is_err());
        assert!(client.get("/me/top/tracks", &[]).await.is_err());
        assert!(client.get("/search", &[]).await.is_ok());
        assert_eq!(worker.join().unwrap(), vec!["/me/top/tracks", "/search"]);
    }

    #[tokio::test]
    async fn rate_limit_prevents_an_immediate_second_request() {
        let (client, _directory, worker) = crate::fixtures::client(vec![(429, "{}".into())]).await;
        assert!(
            client
                .get("/me", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTP 429")
        );
        assert!(
            client
                .get("/me", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTP 429")
        );
        assert_eq!(worker.join().unwrap().len(), 1);
        let account = client.account.lock().await.clone();
        state::save(&client.path, &account).unwrap();
        let restarted = Client::new(
            client.path.parent().unwrap().to_path_buf(),
            client.cache.clone(),
        )
        .unwrap();
        assert!(
            restarted
                .get("/me", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTP 429")
        );
        assert!(
            restarted.settings(None).await["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("HTTP 429"))
        );
        assert_eq!(state::saved_pause(&client.rate_path, "another-app"), 0);
    }

    #[tokio::test]
    async fn http_timeout_is_bounded_and_does_not_include_the_url_or_token() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            std::thread::sleep(Duration::from_millis(150));
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        });
        let directory = tempfile::tempdir().unwrap();
        let mut client = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        client.http = Http::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        client.api_base = format!("http://{address}");
        *client.account.lock().await = Account {
            refresh_token: "private-token".into(),
            access_token: "private-token".into(),
            expires: u64::MAX,
            ..Default::default()
        };
        let error = client.get("/me", &[]).await.unwrap_err().to_string();
        assert!(!error.contains("private-token"));
        assert!(!error.contains(&address.to_string()));
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;
    #[tokio::test]
    async fn auxiliary_grant_requires_the_same_user_and_preserves_primary() {
        for same_user in [true, false] {
            let token = json!({"access_token":"shared-only","refresh_token":"shared-refresh","expires_in":3600,"scope":"user-read-private"});
            let profile = json!({"id":if same_user {"test"} else {"other"}});
            let (mut client, _dir, worker) =
                crate::fixtures::client(vec![(200, token.to_string()), (200, profile.to_string())])
                    .await;
            client.token_endpoint = format!("{}/token", client.api_base);
            client.account.lock().await.client_id = "personal".into();
            client.account.lock().await.playback =
                Some(Credentials::with_access_token("playback-test"));
            let before = client.account.lock().await.clone();
            let mut flow = Flow::new(WEB_CLIENT.into(), false);
            flow.shared = true;
            let address = format!("{}?state={}&code=mock", flow.redirect, flow.state);
            *client.pending.lock().await = Some(flow);
            assert_eq!(client.finish(&address).await.is_ok(), same_user);
            let after = client.account.lock().await.clone();
            assert_eq!(after.client_id, before.client_id);
            assert_eq!(after.access_token, before.access_token);
            assert_eq!(after.refresh_token, before.refresh_token);
            assert!(after.playback.is_some());
            assert_eq!(after.shared.is_some(), same_user);
            assert_eq!(worker.join().unwrap(), vec!["/token", "/me"]);
        }
    }
    #[tokio::test]
    async fn shared_refresh_failure_keeps_primary_and_old_accounts_have_no_write_scopes() {
        let (mut client, _dir, worker) =
            crate::fixtures::client(vec![(400, "private-body".into())]).await;
        client.token_endpoint = format!("{}/token", client.api_base);
        {
            let mut a = client.account.lock().await;
            a.client_id = "personal".into();
            a.shared = Some(state::WebGrant {
                refresh_token: "shared-test".into(),
                ..Default::default()
            });
        }
        let primary = client.account.lock().await.access_token.clone();
        let error = client
            .refresh_for(crate::web_api::WebSource::Shared, None)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<ApiFailure>().unwrap().status, 401);
        assert_eq!(client.account.lock().await.access_token, primary);
        assert!(client.account.lock().await.shared.is_some());
        assert!(!client.can_edit_playlists().await);
        assert_eq!(worker.join().unwrap(), vec!["/token"]);
    }
}
