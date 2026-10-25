//! Lossless playback through Spotify's Soloist receiver (Linux): the setup
//! guide's state, and keeping the receiver running while Lossless is chosen.
//!
//! Soloist is Spotify's own player. Spotifast never ships it: the guide
//! fetches Spotify's download on request (and again before Spotify stops a
//! build), stores the listener's own key in Secret Service, and waits for
//! the listener to pair it once in Spotify. Soloist restores that session
//! on later starts.

use super::*;

/// How long the guide waits for Soloist to join the account.
pub(super) const PAIRING_LIMIT: Duration = Duration::from_secs(90);
/// How often the network is searched for Soloist while it waits.
const PAIRING_SEARCH: Duration = Duration::from_secs(5);
/// How long a failed download of Spotify's current build waits to try again.
const AUTO_UPDATE_RETRY: Duration = Duration::from_secs(15 * 60);
/// The longest wait between status reads that may settle by themselves.
const STATUS_RETRY_LIMIT: Duration = Duration::from_secs(120);
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
    /// When to read the status again, because it may settle by itself: the
    /// build was slow to answer or no keyring was running yet.
    pub(super) retry_at: Option<Instant>,
    retries: u32,
    /// What Soloist's local feed last said about its sign-in.
    signed_in: Option<bool>,
    /// The receiver process last seen. Another one, or none, has an empty
    /// player: Soloist keeps no place across a restart.
    pid: Option<u32>,
    /// Where Soloist was in the music, noted while it plays.
    place: Option<Place>,
    /// Where a receiver that stopped was. Its next start picks up here: at
    /// once if it was playing, otherwise at the next press of play.
    pub(super) interrupted: Option<Place>,
    /// This run has fetched Spotify's current build on its own once, so a
    /// build Spotify has not replaced yet cannot download in a loop.
    auto_updated: bool,
    /// The install under way is that one, not one the listener asked for.
    updating_itself: bool,
    /// When a download of its own that failed, as when Spotify's CDN cannot
    /// be reached, is tried again.
    pub(super) auto_update_at: Option<Instant>,
    /// A resume Spotify has yet to answer.
    pub(super) resuming: Option<Place>,
    /// How many times this place has been resumed so far.
    resume_tries: u32,
    /// When to try a refused resume again.
    pub(super) resume_at: Option<Instant>,
}

/// A song and the point in it, as Soloist last played it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Place {
    uri: String,
    context: Option<String>,
    position_ms: u32,
    playing: bool,
}

/// How many times a resume is tried before Spotify's refusal is shown.
const RESUME_TRIES: u32 = 5;
/// How long a receiver that has only just started may stay unknown to
/// Spotify's Connect service between tries.
const RESUME_RETRY: Duration = Duration::from_secs(2);

impl App {
    /// Soloist's Connect id once it plays for this account. Found by the id
    /// Soloist keeps on disk, never by its name. Spotify places it in the
    /// account: the session's Connect cluster lists it the moment it joins,
    /// or the Web API's device list does. Soloist's own local feed has the
    /// last word on whether it is signed in, so a lagging list neither hides
    /// it once it has joined nor keeps it after it signs out.
    pub fn soloist_device_id(&self) -> Option<String> {
        let id = self
            .lossless
            .status
            .as_ref()
            .and_then(|status| status.device_id.clone())
            .or_else(|| self.backend.soloist().device_id())?;
        if self.backend.soloist().signed_in() == Some(false) {
            return None;
        }
        let listed = self
            .devices
            .iter()
            .any(|device| device.id.as_ref() == Some(&id))
            || crate::quality::in_account(crate::quality::current(), &id);
        listed.then_some(id)
    }

