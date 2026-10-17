use crate::{
    auth::{Client, random},
    state::{reference, text},
};
use anyhow::{Context, Result, bail};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::SpotifyUri;
use librespot_metadata::{Metadata, Track, audio::AudioFileFormat};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
    time::Instant,
};
use tiny_http::{Header, Method, Response, Server, StatusCode};

const OFFSET: u64 = 0xa7;
trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}
struct Asset {
    file: Mutex<Box<dyn ReadSeek + Send>>,
    length: u64,
    touched: Mutex<Instant>,
}

pub struct Streams {
    assets: Arc<Mutex<HashMap<String, Arc<Asset>>>>,
    port: u16,
    server: Arc<Server>,
}

impl Streams {
    pub fn new() -> Result<Self> {
        let server = Arc::new(
            Server::http("127.0.0.1:0")
                .map_err(|_| anyhow::anyhow!("Cannot start the local audio endpoint."))?,
        );
        let port = server
            .server_addr()
            .to_ip()
            .context("Invalid local endpoint.")?
            .port();
        let assets = Arc::new(Mutex::new(HashMap::new()));
        for _ in 0..4 {
            let server = server.clone();
            let assets = assets.clone();
            std::thread::Builder::new()
                .name("spotify-audio".into())
                .spawn(move || {
                    while let Ok(request) = server.recv() {
                        serve(request, &assets);
                    }
                })?;
        }
        Ok(Self {
            assets,
            port,
            server,
        })
    }

    pub fn clear(&self) {
        self.assets.lock().unwrap().clear();
    }

    pub async fn resolve(&self, client: &Client, params: &Value) -> Result<Value> {
        let (_, id) = reference(text(params, "ref"), Some("track"))?;
        let uri = SpotifyUri::from_uri(&format!("spotify:track:{id}"))?;
        let session = client.session().await?;
        let track = Track::get(&session, &uri)
            .await
            .map_err(|_| anyhow::anyhow!("Cannot read Spotify playback metadata."))?;
        let mut candidates = vec![track];
        if candidates[0].files.is_empty() {
            let alternatives: Vec<_> = candidates[0].alternatives.iter().take(5).cloned().collect();
            for uri in alternatives {
                if let Ok(track) = Track::get(&session, &uri).await {
                    candidates.push(track);
                }
            }
        }
        let choices = [
            (AudioFileFormat::OGG_VORBIS_320, 40000),
            (AudioFileFormat::OGG_VORBIS_160, 20000),
            (AudioFileFormat::OGG_VORBIS_96, 12000),
        ];
        let (track, file_id, rate) = candidates
            .iter()
            .find_map(|track| {
                choices.iter().find_map(|(format, rate)| {
                    track.files.get(format).map(|id| (track, *id, *rate))
                })
            })
            .context("Spotify did not offer a supported full-length Vorbis stream.")?;
        let SpotifyUri::Track { id: track_id } = track.id else {
            bail!("Invalid playback identity.");
        };
        let key = session
            .audio_key()
            .request(track_id, file_id)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Spotify denied the audio key. Check Premium and track availability."
                )
            })?;
        let file = AudioFile::open(&session, file_id, rate)
            .await
            .map_err(|_| anyhow::anyhow!("Cannot open the Spotify audio stream."))?;
        let length = file.get_stream_loader_controller()?.len() as u64;
        if length <= OFFSET {
            bail!("Spotify returned an incomplete audio stream.");
        }
        let mut file = AudioDecrypt::new(Some(key), file);
        let (file, normalization) = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut header = [0u8; 167];
            file.read_exact(&mut header)
                .context("Spotify audio header is incomplete.")?;
            let mut magic = [0; 4];
            file.read_exact(&mut magic)
                .context("Spotify audio stream is incomplete.")?;
            if &magic != b"OggS" {
                bail!("Spotify did not return a valid Vorbis stream.");
            }
            file.seek(SeekFrom::Start(OFFSET))?;
            let gain = f32::from_le_bytes(header[144..148].try_into().unwrap());
            let peak = f32::from_le_bytes(header[148..152].try_into().unwrap());
            let normalization = if gain.is_finite() && peak.is_finite() && peak > 0.0 {
                json!({"gain_db":gain,"peak":peak,"source":"provider","scope":"track"})
            } else {
                Value::Null
            };
            Ok((file, normalization))
        })
        .await??;
        let ticket = random();
        let mut assets = self.assets.lock().unwrap();
        assets.retain(|_, a| a.touched.lock().unwrap().elapsed().as_secs() < 3600);
        if assets.len() >= 128 {
            // I preserve active addresses; a pause or a seek may reuse them later.
            bail!("Too many active Spotify streams. Restart the plugin to release old streams.");
        }
        assets.insert(
            ticket.clone(),
            Arc::new(Asset {
                file: Mutex::new(Box::new(file)),
                length: length - OFFSET,
                touched: Mutex::new(Instant::now()),
            }),
        );
        Ok(
            json!({"url":format!("http://127.0.0.1:{}/{ticket}",self.port),
            "delivery":"direct","preview":false,"format":{"codec":"ogg"},"normalization":normalization}),
        )
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

