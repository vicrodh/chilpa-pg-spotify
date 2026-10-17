// I adapt Spotifast's session station reader (MIT, Carmine Paolino).
use crate::{
    auth::Client,
    native_catalog::{batch, track_card},
    state::reference,
};
use anyhow::{Context, Result, ensure};
use librespot_core::{SpotifyId, SpotifyUri};
use librespot_protocol::{
    context::Context as RadioContext, extension_kind::ExtensionKind, metadata,
};
use protobuf::Message;
use serde_json::{Value, json};
use std::time::Duration;

impl Client {
    pub async fn radio_page(&self, opaque: &str, refresh: bool) -> Result<Value> {
        let seed = opaque
            .strip_prefix("radio:")
            .context("Invalid radio reference.")?;
        let (kind, id) = reference(seed, None)?;
        ensure!(!seed.contains('@'), "Invalid radio reference.");
        self.cached(&format!("session:radio:{kind}:{id}"), if refresh { 0 } else { u64::MAX }, refresh, || async {
            let work = async {
                let session = self.session().await?;
                let context = session.spclient().get_context(&format!("spotify:station:{kind}:{id}")).await
                    .map_err(|_| anyhow::anyhow!("The playback session could not load this radio."))?;
                let uris = station_tracks(&context)?;
                let found = batch(&session, ExtensionKind::TRACK_V4, &uris).await?;
                let mut items = Vec::with_capacity(uris.len());
                for (position, uri) in uris.iter().enumerate() {
                    let bytes = found.get(&uri.to_uri()).context("The radio track metadata is incomplete. Refresh the radio to try again.")?;
                    let mut card = track_card(&metadata::Track::parse_from_bytes(bytes).context("The radio returned invalid track metadata.")?)?;
                    card["ref"] = json!(format!("{}@radio:{kind}:{id}@entry:{position}", uri.to_uri().trim_start_matches("spotify:")));
                    items.push(card);
                }
                // I snapshot the supplied finite mix; station continuation is not a load-more page.
                Ok(json!({"title":"Radio","kind":"track","items":items,"tracks":items,
                    "tabs":[{"title":"Overview","ref":seed,"active":false},{"title":"Radio","ref":opaque,"active":true}],
                    "complete":true}))
            };
            tokio::time::timeout(Duration::from_secs(25), work).await
                .map_err(|_| anyhow::anyhow!("The playback session took too long to load this radio."))?
        }).await
    }
}

fn station_tracks(context: &RadioContext) -> Result<Vec<SpotifyUri>> {
    ensure!(
        !context.loading(),
        "The radio is still loading. Refresh it to try again."
    );
    let mut tracks = vec![];
    for page in &context.pages {
        ensure!(
            !page.loading(),
            "The radio page is still loading. Refresh it to try again."
        );
        ensure!(
            !page.tracks.is_empty() || page.page_url().is_empty(),
            "The radio returned a page without its tracks."
        );
        for track in &page.tracks {
            let uri = match track.uri.as_deref().filter(|s| !s.is_empty()) {
                Some(value) => SpotifyUri::from_uri(value)
                    .context("The radio returned an invalid track reference.")?,
                None => SpotifyUri::Track {
                    id: SpotifyId::from_raw(
                        track
                            .gid
                            .as_deref()
                            .context("The radio returned a track without an identifier.")?,
                    )?,
                },
            };
            ensure!(
                matches!(uri, SpotifyUri::Track { .. }),
                "The radio returned an unsupported item."
            );
            tracks.push(uri);
            ensure!(
                tracks.len() <= 500,
                "The radio exceeds the 500-track limit."
            );
        }
    }
    ensure!(
        !tracks.is_empty(),
        "No tracks are available for this radio."
    );
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{context_page::ContextPage, context_track::ContextTrack};
    #[test]
    fn station_rows_preserve_provider_order_and_occurrences() {
        let a = "spotify:track:1234567890123456789012";
        let b = "spotify:track:0VjIjW4GlUZAMYd2vXMi3b";
        let gid = match SpotifyUri::from_uri(b).unwrap() {
            SpotifyUri::Track { id } => id.to_raw().to_vec(),
            _ => unreachable!(),
        };
        let context = RadioContext {
            pages: vec![ContextPage {
                tracks: vec![
                    ContextTrack {
                        uri: Some(a.into()),
                        ..Default::default()
                    },
                    ContextTrack {
                        gid: Some(gid),
                        ..Default::default()
                    },
                    ContextTrack {
                        uri: Some(a.into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows: Vec<_> = station_tracks(&context)
            .unwrap()
            .iter()
            .map(SpotifyUri::to_uri)
            .collect();
        assert_eq!(rows, [a, b, a]);
    }
    #[test]
    fn inline_station_mix_accepts_continuation_without_following_it() {
        let uri = "spotify:track:0VjIjW4GlUZAMYd2vXMi3b";
        let context = RadioContext {
            pages: vec![
                ContextPage {
                    page_url: Some("hm://radio/first".into()),
                    next_page_url: Some("hm://radio/continuation".into()),
                    tracks: vec![ContextTrack {
                        uri: Some(uri.into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ContextPage {
                    next_page_url: Some("hm://radio/another".into()),
                    tracks: vec![ContextTrack {
                        uri: Some(uri.into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let rows: Vec<_> = station_tracks(&context)
            .unwrap()
            .iter()
            .map(SpotifyUri::to_uri)
            .collect();
        assert_eq!(rows, [uri, uri]);
    }
    #[test]
    fn deferred_page_without_inline_tracks_remains_an_error() {
        let context = RadioContext {
            pages: vec![ContextPage {
                page_url: Some("hm://radio/deferred".into()),
                next_page_url: Some("hm://radio/continuation".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            station_tracks(&context)
                .unwrap_err()
                .to_string()
                .contains("without its tracks")
        );
    }
    #[test]
    fn empty_partial_and_invalid_contexts_fail() {
        assert!(station_tracks(&RadioContext::default()).is_err());
        let mut context = RadioContext {
            pages: vec![ContextPage {
                next_page_url: Some("https://example.invalid/page".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(station_tracks(&context).is_err());
        context.pages[0].next_page_url = None;
        context.pages[0].tracks.push(ContextTrack {
            gid: Some(vec![1]),
            ..Default::default()
        });
        assert!(station_tracks(&context).is_err());
    }
}
