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
