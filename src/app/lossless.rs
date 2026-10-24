//! Lossless playback through Spotify's Soloist receiver (Linux): the setup
//! guide's state, and keeping the receiver running while Lossless is chosen.
//!
//! Soloist is Spotify's own player. Spotifast never ships it: the guide
//! fetches Spotify's download on request, stores the listener's own key in
//! Secret Service, and waits for the listener to pair it once in Spotify.
//! Soloist restores that session on later starts.

use super::*;

/// How long the guide waits for Soloist to join the account.
pub(super) const PAIRING_LIMIT: Duration = Duration::from_secs(90);
/// How often the network is searched for Soloist while it waits.
const PAIRING_SEARCH: Duration = Duration::from_secs(5);
/// A build this close to Spotify's cut-off is worth updating.
pub const EXPIRY_WARNING_DAYS: i32 = 14;

/// No `Debug`: `key` holds what the listener typed until it is sent.
#[derive(Default)]
pub struct LosslessSetup {
    /// Unknown until the backend has looked.
    pub status: Option<crate::soloist::Status>,
    pub installing: bool,
    pub install_error: Option<String>,
    /// The key as typed; emptied the moment it is sent.
    pub key: String,
    pub saving_key: bool,
    pub key_error: Option<String>,
    /// Lossless was turned on and Soloist has no account yet.
    pub pairing: Option<Instant>,
    pub pairing_failed: bool,
    searched_at: Option<Instant>,
    /// A status read is on its way; another would repeat its process start
    /// and Secret Service read.
    pub(super) status_requested: bool,
    pub(super) status_again: bool,
    /// When music playing on librespot was last moved to Soloist.
    handed_off_at: Option<Instant>,
    /// What the supervisor was last told; `None` makes the next frame tell it.
    pub(super) supervised: Option<bool>,
    expiry_warned: bool,
}

impl App {
    /// Soloist's row in the account's device list, once it has signed in.
    /// Found by the id Soloist keeps on disk, never by its name.
    pub fn soloist_device_id(&self) -> Option<String> {
        let id = self
            .lossless
            .status
            .as_ref()
            .and_then(|status| status.device_id.clone())
            .or_else(|| self.backend.soloist().device_id())?;
        self.devices
            .iter()
            .find(|device| device.id.as_ref() == Some(&id))
            .and_then(|device| device.id.clone())
    }

    /// One read at a time. A request while one is out (after an install or
    /// a saved key) reads once more when it answers, so nothing stale stays.
    fn request_soloist_status(&mut self) {
        if self.lossless.status_requested {
            self.lossless.status_again = true;
        } else {
            self.lossless.status_requested = true;
            self.backend.send(Command::SoloistStatus);
        }
    }

    pub fn soloist_paired(&self) -> bool {
        self.soloist_device_id().is_some()
    }

    /// Lossless is chosen and set up, so this computer's music goes to Soloist.
    pub fn lossless_wanted(&self) -> bool {
        self.settings.lossless
            && self
                .lossless
                .status
                .as_ref()
                .is_some_and(crate::soloist::Status::ready)
    }

    /// Where this computer's music plays while Lossless is on: Soloist's
    /// Connect id once it has joined the account. Until then, and whenever
    /// it stops, music stays on librespot.
    pub fn lossless_device(&self) -> Option<String> {
        self.lossless_wanted()
            .then(|| self.soloist_device_id())
            .flatten()
    }

    /// Days before Spotify stops the installed build, when that is soon.
    pub fn soloist_expiring(&self) -> Option<i32> {
        let build = self.lossless.status.as_ref()?.build.as_ref()?;
        let days = build.days_left(jiff::Zoned::now().date());
        (days <= EXPIRY_WARNING_DAYS).then_some(days)
    }

