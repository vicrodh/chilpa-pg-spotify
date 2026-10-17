use crate::state::now;
use anyhow::{Context, Result, bail};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use url::Url;

pub fn code(redirect: &str, state: &str, expires: u64, address: &str) -> Result<String> {
    if now() >= expires {
        bail!("Sign-in expired. Start again.");
    }
    let url = Url::parse(address.trim())
        .map_err(|_| anyhow::anyhow!("Paste the complete return address."))?;
    let base = Url::parse(redirect)?;
    let pairs: Vec<_> = url.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(k, _)| k == "state").collect();
    let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
    if url.origin() != base.origin()
        || url.path() != base.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || states.len() != 1
        || states[0].1 != state
        || codes.len() != 1
        || codes[0].1.is_empty()
    {
        bail!("This return address does not belong to the current sign-in attempt.");
    }
    Ok(codes[0].1.to_string())
}

pub struct Capture {
    pub redirect: String,
    outcome: Arc<Mutex<Option<std::result::Result<String, String>>>>,
    task: Option<JoinHandle<()>>,
}

impl Capture {
    pub async fn bind(redirect: &str, state: &str, expires: u64) -> Result<Self> {
        let mut base = Url::parse(redirect)?;
        let port = base.port().context("Missing callback port.")?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Sign-in port {port} is busy. Close the app using it and reconnect."
                )
            })?;
        base.set_port(Some(listener.local_addr()?.port())).unwrap();
        let redirect = base.to_string();
        let expected = redirect.clone();
        let state = state.to_owned();
        let outcome = Arc::new(Mutex::new(None));
        let result = outcome.clone();
        let task = tokio::spawn(async move {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(expires.saturating_sub(now()));
            loop {
                let socket = match tokio::time::timeout_at(deadline, listener.accept()).await {
                    Ok(Ok((socket, _))) => socket,
                    _ => {
                        *result.lock().unwrap() =
                            Some(Err("Sign-in expired. Connect to Spotify again.".into()));
                        return;
                    }
                };
                let mut socket = socket;
                let received =
                    tokio::time::timeout(Duration::from_secs(5), read_request(&mut socket)).await;
                let address = received.ok().and_then(Result::ok).and_then(|target| {
                    let url = Url::parse(&expected).ok()?.join(&target).ok()?;
                    Some(url.to_string())
                });
                let valid = address
                    .as_ref()
                    .is_some_and(|url| code(&expected, &state, expires, url).is_ok());
                let denied = address
                    .as_ref()
                    .is_some_and(|address| denied(&expected, &state, address));
                let (status, body) = if valid {
                    (
                        "200 OK",
                        "Authorization received. Return to Chilpa; sign-in will finish automatically.",
                    )
                } else if denied {
                    (
                        "200 OK",
                        "Authorization canceled. Return to Chilpa to try again.",
                    )
                } else {
                    (
                        "400 Bad Request",
                        "This response does not belong to Chilpa's pending sign-in.",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    socket.write_all(response.as_bytes()),
                )
                .await;
                if valid {
                    *result.lock().unwrap() = Some(Ok(address.unwrap()));
                    return;
                }
                if denied {
                    *result.lock().unwrap() = Some(Err(
                        "You canceled Spotify authorization. Reconnect whenever you are ready."
                            .into(),
                    ));
                    return;
                }
            }
        });
        Ok(Self {
            redirect,
            outcome,
            task: Some(task),
        })
    }

    pub fn take(&self) -> Option<std::result::Result<String, String>> {
        self.outcome.lock().unwrap().take()
    }

    pub fn received(&self) -> bool {
        self.outcome.lock().unwrap().is_some()
    }

    pub async fn close(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut block = [0; 512];
    loop {
        let count = socket.read(&mut block).await?;
        if count == 0 {
            bail!("Incomplete callback request.");
        }
        bytes.extend_from_slice(&block[..count]);
        if bytes.len() > 8192 {
            bail!("Callback request is too large.");
        }
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes)?;
    let mut parts = request.lines().next().unwrap_or("").split_whitespace();
    if parts.next() != Some("GET") {
        bail!("Invalid callback method.");
    }
    let target = parts.next().context("Missing callback path.")?;
    if !target.starts_with('/')
        || target.starts_with("//")
        || parts.next() != Some("HTTP/1.1")
        || parts.next().is_some()
    {
        bail!("Invalid callback request.");
    }
    Ok(target.into())
}

fn denied(redirect: &str, state: &str, address: &str) -> bool {
    let (Ok(base), Ok(url)) = (Url::parse(redirect), Url::parse(address)) else {
        return false;
    };
    let states: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .collect();
    url.origin() == base.origin()
        && url.path() == base.path()
        && states.len() == 1
        && states[0].1 == state
        && url
            .query_pairs()
            .any(|(key, value)| key == "error" && value == "access_denied")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn callback_capture_ignores_foreign_state_and_accepts_the_browser_response() {
        let capture = Capture::bind("http://127.0.0.1:0/login", "expected", now() + 60)
            .await
            .unwrap();
        let http = reqwest::Client::new();
        assert_eq!(
            http.get(format!("{}?state=wrong&code=test", capture.redirect))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert!(capture.take().is_none());
        assert_eq!(
            http.get(format!("{}?state=expected&code=test", capture.redirect))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        let address = capture.take().unwrap().unwrap();
        assert_eq!(
            code(&capture.redirect, "expected", now() + 60, &address).unwrap(),
            "test"
        );
        assert!(capture.take().is_none());
    }
    #[tokio::test]
    async fn cancel_releases_the_port_and_occupied_ports_fail_before_sign_in() {
        let capture = Capture::bind("http://127.0.0.1:0/login", "expected", now() + 60)
            .await
            .unwrap();
        let redirect = capture.redirect.clone();
        assert!(Capture::bind(&redirect, "other", now() + 60).await.is_err());
        capture.close().await;
        Capture::bind(&redirect, "other", now() + 60)
            .await
            .unwrap()
            .close()
            .await;
    }
    #[tokio::test]
    async fn permission_denial_and_expiry_stop_the_listener() {
        let capture = Capture::bind("http://127.0.0.1:0/login", "expected", now() + 60)
            .await
            .unwrap();
        reqwest::get(format!(
            "{}?state=expected&error=access_denied",
            capture.redirect
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(capture.take().unwrap().is_err());
        let expired = Capture::bind("http://127.0.0.1:0/login", "expected", 0)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(expired.take().unwrap().is_err());
    }
}
