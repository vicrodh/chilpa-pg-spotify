use crate::{
    auth::Client,
    state::{reference, text},
};
use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};

pub fn card(raw: &Value, kind: &str) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let id = text(raw, "id");
    let reference = format!("{kind}:{id}");
    super::state::reference(&reference, Some(kind)).ok()?;
    let album = &raw["album"];
    let artist = &raw["artists"][0];
    let image = raw["images"][0]["url"]
        .as_str()
        .or_else(|| album["images"][0]["url"].as_str())
        .unwrap_or("");
    let mut item = json!({"kind":kind,"ref":reference,"title":text(raw,"name"),"image":image,
        "artist":text(artist,"name"),"description":text(raw,"description")});
    if !text(artist, "id").is_empty() {
        item["artist_ref"] = json!(format!("artist:{}", text(artist, "id")));
    }
    if !text(album, "id").is_empty() {
        item["album_ref"] = json!(format!("album:{}", text(album, "id")));
        item["album"] = json!(text(album, "name"));
    }
    for (from, to) in [
        ("duration_ms", "duration_ms"),
        ("track_number", "track_number"),
        ("disc_number", "disc_number"),
        ("explicit", "explicit"),
        ("is_playable", "available"),
        ("total_tracks", "tracks"),
    ] {
        if let Some(v) = raw.get(from) {
            item[to] = v.clone();
        }
    }
    if raw["is_local"] == true || text(raw, "type") == "episode" {
        item["available"] = json!(false);
    }
    let date = raw["release_date"]
        .as_str()
        .or_else(|| album["release_date"].as_str())
        .unwrap_or("");
    if !date.is_empty() {
        item["release_date"] = json!(date);
        if let Some(year) = date.get(..4).and_then(|s| s.parse::<u32>().ok()) {
            item["year"] = json!(year);
        }
    }
    if let Some(isrc) = raw["external_ids"]["isrc"].as_str() {
        item["isrc"] = json!(isrc);
    }
    if kind == "track" {
        item["quality"] = json!({"codec":"vorbis"});
    }
    if kind == "playlist" {
        item["artist"] = json!(text(&raw["owner"], "display_name"));
        item["owner_id"] = raw["owner"]["id"].clone();
        item["editable"] = json!(false);
        item["copyable"] = json!(false);
    }
    Some(item)
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn rows(value: &Value, kind: &str) -> Vec<Value> {
    array(value).iter().filter_map(|v| card(v, kind)).collect()
}
fn search_rows(value: Value, kinds: &[&str]) -> Result<Value> {
    let mut result = json!({"totals":{}});
    for kind in kinds {
        let key = format!("{kind}s");
        let items = value[&key]["items"]
            .as_array()
            .context("Invalid search response.")?;
        result[&key] = json!(
            items
                .iter()
                .filter_map(|item| card(item, kind))
                .collect::<Vec<_>>()
        );
        result["totals"][&key] = value[&key]["total"].clone();
    }
    Ok(result)
}
fn paging(params: &Value) -> (usize, usize) {
    (
        params["offset"].as_u64().unwrap_or(0).min(100000) as usize,
        params["limit"].as_u64().unwrap_or(50).clamp(1, 500) as usize,
    )
}
fn radio_card(seed: &str, header: &Value) -> Value {
    json!({"kind":"playlist","ref":format!("radio:{seed}"),"title":"Radio",
        "description":header["title"],"image":header["image"],"editable":false,"copyable":false})
}

fn radio_entity(seed: &str, mut value: Value) -> Value {
    let card = radio_card(seed, &Value::Null);
    value["playlist"] = card.clone();
    value["header"] = card;
    value
}

fn collection_page(kind: &str, value: &Value) -> Value {
    let mut page = json!({"title":value[kind]["title"],"header":value[kind],"kind":"track",
        "items":value["tracks"],"tracks":value["tracks"],kind:value[kind]});
    if ["album", "playlist"].contains(&kind) {
        let mut shelves = value["shelves"].as_array().cloned().unwrap_or_default();
        shelves.insert(
            0,
            shelf(
                "tracks",
                "Tracks",
                "track",
                array(&value["tracks"]).to_vec(),
            ),
        );
        page["shelves"] = json!(shelves);
    }
    page
}

fn add_radio_navigation(seed: &str, kind: &str, value: &mut Value) {
    let card = radio_card(seed, &value[kind]);
    if !value["shelves"].is_array() {
        value["shelves"] = json!([]);
    }
    value["shelves"]
        .as_array_mut()
        .unwrap()
        .push(shelf("radio", "Radio", "playlist", vec![card]));
}

