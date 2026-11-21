mod auth;
mod callback;
mod catalog;
mod discover;
mod enrichment;
#[cfg(test)]
mod fixtures;
mod genre_discovery;
mod i18n;
mod limits;
mod lyrics;
mod native_catalog;
mod playlist_actions;
mod radio;
mod session_credits;
mod session_enrichment;
mod session_home;
mod session_labels;
mod session_playlists;
mod snapshot;
mod state;
mod stream;
mod web_api;
mod wizard;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, RwLock, Semaphore},
};

struct Service {
    client: auth::Client,
    streams: stream::Streams,
    gate: RwLock<()>,
}
impl Service {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value> {
        if method == "favorites.set" || method == "page.action" {
            let _guard = self.gate.write().await;
            return if method == "favorites.set" {
                self.client.favorite(params).await
            } else {
                match state::text(params, "action") {
                    "favorite" | "playlist.create" | "playlist.add" => {
                        self.client.page_action(params).await
                    }
                    _ => bail!("Unknown plugin operation."),
                }
            };
        }
        if matches!(
            method,
            "settings.submit" | "settings.open" | "auth.login" | "auth.logout"
        ) {
            let _guard = self.gate.write().await;
            if method == "auth.logout" {
                self.client.logout().await?;
                self.streams.clear();
                return Ok(json!({"signed_in":false}));
            }
            if matches!(method, "settings.open" | "auth.login") {
                let callback = self.client.callback_received().await;
                let form = self.client.settings(None).await;
                if callback {
                    self.streams.clear();
                }
                return Ok(form);
            }
            let result = self.client.submit(params).await;
            if matches!(state::text(params, "action"), "sign_out" | "finish") {
                self.streams.clear();
            }
            return match result {
                Ok(v) => Ok(v),
                Err(e) => Ok(self.client.settings(Some(&e.to_string())).await),
            };
        }
        let _guard = self.gate.read().await;
        match method {
            "auth.status" => Ok(self.client.status().await),
            "inspect" => Ok(json!({"settings_pages":["account"]})),
            "link.resolve" => Ok(catalog::link(state::text(params, "url")).unwrap_or(Value::Null)),
            "search" => self.client.search(params).await,
            "library.list" => self.client.library(params).await,
            "home" => self.client.home().await,
            "page.get" => self.client.page(params).await,
            "track.get" => self.client.entity(params, "track").await,
            "track.lyrics" => self.client.lyrics(params).await,
            "album.get" => self.client.entity(params, "album").await,
            "artist.get" => self.client.entity(params, "artist").await,
            "playlist.get" => self.client.entity(params, "playlist").await,
            "track.resolve" => self.streams.resolve(&self.client, params).await,
            _ => bail!("Unknown plugin operation."),
        }
    }
}

fn capabilities() -> Value {
    json!({"protocol":1,"capabilities":{"auth":"browser","search":true,"pages":true,"genre_discovery":1,"home":true,
        "resolve":true,"favorites":true,"library":["albums","tracks","artists","playlists"],
        "playlist_edit":true,"folder_edit":false,"scene_search_limit":5,"track_lyrics":1}})
}

/// A writable folder the host names: `key` in `initialize`, else the
/// environment's `variable`. A missing or relative folder is an error; no
/// default is substituted.
fn folder(params: &Value, key: &str, variable: &str) -> Result<PathBuf> {
    let named = match params.get(key) {
        None | Some(Value::Null) => std::env::var_os(variable).map(PathBuf::from),
        Some(found) => found.as_str().map(PathBuf::from),
    };
    named.filter(|path| path.is_absolute()).ok_or_else(|| {
        anyhow::anyhow!(
            "initialize did not name an absolute {key}, and {variable} is not set to one."
        )
    })
}