    /// Run every frame: starts or stops the receiver to match the setting,
    /// and waits for Spotify to pair it with this account.
    pub(super) fn follow_lossless(&mut self, ctx: &egui::Context) {
        if self.lossless.status.is_none() && !self.lossless.status_requested {
            self.request_soloist_status();
        }
        let wanted = self.lossless_wanted();
        if self.lossless.status.is_some() && self.lossless.supervised != Some(wanted) {
            self.backend.supervise_soloist(wanted);
            self.lossless.supervised = Some(wanted);
        }
        if !wanted {
            self.lossless.pairing = None;
        }
        if let Some(days) = self.soloist_expiring()
            && wanted
            && !self.lossless.expiry_warned
        {
            self.lossless.expiry_warned = true;
            self.toast_error(expiry_message(self.locale, days));
        }
        // Music already playing here moves to Soloist once it can play,
        // keeping its song, position and queue.
        if let Some(device_id) = self.lossless_device()
            && self.local.is_active()
            && !self.transferring_remote()
            && self
                .lossless
                .handed_off_at
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
        {
            self.lossless.handed_off_at = Some(Instant::now());
            let play = self.now_playing().is_some_and(|now| now.playing);
            self.transfer_remote(device_id, play);
        }
        if self.soloist_paired() {
            let waiting = self.lossless.pairing.take().is_some();
            let timed_out = std::mem::take(&mut self.lossless.pairing_failed);
            if wanted && (waiting || timed_out) {
                self.toast(
                    gettext(self.locale, "{name} is ready")
                        .replace("{name}", &self.settings.device_name),
                );
                self.refresh_devices();
            }
            return;
        }
        let Some(since) = self.lossless.pairing else {
            return;
        };
        if since.elapsed() > PAIRING_LIMIT {
            self.lossless.pairing = None;
            self.lossless.pairing_failed = true;
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        if self
            .lossless
            .searched_at
            .is_none_or(|at| at.elapsed() >= PAIRING_SEARCH)
        {
            self.lossless.searched_at = Some(Instant::now());
            self.devices_fetched_at = None;
            self.refresh_devices();
            self.backend.send(Command::DiscoverReceivers);
            // Soloist writes its id on first start, after setup read the status.
            if !self.lossless.status_requested
                && self
                    .lossless
                    .status
                    .as_ref()
                    .is_some_and(|status| status.device_id.is_none())
            {
                self.request_soloist_status();
            }
        }
    }

    pub(super) fn on_soloist_status(&mut self, status: crate::soloist::Status) {
        self.lossless.status_requested = false;
        if std::mem::take(&mut self.lossless.status_again) {
            self.request_soloist_status();
        }
        // Addresses and the id do not change what the receiver runs with.
        let changed = self.lossless.status.as_ref().is_none_or(|old| {
            (&old.config, &old.build, old.key_saved)
                != (&status.config, &status.build, status.key_saved)
        });
        self.lossless.status = Some(status);
        if changed && self.lossless.supervised == Some(true) {
            // A new build or key: the receiver starts again with it.
            self.lossless.supervised = None;
        }
    }

    pub(super) fn on_soloist_installed(&mut self, result: Result<crate::soloist::Build, String>) {
        self.lossless.installing = false;
        match result {
            Ok(_) => {
                self.lossless.install_error = None;
                self.lossless.expiry_warned = false;
                self.request_soloist_status();
            }
            Err(error) => self.lossless.install_error = Some(error),
        }
    }

    pub(super) fn on_soloist_key_saved(&mut self, result: Result<(), String>) {
        self.lossless.saving_key = false;
        match result {
            Ok(()) => {
                self.lossless.key_error = None;
                // A replaced key leaves `key_saved` as it was, so the status
                // read cannot tell the receiver to start again with it.
                if self.lossless.supervised == Some(true) {
                    self.lossless.supervised = None;
                }
                self.request_soloist_status();
            }
            Err(error) => self.lossless.key_error = Some(error),
        }
    }

    /// The Lossless choice: on at once when set up, otherwise the guide.
    pub(super) fn choose_lossless(&mut self) {
        let guide_open = matches!(self.dialog, Some(Dialog::LosslessSetup));
        if self.settings.lossless && !guide_open {
            // Chosen again: the guide, to update, reconnect, or replace the
            // key of a Soloist that cannot sign in.
            self.dialog = Some(Dialog::LosslessSetup);
            self.request_soloist_status();
            return;
        }
        if !self
            .lossless
            .status
            .as_ref()
            .is_some_and(crate::soloist::Status::ready)
        {
            self.dialog = Some(Dialog::LosslessSetup);
            self.request_soloist_status();
            return;
        }
        if !self.settings.lossless {
            self.settings.lossless = true;
            self.mark_settings_dirty();
        }
        self.lossless.pairing_failed = false;
        if !self.soloist_paired() {
            self.lossless.pairing = Some(Instant::now());
            self.lossless.searched_at = None;
        }
    }

    pub(super) fn install_soloist(&mut self) {
        if self.lossless.installing {
            return;
        }
        self.lossless.installing = true;
        self.lossless.install_error = None;
        self.backend.send(Command::SoloistInstall {
            device_name: self.settings.device_name.clone(),
        });
    }

    pub(super) fn save_soloist_key(&mut self) {
        let key = std::mem::take(&mut self.lossless.key);
        if key.trim().is_empty() || self.lossless.saving_key {
            return;
        }
        self.lossless.saving_key = true;
        self.lossless.key_error = None;
        self.backend
            .send(Command::SoloistSaveKey(crate::backend::SoloistKey(key)));
    }
}

/// Guide stages for demo captures, without a receiver or Secret Service.
#[cfg(feature = "demo")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DemoStage {
    /// Downloaded, no key yet.
    Started,
    /// Downloaded with a key, waiting for Soloist to join.
    Connecting,
    /// Signed in and chosen: Soloist is this computer.
    On,
}

#[cfg(feature = "demo")]
impl App {
    pub fn demo_lossless(&mut self, stage: DemoStage) {
        let id = "demo-soloist";
        self.lossless.status = Some(crate::soloist::Status {
            build: Some(crate::soloist::Build {
                version: "1.3.10.13".into(),
                expires: jiff::civil::date(2027, 1, 21),
            }),
            key_saved: stage != DemoStage::Started,
            device_id: Some(id.into()),
            ..Default::default()
        });
        // Nothing to start in a capture.
        self.lossless.supervised = Some(self.lossless_wanted());
        match stage {
            DemoStage::Started => {}
            DemoStage::Connecting => {
                self.settings.lossless = true;
                self.lossless.pairing = Some(Instant::now());
                self.lossless.searched_at = Some(Instant::now());
            }
            DemoStage::On => {
                self.settings.lossless = true;
                self.backend.soloist().assume_device_id(id);
                self.devices.push(Device {
                    id: Some(id.into()),
                    name: crate::soloist::device_name_for(&self.settings.device_name),
                    is_restricted: true,
                    volume_percent: Some(40),
                    supports_volume: Some(true),
                    kind: "computer".into(),
                    ..Default::default()
                });
                self.lossless.supervised = Some(true);
            }
        }
    }
}

pub fn expiry_message(locale: Locale, days: i32) -> String {
    if days <= 0 {
        gettext(
            locale,
            "Spotify has stopped this Soloist build. Update it in Settings to keep playing lossless.",
        )
        .into_owned()
    } else {
        ngettext(
            locale,
            // Translators: {count} is a number of days. Soloist is the name of Spotify's player.
            "Spotify stops this Soloist build in {count} day. Update it in Settings.",
            "Spotify stops this Soloist build in {count} days. Update it in Settings.",
            days as u32,
        )
        .replace("{count}", &days.to_string())
    }
}