fn shelf(id: &str, title: &str, kind: &str, items: Vec<Value>) -> Value {
    json!({"id":id,"title":title,"kind":kind,"items":items})
}

impl Client {
    pub async fn search(&self, params: &Value) -> Result<Value> {
        let mut value = self.search_inner(params).await?;
        self.project_playlist_permissions(&mut value, "playlists")
            .await;
        Ok(value)
    }

    async fn project_playlist_permissions(&self, value: &mut Value, key: &str) {
        if let Some(items) = value[key].as_array_mut() {
            for item in items {
                if item["kind"] == "playlist" {
                    item["editable"] = json!(
                        self.playlist_editable(&json!({"owner":{"id":item["owner_id"]}}))
                            .await
                    );
                }
            }
        }
    }

    async fn search_inner(&self, params: &Value) -> Result<Value> {
        let query = text(params, "text").trim();
        if query.is_empty() {
            return Ok(json!({"tracks":[],"albums":[],"artists":[],"playlists":[]}));
        }
        if let Some(link) = link(query) {
            let reference = text(&link, "ref");
            let (kind, id) = reference.split_once(':').unwrap();
            let item = if kind == "playlist" && self.has_session().await {
                self.session_playlist_header(id).await?
            } else if kind == "playlist" {
                let raw = self
                    .get(
                        &format!("/playlists/{id}"),
                        &[("fields", "id,name,description,images,owner".into())],
                    )
                    .await?;
                let mut item = card(&raw, kind).context("Invalid linked playlist.")?;
                item["editable"] = json!(self.playlist_editable(&raw).await);
                item
            } else {
                let entity = self.entity(&json!({"ref":reference}), kind).await?;
                entity[kind].as_object().context("Invalid linked item.")?;
                entity[kind].clone()
            };
            let mut result = json!({"tracks":[],"albums":[],"artists":[],"playlists":[]});
            result[format!("{kind}s")] = json!([item]);
            return Ok(result);
        }
        let offset = params["offset"].as_u64().unwrap_or(0);
        if offset > 1000 {
            bail!("Search offset must be between 0 and 1000.");
        }
        let personal = self.primary_is_personal().await;
        let shared = self.has_shared_web().await;
        let limit = params["limit"]
            .as_u64()
            .unwrap_or(10)
            .clamp(1, if personal { 10 } else { 20 });
        let key = format!("search:v2:{personal}:{shared}:{query}:{offset}:{limit}");
        self.cached(&key, 60, false, || async {
            tokio::time::timeout(std::time::Duration::from_secs(24), async {
                let make_query = |kinds: &str| {
                    vec![
                        ("q", query.into()),
                        ("type", kinds.into()),
                        ("limit", limit.to_string()),
                        ("offset", offset.to_string()),
                    ]
                };
                let catalogue_query = make_query("track,album,artist");
                let playlist_query = make_query("playlist");
                let (catalogue, playlists) = tokio::join!(
                    async {
                        self.get_for_catalogue(
                            crate::web_api::WebSource::Primary,
                            "/search",
                            &catalogue_query,
                        )
                        .await
                        .and_then(|v| search_rows(v, &["track", "album", "artist"]))
                    },
                    async {
                        tokio::time::timeout(std::time::Duration::from_secs(4), async {
                            self.get_for_playlist_search(
                                if shared {
                                    crate::web_api::WebSource::Shared
                                } else {
                                    crate::web_api::WebSource::Primary
                                },
                                "/search",
                                &playlist_query,
                            )
                            .await
                            .and_then(|v| search_rows(v, &["playlist"]))
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("Playlist search took too long."))?
                    }
                );
                let mut result =
                    json!({"tracks":[],"albums":[],"artists":[],"playlists":[],"totals":{}});
                let mut successes = 0;
                let mut failure = None;
                for branch in [catalogue, playlists] {
                    match branch {
                        Ok(value) => {
                            successes += 1;
                            for (key, rows) in value.as_object().unwrap() {
                                if key == "totals" {
                                    for (kind, total) in rows.as_object().unwrap() {
                                        result["totals"][kind] = total.clone();
                                    }
                                } else {
                                    result[key] = rows.clone();
                                }
                            }
                        }
                        Err(error) => failure = Some(error),
                    }
                }
                self.set_search_notice(failure.as_ref().map(|_| {
                    "Some search categories could not be loaded. Available results are shown."
                        .to_owned()
                }))
                .await;
                if successes == 0 {
                    return Err(failure.unwrap());
                }
                if failure.is_some() {
                    result["partial"] = json!(true);
                }
                Ok(result)
            })
            .await
            .map_err(|_| anyhow::anyhow!("Search took too long. Try again."))?
        })
        .await
    }

    pub async fn library(&self, params: &Value) -> Result<Value> {
        let mut value = self.library_inner(params).await?;
        self.project_playlist_permissions(&mut value, "items").await;
        Ok(value)
    }

    async fn library_inner(&self, params: &Value) -> Result<Value> {
        let kind = text(params, "kind");
        let (offset, limit) = paging(params);
        self.cached(
            &format!("library:{kind}:{offset}:{limit}"),
            if kind == "playlists" { 1800 } else { 21600 },
            params["refresh"].as_bool().unwrap_or(false),
            || self.library_fetch(params),
        )
        .await
    }

    async fn library_fetch(&self, params: &Value) -> Result<Value> {
        let kind = text(params, "kind");
        let path = match kind {
            "tracks" => "/me/tracks",
            "albums" => "/me/albums",
            "playlists" => "/me/playlists",
            "artists" => "/me/following",
            _ => bail!("Unknown Spotify library category."),
        };
        let (offset, limit) = paging(params);
        if kind == "playlists" && self.has_session().await {
            return self
                .session_playlist_library(
                    offset,
                    limit,
                    params["refresh"].as_bool().unwrap_or(false),
                )
                .await;
        }
        let changed = if offset == 0 && params["refresh"].as_bool().unwrap_or(false) {
            self.refresh_library_head(path, kind).await?
        } else {
            false
        };
        let mut output = vec![];
        let mut position = if kind == "artists" { 0 } else { offset };
        let mut after = String::new();
        let total;
        loop {
            let count = 50;
            let mut query = vec![("limit", count.to_string())];
            if kind == "artists" {
                query.push(("type", "artist".into()));
                if !after.is_empty() {
                    query.push(("after", after.clone()));
                }
            } else {
                query.push(("offset", position.to_string()));
            }
            let value = self.get(path, &query).await?;
            let page = if kind == "artists" {
                &value["artists"]
            } else {
                &value
            };
            let batch = page["items"]
                .as_array()
                .context("Spotify returned an invalid library page.")?;
            for raw in batch {
                if position >= offset && output.len() < limit {
                    let raw = match kind {
                        "tracks" => &raw["track"],
                        "albums" => &raw["album"],
                        _ => raw,
                    };
                    if let Some(item) = card(raw, kind.trim_end_matches('s')) {
                        output.push(item);
                    }
                }
                position += 1;
            }
            let next = text(&page["cursors"], "after").to_string();
            if output.len() >= limit
                || batch.is_empty()
                || page["next"].is_null()
                || (kind == "artists" && (next.is_empty() || next == after))
            {
                total = page["total"].as_u64().unwrap_or(position as u64);
                break;
            }
            after = next;
            if position > 100000 {
                bail!("Spotify library exceeds this plugin's listing limit.");
            }
        }
        // I replace complete snapshots only after every requested page succeeds.
        // A new head followed by a 429 must not delete the last full library.
        if changed && !crate::snapshot::is_incomplete() {
            self.snapshots
                .invalidate(&self.identity().await?, &format!("library:{kind}:"));
        }
        Ok(json!({"items":output,"total":total}))
    }

    async fn refresh_library_head(&self, path: &str, kind: &str) -> Result<bool> {
        let mut query = vec![("limit", "50".into())];
        if kind == "artists" {
            query.push(("type", "artist".into()));
        } else {
            query.push(("offset", "0".into()));
        }
        let key = format!("web:Primary:0:{path}:{}", serde_json::to_string(&query)?);
        let identity = self.identity().await?;
        let previous = self.snapshots.peek(&identity, &key);
        let value = self
            .cached(&format!("library-head-probe:{kind}"), 60, false, || {
                self.api(Method::GET, path, &query)
            })
            .await?;
        if value["cached"] == true {
            return Ok(false);
        }
        let changed = previous.as_ref() != Some(&value);
        if changed {
            self.snapshots
                .invalidate(&identity, &format!("web:Primary:0:{path}:"));
            self.snapshots
                .invalidate(&identity, &format!("web:{path}:"));
        }
        self.snapshots.put(&identity, &key, value)?;
        Ok(changed)
    }

    pub async fn entity(&self, params: &Value, kind: &str) -> Result<Value> {
        let requested = text(params, "ref");
        if kind == "playlist" && requested.starts_with("radio:") {
            let seed = requested.strip_prefix("radio:").unwrap();
            let value = self
                .radio_page(requested, params["refresh"].as_bool().unwrap_or(false))
                .await?;
            return Ok(radio_entity(seed, value));
        }
        let (_, id) = reference(text(params, "ref"), Some(kind))?;
        if self.has_session().await {
            let mut value = self.session_entity(kind, &id, 0).await?;
            if kind == "playlist" {
                value["playlist"]["editable"] = json!(
                    self.playlist_editable(&json!({"owner":{"id":value["playlist"]["owner_id"]}}))
                        .await
                );
            }
            if ["artist", "album", "playlist"].contains(&kind) {
                add_radio_navigation(&format!("{kind}:{id}"), kind, &mut value);
            }
            return Ok(value);
        }
        self.web_entity(params, kind).await
    }

    async fn web_entity(&self, params: &Value, kind: &str) -> Result<Value> {
        let (_, id) = reference(text(params, "ref"), Some(kind))?;
        let raw = self.get(&format!("/{kind}s/{id}"), &[]).await?;
        let mut item = card(&raw, kind).context("Spotify returned an invalid item.")?;
        if kind == "playlist" {
            item["editable"] = json!(self.playlist_editable(&raw).await);
        }
        if kind == "track" {
            return Ok(json!({"track":item}));
        }
        if kind == "artist" {
            let albums = self.artist_releases(&id, 0).await?;
            let mut releases = shelf(
                "albums",
                "Albums and singles",
                "album",
                array(&albums["items"]).to_vec(),
            );
            releases["more"] = albums["next"].clone();
            return Ok(json!({"artist":item,"albums":albums["items"],"shelves":[releases]}));
        }
        let path = if kind == "album" {
            format!("/albums/{id}/tracks")
        } else {
            format!("/playlists/{id}/items")
        };
        let mut tracks = vec![];
        let mut offset = 0usize;
        // Get Album already includes its first track page. I keep it rather than
        // throwing it away and making the screen wait on a redundant request.
        let mut embedded = if kind == "album" {
            raw.get("tracks").cloned()
        } else {
            None
        };
        loop {
            let page = match embedded.take() {
                Some(page) => page,
                None => {
                    self.get(
                        &path,
                        &[("limit", "50".into()), ("offset", offset.to_string())],
                    )
                    .await?
                }
            };
            let batch = page["items"]
                .as_array()
                .context("Spotify returned an invalid track listing.")?;
            for entry in batch {
                let mut track = if kind == "playlist" {
                    entry
                        .get("item")
                        .or_else(|| entry.get("track"))
                        .unwrap_or(&Value::Null)
                        .clone()
                } else {
                    entry.clone()
                };
                if kind == "album" {
                    track["album"] = json!({"id":raw["id"],"name":raw["name"],
                        "images":raw["images"],"release_date":raw["release_date"]});
                }
                if let Some(mut row) = card(&track, "track") {
                    if kind == "playlist" {
                        row["ref"] = json!(format!(
                            "{}@playlist:{id}@entry:{offset}",
                            text(&row, "ref")
                        ));
                    }
                    tracks.push(row);
                } else if kind == "playlist" {
                    // I retain unavailable and local occurrences at their original position.
                    tracks.push(json!({"kind":"track","ref":format!("unavailable:{id}:{offset}"),
                        "title":track["name"].as_str().unwrap_or("Unavailable track"),"available":false}));
                }
                offset += 1;
            }
            if batch.is_empty() || page["next"].is_null() {
                break;
            }
            if offset >= 10000 {
                bail!("This collection exceeds the 10,000-track listing limit.");
            }
        }
        Ok(json!({kind:item,"tracks":tracks,"total":offset}))
    }

    async fn artist_releases(&self, id: &str, offset: usize) -> Result<Value> {
        super::state::reference(&format!("artist:{id}"), Some("artist"))?;
        if offset > 10000 {
            bail!("Invalid artist release offset.");
        }
        let page = self
            .get(
                &format!("/artists/{id}/albums"),
                &[("limit", "10".into()), ("offset", offset.to_string())],
            )
            .await?;
        let items = page["items"]
            .as_array()
            .context("Spotify returned an invalid artist album page.")?;
        let next = if !items.is_empty() && page["next"].is_string() {
            Some(format!("artist-albums:{id}:{}", offset + items.len()))
        } else {
            None
        };
        Ok(
            json!({"title":"Albums and singles","kind":"album","items":rows(&page["items"], "album"),"next":next}),
        )
    }

    pub async fn home(&self) -> Result<Value> {
        self.discover("discover:home", false).await
    }

    pub async fn page(&self, params: &Value) -> Result<Value> {
        let reference = text(params, "ref");
        if reference.starts_with("radio:") {
            let value = self
                .radio_page(reference, params["refresh"].as_bool().unwrap_or(false))
                .await?;
            return Ok(radio_entity(
                reference.strip_prefix("radio:").unwrap(),
                value,
            ));
        }
        if let Some(rest) = reference.strip_prefix("artist-albums:") {
            let (id, offset) = rest
                .split_once(':')
                .context("Invalid artist release page.")?;
            return self
                .artist_releases(
                    id,
                    offset.parse().context("Invalid artist release offset.")?,
                )
                .await;
        }
        if let Some(rest) = reference.strip_prefix("session-artist-albums:") {
            let (id, offset) = rest
                .split_once(':')
                .context("Invalid artist release page.")?;
            super::state::reference(&format!("artist:{id}"), Some("artist"))?;
            return self
                .session_entity(
                    "artist",
                    id,
                    offset.parse().context("Invalid artist release offset.")?,
                )
                .await;
        }
        if reference.starts_with("discover:")
            || reference.starts_with("discovery:")
            || reference.is_empty()
        {
            return self
                .discover(reference, params["refresh"].as_bool().unwrap_or(false))
                .await;
        }
        if let Some(kind) = reference.strip_prefix("library:") {
            let mut value = self.library(&json!({"kind":kind,"limit":100})).await?;
            value["title"] = json!(kind);
            return Ok(value);
        }
        let (kind, _) = super::state::reference(reference, None)?;
        let value = self.entity(params, &kind).await?;
        if kind == "artist" {
            return Ok(
                json!({"title":value["artist"]["title"],"header":value["artist"],"shelves":value["shelves"]}),
            );
        }
        Ok(collection_page(&kind, &value))
    }

    pub async fn favorite(&self, params: &Value) -> Result<Value> {
        let (kind, id) = reference(text(params, "ref"), None)?;
        if !["track", "album", "artist", "playlist"].contains(&kind.as_str()) {
            bail!("Unsupported favorite type.");
        }
        let on = params["on"]
            .as_bool()
            .context("Favorite state must be true or false.")?;
        self.api(
            if on { Method::PUT } else { Method::DELETE },
            "/me/library",
            &[("uris", format!("spotify:{kind}:{id}"))],
        )
        .await?;
        self.snapshots
            .invalidate(&self.identity().await?, "web:/me/");
        self.snapshots
            .invalidate(&self.identity().await?, "web:Primary:0:/me/");
        self.snapshots
            .invalidate(&self.identity().await?, "library:");
        self.snapshots
            .invalidate(&self.identity().await?, "library-head-probe:");
        self.snapshots
            .invalidate(&self.identity().await?, "discover:");
        self.snapshots
            .invalidate(&self.identity().await?, "session:rootlist");
        Ok(json!({"changed_refs":[params["ref"]]}))
    }
}

pub fn link(value: &str) -> Option<Value> {
    if value.chars().any(char::is_control) || value.contains('\\') {
        return None;
    }
    let normalized;
    let mut parts: Vec<&str>;
    if let Some(rest) = value
        .strip_prefix("spotify:")
        .filter(|_| !value.starts_with("spotify://"))
    {
        parts = rest.split(':').collect();
    } else {
        let url = url::Url::parse(value).ok()?;
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            return None;
        }
        if url.scheme() == "spotify" {
            let host = url.host_str()?;
            normalized = if host == "open.spotify.com" {
                url.path().trim_start_matches('/').to_owned()
            } else {
                format!("{host}{}", url.path())
            };
        } else {
            if !["https", "http"].contains(&url.scheme())
                || url.host_str() != Some("open.spotify.com")
            {
                return None;
            }
            normalized = url.path().trim_start_matches('/').to_owned();
        }
        parts = normalized.split('/').collect();
        if parts.first().is_some_and(|p| p.starts_with("intl-")) {
            parts.remove(0);
        }
    }
    if parts.len() == 4 && parts[0] == "user" && parts[2] == "playlist" && !parts[1].is_empty() {
        parts.drain(..2);
    }
    if parts.len() != 2 || !["track", "album", "artist", "playlist"].contains(&parts[0]) {
        return None;
    }
    let encoded = format!("id={}", parts[1].replace('+', "%2B"));
    let id = url::form_urlencoded::parse(encoded.as_bytes())
        .next()?
        .1
        .into_owned();
    let reference = format!("{}:{id}", parts[0]);
    super::state::reference(&reference, None).ok()?;
    Some(json!({"kind":parts[0],"ref":reference}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn independent_searches_keep_valid_results_when_one_category_fails() {
        let body = json!({"tracks":{"items":[{"id":"1234567890123456789012","name":"Track"}]},"albums":{"items":[]},"artists":{"items":[]},"playlists":{"items":[{"id":"1234567890123456789012","name":"List"}]}}).to_string();
        let (client, _directory, worker) = crate::fixtures::routed(vec![
            ("/search?q=artist:Example", 200, body),
            ("/search?q=artist:Example", 403, "{}".into()),
        ])
        .await;
        let result = client
            .search(&json!({"text":"artist:Example","limit":50}))
            .await
            .unwrap();
        assert_eq!(result["partial"], true);
        assert!(
            result["tracks"].as_array().unwrap().len()
                + result["playlists"].as_array().unwrap().len()
                > 0
        );
        let paths = worker.join().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| !p.contains("limit=50")));
        assert!(paths.iter().any(|p| p.contains("type=playlist")));
        assert!(
            paths
                .iter()
                .any(|p| p.contains("type=track%2Calbum%2Cartist"))
        );
    }