/// Opens storage in the folders `initialize` names and the local endpoint.
fn start(params: &Value) -> Result<Service> {
    let data = folder(params, "data_dir", "CHILPA_PLUGIN_DATA_DIR")?;
    let cache = folder(params, "cache_dir", "CHILPA_PLUGIN_CACHE_DIR")?;
    match (auth::Client::new(data, cache), stream::Streams::new()) {
        (Ok(client), Ok(streams)) => Ok(Service {
            client,
            streams,
            gate: RwLock::new(()),
        }),
        _ => bail!("Cannot initialize the Spotify plugin's private storage or local endpoint."),
    }
}

async fn send(output: &Mutex<tokio::io::Stdout>, value: Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    let mut output = output.lock().await;
    output.write_all(&bytes).await?;
    output.flush().await
}

#[tokio::main(worker_threads = 4)]
async fn main() {
    // I never initialize a logging backend: upstream diagnostics can contain private data.
    if run().await.is_err() {
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut input = BufReader::new(tokio::io::stdin());
    let slots = Arc::new(Semaphore::new(16));
    let mut service: Option<Arc<Service>> = None;
    loop {
        // I bound every line before parsing, including a peer that never sends a newline.
        let mut bytes = Vec::new();
        let count = tokio::io::AsyncReadExt::take(&mut input, 1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            break;
        }
        if bytes.len() > 1024 * 1024 {
            break;
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                send(&output,json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON."}})).await?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if request["jsonrpc"] != "2.0"
            || !request["method"].is_string()
            || !(id.is_null() || id.is_string() || id.is_number())
            || request.get("params").is_some_and(|p| !p.is_object())
        {
            send(&output,json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid JSON-RPC request."}})).await?;
            continue;
        }
        if !request.as_object().unwrap().contains_key("id") {
            continue;
        }
        let method = state::text(&request, "method").to_owned();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if method == "initialize" {
            let result = if service.is_some() || params["protocol"] != 1 {
                Err(anyhow::anyhow!("Unsupported or repeated initialization."))
            } else {
                start(&params).map(|started| {
                    service = Some(Arc::new(started));
                    capabilities()
                })
            };
            let response = match result {
                Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Err(e) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}})
                }
            };
            send(&output, response).await?;
            continue;
        }
        let Some(service) = service.clone() else {
            send(&output,json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Initialize the plugin first."}})).await?;
            continue;
        };
        let permit = match slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                send(&output,json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Plugin is busy. Try again."}})).await?;
                continue;
            }
        };
        let output = output.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let seconds = if method == "track.resolve" { 18 } else { 55 };
            let result = tokio::time::timeout(
                Duration::from_secs(seconds),
                service.handle(&method, &params),
            )
            .await;
            let response = match result {
                Ok(Ok(mut result)) => {
                    i18n::document(
                        &method,
                        &params,
                        &mut result,
                        service.client.spanish().await,
                    );
                    json!({"jsonrpc":"2.0","id":id,"result":result})
                }
                Ok(Err(e)) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":if e.to_string()=="Unknown plugin operation." {-32601} else {-32000},"message":i18n::text(&e.to_string(),service.client.spanish().await),"data":web_api::error_data(&e)}})
                }
                Err(_) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Spotify request timed out. Try again.","data":{"status":504,"terminal":false}}})
                }
            };
            if send(&output, response).await.is_err() {
                std::process::exit(0);
            }
        });
    }
    // Tokio's stdin worker is already at EOF. Exit releases listeners and temporary audio files.
    std::process::exit(0);
}

#[cfg(test)]
mod folder_tests {
    use super::*;

    /// A variable no environment sets, so only `initialize` can name the folder.
    const UNSET: &str = "CHILPA_PLUGIN_TEST_FOLDER_NEVER_SET";

    #[test]
    fn a_folder_comes_from_initialize_and_is_never_guessed() {
        let named = folder(&json!({"data_dir": "/profile/data"}), "data_dir", UNSET).unwrap();
        assert_eq!(named, PathBuf::from("/profile/data"));
        for params in [
            json!({}),
            json!({"data_dir": null}),
            json!({"data_dir": "data"}),
            json!({"data_dir": 7}),
        ] {
            let error = folder(&params, "data_dir", UNSET).unwrap_err().to_string();
            assert!(error.contains("data_dir"), "{params}: {error}");
        }
    }
}
