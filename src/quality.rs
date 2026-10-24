//! The playback quality Spotify reports for the device that plays.
//!
//! Spotify's Connect cluster carries the active player's
//! `PlayerState.playback_quality`: the bitrate level the device is playing
//! and the level it aims for. Spotify's own apps show their "Lossless" and
//! "Lossless 24-bit" labels from it. Spotifast reads the same field from the
//! cluster updates its session already receives, so a label shown here is
//! what the device itself reported, never a guess from its name or settings.
//! librespot plays at most 320 kbit/s Ogg Vorbis and never reports lossless;
//! a lossless report can only come from another device, such as Soloist.

use std::sync::{Arc, Mutex};

use librespot_protocol::connect::Cluster;
use librespot_protocol::player::BitrateLevel;

/// A bitrate level a device reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Low,
    Normal,
    High,
    VeryHigh,
    /// Lossless FLAC, 16-bit.
    Lossless,
    /// Lossless FLAC, up to 24-bit.
    Lossless24,
}

impl Level {
    fn from_protocol(level: BitrateLevel) -> Option<Self> {
        Some(match level {
            BitrateLevel::unknown_bitrate_level => return None,
            BitrateLevel::low => Self::Low,
            BitrateLevel::normal => Self::Normal,
            BitrateLevel::high => Self::High,
            BitrateLevel::very_high => Self::VeryHigh,
            BitrateLevel::hifi => Self::Lossless,
            BitrateLevel::hifi24 => Self::Lossless24,
        })
    }

    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Lossless | Self::Lossless24)
    }

    /// The label Spotify's apps show for a lossless level. Other levels get
    /// none: the interface only marks lossless playback.
    pub fn label(self, locale: crate::i18n::Locale) -> Option<String> {
        use crate::i18n::gettext;
        Some(
            match self {
                Self::Lossless => gettext(locale, "Lossless"),
                Self::Lossless24 => gettext(locale, "Lossless 24-bit"),
                _ => return None,
            }
            .into_owned(),
        )
    }
}

/// What the active device last reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reported {
    /// The Connect device the report describes.
    pub device_id: String,
    /// The level it plays.
    pub level: Level,
    /// The level it aims for, when Spotify says.
    pub target: Option<Level>,
}

/// What the session last heard: the active device's report, and which
/// devices say they can play lossless.
#[derive(Debug, Default)]
pub struct State {
    reported: Option<Reported>,
    lossless: std::collections::BTreeSet<String>,
    /// Every device the cluster lists: those signed in to this account.
    devices: std::collections::BTreeSet<String>,
}

/// Shared between the session's task and the interface.
pub type Shared = Arc<Mutex<State>>;

/// The report from this process's playback session. One session runs at a
/// time, and each new one starts watching here.
pub fn current() -> &'static Shared {
    static CURRENT: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();
    CURRENT.get_or_init(Shared::default)
}

/// Follows the cluster updates `session` receives until it ends. librespot's
/// Connect task keeps its own subscription; the dealer hands each update to
/// every subscriber.
pub fn watch(session: &librespot_core::Session, shared: &Shared) {
    use futures_util::StreamExt;
    use librespot_core::dealer::protocol::Message;
    use librespot_protocol::connect::ClusterUpdate;

    let updates = session.dealer().listen_for(
        "hm://connect-state/v1/cluster",
        Message::from_raw::<ClusterUpdate>,
    );
    let mut updates = match updates {
        Ok(updates) => updates,
        Err(error) => {
            log::warn!("Couldn't follow the playback quality Spotify reports: {error}");
            return;
        }
    };
    *shared.lock().unwrap_or_else(|p| p.into_inner()) = State::default();
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            if let Ok(update) = update {
                note(&shared, &update.cluster);
            }
        }
    });
}

/// The active device's report in a cluster, if it made one.
pub fn from_cluster(cluster: &Cluster) -> Option<Reported> {
    if cluster.active_device_id.is_empty() {
        return None;
    }
    let quality = cluster.player_state.playback_quality.as_ref()?;
    let level = Level::from_protocol(quality.bitrate_level.enum_value().ok()?)?;
    let target = quality
        .target_bitrate_level
        .enum_value()
        .ok()
        .and_then(Level::from_protocol);
    Some(Reported {
        device_id: cluster.active_device_id.clone(),
        level,
        target,
    })
}

