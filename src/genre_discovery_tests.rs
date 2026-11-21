use super::*;
use crate::fixtures::{id, routed};

fn artist(index: usize) -> Value {
    json!({"id":id(index),"name":format!("Artist {index}")})
}
fn track(index: usize, album: usize, date: &str) -> Value {
    json!({"id":id(index),"name":"Track","album":{
        "id":id(album),"name":format!("Album {album}"),"release_date":date,
        "artists":[artist(100)],"images":[{"url":"https://i.scdn.co/image/fixture"}]
    }})
}
fn artist_page(rows: Vec<Value>, more: bool) -> String {
    json!({"artists":{"items":rows,"next":if more {json!("https://untrusted.invalid/")} else {Value::Null}}}).to_string()
}
fn track_page(rows: Vec<Value>) -> String {
    json!({"tracks":{"items":rows,"next":null}}).to_string()
}

#[tokio::test]
async fn genre_filters_produce_deduplicated_recent_albums_and_openable_artists() {
    let (client, _dir, worker) = routed(vec![
        (
            "/search?q=genre:\"indie rock\"",
            200,
            artist_page(vec![artist(1), artist(1), Value::Null], true),
        ),
        (
            "/search?q=genre:\"indie rock\"",
            200,
            artist_page(vec![artist(2)], false),
        ),
        (
            "/search?q=genre:\"indie rock\" year:2026-2026",
            200,
            track_page(vec![
                track(1, 10, "2026-11-01"),
                track(2, 10, "2026-11-01"),
                track(3, 11, "2026-11-19"),
                track(4, 12, "2020-01-01"),
                track(5, 13, "2026-12-01"),
                track(6, 14, "2026"),
            ]),
        ),
    ])
    .await;
    let page = client
        .genre_fetch(
            "indie rock",
            NaiveDate::from_ymd_opt(2026, 11, 20).unwrap(),
            20,
        )
        .await
        .unwrap();
    let albums = &page["shelves"][0]["items"];
    assert_eq!(albums.as_array().unwrap().len(), 2);
    assert_eq!(albums[0]["ref"], format!("album:{}", id(11)));
    assert_eq!(albums[1]["artist_ref"], format!("artist:{}", id(100)));
    assert_eq!(page["shelves"][1]["items"].as_array().unwrap().len(), 2);
    for path in worker.join().unwrap() {
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.path(), "/search");
        assert_eq!(params["limit"], "10");
        assert!(["artist", "track"].contains(&params["type"].as_ref()));
    }
}

#[tokio::test]
async fn malformed_and_refused_genres_are_errors_but_valid_empty_genres_are_empty() {
    for (status, body, succeeds) in [
        (200, "{}".to_string(), false),
        (403, "{}".to_string(), false),
        (200, artist_page(vec![], false), true),
    ] {
        let (client, _dir, worker) =
            routed(vec![("/search?q=genre:\"unknown\"", status, body)]).await;
        let result = client.genre_rows("genre:\"unknown\"", "artist", 20).await;
        assert_eq!(result.is_ok(), succeeds);
        if succeeds {
            assert!(result.unwrap().is_empty());
        }
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn pagination_is_bounded_even_when_provider_repeats_the_same_page() {
    let body = artist_page(vec![artist(1)], true);
    let (client, _dir, worker) = routed(vec![
        ("/search?q=genre:\"rock\"", 200, body.clone()),
        ("/search?q=genre:\"rock\"", 200, body.clone()),
        ("/search?q=genre:\"rock\"", 200, body),
    ])
    .await;
    assert_eq!(
        client
            .genre_rows("genre:\"rock\"", "artist", 20)
            .await
            .unwrap()
            .len(),
        1
    );
    let paths = worker.join().unwrap();
    assert_eq!(paths.len(), 3);
    assert!(paths[2].contains("offset=20"));
}

fn age_snapshots(root: &std::path::Path) {
    for entry in std::fs::read_dir(root.join("cache/snapshots")).unwrap() {
        let path = entry.unwrap().path();
        let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["at"] = json!(now() - 120);
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn page_route_coalesces_reads_and_refresh_observes_changes_without_nested_cache() {
    let today = chrono::DateTime::from_timestamp(now() as i64, 0)
        .unwrap()
        .date_naive();
    let since = today - chrono::Duration::days(90);
    let tracks: &'static str = Box::leak(
        format!(
            "/search?q=genre:\"rock\" year:{}-{}",
            since.year(),
            today.year()
        )
        .into_boxed_str(),
    );
    let (client, dir, worker) = routed(vec![
        (
            "/search?q=genre:\"rock\"",
            200,
            artist_page(vec![artist(1)], false),
        ),
        (tracks, 200, track_page(vec![])),
        ("/search?q=genre:\"rock\"", 503, "{}".into()),
        (tracks, 200, track_page(vec![])),
        (
            "/search?q=genre:\"rock\"",
            200,
            artist_page(vec![artist(2)], false),
        ),
        (tracks, 200, track_page(vec![])),
    ])
    .await;
    let params = json!({"ref":"genre-discovery","filters":{"genre":" Rock "},"limit":60});
    let (first, second) = tokio::join!(client.page(&params), client.page(&params));
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    age_snapshots(dir.path());
    assert_eq!(client.page(&params).await.unwrap(), first);
    let mut refresh = params.clone();
    refresh["refresh"] = json!(true);
    let stale = client.page(&refresh).await.unwrap();
    assert_eq!(stale["cached"], true);
    assert_eq!(stale["shelves"], first["shelves"]);
    let changed = client.page(&refresh).await.unwrap();
    assert_ne!(first, changed);
    assert_eq!(
        changed["shelves"][1]["items"][0]["ref"],
        format!("artist:{}", id(2))
    );
    assert_eq!(worker.join().unwrap().len(), 6);
    assert_eq!(crate::capabilities()["capabilities"]["genre_discovery"], 1);
}

#[test]
fn filter_validation_does_not_accept_search_operators_or_empty_names() {
    for genre in [
        "",
        "  ",
        "rock\" OR artist:other",
        "rock\\",
        "rock\nmetal",
        "genre:rock",
    ] {
        assert!(genre_name(&json!({"filters":{"genre":genre}})).is_err());
    }
    assert!(genre_name(&json!({"filters":{"genre":"x".repeat(1025)}})).is_err());
    assert_eq!(
        genre_name(&json!({"filters":{"genre":" R&B "}})).unwrap(),
        "r&b"
    );
}
