// I adapt Spotifast's color-lyrics mapping; see licenses/Spotifast-MIT.txt.
use crate::{
    auth::Client,
    state::{reference, text},
};
use anyhow::{Context, Result, bail};
use librespot_core::{SpotifyId, error::ErrorKind};
use serde_json::{Value, json};
use std::time::Duration;

impl Client {
    pub async fn lyrics(&self, params: &Value) -> Result<Value> {
        let (_, id) = reference(text(params, "ref"), Some("track"))?;
        let id = SpotifyId::from_base62(&id).context("Invalid Spotify track identity.")?;
        let work = async {
            // The playback session is independent of Web API application quotas.
            let session = self.session().await?;
            match session.spclient().get_lyrics(&id).await {
                Ok(bytes) => {
                    if bytes.len() > 512 * 1024 {
                        bail!("Spotify returned lyrics exceeding the size limit.");
                    }
                    convert(
                        &serde_json::from_slice(&bytes)
                            .context("Spotify returned unreadable lyrics.")?,
                    )
                }
                Err(error) if error.kind == ErrorKind::NotFound => Ok(json!({"status":"none"})),
                Err(error) if error.kind == ErrorKind::PermissionDenied => {
                    bail!("Spotify did not authorize lyrics for this track.")
                }
                Err(error) if error.kind == ErrorKind::ResourceExhausted => {
                    bail!("Spotify temporarily limited lyrics requests. Try again later.")
                }
                Err(_) => {
                    bail!("No se pudieron consultar las letras de Spotify. Vuelve a intentarlo.")
                }
            }
        };
        tokio::time::timeout(Duration::from_millis(3500), work)
            .await
            .map_err(|_| anyhow::anyhow!("The Spotify lyrics request timed out."))?
    }
}

fn convert(value: &Value) -> Result<Value> {
    let lyrics = value
        .get("lyrics")
        .filter(|v| v.is_object())
        .context("Spotify returned invalid lyrics.")?;
    let raw = lyrics["lines"]
        .as_array()
        .context("Spotify returned invalid lyric lines.")?;
    let mut lines = vec![];
    for line in raw {
        let words = line["words"]
            .as_str()
            .context("Spotify returned an invalid lyric line.")?
            .trim();
        if words.is_empty() {
            continue;
        }
        let at = line["startTimeMs"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| line["startTimeMs"].as_u64())
            .filter(|n| *n < 86_400_000);
        lines.push((at, words.replace(['\r', '\n'], " ")));
    }
    if lines.is_empty() {
        return Ok(json!({"status":"none"}));
    }
    let synced = lyrics["syncType"] == "LINE_SYNCED"
        && lines.iter().all(|(at, _)| at.is_some())
        && lines.windows(2).all(|pair| pair[0].0 <= pair[1].0);
    if lines.iter().map(|(_, words)| words.len()).sum::<usize>() > 512 * 1024 {
        bail!("Spotify returned lyrics exceeding the size limit.");
    }
    let output: Vec<Value> = lines
        .into_iter()
        .map(|(at, words)| {
            let mut line = json!({"text":words});
            if synced {
                line["start_ms"] = json!(at.unwrap());
            }
            line
        })
        .collect();
    let supplier = text(lyrics, "providerDisplayName").trim();
    let mut document = json!({"schema":1,"timing":if synced {"line"} else {"unsynced"},"lines":output,
        "source":{"provider":"spotify","name":if supplier.is_empty() {"Spotify".into()} else {format!("Spotify / {supplier}")}}});
    if let Some(language) = lyrics["language"].as_str().filter(|s| !s.is_empty()) {
        document["language"] = json!(language);
    }
    Ok(json!({"status":"found","lyrics":{"format":"document","document":document}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronized_lyrics_keep_milliseconds_and_playlist_occurrences_use_the_track() {
        let found = convert(&json!({"lyrics":{"syncType":"LINE_SYNCED","lines":[
            {"startTimeMs":"1234","words":"First placeholder"},
            {"startTimeMs":65432,"words":"Second placeholder"}],"providerDisplayName":"Fixture provider","language":"en"}}))
        .unwrap();
        let document = &found["lyrics"]["document"];
        assert_eq!(found["lyrics"]["format"], "document");
        assert_eq!(document["timing"], "line");
        assert_eq!(document["lines"][0]["start_ms"], 1234);
        assert_eq!(document["lines"][1]["start_ms"], 65432);
        assert_eq!(document["language"], "en");
        assert_eq!(document["source"]["name"], "Spotify / Fixture provider");
        assert_eq!(
            reference(
                "track:1234567890123456789012@playlist:abcdefghijklmnopqrstuv@entry:4",
                Some("track")
            )
            .unwrap()
            .1,
            "1234567890123456789012"
        );
    }
    #[test]
    fn invalid_timestamps_fall_back_to_plain_and_malformed_responses_are_not_absence() {
        for times in [["bad", "100"], ["200", "100"]] {
            let found = convert(&json!({"lyrics":{"syncType":"LINE_SYNCED","lines":[
                {"startTimeMs":times[0],"words":"First placeholder"},
                {"startTimeMs":times[1],"words":"Second placeholder"}]}}))
            .unwrap();
            assert_eq!(found["lyrics"]["document"]["timing"], "unsynced");
            assert!(
                found["lyrics"]["document"]["lines"][0]
                    .get("start_ms")
                    .is_none()
            );
        }
        assert_eq!(
            convert(&json!({"lyrics":{"lines":[]}})).unwrap()["status"],
            "none"
        );
        for bad in [
            json!({}),
            json!({"lyrics":{"lines":[{}]}}),
            json!({"lyrics":{"lines":null}}),
        ] {
            assert!(convert(&bad).is_err());
        }
    }
}
