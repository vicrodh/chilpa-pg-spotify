// I adapt Spotifast's session_reads and rootlist readers (MIT, Carmine Paolino).
use crate::{
    auth::{ApiFailure, Client},
    catalog::card,
    native_catalog::{batch, track_card},
};
use anyhow::{Context, Result, ensure};
use librespot_core::{Session, SpotifyUri, error::ErrorKind};
use librespot_metadata::Playlist;
use librespot_protocol::{
    extension_kind::ExtensionKind, metadata, playlist4_external::SelectedListContent,
};
use protobuf::Message;
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

impl Client {
    async fn playlist_header(&self, id: &str, list: &Playlist) -> Result<Value> {
        let mut value = header(id, list)?;
        let owner = match &list.id {
            SpotifyUri::Playlist {
                user: Some(user), ..
            } => user.as_str(),
            _ => "",
        };
        value["owner_id"] = json!(owner);
        value["editable"] = json!(self.playlist_editable(&json!({"owner":{"id":owner}})).await);
        Ok(value)
    }

    pub async fn session_playlist_header(&self, id: &str) -> Result<Value> {
        let mut value = self
            .cached(
                &format!("session:playlist-header:{id}"),
                1800,
                false,
                || async {
                    let session = self.session().await?;
                    let uri = SpotifyUri::from_uri(&format!("spotify:playlist:{id}"))?;
                    self.playlist_header(id, &window(&session, &uri, 0, 0).await?)
                        .await
                },
            )
            .await?;
        value["editable"] = json!(
            self.playlist_editable(&json!({"owner":{"id":value["owner_id"]}}))
                .await
        );
        Ok(value)
    }

    pub async fn session_featured(&self) -> Result<Vec<Value>> {
        // These are public Spotify editorial contexts, not personal mixes.
        // I resolve their actual names and images instead of inventing cards.
        let (new, finds, charts) = tokio::join!(
            self.session_playlist_header("37i9dQZF1DX4JAvHpjipBk"),
            self.session_playlist_header("37i9dQZF1DWWjGdmeTyeJ6"),
            self.session_playlist_header("37i9dQZEVXbMDoHDwVN2tF")
        );
        let mut cards = vec![];
        let mut error = None;
        for result in [new, finds, charts] {
            match result {
                Ok(card) => cards.push(card),
                Err(e) => {
                    crate::snapshot::mark_incomplete();
                    error = Some(e);
                }
            }
        }
        if cards.is_empty() {
            return Err(error.context("Spotify returned no featured playlists.")?);
        }
        Ok(cards)
    }

    pub async fn session_playlist(&self, id: &str) -> Result<Value> {
        let session = self.session().await?;
        let uri = SpotifyUri::from_uri(&format!("spotify:playlist:{id}"))?;
        let mut tracks = vec![];
        let mut first: Option<Playlist> = None;
        loop {
            let list = window(&session, &uri, tracks.len(), 500).await?;
            if let Some(first) = &first {
                ensure!(
                    first.revision == list.revision && first.length == list.length,
                    "The playlist changed while loading. Open it again."
                );
            }
            let total = usize::try_from(list.length)
                .context("Spotify returned an invalid playlist length.")?;
            ensure!(
                total <= 10000,
                "This playlist exceeds the 10,000-track limit."
            );
            let rows = range(
                list.contents.items.len(),
                list.contents.position,
                tracks.len(),
                500,
            )?;
            let rows = &list.contents.items[rows];
            ensure!(
                rows.len() == (total.saturating_sub(tracks.len())).min(500),
                "Spotify returned an incomplete playlist."
            );
            let uris: Vec<_> = rows
                .iter()
                .filter(|row| matches!(row.id, SpotifyUri::Track { .. }))
                .map(|r| r.id.clone())
                .collect();
            let found = batch(&session, ExtensionKind::TRACK_V4, &uris).await?;
            for row in rows {
                let mut value = if let Some(bytes) = found.get(&row.id.to_uri()) {
                    track_card(
                        &metadata::Track::parse_from_bytes(bytes)
                            .context("Spotify returned an invalid track.")?,
                    )?
                } else {
                    json!({"kind":"track","title":"Unavailable item","available":false})
                };
                occurrence(&mut value, &row.id.to_uri(), id, tracks.len());
                tracks.push(value);
            }
            if first.is_none() {
                first = Some(list);
            }
            if tracks.len() >= total {
                break;
            }
        }
        let header = self
            .playlist_header(id, &first.context("Spotify did not return the playlist.")?)
            .await?;
        Ok(json!({"playlist":header,"total":tracks.len(),"tracks":tracks}))
    }