    #[tokio::test]
    async fn search_failures_and_invalid_offsets_remain_terminal() {
        let (client, _directory, worker) = crate::fixtures::routed(vec![
            ("/search?q=Example", 403, "{}".into()),
            ("/search?q=Example", 403, "{}".into()),
        ])
        .await;
        assert!(
            client
                .search(&json!({"text":"Example","offset":1001}))
                .await
                .is_err()
        );
        assert!(client.search(&json!({"text":"Example"})).await.is_err());
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[test]
    fn generic_collection_pages_keep_tracks_visible_beside_radio() {
        let rows = json!([{"kind":"track","ref":"track:1234567890123456789012"}]);
        for kind in ["album", "playlist"] {
            let mut entity = json!({kind:{"title":"Collection"},"tracks":rows});
            add_radio_navigation(&format!("{kind}:1234567890123456789012"), kind, &mut entity);
            let page = collection_page(kind, &entity);
            assert_eq!(page["shelves"][0]["id"], "tracks");
            assert_eq!(page["shelves"][0]["items"], rows);
            assert_eq!(page["shelves"][1]["id"], "radio");
            assert_eq!(page["tracks"], rows);
        }
        assert!(
            collection_page("track", &json!({"track":{"title":"Track"}}))
                .get("shelves")
                .is_none()
        );
    }

    #[test]
    fn radio_navigation_uses_the_native_header_action_and_playlist_projection() {
        let seed = "artist:1234567890123456789012";
        let mut artist = json!({"artist":{"title":"Artist","image":"cover"},"shelves":[]});
        add_radio_navigation(seed, "artist", &mut artist);
        let navigation = &artist["shelves"][0]["items"][0];
        assert_eq!(navigation["kind"], "playlist");
        assert_eq!(navigation["ref"], format!("radio:{seed}"));
        let rows = json!([{"ref":"track:1234567890123456789012@radio:artist:1234567890123456789012@entry:0"}]);
        let projected = radio_entity(seed, json!({"items":rows,"tracks":rows}));
        assert_eq!(projected["playlist"]["ref"], navigation["ref"]);
        assert_eq!(projected["playlist"], projected["header"]);
        assert_eq!(projected["tracks"], rows);
    }

    #[test]
    fn canonical_music_links_preserve_identity_and_reject_ambiguous_paths() {
        let id = "1234567890123456789012";
        for value in [
            format!("spotify://track/{id}"),
            format!("spotify:user:owner:playlist:{id}"),
            format!("https://open.spotify.com/user/owner/playlist/{id}"),
        ] {
            assert!(link(&value).is_some(), "{value}");
        }
        for value in [
            format!("https://owner@open.spotify.com/track/{id}"),
            format!("spotify://track/{id}/extra"),
            format!("spotify:track:{id}\n"),
            format!("https://open.spotify.com/track/%252F{id}"),
        ] {
            assert!(link(&value).is_none(), "{value}");
        }
        assert!(search_rows(json!({}), &["track"]).is_err());
        assert!(search_rows(json!({"tracks":{"items":[]}}), &["track"]).is_ok());
    }

    #[test]
    fn empty_and_malformed_cards_do_not_become_playable_tracks() {
        assert!(card(&Value::Null, "track").is_none());
        assert!(card(&json!({"id":"invalid"}), "track").is_none());
        let row = card(
            &json!({"id":"1234567890123456789012","name":"Song","is_playable":false}),
            "track",
        )
        .unwrap();
        assert_eq!(row["available"], false);
        assert!(row["quality"].get("bits").is_none());
    }
    #[test]
    fn links_cannot_escape_spotify() {
        assert!(link("https://open.spotify.com.evil/track/1234567890123456789012").is_none());
        assert!(link("https://open.spotify.com/track/../../etc").is_none());
        assert_eq!(
            link("https://open.spotify.com/intl-es/album/1234567890123456789012?si=test").unwrap()
                ["ref"],
            "album:1234567890123456789012"
        );
    }

    #[tokio::test]
    async fn library_follows_offsets_and_distinguishes_empty_from_malformed() {
        let item = json!({"album":{"id":"1234567890123456789012","name":"Record"}});
        let first = json!({"items":vec![item.clone();50],"total":51,"next":"ignored-provider-url"});
        let second = json!({"items":[item],"total":51,"next":null});
        let (client, _directory, worker) = crate::fixtures::client(vec![
            (200, first.to_string()),
            (200, second.to_string()),
            (200, json!({"items":[],"total":0,"next":null}).to_string()),
            (200, "{}".into()),
        ])
        .await;
        let result = client
            .library(&json!({"kind":"albums","limit":100}))
            .await
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 51);
        assert_eq!(result["total"], 51);
        client.account.lock().await.user = "empty-account".into();
        assert_eq!(
            client.library(&json!({"kind":"albums"})).await.unwrap()["total"],
            0
        );
        client.account.lock().await.user = "malformed-account".into();
        assert!(client.library(&json!({"kind":"albums"})).await.is_err());
        let paths = worker.join().unwrap();
        assert!(paths[0].contains("offset=0"));
        assert!(paths[1].contains("offset=50"));
        assert!(paths.iter().all(|p| p.starts_with("/me/albums?")));
    }