fn range(value: Option<&str>, length: u64) -> Option<(u64, u64)> {
    let Some(value) = value else {
        return Some((0, length));
    };
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        return (suffix > 0).then_some((length.saturating_sub(suffix), length));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        length
    } else {
        end.parse::<u64>().ok()?.saturating_add(1).min(length)
    };
    (start < end && start < length).then_some((start, end))
}

struct Reader {
    asset: Arc<Asset>,
    position: u64,
    end: u64,
}
impl Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = (self.end - self.position)
            .min(output.len() as u64)
            .min(128 * 1024) as usize;
        if count == 0 {
            return Ok(0);
        }
        let mut file = self.asset.file.lock().unwrap();
        file.seek(SeekFrom::Start(OFFSET + self.position))?;
        let count = file.read(&mut output[..count])?;
        self.position += count as u64;
        *self.asset.touched.lock().unwrap() = Instant::now();
        Ok(count)
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).unwrap()
}
fn serve(request: tiny_http::Request, assets: &Mutex<HashMap<String, Arc<Asset>>>) {
    if !matches!(request.method(), Method::Get | Method::Head) {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return;
    }
    let asset = assets
        .lock()
        .unwrap()
        .get(request.url().trim_start_matches('/'))
        .cloned();
    let Some(asset) = asset else {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return;
    };
    *asset.touched.lock().unwrap() = Instant::now();
    let requested = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str());
    let Some((start, end)) = range(requested, asset.length) else {
        let _ = request.respond(Response::empty(StatusCode(416)).with_header(header(
            "Content-Range",
            &format!("bytes */{}", asset.length),
        )));
        return;
    };
    let mut headers = vec![
        header("Content-Type", "audio/ogg"),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", "no-store"),
    ];
    if requested.is_some() {
        headers.push(header(
            "Content-Range",
            &format!("bytes {start}-{}/{}", end - 1, asset.length),
        ));
    }
    let status = StatusCode(if requested.is_some() { 206 } else { 200 });
    let length = end - start;
    let reader = Reader {
        asset,
        position: start,
        end,
    };
    let _ = request.respond(Response::new(
        status,
        headers,
        reader,
        Some(length as usize),
        None,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_ranges_preserve_seek_and_reject_invalid_ranges() {
        assert_eq!(range(None, 100), Some((0, 100)));
        assert_eq!(range(Some("bytes=20-29"), 100), Some((20, 30)));
        assert_eq!(range(Some("bytes=90-"), 100), Some((90, 100)));
        assert_eq!(range(Some("bytes=-5"), 100), Some((95, 100)));
        for bad in [
            "bytes=100-",
            "bytes=50-2",
            "bytes=1-2,3-4",
            "bytes=-0",
            "other=0-1",
        ] {
            assert_eq!(range(Some(bad), 100), None);
        }
    }
    #[test]
    fn independent_readers_seek_without_corrupting_each_other() {
        let mut bytes = vec![0u8; OFFSET as usize];
        bytes.extend(0..100u8);
        let asset = Arc::new(Asset {
            file: Mutex::new(Box::new(io::Cursor::new(bytes))),
            length: 100,
            touched: Mutex::new(Instant::now()),
        });
        let mut first = Reader {
            asset: asset.clone(),
            position: 20,
            end: 30,
        };
        let mut second = Reader {
            asset,
            position: 80,
            end: 85,
        };
        let mut part = [0u8; 5];
        first.read_exact(&mut part).unwrap();
        assert_eq!(part, [20, 21, 22, 23, 24]);
        second.read_exact(&mut part).unwrap();
        assert_eq!(part, [80, 81, 82, 83, 84]);
        first.read_exact(&mut part).unwrap();
        assert_eq!(part, [25, 26, 27, 28, 29]);
        assert_eq!(first.read(&mut part).unwrap(), 0);
    }
}
