use crate::{auth::Client, state::Account};
use std::{sync::Arc, thread::JoinHandle, time::Duration};
use tiny_http::{Header, Response, Server, StatusCode};

pub async fn routed(
    mut replies: Vec<(&'static str, u16, String)>,
) -> (Client, tempfile::TempDir, JoinHandle<Vec<String>>) {
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = format!("http://{}", server.server_addr());
    let worker = std::thread::spawn(move || {
        let mut paths = vec![];
        while !replies.is_empty() {
            let request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .expect("expected API request");
            let url = url::Url::parse(&format!("http://fixture{}", request.url())).unwrap();
            let key = if url.path() == "/search" {
                format!(
                    "/search?q={}",
                    url.query_pairs().find(|(k, _)| k == "q").unwrap().1
                )
            } else {
                url.path().to_string()
            };
            let index = replies
                .iter()
                .position(|(expected, _, _)| *expected == key)
                .expect("unexpected API route");
            let (_, status, body) = replies.remove(index);
            paths.push(request.url().to_string());
            request
                .respond(Response::from_string(body).with_status_code(StatusCode(status)))
                .unwrap();
        }
        paths
    });
    let directory = tempfile::tempdir().unwrap();
    let mut client = Client::new(
        directory.path().join("data"),
        directory.path().join("cache"),
    )
    .unwrap();
    client.token_endpoint = format!("{address}/token");
    client.api_base = address;
    *client.account.lock().await = Account {
        user: "test".into(),
        access_token: "test-only".into(),
        refresh_token: "test-only".into(),
        expires: u64::MAX,
        ..Default::default()
    };
    (client, directory, worker)
}

pub async fn client(
    replies: Vec<(u16, String)>,
) -> (Client, tempfile::TempDir, JoinHandle<Vec<String>>) {
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let address = format!("http://{}", server.server_addr());
    let worker = std::thread::spawn(move || {
        let mut paths = vec![];
        for (status, body) in replies {
            let request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .expect("expected API request");
            paths.push(request.url().to_owned());
            let response = Response::from_string(body)
                .with_status_code(StatusCode(status))
                .with_header(Header::from_bytes("Retry-After", "60").unwrap());
            request.respond(response).unwrap();
        }
        paths
    });
    let directory = tempfile::tempdir().unwrap();
    let mut client = Client::new(
        directory.path().join("data"),
        directory.path().join("cache"),
    )
    .unwrap();
    client.token_endpoint = format!("{address}/token");
    client.api_base = address;
    *client.account.lock().await = Account {
        user: "test".into(),
        access_token: "test-only".into(),
        refresh_token: "test-only".into(),
        expires: u64::MAX,
        ..Default::default()
    };
    (client, directory, worker)
}

pub fn id(index: usize) -> String {
    format!("{index:022}")
}

pub fn album_metadata(index: usize, label: &str, lines: &[(&str, bool)]) -> Vec<u8> {
    use librespot_protocol::metadata::{Album, Copyright, copyright};
    use protobuf::Message;
    let mut album = Album::new();
    album.set_gid(
        librespot_core::SpotifyId::from_base62(&id(index))
            .unwrap()
            .to_raw()
            .to_vec(),
    );
    album.set_label(label.into());
    album.copyright = lines
        .iter()
        .map(|(text, phonographic)| {
            let mut line = Copyright::new();
            line.set_type(if *phonographic {
                copyright::Type::P
            } else {
                copyright::Type::C
            });
            line.set_text((*text).into());
            line
        })
        .collect();
    album.write_to_bytes().unwrap()
}

pub fn track_metadata(index: usize, names: &[&str]) -> Vec<u8> {
    use librespot_protocol::metadata::{ArtistWithRole, Track, artist_with_role::ArtistRole};
    use protobuf::Message;
    let mut track = Track::new();
    track.set_gid(
        librespot_core::SpotifyId::from_base62(&id(index))
            .unwrap()
            .to_raw()
            .to_vec(),
    );
    track.artist_with_role = names
        .iter()
        .map(|name| {
            let mut artist = ArtistWithRole::new();
            artist.set_artist_name((*name).into());
            artist.set_role(ArtistRole::ARTIST_ROLE_COMPOSER);
            artist
        })
        .collect();
    track.write_to_bytes().unwrap()
}

pub fn metadata_reply(
    kind: librespot_protocol::extension_kind::ExtensionKind,
    rows: Vec<(usize, u16, Vec<u8>)>,
) -> Vec<u8> {
    use librespot_protocol::{
        entity_extension_data::{EntityExtensionData, EntityExtensionDataHeader},
        extended_metadata::{BatchedExtensionResponse, EntityExtensionDataArray},
        extension_kind::ExtensionKind,
    };
    use protobuf::{EnumOrUnknown, Message, MessageField, well_known_types::any::Any};
    let entity = if kind == ExtensionKind::ALBUM_V4 {
        "album"
    } else {
        "track"
    };
    BatchedExtensionResponse {
        extended_metadata: vec![EntityExtensionDataArray {
            extension_kind: EnumOrUnknown::new(kind),
            extension_data: rows
                .into_iter()
                .map(|(index, status, bytes)| EntityExtensionData {
                    entity_uri: format!("spotify:{entity}:{}", id(index)),
                    header: MessageField::some(EntityExtensionDataHeader {
                        status_code: status.into(),
                        ..Default::default()
                    }),
                    extension_data: MessageField::some(Any {
                        value: bytes,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}

pub async fn metadata_client(
    replies: Vec<(u16, Vec<u8>, Duration)>,
) -> (
    Client,
    tempfile::TempDir,
    JoinHandle<Vec<librespot_protocol::extended_metadata::BatchedEntityRequest>>,
) {
    use librespot_protocol::extended_metadata::BatchedEntityRequest;
    use protobuf::Message;
    let (mut client, directory, empty) = client(vec![]).await;
    assert!(empty.join().unwrap().is_empty());
    let server = Server::http("127.0.0.1:0").unwrap();
    client.metadata_fixture = Some(format!("http://{}", server.server_addr()));
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, bytes, delay) in replies {
            let mut request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .expect("expected metadata request");
            assert_eq!(request.method(), &tiny_http::Method::Post);
            assert_eq!(request.url(), crate::session_enrichment::ENDPOINT);
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("content-type")
                        && header.value.as_str() == "application/x-protobuf")
            );
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let body = BatchedEntityRequest::parse_from_bytes(&body).unwrap();
            assert!((1..=20).contains(&body.entity_request.len()));
            for entity in &body.entity_request {
                assert_eq!(entity.query.len(), 1);
                let expected = if entity.entity_uri.starts_with("spotify:album:") {
                    9
                } else {
                    10
                };
                assert_eq!(entity.query[0].extension_kind.value(), expected);
            }
            requests.push(body);
            std::thread::sleep(delay);
            let _ =
                request.respond(Response::from_data(bytes).with_status_code(StatusCode(status)));
        }
        assert!(
            server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none(),
            "unexpected metadata request"
        );
        requests
    });
    (client, directory, worker)
}

pub async fn enrichment_diagnostic(client: &Client, feature: &str) -> serde_json::Value {
    client
        .snapshots
        .peek(&client.identity().await.unwrap(), crate::enrichment::KEY)
        .unwrap()[feature]
        .clone()
}
