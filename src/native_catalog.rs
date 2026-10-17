// I adapt Spotifast's bounded session metadata batches; see licenses/Spotifast-MIT.txt.
use crate::{auth::Client, catalog::card};
use anyhow::{Context, Result, bail};
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri};
use librespot_metadata::{Artist, Metadata};
use librespot_protocol::{
    extended_metadata::{
        BatchedEntityRequest, BatchedExtensionResponse, EntityRequest, ExtensionQuery,
    },
    extension_kind::ExtensionKind,
    metadata,
};
use protobuf::{EnumOrUnknown, Message};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

impl Client {
    pub async fn session_entity(&self, kind: &str, id: &str, offset: usize) -> Result<Value> {
        self.cached(
            &format!("session:v2:{kind}:{id}:{offset}"),
            if kind == "playlist" { 600 } else { 3600 },
            false,
            || self.session_entity_fetch(kind, id, offset),
        )
        .await
    }

    async fn session_entity_fetch(&self, kind: &str, id: &str, offset: usize) -> Result<Value> {
        let work = async {
            let session = self.session().await?;
            let uri = SpotifyUri::from_uri(&format!("spotify:{kind}:{id}"))?;
            if kind == "playlist" {
                return self.session_playlist(id).await;
            }
            if kind == "track" {
                let replies = batch(&session, ExtensionKind::TRACK_V4, &[uri.clone()]).await?;
                let bytes = replies
                    .get(&uri.to_uri())
                    .context("Spotify did not return this track.")?;
                return Ok(
                    json!({"track":track_card(&metadata::Track::parse_from_bytes(bytes)?)?}),
                );
            }
            if kind == "artist" {
                if offset > 10000 {
                    bail!("Invalid artist release offset.");
                }
                let artist = Artist::get(&session, &uri).await.map_err(|_| {
                    anyhow::anyhow!("The artist could not be read through the playback session.")
                })?;
                let mut seen = BTreeSet::new();
                let releases: Vec<_> = artist
                    .albums
                    .current_releases()
                    .chain(artist.singles.current_releases())
                    .chain(artist.compilations.current_releases())
                    .filter(|uri| seen.insert(uri.to_uri()))
                    .cloned()
                    .collect();
                let selected: Vec<_> = releases.iter().skip(offset).take(10).cloned().collect();
                let replies = batch(&session, ExtensionKind::ALBUM_V4, &selected).await?;
                let mut items = vec![];
                for uri in selected {
                    if let Some(bytes) = replies.get(&uri.to_uri()) {
                        let album = metadata::Album::parse_from_bytes(bytes)
                            .context("Spotify returned invalid album metadata.")?;
                        items.push(
                            card(&album_json(&album)?, "album")
                                .context("Spotify returned an invalid album.")?,
                        );
                    }
                }
                let image = artist
                    .portraits
                    .iter()
                    .max_by_key(|i| i.width)
                    .map(|i| format!("https://i.scdn.co/image/{}", i.id))
                    .unwrap_or_default();
                let header = json!({"kind":"artist","ref":format!("artist:{id}"),"title":artist.name,"image":image});
                let next = (offset + 10 < releases.len())
                    .then(|| format!("session-artist-albums:{id}:{}", offset + 10));
                let mut shelves = vec![];
                if offset == 0 {
                    if let Some(ranking) = artist
                        .top_tracks
                        .iter()
                        .find(|t| t.country == session.country())
                        .or_else(|| artist.top_tracks.iter().find(|t| t.country == "ZZ"))
                    {
                        let uris: Vec<_> = ranking.tracks.iter().take(10).cloned().collect();
                        if let Ok(found) = batch(&session, ExtensionKind::TRACK_V4, &uris).await {
                            let mut popular = vec![];
                            let mut seen_tracks = BTreeSet::new();
                            for uri in uris {
                                if !seen_tracks.insert(uri.to_uri()) {
                                    continue;
                                }
                                if let Some(bytes) = found.get(&uri.to_uri()) {
                                    let track = metadata::Track::parse_from_bytes(bytes)?;
                                    if track.artist.iter().any(|artist| {
                                        spotify_id(artist.gid()).is_ok_and(|found| found == id)
                                    }) {
                                        popular.push(track_card(&track)?);
                                    }
                                }
                            }
                            if !popular.is_empty() {
                                shelves.push(json!({"id":"tracks","title":"Popular tracks","kind":"track","items":popular}));
                            }
                        } else {
                            crate::snapshot::mark_incomplete();
                        }
                    }
                }
                shelves.push(json!({"id":"albums","title":"Albums and singles","kind":"album","items":items,"more":next}));
                return Ok(json!({"artist":header,"albums":items,"shelves":shelves,
                    "title":"Albums and singles","kind":"album","items":items,"next":next}));
            }
            let bytes = session
                .spclient()
                .get_album_metadata(&uri)
                .await
                .map_err(|_| {
                    anyhow::anyhow!("The album could not be read through the playback session.")
                })?;
            let album = metadata::Album::parse_from_bytes(&bytes)
                .context("Spotify returned invalid album metadata.")?;
            let raw = album_json(&album)?;
            let mut header = card(&raw, "album").context("Spotify returned an invalid album.")?;
            header["ref"] = json!(format!("album:{id}"));
            let uris: Vec<_> = album
                .disc
                .iter()
                .flat_map(|d| &d.track)
                .map(|t| {
                    Ok(SpotifyUri::Track {
                        id: SpotifyId::from_raw(t.gid())
                            .context("Spotify returned an invalid track reference.")?,
                    })
                })
                .collect::<Result<_>>()?;
            if uris.len() > 10000 {
                bail!("This album exceeds the 10,000-track limit.");
            }
            let replies = batch(&session, ExtensionKind::TRACK_V4, &uris).await?;
            let mut tracks = vec![];
            for uri in &uris {
                let reference = uri.to_uri().replacen("spotify:", "", 1);
                if let Some(bytes) = replies.get(&uri.to_uri()) {
                    let track = metadata::Track::parse_from_bytes(bytes)
                        .context("Spotify returned invalid track metadata.")?;
                    let mut row = track_card(&track)?;
                    row["album"] = header["title"].clone();
                    row["ref"] = json!(reference);
                    row["album_ref"] = header["ref"].clone();
                    tracks.push(row);
                } else {
                    tracks.push(json!({"kind":"track","ref":reference,"title":"Unavailable track","available":false,"album_ref":header["ref"],"album":header["title"]}));
                }
            }
            Ok(json!({"album":header,"tracks":tracks,"total":uris.len()}))
        };
        tokio::time::timeout(Duration::from_secs(25), work)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Spotify took too long to read metadata through the playback session."
                )
            })?
    }
}

