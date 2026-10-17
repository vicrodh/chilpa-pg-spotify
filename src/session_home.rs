// I adapt Spotifast's made_for_you.rs (MIT, Carmine Paolino), itself based
// on Psst's homeSection integration. The private contract can change.
use crate::{
    auth::{ApiFailure, Client},
    catalog::card,
    state::reference,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;

const SECTION: &str = "spotify:section:0JQ5DAUnp4wcj0bCb3wh3S";
const HASH: &str = "eb3fba2d388cf4fc4d696b1757a58584e9538a3b515ea742e9cc9465807340be";
const DJ: &str = "spotify:playlist:37i9dQZF1EYkqdzj48dyYq";

impl Client {
    pub async fn session_home(&self) -> Result<Vec<Value>> {
        let identity = self.identity().await?;
        if self
            .snapshots
            .peek(&identity, "capability:home")
            .is_some_and(|v| v["until"].as_u64().unwrap_or(0) > crate::state::now())
        {
            return Err(ApiFailure {
                status: 403,
                wait: 0,
            }
            .into());
        }
        let remaining = self.partner_limits.lock().await.remaining("/home");
        if remaining > 0 {
            return Err(ApiFailure {
                status: 429,
                wait: remaining,
            }
            .into());
        }
        let context = self.token().await?;
        let session = self.session().await?;
        let token = session
            .login5()
            .auth_token()
            .await
            .map_err(|_| anyhow::anyhow!("Spotify Home could not be authorized."))?;
        let client_token = session
            .spclient()
            .client_token()
            .await
            .map_err(|_| anyhow::anyhow!("Spotify Home could not be authorized."))?;
        let mut result = vec![];
        let mut seen = HashSet::new();
        for offset in (0..200).step_by(20) {
            let response = self
                .http
                .post("https://api-partner.spotify.com/pathfinder/v2/query")
                .bearer_auth(&token.access_token)
                .header("client-token", &client_token)
                .json(&query(&session.country(), &context, offset))
                .send()
                .await
                .map_err(|_| ApiFailure {
                    status: 503,
                    wait: 0,
                })?;
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
                    .record("/home", seconds, false);
                return Err(ApiFailure {
                    status: 429,
                    wait: seconds,
                }
                .into());
            }
            if response.status().as_u16() == 403 {
                self.snapshots.put(
                    &identity,
                    "capability:home",
                    json!({"until":crate::state::now() + 3600}),
                )?;
            }
            if !response.status().is_success() {
                return Err(ApiFailure {
                    status: response.status().as_u16(),
                    wait: 0,
                }
                .into());
            }
            let (cards, count, total) = parse(Self::checked(response).await?)?;
            let mut added = 0;
            for card in cards {
                if seen.insert(card["ref"].as_str().unwrap_or("").to_owned()) {
                    result.push(card);
                    added += 1;
                }
            }
            if count == 0 || total.map_or(count < 20, |t| offset + count >= t) {
                return Ok(result);
            }
            if added == 0 && count > 0 {
                bail!("Spotify repeated a Home page; the saved copy was kept.");
            }
        }
        bail!("Spotify Home exceeded the paging limit.")
    }
}

fn query(country: &str, context: &str, offset: usize) -> Value {
    json!({"operationName":"homeSection","variables":{"uri":SECTION,"country":country,"timeZone":"UTC","sp_t":context,
        "sectionItemsLimit":20,"sectionItemsOffset":offset},"extensions":{"persistedQuery":{"version":1,"sha256Hash":HASH}}})
}

