use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::{Config, Handle, default_name, existing};
use crate::api::models::{
    Album, ArtistRef, Context, Device, Episode, Image, PlayableItem, PlaybackState, Show, Track,
};
use crate::paths::AppDirs;

const MAX_EVENT: u64 = 256 * 1024;

#[derive(Debug)]
pub(crate) enum Update {
    Playback(Box<PlaybackState>),
    Inactive,
}

#[derive(Default)]
struct Parser {
    name: Option<String>,
    /// Set by the first `auth_state` event; before it, `logged_in` means nothing.
    auth_seen: bool,
    logged_in: bool,
    timestamp: u64,
    /// The volume Soloist last reported, 0 to 100.
    volume: Option<u8>,
}

impl Parser {
    fn read(&mut self, line: &str, now_ms: u64) -> Option<Update> {
        if line.len() as u64 >= MAX_EVENT {
            return None;
        }
        let event: Value = serde_json::from_str(line.get(line.find('{')?..)?).ok()?;
        if let Some(volume) = event["volume"].as_u64() {
            self.volume = Some(volume.min(100) as u8);
        }
        match event["type"].as_str()? {
            "auth_state" => {
                self.auth_seen = true;
                self.logged_in = event["logged_in"].as_bool() == Some(true);
                if let Some(name) = event["device_name"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
                {
                    self.name = Some(name.into());
                }
                (!self.logged_in || event["is_active"].as_bool() == Some(false))
                    .then_some(Update::Inactive)
            }
            "playback_state" if self.logged_in => {
                if !event["is_active"].as_bool()? {
                    return Some(Update::Inactive);
                }
                let playing = match event["status"].as_str()? {
                    "playing" => true,
                    "paused" | "stopped" => false,
                    _ => return None,
                };
                let timestamp = event["position"]["timestamp_ms"].as_u64()?;
                if timestamp < self.timestamp {
                    return None;
                }
                self.timestamp = timestamp;
                let item = parse_item(&event["item"]);
                let position = event["position"]["position_ms"].as_u64().unwrap_or(0);
                let elapsed = if playing {
                    now_ms.saturating_sub(timestamp)
                } else {
                    0
                };
                let position = position.saturating_add(elapsed).min(u32::MAX.into()) as u32;
                let position = item
                    .as_ref()
                    .filter(|item| item.duration_ms() > 0)
                    .map_or(position, |item| position.min(item.duration_ms()));
                let context = event["context"]["uri"]
                    .as_str()
                    .filter(|uri| !uri.is_empty())
                    .map(|uri| Context {
                        uri: uri.into(),
                        kind: crate::util::uri_kind(uri).unwrap_or_default().into(),
                    });
                Some(Update::Playback(Box::new(PlaybackState {
                    device: Some(Device {
                        name: self.name.clone().unwrap_or_else(default_name),
                        is_active: true,
                        is_restricted: true,
                        volume_percent: event["volume"]
                            .as_u64()
                            .map(|volume| volume.min(100) as u8),
                        supports_volume: Some(true),
                        kind: "computer".into(),
                        ..Default::default()
                    }),
                    context,
                    item,
                    is_playing: playing,
                    progress_ms: Some(position),
                    timestamp,
                    shuffle_state: event["options"]["shuffle"].as_bool().unwrap_or(false),
                    repeat_state: event["options"]["repeat"].as_str().unwrap_or("off").into(),
                    ..Default::default()
                })))
            }
            _ => None,
        }
    }
}

fn entity_name(entity: &Value) -> String {
    entity["decorations"]["identity"]["name"]
        .as_str()
        .unwrap_or_default()
        .into()
}

fn parse_item(entity: &Value) -> Option<PlayableItem> {
    let uri = entity["uri"].as_str()?;
    let id = crate::util::uri_id(uri)?.to_owned();
    let decorations = &entity["decorations"];
    let duration_ms = decorations["playback"]["duration_ms"]
        .as_u64()
        .unwrap_or(0)
        .min(u32::MAX.into()) as u32;
    let images: Vec<Image> = decorations["visual_identity"]["cover"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cover| {
            let size = match cover["size"].as_str() {
                Some("small") => Some(64),
                Some("default") => Some(300),
                Some("large" | "xlarge") => Some(640),
                _ => None,
            };
            Some(Image {
                url: cover["url"].as_str()?.into(),
                width: size,
                height: size,
            })
        })
        .collect();
    let parent = &decorations["parent"]["entity"];
    let parent_uri = parent["uri"].as_str().unwrap_or_default();
    match crate::util::uri_kind(uri)? {
        "track" => {
            let artists = decorations["creators"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|creator| {
                    let artist = &creator["entity"];
                    let uri = artist["uri"].as_str();
                    ArtistRef {
                        name: entity_name(artist),
                        id: uri.and_then(crate::util::uri_id).map(str::to_owned),
                        uri: uri.map(str::to_owned),
                    }
                })
                .collect();
            Some(PlayableItem::Track(Track {
                id: Some(id),
                uri: uri.into(),
                name: entity_name(entity),
                duration_ms,
                artists,
                album: Some(Album {
                    id: crate::util::uri_id(parent_uri).unwrap_or_default().into(),
                    uri: parent_uri.into(),
                    name: entity_name(parent),
                    images,
                    ..Default::default()
                }),
                ..Default::default()
            }))
        }
        "episode" => Some(PlayableItem::Episode(Episode {
            id,
            uri: uri.into(),
            name: entity_name(entity),
            duration_ms,
            images,
            show: Some(Show {
                id: crate::util::uri_id(parent_uri).unwrap_or_default().into(),
                uri: parent_uri.into(),
                name: entity_name(parent),
                ..Default::default()
            }),
            ..Default::default()
        })),
        _ => None,
    }
}

fn source(dirs: &AppDirs) -> Option<(Config, u32, Option<String>)> {
    let config = match Config::load(&dirs.soloist_file()).ok()? {
        Some(config) => config,
        None => {
            let root = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                })?;
            let data_dir = root.join("spotilossless/soloist/session");
            let pid: u32 = std::fs::read_to_string(data_dir.join("soloist.pid"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            let binary = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
            if binary.file_name()? != "soloist" {
                return None;
            }
            Config {
                binary,
                data_dir,
                device_name: default_name(),
            }
        }
    };
    let address: std::net::IpAddr = std::fs::read_to_string(config.data_dir.join("ws.addr"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if !address.is_loopback() {
        return None;
    }
    let pid = existing(&config)?.pid;
    let device_id = super::read_device_id(&config.data_dir);
    Some((config, pid, device_id))
}

async fn trace(
    config: &Config,
    handle: &Handle,
    notify: &(impl Fn(Option<Update>, Instant) + Sync),
) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new(&config.binary)
        .args(["ctl", "trace", "-D"])
        .arg(&config.data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("Soloist trace has no output"))?,
    );
    let mut parser = Parser::default();
    loop {
        let mut line = String::new();
        let read = (&mut reader).take(MAX_EVENT).read_line(&mut line).await?;
        if read == 0 || read as u64 >= MAX_EVENT {
            break;
        }
        let received_at = Instant::now();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX.into()) as u64;
        if let Some(update) = parser.read(&line, now_ms) {
            notify(Some(update), received_at);
        }
        let mut snapshot = handle
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(name) = &parser.name {
            snapshot.device_name = Some(name.clone());
        }
        if parser.auth_seen {
            snapshot.signed_in = Some(parser.logged_in);
        }
        if parser.volume.is_some() {
            snapshot.volume = parser.volume;
        }
    }
    handle
        .0
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .signed_in = None;
    child.kill().await.ok();
    child.wait().await.ok();
    Ok(())
}