fn spotify_id(gid: &[u8]) -> Result<String> {
    SpotifyId::from_raw(gid)
        .map(|id| id.to_base62())
        .context("Spotify returned an invalid identity.")
}
fn artists(raw: &[metadata::Artist]) -> Vec<Value> {
    raw.iter()
        .filter_map(|a| Some(json!({"id":spotify_id(a.gid()).ok()?,"name":a.name()})))
        .collect()
}
fn album_json(album: &metadata::Album) -> Result<Value> {
    let images: Vec<_> = album.cover_group.image.iter().map(|i| json!({"url":format!("https://i.scdn.co/image/{}",FileId::from(i)),"width":i.width(),"height":i.height()})).collect();
    let date = album.date.get_or_default();
    let released = if date.has_year() {
        format!("{:04}", date.year())
    } else {
        String::new()
    };
    Ok(
        json!({"id":spotify_id(album.gid())?,"name":album.name(),"artists":artists(&album.artist),"images":images,
        "release_date":released,"total_tracks":album.disc.iter().map(|d|d.track.len()).sum::<usize>()}),
    )
}

pub(crate) async fn batch(
    session: &Session,
    kind: ExtensionKind,
    uris: &[SpotifyUri],
) -> Result<HashMap<String, Vec<u8>>> {
    let unique: BTreeSet<_> = uris.iter().map(SpotifyUri::to_uri).collect();
    let unique: Vec<_> = unique.into_iter().collect();
    let mut result = HashMap::new();
    for group in unique.chunks(50) {
        let request = BatchedEntityRequest {
            entity_request: group
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
        let response = session
            .spclient()
            .get_extended_metadata(request)
            .await
            .map_err(|_| {
                anyhow::anyhow!("Metadata could not be read through the playback session.")
            })?;
        result.extend(answers(group, kind, response)?);
    }
    Ok(result)
}

pub(crate) fn track_card(track: &metadata::Track) -> Result<Value> {
    let isrc = track
        .external_id
        .iter()
        .find(|id| id.type_().eq_ignore_ascii_case("isrc"))
        .map(|id| id.id());
    card(&json!({"external_ids":{"isrc":isrc},"id":spotify_id(track.gid())?,"name":track.name(),"type":"track","artists":artists(&track.artist),
        "album":album_json(track.album.get_or_default())?,"duration_ms":track.duration().max(0),
        "disc_number":track.disc_number().max(0),"track_number":track.number().max(0),"explicit":track.explicit()}),"track")
        .context("Spotify returned an invalid track.")
}

fn answers(
    asked: &[String],
    kind: ExtensionKind,
    response: BatchedExtensionResponse,
) -> Result<HashMap<String, Vec<u8>>> {
    let mut missing: BTreeSet<_> = asked.iter().cloned().collect();
    let mut rows = HashMap::new();
    for array in response.extended_metadata {
        if array.extension_kind.enum_value().ok() != Some(kind) {
            continue;
        }
        if !matches!(array.header.provider_error_status, 0 | 200) {
            bail!("Spotify rejected the catalog metadata batch.");
        }
        for data in array.extension_data {
            if !missing.remove(&data.entity_uri) {
                bail!("Spotify returned an unexpected catalog response.");
            }
            match data.header.status_code {
                0 | 200 => {
                    rows.insert(
                        data.entity_uri,
                        data.extension_data
                            .into_option()
                            .context("Spotify returned incomplete catalog metadata.")?
                            .value,
                    );
                }
                404 | 451 => {}
                _ => bail!("Spotify could not answer a catalog metadata request."),
            }
        }
    }
    if !missing.is_empty() {
        bail!("Spotify did not answer all catalog metadata requests.");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{
        entity_extension_data::{EntityExtensionData, EntityExtensionDataHeader},
        extended_metadata::EntityExtensionDataArray,
    };
    use protobuf::{MessageField, well_known_types::any::Any};

    fn reply(status: i32, payload: bool) -> BatchedExtensionResponse {
        BatchedExtensionResponse {
            extended_metadata: vec![EntityExtensionDataArray {
                extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                extension_data: vec![EntityExtensionData {
                    entity_uri: "spotify:track:test".into(),
                    header: MessageField::some(EntityExtensionDataHeader {
                        status_code: status,
                        ..Default::default()
                    }),
                    extension_data: if payload {
                        MessageField::some(Any {
                            value: b"fixture".to_vec(),
                            ..Default::default()
                        })
                    } else {
                        MessageField::none()
                    },
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn native_tracks_preserve_recording_ids_and_artist_references() {
        let mut track = metadata::Track::new();
        track.set_gid(vec![1; 16]);
        track.set_name("Recording".into());
        let mut album = metadata::Album::new();
        album.set_gid(vec![2; 16]);
        track.album = protobuf::MessageField::some(album);
        let mut artist = metadata::Artist::new();
        artist.set_gid(vec![3; 16]);
        artist.set_name("Artist".into());
        track.artist.push(artist);
        let mut external = metadata::ExternalId::new();
        external.set_type("isrc".into());
        external.set_id("USABC1234567".into());
        track.external_id.push(external);
        let card = track_card(&track).unwrap();
        assert_eq!(card["isrc"], "USABC1234567");
        assert!(card["artist_ref"].as_str().unwrap().starts_with("artist:"));
        track.external_id.clear();
        assert!(track_card(&track).unwrap().get("isrc").is_none());
    }

    #[test]
    fn only_explicitly_missing_rows_are_unavailable_while_refusals_remain_errors() {
        let asked = ["spotify:track:test".into()];
        assert_eq!(
            answers(&asked, ExtensionKind::TRACK_V4, reply(200, true)).unwrap()[&asked[0]],
            b"fixture"
        );
        for status in [404, 451] {
            assert!(
                answers(&asked, ExtensionKind::TRACK_V4, reply(status, false))
                    .unwrap()
                    .is_empty()
            );
        }
        for status in [401, 403, 429, 500] {
            assert!(answers(&asked, ExtensionKind::TRACK_V4, reply(status, false)).is_err());
        }
        assert!(answers(&asked, ExtensionKind::TRACK_V4, reply(200, false)).is_err());
        assert!(
            answers(
                &["spotify:track:other".into()],
                ExtensionKind::TRACK_V4,
                reply(200, true)
            )
            .is_err()
        );
    }

    #[test]
    fn missing_or_refused_batch_rows_are_errors_instead_of_empty_albums() {
        assert!(
            answers(
                &["spotify:track:test".into()],
                ExtensionKind::TRACK_V4,
                BatchedExtensionResponse::new()
            )
            .is_err()
        );
        assert!(
            answers(
                &[],
                ExtensionKind::TRACK_V4,
                BatchedExtensionResponse::new()
            )
            .unwrap()
            .is_empty()
        );
        assert!(album_json(&metadata::Album::new()).is_err());
    }
}