fn parse(value: Value) -> Result<(Vec<Value>, usize, Option<usize>)> {
    if value
        .get("errors")
        .is_some_and(|e| !e.is_null() && e.as_array().is_none_or(|a| !a.is_empty()))
    {
        bail!("Spotify could not load Made for you through the playback session.");
    }
    let sections = value
        .pointer("/data/homeSections/sections")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .context("Spotify did not return Made for you.")?;
    let mut cards = vec![];
    let mut count = 0;
    let mut total = Some(0usize);
    for section in sections {
        let items = section
            .pointer("/sectionItems/items")
            .and_then(Value::as_array)
            .context("Spotify returned an invalid Home response.")?;
        count += items.len();
        total = total.and_then(|n| {
            section
                .pointer("/sectionItems/totalCount")
                .and_then(Value::as_u64)
                .map(|t| n.saturating_add(t as usize))
        });
        for item in items {
            let Some(data) = item.pointer("/content/data") else {
                continue;
            };
            if data["__typename"] != "Playlist" {
                continue;
            }
            let Some(uri) = data["uri"].as_str().filter(|u| *u != DJ) else {
                continue;
            };
            let Some(short) = uri.strip_prefix("spotify:") else {
                continue;
            };
            let Ok((_, id)) = reference(short, Some("playlist")) else {
                continue;
            };
            let Some(name) = data["name"].as_str() else {
                continue;
            };
            let images: Vec<_> = data
                .pointer("/images/items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v["sources"].as_array())
                .flatten()
                .filter(|v| v["url"].as_str().is_some_and(|s| s.starts_with("https://")))
                .cloned()
                .collect();
            let raw = json!({"id":id,"name":name,"description":data["description"],"images":images,"owner":{"display_name":data.pointer("/ownerV2/data/name")}});
            if let Some(card) = card(&raw, "playlist") {
                cards.push(card);
            }
        }
    }
    Ok((cards, count, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires an explicitly supplied private account; prints only status and fixed diagnostic flags"]
    async fn live_home_diagnostic() {
        let path = std::env::var("SPOTIFY_TEST_ACCOUNT").expect("explicit account path");
        let directory = tempfile::tempdir().unwrap();
        let client = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        let mut account = crate::state::load(std::path::Path::new(&path)).unwrap();
        account.refresh_token.clear();
        let context = account.access_token.clone();
        *client.account.lock().await = account;
        let session = client.session().await.unwrap();
        let token = session.login5().auth_token().await.unwrap();
        let client_token = session.spclient().client_token().await.unwrap();
        let response = client
            .http
            .post("https://api-partner.spotify.com/pathfinder/v2/query")
            .bearer_auth(&token.access_token)
            .header("client-token", &client_token)
            .json(&query(&session.country(), &context, 0))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap();
        let body = String::from_utf8_lossy(&body).to_lowercase();
        let words = [
            "scope",
            "client",
            "token",
            "forbidden",
            "country",
            "grant",
            "premium",
            "quota",
            "persisted",
            "sp_t",
            "unauthor",
            "login",
            "permission",
        ];
        let flags: Vec<_> = words
            .into_iter()
            .filter(|word| body.contains(word))
            .collect();
        println!(
            "HOME_DIAGNOSTIC status={status} flags={flags:?} country_ready={}",
            !session.country().is_empty()
        );
    }
    #[test]
    fn home_keeps_localized_playlists_and_rejects_graphql_errors() {
        let value = json!({"data":{"homeSections":{"sections":[{"sectionItems":{"totalCount":3,"items":[
            {"content":{"data":{"__typename":"Playlist","uri":"spotify:playlist:1234567890123456789012","name":"Para ti"}}},
            {"content":{"data":{"__typename":"Playlist","uri":DJ,"name":"DJ"}}},
            {"content":{"data":{"__typename":"FutureCard"}}}
        ]}}]}}});
        let (cards, count, total) = parse(value).unwrap();
        assert_eq!((cards.len(), count, total), (1, 3, Some(3)));
        assert_eq!(cards[0]["ref"], "playlist:1234567890123456789012");
        assert_eq!(cards[0]["title"], "Para ti");
        assert!(parse(json!({"errors":[{"message":"private-body"}]})).is_err());
        assert!(parse(json!({"data":null})).is_err());
        assert_eq!(query("MX", "context", 20)["variables"]["sp_t"], "context");
    }
}
