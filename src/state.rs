use anyhow::{Context, Result, bail};
use librespot_core::authentication::Credentials;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Account {
    #[serde(default)]
    pub language: String,
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires: u64,
    pub user: String,
    pub name: String,
    pub playback: Option<Credentials>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub shared: Option<WebGrant>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct WebGrant {
    pub access_token: String,
    pub refresh_token: String,
    pub expires: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
}
impl WebGrant {
    pub fn from_account(account: &Account) -> Self {
        Self {
            access_token: account.access_token.clone(),
            refresh_token: account.refresh_token.clone(),
            expires: account.expires,
            scopes: account.scopes.clone(),
        }
    }
}

pub fn load(path: &Path) -> Result<Account> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("Stored account is invalid; sign out and sign in again."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Account::default()),
        Err(_) => bail!("Cannot read the plugin account file."),
    }
}

pub fn save(path: &Path, account: &Account) -> Result<()> {
    save_json(path, &serde_json::to_value(account)?)
}

pub fn save_json(path: &Path, value: &Value) -> Result<()> {
    let directory = path.parent().context("Missing account directory.")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    // NamedTempFile starts with owner-only permissions on Unix.
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot save the plugin account."))?;
    Ok(())
}

pub fn saved_pause(path: &Path, client: &str) -> u64 {
    let value: Value = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if value["client_id"] == client {
        value["until"].as_u64().unwrap_or(0)
    } else {
        0
    }
}

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn reference(value: &str, expected: Option<&str>) -> Result<(String, String)> {
    let base = value.split('@').next().unwrap_or("");
    let (kind, id) = base.split_once(':').context("Invalid Spotify reference.")?;
    if !["track", "album", "artist", "playlist"].contains(&kind)
        || expected.is_some_and(|k| k != kind)
        || id.len() != 22
        || !id.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        bail!("Invalid Spotify reference.");
    }
    Ok((kind.into(), id.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn references_reject_paths_and_keep_context() {
        assert!(reference("track:../../etc/passwd", None).is_err());
        assert!(reference("album:1234567890123456789012", Some("track")).is_err());
        assert_eq!(
            reference("track:1234567890123456789012@entry:4", Some("track"))
                .unwrap()
                .1,
            "1234567890123456789012"
        );
    }
    #[test]
    fn credentials_survive_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let mut account = Account {
            user: "test".into(),
            ..Default::default()
        };
        save(&path, &account).unwrap();
        account.user = "replacement".into();
        save(&path, &account).unwrap();
        assert_eq!(load(&path).unwrap().user, "replacement");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
