use crate::{auth::Client, catalog::card, state::text};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

impl Client {
    pub async fn discover(&self, reference: &str, refresh: bool) -> Result<Value> {
        let reference = if reference.is_empty() || reference == "discover:foryou" {
            "discover:home"
        } else {
            reference
        };
        self.cached(reference, 1200, refresh, || async {
            tokio::time::timeout(Duration::from_secs(25), self.discover_fetch(reference))
                .await
                .map_err(|_| anyhow::anyhow!("Discover took too long to respond. Try again."))?
        })
        .await
    }

    async fn discover_fetch(&self, reference: &str) -> Result<Value> {
        if let Some(rest) = reference.strip_prefix("discovery:") {
            let (kind, offset) = rest.split_once(':').context("Invalid discovery page.")?;
            let offset = offset
                .parse::<u32>()
                .ok()
                .filter(|n| *n <= 1000)
                .context("Invalid discovery offset.")?;
            let (title, query) = match kind {
                "new" => ("New releases: last two weeks", "tag:new"),
                "hidden" => ("Lesser-known albums", "tag:hipster"),
                _ => bail!("Unknown discovery page."),
            };
            let (items, more) = self.discover_albums(query, offset, kind).await?;
            return Ok(json!({"title":title,"items":items,"next":more}));
        }
        let mut shelves = vec![];
        let mut failures = vec![];
        let mut personal = true;
        let mut fallback = false;
        match reference {
            "discover:home" if self.has_session().await => {
                // Like Spotifast, personalized Home uses Pathfinder, not /me/top
                // or a burst of public Web API requests on every visit.
                match self.session_home().await {
                    Ok(cards) if !cards.is_empty() => shelves.push(shelf(
                        "made-for-you",
                        "Made for you",
                        "playlist",
                        cards,
                        None,
                    )),
                    result => {
                        if result.is_err() {
                            if let Some(mut previous) = self
                                .snapshots
                                .peek(&self.identity().await?, reference)
                                .filter(|v| {
                                    v["personal"] == true
                                        && v["shelves"].as_array().is_some_and(|s| !s.is_empty())
                                })
                            {
                                crate::snapshot::mark_incomplete();
                                previous["cached"] = json!(true);
                                return Ok(previous);
                            }
                        }
                        // Spotifast also has a fallback when Pathfinder refuses a
                        // client. I use session playlist headers so a Web quota
                        // does not blank the fallback as well.
                        let cards = self.session_featured().await?;
                        shelves.push(shelf(
                            "featured",
                            "Featured by Spotify",
                            "playlist",
                            cards,
                            None,
                        ));
                        personal = false;
                        fallback = true;
                    }
                }
            }
            "discover:home" => {
                let (tracks, artists, new) = tokio::join!(
                    async {
                        self.get(
                            "/me/top/tracks",
                            &[("limit", "20".into()), ("time_range", "short_term".into())],
                        )
                        .await
                    },
                    async {
                        self.get(
                            "/me/top/artists",
                            &[("limit", "10".into()), ("time_range", "medium_term".into())],
                        )
                        .await
                    },
                    self.discover_albums("tag:new", 0, "new")
                );
                match new {
                    Ok((cards, more)) => shelves.push(shelf(
                        "new-releases",
                        "New releases: last two weeks",
                        "album",
                        cards,
                        more,
                    )),
                    Err(e) => failures.push(e),
                }
                match tracks.and_then(|v| items(&v["items"], "track")) {
                    Ok(cards) => shelves.push(shelf(
                        "top-tracks",
                        "Your top tracks this month",
                        "track",
                        cards,
                        None,
                    )),
                    Err(e) => failures.push(e),
                }
                match artists.and_then(|v| items(&v["items"], "artist")) {
                    Ok(cards) => shelves.push(shelf(
                        "top-artists",
                        "Your top artists",
                        "artist",
                        cards,
                        None,
                    )),
                    Err(e) => failures.push(e),
                }
            }
            "discover:releases" => {
                let (new, hidden) = tokio::join!(
                    self.discover_albums("tag:new", 0, "new"),
                    self.discover_albums("tag:hipster", 0, "hidden")
                );
                for (result, id, title) in [
                    (new, "new-releases", "New releases: last two weeks"),
                    (hidden, "hidden-albums", "Lesser-known albums"),
                ] {
                    match result {
                        Ok((cards, more)) => shelves.push(shelf(id, title, "album", cards, more)),
                        Err(e) => failures.push(e),
                    }
                }
            }
            "discover:activity" => {
                let (recent, albums, playlists) = tokio::join!(
                    async {
                        self.get("/me/player/recently-played", &[("limit", "20".into())])
                            .await
                    },
                    async { self.library(&json!({"kind":"albums","limit":20})).await },
                    async { self.library(&json!({"kind":"playlists","limit":20})).await }
                );
                match recent.and_then(|v| {
                    v["items"]
                        .as_array()
                        .context("Spotify returned an invalid listening history.")
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|v| card(&v["track"], "track"))
                                .collect()
                        })
                }) {
                    Ok(cards) => {
                        shelves.push(shelf("recent", "Recently played", "track", cards, None))
                    }
                    Err(e) => failures.push(e),
                }
                for (result, id, title, kind) in [
                    (albums, "saved-albums", "Your albums", "album"),
                    (playlists, "playlists", "Your playlists", "playlist"),
                ] {
                    match result {
                        Ok(value) => shelves.push(shelf(
                            id,
                            title,
                            kind,
                            value["items"]
                                .as_array()
                                .context("Invalid library cards.")?
                                .clone(),
                            None,
                        )),
                        Err(e) => failures.push(e),
                    }
                }
            }
            _ => bail!("Unknown Spotify Discover tab."),
        }
        if shelves.is_empty()
            && !failures.is_empty()
            && reference == "discover:releases"
            && self.has_session().await
        {
            let cards = self.session_featured().await?;
            shelves.push(shelf(
                "new-music-playlists",
                "New music and charts",
                "playlist",
                cards,
                None,
            ));
            personal = false;
            fallback = true;
        }
        if shelves.is_empty() && !failures.is_empty() {
            return Err(failures.remove(0));
        }
        let tabs = [
            (
                if personal { "For you" } else { "Featured" },
                "discover:home",
            ),
            ("New releases", "discover:releases"),
            ("Your activity", "discover:activity"),
        ]
        .into_iter()
        .map(|(title, key)| json!({"title":title,"ref":key,"active":key==reference}))
        .collect::<Vec<_>>();
        Ok(
            json!({"title":"Spotify","personal":personal,"home_fallback":fallback,"tabs":tabs,"shelves":shelves,"partial":!failures.is_empty()}),
        )
    }

    async fn discover_albums(
        &self,
        query: &str,
        offset: u32,
        kind: &str,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let response = self
            .get(
                "/search",
                &[
                    ("q", query.into()),
                    ("type", "album".into()),
                    ("limit", "10".into()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        let page = &response["albums"];
        let items = items(&page["items"], "album")?;
        let more = (!page["next"].is_null() && !text(page, "next").is_empty() && offset < 1000)
            .then(|| format!("discovery:{kind}:{}", offset + 10));
        Ok((items, more))
    }
}

fn items(raw: &Value, kind: &str) -> Result<Vec<Value>> {
    Ok(raw
        .as_array()
        .context("Spotify returned an invalid Discover listing.")?
        .iter()
        .filter_map(|v| card(v, kind))
        .collect())
}
fn shelf(id: &str, title: &str, kind: &str, items: Vec<Value>, more: Option<String>) -> Value {
    json!({"id":id,"title":title,"kind":kind,"items":items,"more":more})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn albums(next: Value) -> String {
        json!({"albums":{"items":[{"id":"1234567890123456789012","name":"Release","type":"album"}],"next":next}}).to_string()
    }
    #[tokio::test]
    async fn discoveries_use_search_filters_and_cache_concurrent_reads() {
        let (client, _directory, worker) = crate::fixtures::routed(vec![
            (
                "/search?q=tag:new",
                200,
                albums(json!("ignored-provider-url")),
            ),
            ("/search?q=tag:hipster", 200, albums(Value::Null)),
            ("/search?q=tag:new", 200, albums(Value::Null)),
        ])
        .await;
        let (first, second) = tokio::join!(
            client.discover("discover:releases", false),
            client.discover("discover:releases", false)
        );
        let first = first.unwrap();
        assert_eq!(first, second.unwrap());
        assert_eq!(first["shelves"][0]["more"], "discovery:new:10");
        let next = client.discover("discovery:new:10", false).await.unwrap();
        assert_eq!(next["items"].as_array().unwrap().len(), 1);
        let paths = worker.join().unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths.iter().any(|p| p.contains("offset=10")));
        assert!(paths.iter().all(|p| p.starts_with("/search?")));
    }
    #[tokio::test]
    async fn personal_home_has_discovery_and_tops_instead_of_saved_library_shelves() {
        let (client, _directory, worker) = crate::fixtures::routed(vec![
            ("/me/top/tracks", 200, json!({"items":[]}).to_string()),
            ("/me/top/artists", 200, json!({"items":[]}).to_string()),
            ("/search?q=tag:new", 200, albums(Value::Null)),
        ])
        .await;
        let document = client.discover("discover:home", false).await.unwrap();
        assert_eq!(document["tabs"].as_array().unwrap().len(), 3);
        assert_eq!(document["shelves"][0]["id"], "new-releases");
        assert!(
            worker
                .join()
                .unwrap()
                .iter()
                .all(|p| !p.contains("/me/albums") && !p.contains("/me/playlists"))
        );
    }
    #[tokio::test]
    async fn repeated_refresh_is_coalesced_and_account_changes_do_not_reuse_discovery() {
        let (client, _directory, worker) = crate::fixtures::routed(vec![
            ("/search?q=tag:new", 200, albums(Value::Null)),
            ("/search?q=tag:hipster", 200, albums(Value::Null)),
            ("/search?q=tag:new", 200, albums(Value::Null)),
            ("/search?q=tag:hipster", 200, albums(Value::Null)),
        ])
        .await;
        client.discover("discover:releases", false).await.unwrap();
        client.discover("discover:releases", true).await.unwrap();
        client.account.lock().await.user = "other-account".into();
        client.discover("discover:releases", false).await.unwrap();
        assert_eq!(worker.join().unwrap().len(), 4);
    }
    #[tokio::test]
    async fn malformed_discovery_is_an_error_not_an_empty_shelf() {
        let (client, _directory, worker) =
            crate::fixtures::routed(vec![("/search?q=tag:new", 200, "{}".into())]).await;
        assert!(client.discover("discovery:new:0", false).await.is_err());
        assert!(client.discover("discovery:new:1001", false).await.is_err());
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