    pub async fn session_playlist_library(
        &self,
        offset: usize,
        limit: usize,
        refresh: bool,
    ) -> Result<Value> {
        let root = self
            .cached("session:rootlist", 1800, refresh, || async {
                let session = self.session().await?;
                let mut ids = vec![];
                let mut seen = HashSet::new();
                let mut position = 0;
                loop {
                    let bytes = session
                        .spclient()
                        .get_rootlist(position, Some(500))
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "Your playlists could not be read through the playback session."
                            )
                        })?;
                    let list = SelectedListContent::parse_from_bytes(&bytes)?;
                    let contents = list
                        .contents
                        .as_ref()
                        .context("Spotify did not return your playlists.")?;
                    let count = contents.items.len();
                    for row in &contents.items {
                        if let Ok(SpotifyUri::Playlist { id, .. }) = SpotifyUri::from_uri(row.uri())
                        {
                            if seen.insert(id.to_base62()) {
                                ids.push(id.to_base62());
                            }
                        }
                    }
                    if !contents.truncated() {
                        break;
                    }
                    ensure!(
                        count > 0 && position + count <= 100000,
                        "Spotify returned an incomplete playlist library."
                    );
                    position += count;
                }
                Ok(json!({"ids":ids}))
            })
            .await?;
        let ids = root["ids"]
            .as_array()
            .context("The saved playlist copy is invalid.")?;
        let selected: Vec<_> = ids
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(Value::as_str)
            .collect();
        let mut items = vec![];
        // Four independent header reads at a time; a large library never starts
        // hundreds of requests together, and each completed header survives restart.
        for group in selected.chunks(4) {
            let load = |index: usize| async move {
                let Some(id) = group.get(index) else {
                    return Ok(None);
                };
                let value = self
                    .cached(
                        &format!("session:playlist-header:{id}"),
                        1800,
                        refresh,
                        || async {
                            let session = self.session().await?;
                            let uri = SpotifyUri::from_uri(&format!("spotify:playlist:{id}"))?;
                            self.playlist_header(id, &window(&session, &uri, 0, 0).await?)
                                .await
                        },
                    )
                    .await;
                match value {
                    Ok(value) => Ok(Some(value)),
                    Err(e)
                        if e.downcast_ref::<ApiFailure>()
                            .is_some_and(|e| matches!(e.status, 403 | 404)) =>
                    {
                        Ok(Some(
                            json!({"kind":"playlist","ref":format!("playlist:{id}"),"title":"Unavailable playlist","available":false}),
                        ))
                    }
                    Err(e) => Err(e),
                }
            };
            let (a, b, c, d): (Result<Option<Value>>, _, _, _) =
                tokio::join!(load(0), load(1), load(2), load(3));
            for value in [a, b, c, d] {
                if let Some(mut value) = value? {
                    value["editable"] = json!(
                        self.playlist_editable(&json!({"owner":{"id":value["owner_id"]}}))
                            .await
                    );
                    items.push(value);
                }
            }
        }
        Ok(json!({"items":items,"total":ids.len()}))
    }
}

async fn window(
    session: &Session,
    uri: &SpotifyUri,
    offset: usize,
    length: usize,
) -> Result<Playlist> {
    tokio::time::timeout(
        Duration::from_secs(20),
        Playlist::get_range(session, uri, offset, length),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Spotify took too long to read the playlist."))?
    .map_err(|error| match error.kind {
        ErrorKind::NotFound => ApiFailure {
            status: 404,
            wait: 0,
        }
        .into(),
        ErrorKind::PermissionDenied => ApiFailure {
            status: 403,
            wait: 0,
        }
        .into(),
        _ => anyhow::anyhow!("The playlist could not be read through the playback session."),
    })
}

fn range(len: usize, position: i32, offset: usize, limit: usize) -> Result<std::ops::Range<usize>> {
    let position =
        usize::try_from(position).context("Spotify returned an invalid playlist position.")?;
    ensure!(position <= offset, "Spotify skipped playlist tracks.");
    let start = (offset - position).min(len);
    Ok(start..start.saturating_add(limit).min(len))
}

fn occurrence(value: &mut Value, uri: &str, playlist: &str, position: usize) {
    value["ref"] = json!(
        if matches!(SpotifyUri::from_uri(uri), Ok(SpotifyUri::Track { .. })) {
            format!(
                "{}@playlist:{playlist}@entry:{position}",
                uri.trim_start_matches("spotify:")
            )
        } else {
            format!("unavailable:{playlist}:{position}")
        }
    );
}

fn header(id: &str, list: &Playlist) -> Result<Value> {
    let owner = match &list.id {
        SpotifyUri::Playlist {
            user: Some(user), ..
        } => user.as_str(),
        _ => "",
    };
    let mut images = vec![];
    for picture in list.attributes.picture_sizes.iter() {
        let url = if picture.url.starts_with("https://") {
            picture.url.clone()
        } else if let Some(hex) = picture.url.strip_prefix("spotify:image:") {
            format!("https://i.scdn.co/image/{hex}")
        } else {
            continue;
        };
        images.push(json!({"url":url}));
    }
    if images.is_empty() && list.attributes.picture.len() == 20 {
        images.push(json!({"url":format!("https://i.scdn.co/image/{}",librespot_core::FileId::from_raw(&list.attributes.picture))}));
    }
    card(&json!({"id":id,"name":list.attributes.name,"description":list.attributes.description,"images":images,
        "owner":{"display_name":owner},"tracks":{"total":list.length}}),"playlist").context("Spotify returned an invalid playlist.")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playlist_windows_keep_occurrences_and_do_not_skip_missing_ranges() {
        assert_eq!(range(100, 0, 50, 50).unwrap(), 50..100);
        assert_eq!(range(50, 50, 50, 50).unwrap(), 0..50);
        assert!(range(50, 51, 50, 50).is_err());
        let mut a = json!({"album_ref":"album:1234567890123456789012"});
        let mut b = a.clone();
        occurrence(&mut a, "spotify:track:1234567890123456789012", "list", 0);
        occurrence(&mut b, "spotify:track:1234567890123456789012", "list", 1);
        assert_ne!(a["ref"], b["ref"]);
        assert_eq!(a["album_ref"], b["album_ref"]);
    }
}