/// The devices in a cluster that say they can play lossless: Spotify marks
/// them as fully supporting HiFi, or as supporting a lossless quality.
pub fn lossless_devices(cluster: &Cluster) -> std::collections::BTreeSet<String> {
    use librespot_protocol::media::AudioQuality;
    cluster
        .device
        .iter()
        .filter(|(_, device)| {
            let capabilities = &device.capabilities;
            capabilities.supports_hifi.fully_supported
                || matches!(
                    capabilities.supported_audio_quality.enum_value(),
                    Ok(AudioQuality::HIFI | AudioQuality::HIFI_24)
                )
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Stores what a cluster update says. An update without a report clears it,
/// so an old device's report never stays on a new one. An update without
/// devices keeps the ones known.
pub fn note(shared: &Shared, cluster: &Cluster) {
    let reported = from_cluster(cluster);
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    if state.reported != reported {
        log::info!(
            "playback quality reported: {:?}",
            reported
                .as_ref()
                .map(|reported| (reported.level, reported.target))
        );
        state.reported = reported;
    }
    if !cluster.device.is_empty() {
        let lossless = lossless_devices(cluster);
        if state.lossless != lossless {
            log::info!("devices that can play lossless: {}", lossless.len());
            state.lossless = lossless;
        }
        state.devices = cluster.device.keys().cloned().collect();
    }
}

/// Whether the session's last cluster lists `device_id`, so it is signed in
/// to this account. Pushed by Spotify, so it needs no Web API read.
pub fn in_account(shared: &Shared, device_id: &str) -> bool {
    shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .devices
        .contains(device_id)
}

/// Whether `device_id` said it can play lossless.
pub fn supports_lossless(shared: &Shared, device_id: &str) -> bool {
    shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .lossless
        .contains(device_id)
}

/// The report for `device_id`, if that device made the latest one.
pub fn for_device(shared: &Shared, device_id: &str) -> Option<Reported> {
    shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .reported
        .clone()
        .filter(|reported| reported.device_id == device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::player::{PlaybackQuality, PlayerState};
    use protobuf::MessageField;

    fn cluster(active: &str, level: BitrateLevel, target: BitrateLevel) -> Cluster {
        Cluster {
            active_device_id: active.into(),
            player_state: MessageField::some(PlayerState {
                playback_quality: MessageField::some(PlaybackQuality {
                    bitrate_level: level.into(),
                    target_bitrate_level: target.into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reads_the_active_devices_reported_level() {
        let reported = from_cluster(&cluster(
            "soloist",
            BitrateLevel::hifi24,
            BitrateLevel::hifi24,
        ))
        .unwrap();
        assert_eq!(reported.device_id, "soloist");
        assert_eq!(reported.level, Level::Lossless24);
        assert_eq!(reported.target, Some(Level::Lossless24));
        assert!(reported.level.is_lossless());

        let reported = from_cluster(&cluster(
            "phone",
            BitrateLevel::very_high,
            BitrateLevel::unknown_bitrate_level,
        ))
        .unwrap();
        assert_eq!(reported.level, Level::VeryHigh);
        assert_eq!(reported.target, None);
        assert!(!reported.level.is_lossless());
    }

    #[test]
    fn no_report_without_a_level_or_an_active_device() {
        assert_eq!(
            from_cluster(&cluster(
                "soloist",
                BitrateLevel::unknown_bitrate_level,
                BitrateLevel::hifi24,
            )),
            None
        );
        assert_eq!(
            from_cluster(&cluster("", BitrateLevel::hifi24, BitrateLevel::hifi24)),
            None
        );
        assert_eq!(from_cluster(&Cluster::default()), None);
    }

    #[test]
    fn devices_that_report_lossless_support_are_known() {
        use librespot_protocol::connect::{Capabilities, CapabilitySupportDetails, DeviceInfo};
        use librespot_protocol::media::AudioQuality;

        let device = |hifi: bool, quality: AudioQuality| DeviceInfo {
            capabilities: MessageField::some(Capabilities {
                supports_hifi: MessageField::some(CapabilitySupportDetails {
                    fully_supported: hifi,
                    ..Default::default()
                }),
                supported_audio_quality: quality.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut cluster = Cluster::default();
        cluster
            .device
            .insert("soloist".into(), device(true, AudioQuality::DEFAULT));
        cluster
            .device
            .insert("speaker".into(), device(false, AudioQuality::HIFI_24));
        cluster
            .device
            .insert("spotifast".into(), device(false, AudioQuality::VERY_HIGH));
        cluster.device.insert("phone".into(), DeviceInfo::default());
        let shared = Shared::default();
        note(&shared, &cluster);
        assert!(in_account(&shared, "phone"), "any device the cluster lists");
        assert!(!in_account(&shared, "elsewhere"));
        note(&shared, &Cluster::default());
        assert!(
            in_account(&shared, "phone"),
            "an update without devices keeps them"
        );
        assert_eq!(
            lossless_devices(&cluster).into_iter().collect::<Vec<_>>(),
            ["soloist", "speaker"]
        );

        let shared = Shared::default();
        note(&shared, &cluster);
        assert!(supports_lossless(&shared, "soloist"));
        assert!(!supports_lossless(&shared, "spotifast"));
        // An update that lists no devices keeps the ones known.
        note(&shared, &Cluster::default());
        assert!(supports_lossless(&shared, "soloist"));
    }

    #[test]
    fn a_report_belongs_to_its_device_and_an_update_without_one_clears_it() {
        let shared = Shared::default();
        note(
            &shared,
            &cluster("soloist", BitrateLevel::hifi, BitrateLevel::hifi24),
        );
        assert_eq!(
            for_device(&shared, "soloist").map(|reported| reported.level),
            Some(Level::Lossless)
        );
        assert_eq!(for_device(&shared, "phone"), None);
        note(&shared, &Cluster::default());
        assert_eq!(for_device(&shared, "soloist"), None);
    }
}