    #[tokio::test]
    async fn five_hundred_library_rows_survive_polling_restart_and_an_unchanged_refresh() {
        fn page(offset: usize) -> Value {
            let items: Vec<_> = (offset..offset + 50)
                .map(|i| json!({"track":{"id":format!("{i:022}"),"name":format!("Song {i}")}}))
                .collect();
            json!({"items":items,"total":500,"next":if offset < 450 { json!("ignored") } else { Value::Null }})
        }
        let mut replies: Vec<_> = (0..500)
            .step_by(50)
            .map(|offset| (200, page(offset).to_string()))
            .collect();
        replies.push((200, page(0).to_string()));
        let (client, directory, worker) = crate::fixtures::client(replies).await;
        let params = json!({"kind":"tracks","limit":500});
        let (a, b) = tokio::join!(client.library(&params), client.library(&params));
        assert_eq!(a.as_ref().unwrap()["items"].as_array().unwrap().len(), 500);
        assert_eq!(a.unwrap(), b.unwrap());
        crate::state::save(
            &directory.path().join("data/account.json"),
            &*client.account.lock().await,
        )
        .unwrap();
        let mut restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        restarted.api_base = client.api_base.clone();
        assert_eq!(restarted.library(&params).await.unwrap()["total"], 500);
        for file in std::fs::read_dir(directory.path().join("cache/snapshots"))
            .unwrap()
            .flatten()
        {
            let mut value: Value =
                serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
            if value["key"].as_str().unwrap().starts_with("library:") {
                value["at"] = json!(crate::state::now() - 61);
                crate::state::save_json(&file.path(), &value).unwrap();
            }
        }
        assert_eq!(
            restarted
                .library(&json!({"kind":"tracks","limit":500,"refresh":true}))
                .await
                .unwrap()["total"],
            500
        );
        let paths = worker.join().unwrap();
        assert_eq!(
            paths.len(),
            11,
            "ten cold pages, zero for polling/restart, one unchanged head probe"
        );
        assert!(paths.last().unwrap().contains("offset=0"));
    }