    /// Keeps Soloist's row while it plays for this account, though a device
    /// list read before it joined, or one Spotify answers late, leaves it
    /// out. Without the row a lagging read would undo the choice of Soloist.
    pub(super) fn keep_soloist_row(&mut self) {
        let Some(id) = self.soloist_device_id() else {
            return;
        };
        if self
            .devices
            .iter()
            .any(|device| device.id.as_ref() == Some(&id))
        {
            return;
        }
        let name = self
            .backend
            .soloist()
            .device_name()
            .or_else(|| {
                self.lossless
                    .status
                    .as_ref()
                    .and_then(|status| status.config.as_ref())
                    .map(|config| config.device_name.clone())
            })
            .unwrap_or_else(|| crate::soloist::device_name_for(&self.settings.device_name));
        self.devices.push(Device {
            id: Some(id),
            name,
            is_restricted: true,
            kind: "computer".into(),
            ..Default::default()
        });
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
        if self.settings.lossless
            && !self.lossless.status_requested
            && self
                .lossless
                .retry_at
                .is_some_and(|at| at <= Instant::now())
        {
            self.lossless.retry_at = None;
            self.request_soloist_status();
        } else if let Some(at) = self.lossless.retry_at {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
        // Soloist says on its local feed the moment it signs in; ask Spotify
        // for the account's devices then rather than at the next poll.
        let signed_in = self.backend.soloist().signed_in();
        if signed_in != self.lossless.signed_in {
            self.lossless.signed_in = signed_in;
            if signed_in == Some(true) {
                self.devices_fetched_at = None;
                self.refresh_devices();
            }
        }
        self.keep_soloist_row();
        self.follow_soloist_place(ctx);
        let wanted = self.lossless_wanted();
        if self.lossless.status.is_some() && self.lossless.supervised != Some(wanted) {
            self.backend.supervise_soloist(wanted);
            self.lossless.supervised = Some(wanted);
        }
        if !wanted {
            self.lossless.pairing = None;
        }
        // Spotify stops each build after about 90 days, and can stop one
        // sooner. Its current build is fetched before then, and at once when
        // it stops this one, keeping the pairing. The listener hears about it
        // only if that leaves the build still about to stop.
        let stopped = self.backend.soloist().expired();
        let expiring = self.soloist_expiring();
        let retry_due = self
            .lossless
            .auto_update_at
            .is_none_or(|at| at <= Instant::now());
        if let Some(at) = self.lossless.auto_update_at {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
        if self.settings.lossless
            && (stopped || expiring.is_some())
            && !self.lossless.auto_updated
            && retry_due
            && !self.lossless.installing
            && self
                .lossless
                .status
                .as_ref()
                .is_some_and(|status| status.config.is_some())
        {
            self.lossless.auto_updated = true;
            self.lossless.updating_itself = true;
            self.install_soloist();
        } else if let Some(days) = expiring
            && wanted
            && (self.lossless.auto_updated || self.lossless.auto_update_at.is_some())
            && !self.lossless.installing
            // The date read after the update decides, not the one before it.
            && !self.lossless.status_requested
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
        let mut status = status;
        if let Some(old) = &self.lossless.status
            && old.config == status.config
        {
            // The same build that answered before was slow this time, not
            // gone: a busy computer can outlast the limit. A keyring that
            // cannot answer now, perhaps locked again, does not mean the key
            // has left it. Neither stops a receiver that runs.
            if status.build.is_none()
                && status
                    .build_error
                    .as_ref()
                    .is_some_and(|failure| failure.slow)
            {
                status.build.clone_from(&old.build);
            }
            if status.keyring_error.is_some() {
                status.key_saved |= old.key_saved;
            }
        }
        if status.may_settle() {
            let delay = Duration::from_secs(5)
                .saturating_mul(1 << self.lossless.retries.min(5))
                .min(STATUS_RETRY_LIMIT);
            self.lossless.retries = self.lossless.retries.saturating_add(1);
            self.lossless.retry_at = Some(Instant::now() + delay);
        } else {
            self.lossless.retries = 0;
            self.lossless.retry_at = None;
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
        let by_itself = std::mem::take(&mut self.lossless.updating_itself);
        match result {
            Ok(_) => {
                self.lossless.install_error = None;
                // A build the listener fetched is theirs to see; one fetched
                // on its own speaks up only if it is about to stop too.
                if !by_itself {
                    self.lossless.expiry_warned = false;
                }
                self.request_soloist_status();
            }
            Err(error) => {
                // A download of its own that failed is tried again later
                // in this run; the notice says what to do meanwhile.
                if by_itself {
                    self.lossless.auto_updated = false;
                    self.lossless.auto_update_at = Some(Instant::now() + AUTO_UPDATE_RETRY);
                }
                self.lossless.install_error = Some(error);
            }
        }
    }

    /// Notes where Soloist is in the music while it plays. When its process
    /// stops or is replaced, that place is held for the next one, which
    /// starts with an empty player: a plain resume would restart the song.
    fn follow_soloist_place(&mut self, ctx: &egui::Context) {
        if !self.settings.lossless {
            self.lossless.place = None;
            self.lossless.interrupted = None;
            return;
        }
        if self.playing_elsewhere() {
            // Music moved to another device: a later restart of Soloist
            // has nothing of its own to pick up, and must not replace
            // what plays there.
            self.lossless.place = None;
            self.lossless.interrupted = None;
            self.lossless.resuming = None;
            self.lossless.resume_at = None;
            self.lossless.resume_tries = 0;
        }
        let pid = self.backend.soloist().pid();
        if pid != self.lossless.pid {
            let previous = std::mem::replace(&mut self.lossless.pid, pid);
            if previous.is_some() && self.lossless.interrupted.is_none() {
                // What is on show still describes the player that stopped.
                let place = self.soloist_place().or_else(|| self.lossless.place.take());
                log::info!(
                    "Soloist's receiver changed; holding its place: {:?}",
                    place
                        .as_ref()
                        .map(|place| (place.position_ms, place.playing))
                );
                self.lossless.interrupted = place;
            }
            return;
        }
        if pid.is_some() && self.lossless.interrupted.is_none() {
            if let Some(place) = self.soloist_place() {
                self.lossless.place = Some(place);
            }
            return;
        }
        // Back, signed in and in this account: music that was playing
        // carries on from where it stopped.
        if let Some(at) = self.lossless.resume_at {
            let wait = at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                ctx.request_repaint_after(wait);
                return;
            }
        }
        if pid.is_some()
            && self.backend.soloist().signed_in() == Some(true)
            && self
                .lossless
                .interrupted
                .as_ref()
                .is_some_and(|place| place.playing)
            && self.soloist_device_id().is_some()
            && self.soloist_is_target()
        {
            self.resume_interrupted();
        }
    }

    /// Whether what play and pause act on now is Soloist, by its id.
    fn soloist_is_target(&self) -> bool {
        match self.target() {
            Target::Remote(id) => self.backend.soloist().is_device(id.as_deref()),
            Target::Local => false,
        }
    }

    /// Whether music is known to play on a device other than Soloist: one
    /// Spotify or the listener names, or librespot playing here. A target
    /// that only falls back while Soloist restarts does not count.
    fn playing_elsewhere(&self) -> bool {
        match self.target() {
            Target::Remote(Some(id)) => !self.backend.soloist().is_device(Some(&id)),
            Target::Remote(None) => false,
            Target::Local => self.local.is_active(),
        }
    }

    /// Where the playing Soloist is, when Soloist is what plays.
    fn soloist_place(&self) -> Option<Place> {
        let id = self.soloist_device_id()?;
        if self.target() != Target::Remote(Some(id)) {
            return None;
        }
        let now = self.now_playing().filter(|now| !now.local)?;
        Some(Place {
            uri: now.uri,
            context: self.playing_context_uri(),
            position_ms: now.position_ms,
            playing: now.playing,
        })
    }

    /// Plays what a stopped Soloist was playing, from where it was, on the
    /// receiver that replaced it. `false` when nothing was held.
    pub(super) fn resume_interrupted(&mut self) -> bool {
        let Some(place) = self.lossless.interrupted.take() else {
            return false;
        };
        let tries = self.lossless.resume_tries + 1;
        self.lossless.resume_at = None;
        let mut request = match place.context.clone() {
            Some(context) => PlayRequest::context(context).starting_at_uri(place.uri.clone()),
            None => PlayRequest::tracks(vec![place.uri.clone()]),
        };
        request.position_ms = place.position_ms;
        log::info!(
            "Resuming Soloist from {} ms (try {tries})",
            place.position_ms
        );
        self.play_request(request, false);
        // After the play: starting one clears what was held.
        self.lossless.resuming = Some(Place {
            playing: true,
            ..place
        });
        self.lossless.resume_tries = tries;
        true
    }

    /// Spotify answers 404 to a play for a receiver that started a moment
    /// ago, until its Connect service knows the new one. A refused resume is
    /// tried again shortly, without a notice, until it has had its tries.
    /// `true` when the refusal is taken care of here.
    pub(super) fn retry_soloist_resume(&mut self) -> bool {
        let Some(place) = self.lossless.resuming.take() else {
            return false;
        };
        if !place.playing {
            // Paused while Spotify answered: held for the next play.
            self.clear_play_pending();
            self.lossless.resume_tries = 0;
            self.lossless.interrupted = Some(place);
            return true;
        }
        if self.lossless.resume_tries >= RESUME_TRIES {
            self.lossless.resume_tries = 0;
            return false;
        }
        self.lossless.interrupted = Some(place);
        self.lossless.resume_at = Some(Instant::now() + RESUME_RETRY);
        true
    }

    /// A resume Spotify accepted: nothing is left to try again. One paused
    /// while Spotify answered is paused now.
    pub(super) fn soloist_resumed(&mut self) {
        self.lossless.resume_tries = 0;
        if let Some(place) = self.lossless.resuming.take()
            && !place.playing
        {
            self.remote(RemoteAction::Pause, self.backend.soloist().device_id());
        }
    }

    /// Play or pause pressed while a stopped Soloist's place is being picked
    /// up, before Soloist has anything to pause. Paused, the place waits for
    /// the next press of play; played while Spotify answers the resume, it
    /// plays once Spotify does. `false` when the press is not for the place:
    /// none is being picked up, or play starts a held one afresh.
    pub(super) fn want_soloist_resume(&mut self, playing: bool) -> bool {
        if let Some(place) = self.lossless.resuming.as_mut() {
            place.playing = playing;
            return true;
        }
        match self.lossless.interrupted.as_mut() {
            Some(place) if !playing => {
                place.playing = false;
                self.lossless.resume_at = None;
                true
            }
            _ => false,
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