pub(crate) async fn run(
    dirs: AppDirs,
    handle: Handle,
    notify: impl Fn(Option<Update>, Instant) + Send + Sync,
) {
    loop {
        let lookup = dirs.clone();
        if let Ok(Some((config, pid, device_id))) =
            tokio::task::spawn_blocking(move || source(&lookup)).await
        {
            handle.publish(&config, Some(pid), device_id);
            let _ = trace(&config, &handle, &notify).await;
            notify(None, Instant::now());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parser() -> Parser {
        let mut parser = Parser::default();
        parser.read(r#"12:00:00 {"type":"auth_state","logged_in":true,"is_active":true,"device_name":"External Soloist"}"#, 0);
        parser
    }

    fn event() -> Value {
        json!({
            "type":"playback_state", "is_active":true, "status":"playing",
            "position":{"position_ms":199000,"timestamp_ms":1000,"speed":1},
            "item":{"uri":"spotify:track:current","decorations":{
                "identity":{"name":"Song"},"playback":{"duration_ms":300000},
                "creators":[{"entity":{"uri":"spotify:artist:artist","decorations":{"identity":{"name":"Artist"}}}}],
                "parent":{"entity":{"uri":"spotify:album:album","decorations":{"identity":{"name":"Album"}}}},
                "visual_identity":{"cover":[{"url":"https://i.scdn.co/image/cover"}]}
            }},
            "context":{"uri":"spotify:playlist:blend"}, "volume":42,
            "options":{"shuffle":true,"repeat":"context"},"available_actions":{"set_volume":{}}
        })
    }

    #[test]
    fn the_volume_soloist_reports_is_remembered_from_any_event() {
        let mut parser = parser();
        assert_eq!(parser.volume, None);
        parser.read(&event().to_string(), 2000);
        assert_eq!(parser.volume, Some(42));
        parser.read(r#"{"type":"volume_changed","volume":87}"#, 0);
        assert_eq!(parser.volume, Some(87));
        parser.read(r#"{"type":"volume_changed","volume":900}"#, 0);
        assert_eq!(parser.volume, Some(100));
    }

    #[test]
    fn websocket_state_maps_metadata_position_and_controls_without_api_reads() {
        let Some(Update::Playback(state)) = parser().read(&event().to_string(), 2000) else {
            panic!("missing playback")
        };
        assert_eq!(state.progress_ms, Some(200000));
        assert!(state.is_playing && state.shuffle_state);
        assert_eq!(state.repeat_state, "context");
        assert_eq!(state.context.unwrap().uri, "spotify:playlist:blend");
        let device = state.device.unwrap();
        assert_eq!(device.name, "External Soloist");
        assert_eq!(device.volume_percent, Some(42));
        let PlayableItem::Track(track) = state.item.unwrap() else {
            panic!("not a track")
        };
        assert_eq!(track.name, "Song");
        assert_eq!(track.artists[0].name, "Artist");
        assert_eq!(track.album.unwrap().name, "Album");
    }

    #[test]
    fn paused_inactive_malformed_and_out_of_order_events_do_not_advance_playback() {
        let mut parser = parser();
        let mut event = event();
        event["status"] = json!("paused");
        let Some(Update::Playback(state)) = parser.read(&event.to_string(), 5000) else {
            panic!("missing paused playback")
        };
        assert!(!state.is_playing);
        assert_eq!(state.progress_ms, Some(199000));
        event["position"]["timestamp_ms"] = json!(999);
        assert!(parser.read(&event.to_string(), 5000).is_none());
        event["is_active"] = json!(false);
        assert!(matches!(
            parser.read(&event.to_string(), 5000),
            Some(Update::Inactive)
        ));
        assert!(parser.read("malformed", 5000).is_none());
        assert!(
            parser
                .read(r#"{"type":"auth_state","logged_in":false}"#, 5000)
                .is_some()
        );
        assert!(parser.read(&event.to_string(), 5000).is_none());
    }

    #[tokio::test]
    #[ignore = "requires the already running local Soloist; pauses it and reads its WebSocket trace"]
    async fn live_receiver_reports_paused_state_over_local_trace() {
        let (config, pid, device_id) =
            source(&AppDirs::installed()).expect("running local Soloist");
        let status = tokio::process::Command::new(&config.binary)
            .args(["ctl", "pause", "-D"])
            .arg(&config.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), status)
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let handle = Handle::default();
        handle.publish(&config, Some(pid), device_id);
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let monitor = tokio::spawn(async move {
            trace(&config, &handle, &|update, _| {
                let _ = send.send(update);
            })
            .await
        });
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Some(Update::Playback(state))) = receive.recv().await {
                    break state;
                }
            }
        })
        .await;
        monitor.abort();
        let _ = monitor.await;
        let state = state.unwrap();
        assert!(!state.is_playing);
        assert!(state.device.unwrap().is_active);
        assert!(state.item.is_some());
    }
}