    #[tokio::test]
    async fn a_changed_head_followed_by_quota_failure_preserves_the_last_complete_snapshot() {
        let row = json!({"track":{"id":"1234567890123456789012","name":"Song"}});
        let first = json!({"items":vec![row.clone();50],"total":100,"next":"ignored"});
        let last = json!({"items":vec![row;50],"total":100,"next":null});
        let mut changed = first.clone();
        changed["total"] = json!(101);
        let (client, directory, worker) = crate::fixtures::client(vec![
            (200, first.to_string()),
            (200, last.to_string()),
            (200, changed.to_string()),
            (429, "{}".into()),
        ])
        .await;
        let params = json!({"kind":"tracks","limit":100});
        assert_eq!(
            client.library(&params).await.unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        for file in std::fs::read_dir(directory.path().join("cache/snapshots"))
            .unwrap()
            .flatten()
        {
            let mut value: Value =
                serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
            if value["key"].as_str().unwrap().starts_with("library:") {
                value["at"] = json!(crate::state::now() - 61);
                crate::state::save_json(&file.path(), &value).unwrap();
            }
        }
        let stale = client
            .library(&json!({"kind":"tracks","limit":100,"refresh":true}))
            .await
            .unwrap();
        assert_eq!(stale["items"].as_array().unwrap().len(), 100);
        assert_eq!(stale["cached"], true);
        crate::state::save(
            &directory.path().join("data/account.json"),
            &*client.account.lock().await,
        )
        .unwrap();
        let restarted = Client::new(
            directory.path().join("data"),
            directory.path().join("cache"),
        )
        .unwrap();
        assert_eq!(
            restarted.library(&params).await.unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        assert_eq!(worker.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn album_tracks_keep_embedded_pages_order_and_navigation_context() {
        let track = |number| json!({"id":"1234567890123456789012","name":"Placeholder","track_number":number,"duration_ms":1000});
        let album = json!({"id":"abcdefghijklmnopqrstuv","name":"Record","images":[{"url":"https://i.scdn.co/cover"}],
            "tracks":{"items":[track(1),track(2)],"next":"https://ignored.example/next"}});
        let (client, _directory, worker) = crate::fixtures::client(vec![
            (200, album.to_string()),
            (200, json!({"items":[track(3)],"next":null}).to_string()),
        ])
        .await;
        let value = client
            .page(&json!({"ref":"album:abcdefghijklmnopqrstuv"}))
            .await
            .unwrap();
        assert_eq!(value["items"], value["tracks"]);
        assert_eq!(value["header"]["title"], "Record");
        let rows = value["tracks"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["track_number"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(
            rows.iter()
                .all(|r| r["album_ref"] == "album:abcdefghijklmnopqrstuv"
                    && r["image"] == "https://i.scdn.co/cover")
        );
        let paths = worker.join().unwrap();
        assert_eq!(
            paths.len(),
            2,
            "the embedded first page needs no second request"
        );
        assert!(paths[1].contains("offset=2"));
        assert!(paths.iter().all(|p| !p.contains("ignored.example")));
    }

    #[tokio::test]
    async fn artist_releases_respect_the_ten_item_limit_and_offer_load_more() {
        let artist = json!({"id":"abcdefghijklmnopqrstuv","name":"Artist"});
        let album = json!({"id":"1234567890123456789012","name":"Record"});
        let (client, _directory, worker) = crate::fixtures::client(vec![
            (200, artist.to_string()),
            (
                200,
                json!({"items":vec![album.clone();10],"next":"ignored-provider-url"}).to_string(),
            ),
            (200, json!({"items":[album],"next":null}).to_string()),
        ])
        .await;
        let page = client
            .page(&json!({"ref":"artist:abcdefghijklmnopqrstuv"}))
            .await
            .unwrap();
        assert_eq!(page["shelves"][0]["items"].as_array().unwrap().len(), 10);
        assert_eq!(
            page["shelves"][0]["more"],
            "artist-albums:abcdefghijklmnopqrstuv:10"
        );
        let more = client
            .page(&json!({"ref":page["shelves"][0]["more"]}))
            .await
            .unwrap();
        assert_eq!(more["items"].as_array().unwrap().len(), 1);
        assert!(more["next"].is_null());
        let paths = worker.join().unwrap();
        assert!(paths[1].contains("limit=10") && paths[1].contains("offset=0"));
        assert!(paths[2].contains("offset=10"));
        assert!(paths.iter().all(|p| !p.contains("top-tracks")));
    }

    #[tokio::test]
    async fn playlist_duplicates_and_unavailable_occurrences_keep_their_positions() {
        let track = json!({"id":"1234567890123456789012","type":"track","name":"Song",
            "album":{"id":"zyxwvutsrqponmlkjihgfe","name":"Record"},
            "artists":[{"id":"1234567890123456789012","name":"Artist"}]});
        let header = json!({"id":"abcdefghijklmnopqrstuv","name":"List"});
        let page = json!({"items":[{"item":track},{"item":track},{"item":null},
            {"item":{"name":"Local","is_local":true}}],"next":null});
        let (client, _directory, worker) =
            crate::fixtures::client(vec![(200, header.to_string()), (200, page.to_string())]).await;
        let result = client
            .entity(
                &json!({"ref":"playlist:abcdefghijklmnopqrstuv"}),
                "playlist",
            )
            .await
            .unwrap();
        assert_eq!(result["playlist"]["title"], "List");
        let tracks = result["tracks"].as_array().unwrap();
        assert_eq!(tracks.len(), 4);
        assert_ne!(tracks[0]["ref"], tracks[1]["ref"]);
        for row in &tracks[..2] {
            assert_eq!(
                row["album_ref"], "album:zyxwvutsrqponmlkjihgfe",
                "the NPB album link must keep the track's album, not its playlist"
            );
            assert_eq!(row["artist_ref"], "artist:1234567890123456789012");
        }
        assert_eq!(tracks[2]["available"], false);
        assert_eq!(tracks[3]["available"], false);
        assert_eq!(tracks[3]["title"], "Local");
        worker.join().unwrap();
    }
}
