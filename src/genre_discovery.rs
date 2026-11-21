//! Bounded, on-demand genre shelves using Spotify's artist and track filters.
use crate::{
    auth::Client,
    catalog::card,
    state::{now, text},
};
use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

const PAGE_SIZE: usize = 10;
const MAX_PAGES: usize = 3;
const MAX_ITEMS: usize = 20;

fn genre_name(params: &Value) -> Result<String> {
    let name = params["filters"]["genre"]
        .as_str()
        .unwrap_or_default()
        .trim();
    // A genre is one quoted filter value, never user-supplied search syntax.
    if name.is_empty()
        || name.len() > 1024
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | ':'))
    {
        bail!("Invalid genre name.");
    }
    Ok(name.to_lowercase())
}

impl Client {
    pub(crate) async fn genre_discovery(&self, params: &Value) -> Result<Value> {
        let genre = genre_name(params)?;
        let limit = params["limit"]
            .as_u64()
            .unwrap_or(20)
            .clamp(1, MAX_ITEMS as u64) as usize;
        let today = chrono::DateTime::from_timestamp(now() as i64, 0)
            .context("Invalid catalogue date.")?
            .date_naive();
        let key = format!("genre-discovery:v1:{genre}:{today}:{limit}");
        self.cached(&key, 1200, params["refresh"] == true, || async {
            tokio::time::timeout(
                Duration::from_secs(24),
                self.genre_fetch(&genre, today, limit),
            )
            .await
            .context("Genre discovery took too long. Try again.")?
        })
        .await
    }

    async fn genre_fetch(&self, genre: &str, today: NaiveDate, limit: usize) -> Result<Value> {
        let since = today - chrono::Duration::days(90);
        let artist_query = format!("genre:\"{genre}\"");
        let track_query = format!("{artist_query} year:{}-{}", since.year(), today.year());
        let (artists, tracks) = tokio::join!(
            self.genre_rows(&artist_query, "artist", limit),
            self.genre_rows(&track_query, "track", MAX_PAGES * PAGE_SIZE)
        );
        // Failed branches remain errors: never cache a refusal as an empty genre.
        let artists = artists?;
        let albums = recent_albums(&tracks?, since, today, limit);
        Ok(json!({"title":genre,"shelves":[
            {"id":"new_releases","title":"New releases","kind":"album","items":albums},
            {"id":"artists","title":"Artists","kind":"artist","items":artists.into_iter().filter_map(|v| card(&v,"artist")).collect::<Vec<_>>()}
        ]}))
    }

    async fn genre_rows(&self, query: &str, kind: &str, limit: usize) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        // At most three ten-row responses per branch, owned by this request.
        // No worker or timer survives it; the existing bounded store owns snapshots.
        for page_index in 0..MAX_PAGES {
            let reply = self
                .fetch_genre_search(&[
                    ("q", query.into()),
                    ("type", kind.into()),
                    ("limit", PAGE_SIZE.to_string()),
                    ("offset", (page_index * PAGE_SIZE).to_string()),
                ])
                .await?;
            let page = &reply[format!("{kind}s")];
            let batch = page["items"]
                .as_array()
                .context("Spotify returned an invalid genre listing.")?;
            for row in batch.iter().take(PAGE_SIZE) {
                let Some(item) = card(row, kind) else {
                    continue;
                };
                if text(&item, "title").is_empty() || !seen.insert(text(&item, "ref").to_owned()) {
                    continue;
                }
                rows.push(row.clone());
                if rows.len() == limit {
                    return Ok(rows);
                }
            }
            if batch.is_empty() || text(page, "next").is_empty() {
                break;
            }
        }
        Ok(rows)
    }
}

fn recent_albums(tracks: &[Value], since: NaiveDate, today: NaiveDate, limit: usize) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut albums: Vec<_> = tracks
        .iter()
        .filter_map(|track| {
            let raw = &track["album"];
            let date = NaiveDate::parse_from_str(text(raw, "release_date"), "%Y-%m-%d").ok()?;
            if date < since || date > today {
                return None;
            }
            let item = card(raw, "album")?;
            if text(&item, "title").is_empty() || !seen.insert(text(&item, "ref").to_owned()) {
                return None;
            }
            Some(item)
        })
        .collect();
    albums.sort_by(|a, b| {
        text(b, "release_date")
            .cmp(text(a, "release_date"))
            .then_with(|| text(a, "ref").cmp(text(b, "ref")))
    });
    albums.truncate(limit);
    albums
}

#[cfg(test)]
#[path = "genre_discovery_tests.rs"]
mod tests;
