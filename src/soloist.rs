//! Optional Linux Soloist receiver. Spotify's binary owns decoding and audio;
//! Spotifast only supervises it and uses the ordinary Connect controls.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::paths::AppDirs;

pub(crate) mod feed;

const POLL: Duration = Duration::from_secs(2);
const STABLE: Duration = Duration::from_secs(60);
const KEY_LIMIT: u64 = 4096;
const STORE_ERROR: &str = "Soloist's API key is unavailable. Unlock Secret Service and run spotifast soloist setup again.";

fn default_name() -> String {
    "Spotilossless Soloist".into()
}

/// Non-secret configuration, independent of Settings and Spotify sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    #[serde(default = "default_name")]
    pub device_name: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let config: Self = serde_json::from_slice(&bytes).map_err(|_| {
                    "Couldn't read soloist.json. Run spotifast soloist setup again.".to_string()
                })?;
                config.validate()?;
                Ok(Some(config))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Couldn't open soloist.json. Check its permissions.".into()),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !self.binary.is_absolute() || !self.data_dir.is_absolute() {
            return Err("Soloist needs absolute binary and data-directory paths. Run spotifast soloist setup again.".into());
        }
        if self.device_name.trim().is_empty() || self.device_name.chars().any(char::is_control) {
            return Err("Choose a nonempty Soloist device name without control characters.".into());
        }
        Ok(())
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| "Couldn't encode Soloist configuration.".to_string())?;
        fastframe_file::write(path, bytes).map_err(|_| {
            "Couldn't save soloist.json. Check the config directory's permissions.".into()
        })
    }
}

/// Deliberately has no Serialize or public accessor. Neither a process command
/// nor its output is ever formatted: Soloist may echo its launch arguments.
struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("ApiKey([redacted])")
    }
}

impl ApiKey {
    fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        let text =
            String::from_utf8(bytes).map_err(|_| "Soloist needs a UTF-8 API key.".to_string())?;
        let key = text.trim_end_matches(['\r', '\n']);
        if key.is_empty() || key.len() > KEY_LIMIT as usize || key.chars().any(char::is_control) {
            return Err(
                "Provide one nonempty Soloist API key on stdin (at most 4096 bytes).".into(),
            );
        }
        Ok(Self(key.to_owned()))
    }
}

/// Run before opening the UI. The key comes from a pipe, never from CLI flags.
/// The caller must refuse a terminal so the key cannot be echoed on screen.
pub fn setup(dirs: &AppDirs, mut config: Config, input: impl BufRead) -> Result<(), String> {
    config.binary = std::fs::canonicalize(&config.binary)
        .map_err(|_| "Couldn't find the Soloist binary. Supply its installed path.".to_string())?;
    std::fs::create_dir_all(&config.data_dir).map_err(|_| {
        "Couldn't create Soloist's data directory. Check its permissions.".to_string()
    })?;
    config.data_dir = std::fs::canonicalize(&config.data_dir)
        .map_err(|_| "Couldn't open Soloist's data directory.".to_string())?;
    config.validate()?;
    check_binary(&config)?;
    let mut bytes = Vec::new();
    // read_until caps input at one line; take also bounds a missing newline.
    input
        .take(KEY_LIMIT + 3)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Couldn't read Soloist's API key from stdin.".to_string())?;
    let key = ApiKey::parse(bytes)?;
    crate::credentials::store_soloist_key(key.0.as_bytes()).map_err(|_| STORE_ERROR.to_string())?;
    std::fs::create_dir_all(&dirs.config)
        .map_err(|_| "Couldn't create Spotifast's config directory.".to_string())?;
    config.save(&dirs.soloist_file())
}

/// Stores a key typed into the setup guide. Blocking: Secret Service may wait
/// for an unlock prompt, so call it off the UI thread.
pub fn save_key(key: &str) -> Result<(), String> {
    let key = ApiKey::parse(key.trim().as_bytes().to_vec())?;
    crate::credentials::store_soloist_key(key.0.as_bytes()).map_err(|_| STORE_ERROR.to_string())
}

/// Spotify's own download for this computer. Spotify asks that Soloist is
/// not redistributed, so it is fetched from here on the listener's request
/// and never mirrored or bundled.
pub fn download_url() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("https://soloist-builds.spotifycdn.com/soloist_release_x86_64.tar.gz"),
        "aarch64" => Some("https://soloist-builds.spotifycdn.com/soloist_release_arm64.tar.gz"),
        "arm" => Some("https://soloist-builds.spotifycdn.com/soloist_release_arm32.tar.gz"),
        _ => None,
    }
}

/// The Soloist page where listeners make their own key.
pub const KEY_PAGE: &str = "https://developer.spotify.com/dashboard";

/// Larger than any release so far (36 MB unpacked), small enough that a
/// wrong answer cannot fill the disk.
pub const DOWNLOAD_LIMIT: u64 = 128 * 1024 * 1024;

/// Spotify stops a build this long after its build date (exit code 10).
const LIFETIME_DAYS: i32 = 90;

/// An installed build, read from `soloist --version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    /// The first day Spotify no longer starts this build.
    pub expires: jiff::civil::Date,
}

impl Build {
    /// `soloist 1.3.10.13 build 1792778483 (20261023) (g19412df6ea) (linux/x86_64)`
    fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        (words.next()? == "soloist").then_some(())?;
        let version = words.next()?.to_owned();
        let date = text
            .split(['(', ')'])
            .find(|part| part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit()))?;
        let built = jiff::civil::Date::new(
            date[..4].parse().ok()?,
            date[4..6].parse().ok()?,
            date[6..].parse().ok()?,
        )
        .ok()?;
        let expires = built
            .checked_add(jiff::Span::new().days(LIFETIME_DAYS))
            .ok()?;
        Some(Self { version, expires })
    }

    /// Whole days left before Spotify stops this build; zero or less once it has.
    pub fn days_left(&self, today: jiff::civil::Date) -> i32 {
        self.expires.since(today).map_or(0, |span| span.get_days())
    }
}

/// Runs the binary's `--version` with a short limit; an expired or foreign
/// binary answers nothing usable.
pub fn read_build(binary: &Path) -> Option<Build> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(child.stdout.take()?, 4096),
        &mut text,
    )
    .ok()?;
    Build::parse(&text)
}

/// What the setup guide shows. Read off the UI thread: it starts the
/// binary and asks Secret Service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub config: Option<Config>,
    pub build: Option<Build>,
    pub key_saved: bool,
    /// The receiver's Spotify Connect id, from its data directory once it
    /// has run. Setup trusts only this id, never a name on the network.
    pub device_id: Option<String>,
    /// This computer's addresses: a receiver answering from elsewhere is
    /// not the one Spotifast runs, whatever it claims.
    pub local_addresses: Vec<std::net::IpAddr>,
    /// Why the configuration could not be read, if it could not.
    pub error: Option<String>,
}

impl Status {
    pub fn ready(&self) -> bool {
        self.build.is_some() && self.key_saved
    }

    /// Whether a receiver found on the network is this computer's Soloist,
    /// and so belongs to this computer's setup guide.
    pub fn is_own_receiver(&self, receiver: &crate::zeroconf::Receiver) -> bool {
        let local =
            receiver.address.is_loopback() || self.local_addresses.contains(&receiver.address);
        !receiver.cast && local && self.device_id.is_some() && receiver.device_id == self.device_id
    }
}

/// Soloist writes its Connect id, a UUID, to `.device_id` on first start.
pub(crate) fn read_device_id(data_dir: &Path) -> Option<String> {
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(std::fs::File::open(data_dir.join(".device_id")).ok()?, 128),
        &mut text,
    )
    .ok()?;
    let id = text.trim();
    let valid = (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    valid.then(|| id.to_owned())
}

pub fn status(dirs: &AppDirs) -> Status {
    let (config, error) = match Config::load(&dirs.soloist_file()) {
        Ok(config) => (config, None),
        Err(error) => (None, Some(error)),
    };
    let build = config
        .as_ref()
        .and_then(|config| read_build(&config.binary));
    let key_saved = crate::credentials::soloist_key().is_ok_and(|key| key.is_some());
    let device_id = config
        .as_ref()
        .and_then(|config| read_device_id(&config.data_dir));
    let local_addresses = if_addrs::get_if_addrs()
        .map(|interfaces| interfaces.iter().map(if_addrs::Interface::ip).collect())
        .unwrap_or_default();
    Status {
        config,
        build,
        key_saved,
        device_id,
        local_addresses,
        error,
    }
}

/// Where the setup guide keeps Soloist: its builds and, unless an earlier
/// setup chose another one, its paired session.
fn home(dirs: &AppDirs) -> PathBuf {
    dirs.state.join("soloist")
}

/// A session paired before the setup guide existed, reused so the listener
/// does not pair again.
fn earlier_session() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    let session = root.join("spotilossless/soloist/session");
    session.join(".device_id").is_file().then_some(session)
}

/// Unpacks a downloaded archive into its own versioned folder and points
/// `soloist.json` at it, keeping an earlier session and name. Blocking.
/// Older builds are removed once the new one answers `--version`.
pub fn install(dirs: &AppDirs, archive: &Path, device_name: &str) -> Result<Build, String> {
    install_with(dirs, archive, device_name, earlier_session())
}

fn install_with(
    dirs: &AppDirs,
    archive: &Path,
    device_name: &str,
    earlier: Option<PathBuf>,
) -> Result<Build, String> {
    const FAILED: &str = "Couldn't unpack Soloist. Try the download again.";
    let home = home(dirs);
    let staging = home.join(format!("unpacking-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|_| "Couldn't create Soloist's folder. Check its permissions.".to_string())?;
    let unpacked = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&staging)
        .args(["--no-same-owner", "--no-same-permissions", "soloist"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let binary = staging.join("soloist");
    let usable =
        unpacked && std::fs::symlink_metadata(&binary).is_ok_and(|meta| meta.file_type().is_file());
    if !usable {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(FAILED.into());
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700));
    }
    let Some(build) = read_build(&binary) else {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("Spotify's download did not run on this computer. Try again later.".into());
    };
    let folder = home.join(format!("build-{}", build.version));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::rename(&staging, &folder).map_err(|_| FAILED.to_string())?;
    let previous = Config::load(&dirs.soloist_file()).ok().flatten();
    let data_dir = match &previous {
        Some(config) => config.data_dir.clone(),
        None => earlier.unwrap_or_else(|| home.join("session")),
    };
    std::fs::create_dir_all(&data_dir)
        .map_err(|_| "Couldn't create Soloist's data folder. Check its permissions.".to_string())?;
    let config = Config {
        binary: folder.join("soloist"),
        data_dir,
        device_name: previous
            .map(|config| config.device_name)
            .unwrap_or_else(|| device_name.to_owned()),
    };
    config.validate()?;
    std::fs::create_dir_all(&dirs.config)
        .map_err(|_| "Couldn't create Spotifast's config directory.".to_string())?;
    config.save(&dirs.soloist_file())?;
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let old_build = name.to_str().is_some_and(|name| name.starts_with("build-"));
            if old_build && entry.path() != folder {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(build)
}

/// The Connect name the setup guide gives Soloist, beside this computer's own.
pub fn device_name_for(computer: &str) -> String {
    format!("{} Lossless", computer.trim())
}

fn check_binary(config: &Config) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let usable = std::fs::metadata(&config.binary)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    if usable {
        Ok(())
    } else {
        Err("Soloist's binary is missing or not executable. Install a current build and run spotifast soloist setup again.".into())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    device_name: Option<String>,
    pid: Option<u32>,
    /// What the receiver's own trace last said about its account; `None`
    /// before it has said anything.
    signed_in: Option<bool>,
    /// Its Spotify Connect id from `<data_dir>/.device_id`, read off the UI
    /// thread. The only way Soloist is recognised: names are not identities.
    device_id: Option<String>,
}

/// Cheap, thread-safe access for the app and a PipeWire tap. PID is cleared on
/// exit/restart. It can change at any time; a tap must handle a vanished process.
#[derive(Clone, Debug, Default)]
pub struct Handle(Arc<RwLock<Snapshot>>);

impl Handle {
    pub fn pid(&self) -> Option<u32> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).pid
    }

    /// Whether the receiver says it has an account, from its local trace.
    pub fn signed_in(&self) -> Option<bool> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).signed_in
    }

    pub fn device_id(&self) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .device_id
            .clone()
    }

    /// Whether a Connect device is this computer's Soloist, by its id. With
    /// no id read yet nothing is, whatever it is called.
    pub fn is_device(&self, id: Option<&str>) -> bool {
        let snapshot = self.0.read().unwrap_or_else(|p| p.into_inner());
        id.is_some() && id == snapshot.device_id.as_deref()
    }

    /// `device_id` comes from [`read_device_id`] on the publisher's thread.
    /// A known id is kept when the receiver stops: it is still the same
    /// device in the account.
    fn publish(&self, config: &Config, pid: Option<u32>, device_id: Option<String>) {
        let mut snapshot = self.0.write().unwrap_or_else(|p| p.into_inner());
        if pid.is_none() || snapshot.pid != pid {
            snapshot.signed_in = None;
        }
        snapshot.device_name = Some(config.device_name.clone());
        snapshot.pid = pid;
        if device_id.is_some() {
            snapshot.device_id = device_id;
        }
    }

    /// Tests and demo captures stand in for a receiver that has written its id.
    #[cfg(any(test, feature = "demo"))]
    pub(crate) fn assume_device_id(&self, id: &str) {
        self.0.write().unwrap_or_else(|p| p.into_inner()).device_id = Some(id.into());
    }
}

/// Owns only its child. Dropping it never stops a reused external receiver.
pub(crate) struct Supervisor {
    stopping: Arc<AtomicBool>,
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Supervisor {
    /// Reuses `handle`, so the app and tap keep one view across restarts.
    pub fn start(
        dirs: AppDirs,
        handle: Handle,
        enabled: bool,
        report: impl Fn(String) + Clone + Send + 'static,
        changed: impl Fn() + Send + 'static,
    ) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let (stop, receiver) = mpsc::channel();
        let thread = if enabled {
            let state = handle.clone();
            let thread_report = report.clone();
            let stopping = stopping.clone();
            match std::thread::Builder::new()
                .name("spotifast-soloist".into())
                .spawn(move || match Config::load(&dirs.soloist_file()) {
                    Ok(Some(config)) => supervise(
                        config,
                        state,
                        receiver,
                        stopping,
                        &thread_report,
                        &changed,
                        read_key,
                    ),
                    Ok(None) => {}
                    Err(error) => thread_report(error),
                }) {
                Ok(thread) => Some(thread),
                Err(_) => {
                    report(
                        "Couldn't start Soloist supervision. Restart Spotifast to try again."
                            .into(),
                    );
                    None
                }
            }
        } else {
            None
        };
        Self {
            stopping,
            stop,
            thread,
        }
    }

    pub fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A Child is the authority to stop a process. Never construct this from a PID.
struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Exponential retry delay survives short-lived successful launches. Only a
/// full minute of continuous life resets it, so an expired build cannot spin.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
}

impl Backoff {
    fn failed(&mut self) -> Duration {
        let seconds = 2_u64.saturating_pow(self.failures.min(6)).min(60);
        self.failures = self.failures.saturating_add(1);
        Duration::from_secs(seconds)
    }

    fn survived(&mut self, elapsed: Duration) {
        if elapsed >= STABLE {
            self.failures = 0;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    pid: u32,
    started: u64,
}

fn identity(pid: u32) -> Option<Identity> {
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    let started = fields.nth(18)?.parse().ok()?;
    Some(Identity { pid, started })
}

/// A live PID alone is insufficient: stale PID files may name another app.
/// Check executable, data directory, and process start time. Never log cmdline,
/// which includes Soloist's key.
fn existing(config: &Config) -> Option<Identity> {
    let pid = std::fs::read_to_string(config.data_dir.join("soloist.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let process = identity(pid)?;
    let executable = std::fs::canonicalize(&config.binary).ok()?;
    if std::fs::read_link(format!("/proc/{pid}/exe")).ok()? != executable {
        return None;
    }
    let args = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<_> = args.split(|byte| *byte == 0).collect();
    let directory = std::fs::canonicalize(&config.data_dir).ok()?;
    // External launchers can use the long form. Relative arguments belong
    // to the receiver's working directory, not Spotifast's.
    let argument = args
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            if matches!(*argument, b"-D" | b"--data-dir") {
                args.get(index + 1).copied()
            } else {
                argument
                    .strip_prefix(b"--data-dir=")
                    .or_else(|| argument.strip_prefix(b"-D").filter(|path| !path.is_empty()))
            }
        })
        .next_back()?;
    use std::os::unix::ffi::OsStrExt;
    let working_dir = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let matching = std::fs::canonicalize(working_dir.join(std::ffi::OsStr::from_bytes(argument)))
        .is_ok_and(|path| path == directory);
    (matching && identity(pid) == Some(process)).then_some(process)
}

fn quiet_command(config: &Config) -> Command {
    let mut command = Command::new(&config.binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn control_ready(config: &Config, stopping: &AtomicBool) -> bool {
    let Ok(child) = quiet_command(config)
        .args(["ctl", "status", "-D"])
        .arg(&config.data_dir)
        .spawn()
    else {
        return false;
    };
    let mut child = OwnedChild(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !stopping.load(Ordering::Relaxed) && Instant::now() < deadline {
        match child.0.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

/// Secret Service can wait for an unlock prompt. Keep that wait off both the
/// UI and supervisor, so quitting still stops an owned receiver promptly.
fn read_key(stopping: &AtomicBool) -> Result<Option<ApiKey>, String> {
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("soloist-keyring".into())
        .spawn(move || {
            let _ = send.send(crate::credentials::soloist_key());
        })
        .map_err(|_| STORE_ERROR.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(STORE_ERROR.into());
        }
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(bytes))) => return ApiKey::parse(bytes).map(Some),
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(STORE_ERROR.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn launch(config: &Config, key: &ApiKey) -> Result<OwnedChild, String> {
    check_binary(config)?;
    // Soloist accepts its key only via argv, not stdin. Never format this
    // Command, inherit output, or log arguments. Linux process inspection can
    // still read argv; Secret Service protects the key only while at rest.
    let mut command = quiet_command(config);
    // A receiver left behind by a Spotifast that was killed would be reused
    // as someone else's by the next one, which then can never restart it
    // with a new key. It ends with this thread, which outlives it anyway.
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe and changes only the child.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    command
        .arg("-n")
        .arg(&config.device_name)
        .arg("-k")
        .arg(&key.0)
        .arg("-D")
        .arg(&config.data_dir)
        .args(["-w", "127.0.0.1:0"])
        .spawn()
        .map(OwnedChild)
        .map_err(|_| {
            "Couldn't start Soloist. Install a current Linux build and check its permissions."
                .into()
        })
}

fn supervise(
    config: Config,
    state: Handle,
    stop: mpsc::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: &impl Fn(String),
    changed: &impl Fn(),
    load_key: impl Fn(&AtomicBool) -> Result<Option<ApiKey>, String>,
) {
    let mut owned: Option<OwnedChild> = None;
    let mut process = None;
    let mut began = Instant::now();
    let mut next_try = Instant::now();
    let mut backoff = Backoff::default();
    let mut key = None;
    let mut last_error = None;
    let publish = |pid| {
        state.publish(&config, pid, read_device_id(&config.data_dir));
        changed();
    };
    publish(None);
    let mut notify = |message: String| {
        if last_error.as_ref() != Some(&message) {
            report(message.clone());
            last_error = Some(message);
        }
    };
    while !stopping.load(Ordering::Relaxed) {
        // try_wait reaps our child, even when its PID file was never written.
        let exited = owned
            .as_mut()
            .is_some_and(|child| !matches!(child.0.try_wait(), Ok(None)));
        if exited || process.is_some_and(|old: Identity| identity(old.pid) != Some(old)) {
            owned = None;
            process = None;
            publish(None);
            next_try = Instant::now() + backoff.failed();
            notify("Soloist stopped. Its build may have expired (builds last about 90 days). Install a current build if restarting does not help. Spotifast will retry automatically.".into());
        }
        if process.is_some() {
            // A first start writes its id a moment after launch.
            if state.device_id().is_none() && read_device_id(&config.data_dir).is_some() {
                publish(process.map(|process: Identity| process.pid));
            }
            backoff.survived(began.elapsed());
        } else if Instant::now() >= next_try {
            // Recheck on every attempt: an external launcher may have started
            // Soloist during backoff. Never replace or terminate that process.
            if let Some(found) = existing(&config) {
                if control_ready(&config, &stopping) {
                    process = Some(found);
                    began = Instant::now();
                    publish(Some(found.pid));
                } else {
                    notify("Soloist is running but its controls are not ready. Check its pairing and current build. Spotifast will keep waiting.".into());
                    next_try = Instant::now() + backoff.failed();
                }
            } else if let Err(error) = check_binary(&config) {
                notify(error);
                next_try = Instant::now() + backoff.failed();
            } else {
                if key.is_none() {
                    match load_key(&stopping) {
                        Ok(Some(loaded)) => key = Some(loaded),
                        Ok(None) => break,
                        Err(error) => {
                            notify(error);
                            // Unlocking may need user input. Do not leave a new
                            // Secret Service request on every retry.
                            break;
                        }
                    }
                }
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(key) = key.as_ref() {
                    match launch(&config, key) {
                        Ok(child) => {
                            let pid = child.0.id();
                            process = identity(pid);
                            began = Instant::now();
                            owned = Some(child);
                            publish(process.map(|process| process.pid));
                            // A child can exit before /proc is read. Reap it
                            // next iteration rather than launching a duplicate.
                            if process.is_none() {
                                process = Some(Identity { pid, started: 0 });
                            }
                        }
                        Err(error) => {
                            notify(error);
                            next_try = Instant::now() + backoff.failed();
                        }
                    }
                }
            }
        }
        if stop.recv_timeout(POLL) != Err(mpsc::RecvTimeoutError::Timeout) {
            break;
        }
    }
    drop(owned);
    publish(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = AppDirs::installed()
                .cache
                .join("soloist-tests")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn config(&self) -> Config {
            Config {
                binary: PathBuf::from("/bin/sh"),
                data_dir: self.0.clone(),
                device_name: default_name(),
            }
        }

        fn script(&self, text: &str) -> Config {
            use std::os::unix::fs::PermissionsExt;
            let mut config = self.config();
            config.binary = self.0.join("receiver");
            std::fs::write(&config.binary, format!("#!/bin/sh\n{text}\n")).unwrap();
            std::fs::set_permissions(&config.binary, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            config
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn build_date_gives_spotifys_ninety_day_expiry() {
        let build = Build::parse(
            "soloist 1.3.10.13 build 1792778483 (20261023) (g19412df6ea) (linux/x86_64)\n",
        )
        .unwrap();
        assert_eq!(build.version, "1.3.10.13");
        assert_eq!(build.expires, jiff::civil::date(2027, 1, 21));
        assert_eq!(build.days_left(jiff::civil::date(2027, 1, 7)), 14);
        assert_eq!(build.days_left(jiff::civil::date(2027, 1, 21)), 0);
        assert!(build.days_left(jiff::civil::date(2027, 2, 1)) < 0);
        for text in [
            "",
            "spotify 1.0 (20261023)",
            "soloist 1.0 (2026-10-23)",
            "soloist 1.0 (20261399)",
        ] {
            assert_eq!(Build::parse(text), None, "{text}");
        }
    }

    #[test]
    fn install_points_setup_at_the_new_build_keeps_the_session_and_drops_old_builds() {
        let scratch = Scratch::new();
        let dirs = AppDirs {
            config: scratch.0.join("config"),
            state: scratch.0.join("state"),
            cache: scratch.0.join("cache"),
        };
        let archive = |version: &str| {
            let source = scratch.0.join(format!("source-{version}"));
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(
                source.join("soloist"),
                format!(
                    "#!/bin/sh\necho 'soloist {version} build 1 (20261023) (g0) (linux/x86_64)'\n"
                ),
            )
            .unwrap();
            std::fs::write(source.join("CHANGELOG.md"), "").unwrap();
            let archive = scratch.0.join(format!("{version}.tar.gz"));
            assert!(
                Command::new("tar")
                    .arg("-czf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&source)
                    .args(["soloist", "CHANGELOG.md"])
                    .status()
                    .unwrap()
                    .success()
            );
            archive
        };
        let earlier = scratch.0.join("paired-session");
        let build = install_with(
            &dirs,
            &archive("1.0"),
            "Desk Lossless",
            Some(earlier.clone()),
        )
        .unwrap();
        assert_eq!(build.version, "1.0");
        let first = Config::load(&dirs.soloist_file()).unwrap().unwrap();
        assert_eq!(first.data_dir, earlier);
        assert_eq!(first.device_name, "Desk Lossless");
        assert!(first.binary.ends_with("build-1.0/soloist"));
        assert!(
            !first.binary.with_file_name("CHANGELOG.md").exists(),
            "only the binary"
        );

        install_with(&dirs, &archive("2.0"), "Renamed", None).unwrap();
        let second = Config::load(&dirs.soloist_file()).unwrap().unwrap();
        assert_eq!(second.data_dir, first.data_dir, "the paired session stays");
        assert_eq!(second.device_name, "Desk Lossless");
        assert!(second.binary.ends_with("build-2.0/soloist"));
        assert!(!first.binary.exists(), "the old build is removed");
        assert_eq!(read_build(&second.binary).unwrap().version, "2.0");

        let broken = scratch.0.join("broken.tar.gz");
        std::fs::write(&broken, "not an archive").unwrap();
        assert!(install_with(&dirs, &broken, "Desk", None).is_err());
        assert_eq!(Config::load(&dirs.soloist_file()).unwrap().unwrap(), second);
    }

    #[test]
    fn only_this_computers_receiver_with_soloists_own_id_belongs_to_the_guide() {
        let scratch = Scratch::new();
        assert_eq!(read_device_id(&scratch.0), None);
        let id = "0f0e0d0c-0b0a-4908-8706-050403020100";
        std::fs::write(scratch.0.join(".device_id"), format!("{id}\n")).unwrap();
        assert_eq!(read_device_id(&scratch.0).as_deref(), Some(id));
        let receiver = |address: [u8; 4], device_id: &str| crate::zeroconf::Receiver {
            name: "Spotifast Lossless".into(),
            device_id: Some(device_id.into()),
            address: std::net::IpAddr::from(address),
            port: 4070,
            path: "/".into(),
            cast: false,
        };
        let status = Status {
            device_id: Some(id.into()),
            local_addresses: vec![std::net::IpAddr::from([192, 168, 1, 20])],
            ..Default::default()
        };
        assert!(status.is_own_receiver(&receiver([127, 0, 0, 1], id)));
        assert!(status.is_own_receiver(&receiver([192, 168, 1, 20], id)));
        assert!(
            !status.is_own_receiver(&receiver([192, 168, 1, 99], id)),
            "same id from another machine"
        );
        assert!(
            !status.is_own_receiver(&receiver([127, 0, 0, 1], "another-id")),
            "same name, another receiver"
        );
        let unknown = Status {
            device_id: None,
            ..status.clone()
        };
        assert!(!unknown.is_own_receiver(&receiver([127, 0, 0, 1], id)));
        for bad in [
            "",
            "short",
            "not hex at all, but long enough!!",
            &"a".repeat(65),
        ] {
            std::fs::write(scratch.0.join(".device_id"), bad).unwrap();
            assert_eq!(read_device_id(&scratch.0), None, "{bad}");
        }
    }

    #[test]
    fn a_typed_key_is_trimmed_and_checked_before_it_is_stored() {
        for key in ["", "   ", "a\u{7}b"] {
            assert!(save_key(key).is_err());
        }
    }

    #[test]
    fn config_round_trip_is_atomic_and_keeps_only_nonsecrets() {
        let scratch = Scratch::new();
        let config = scratch.config();
        let path = scratch.0.join("soloist.json");
        assert_eq!(Config::load(&path).unwrap(), None);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Some(config.clone()));
        let mut replacement = config;
        replacement.device_name = "Studio".into();
        replacement.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Some(replacement));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(json.get("api_key").is_none());
    }

    #[test]
    fn older_configs_default_the_name_and_ignore_future_fields() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "binary": "/usr/bin/soloist", "data_dir": "/home/listener/soloist", "future": true
        }))
        .unwrap();
        assert_eq!(config.device_name, default_name());
        config.validate().unwrap();
        assert!(serde_json::from_str::<Config>("{}").is_err());
        let mut invalid = config;
        invalid.binary = "relative/soloist".into();
        assert!(invalid.validate().is_err());
        invalid.binary = "/usr/bin/soloist".into();
        invalid.device_name = "\n".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn malformed_config_does_not_echo_its_contents_in_errors() {
        let scratch = Scratch::new();
        let path = scratch.0.join("soloist.json");
        std::fs::write(&path, "not-json-SUPER-SECRET").unwrap();
        assert!(!Config::load(&path).unwrap_err().contains("SUPER-SECRET"));
    }

    #[test]
    fn reuse_requires_live_pid_matching_executable_and_data_directory() {
        let scratch = Scratch::new();
        let config = scratch.config();
        for flag in ["-D", "--data-dir"] {
            let child = Command::new(&config.binary)
                .args(["-c", "read ignored", "--", flag])
                .arg(&config.data_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let child = OwnedChild(child);
            std::fs::write(
                config.data_dir.join("soloist.pid"),
                child.0.id().to_string(),
            )
            .unwrap();
            // Wait for exec; /proc may initially still describe the forked test runner.
            let deadline = Instant::now() + Duration::from_secs(2);
            while existing(&config).is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let found = existing(&config).expect("matching receiver reused");
            assert_eq!(found.pid, child.0.id());
            assert!(found.started > 0);
            assert_ne!(
                identity(found.pid),
                Some(Identity {
                    started: found.started + 1,
                    ..found
                })
            );
            let mut wrong = config.clone();
            wrong.binary = "/bin/sleep".into();
            assert_eq!(existing(&wrong), None);
            let elsewhere = Scratch::new();
            wrong = config.clone();
            wrong.data_dir = elsewhere.0.clone();
            std::fs::write(wrong.data_dir.join("soloist.pid"), found.pid.to_string()).unwrap();
            assert_eq!(existing(&wrong), None);
            drop(child);
            assert_eq!(existing(&config), None);
            for pid in ["0", "1", "4294967295", "not a pid"] {
                std::fs::write(config.data_dir.join("soloist.pid"), pid).unwrap();
                assert_eq!(existing(&config), None);
            }
        }
    }

    #[test]
    fn restart_delay_grows_caps_and_resets_only_after_stable_life() {
        let mut backoff = Backoff::default();
        for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
            assert_eq!(backoff.failed(), Duration::from_secs(seconds));
            backoff.survived(Duration::from_secs(59));
        }
        backoff.survived(STABLE);
        assert_eq!(backoff.failed(), Duration::from_secs(1));
        backoff.failures = u32::MAX;
        assert_eq!(backoff.failed(), Duration::from_secs(60));
    }

    #[test]
    fn secret_never_appears_in_debug_or_launch_error() {
        let key = ApiKey::parse(b"dummy-sensitive-key\r\n".to_vec()).unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        let scratch = Scratch::new();
        let mut config = scratch.config();
        config.binary = scratch.0.join("missing");
        let error = match launch(&config, &key) {
            Ok(_) => panic!("missing binary launched"),
            Err(error) => error,
        };
        assert!(!error.contains(&key.0));
        for bytes in [vec![], b"a\nb".to_vec(), vec![b'a'; 4097], vec![0xff]] {
            assert!(ApiKey::parse(bytes).is_err());
        }
    }

    #[test]
    fn a_launched_receiver_ends_with_the_thread_that_started_it() {
        let scratch = Scratch::new();
        let config = scratch.script("exec sleep 30");
        let pid = std::thread::spawn(move || {
            let child = launch(&config, &ApiKey("dummy-key".into())).unwrap();
            let pid = child.0.id();
            // Skips `OwnedChild`'s kill, as a killed Spotifast would.
            std::mem::forget(child);
            pid
        })
        .join()
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let ended = loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let running = state
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z");
            if !running || Instant::now() >= deadline {
                break !running;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // SAFETY: `pid` is this process's own child, reaped once here.
        unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0) };
        assert!(ended, "the receiver outlived the thread that started it");
    }

    #[test]
    fn handle_knows_soloist_by_its_id_never_by_its_name() {
        let scratch = Scratch::new();
        let mut config = scratch.config();
        config.device_name = "My receiver".into();
        let handle = Handle::default();
        assert!(!handle.is_device(Some("soloist-id")), "no id read yet");
        assert!(!handle.is_device(None));
        handle.publish(&config, Some(123), None);
        assert!(
            !handle.is_device(Some("soloist-id")),
            "a running name is not an id"
        );
        handle.publish(&config, Some(123), Some("soloist-id".into()));
        assert!(handle.is_device(Some("soloist-id")));
        assert!(!handle.is_device(Some("another-id")));
        assert!(!handle.is_device(None));
        assert_eq!(handle.clone().pid(), Some(123));
        handle.publish(&config, None, None);
        assert_eq!(handle.pid(), None);
        assert!(
            handle.is_device(Some("soloist-id")),
            "still the account's device"
        );
    }

    struct RunningFixture {
        supervisor: Supervisor,
        handle: Handle,
        changed: mpsc::Receiver<(Option<u32>, Instant)>,
        reports: Arc<Mutex<Vec<String>>>,
    }

    fn run_fixture(
        config: Config,
        load_key: impl Fn(&AtomicBool) -> Result<Option<ApiKey>, String> + Send + 'static,
    ) -> RunningFixture {
        let state = Handle::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (stop, receiver) = mpsc::channel();
        let (changes, changed) = mpsc::channel();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let report = reports.clone();
        let worker_state = state.clone();
        let worker_stopping = stopping.clone();
        let thread = std::thread::spawn(move || {
            let publish_state = worker_state.clone();
            supervise(
                config,
                worker_state,
                receiver,
                worker_stopping,
                &|message| report.lock().unwrap().push(message),
                &|| {
                    let _ = changes.send((publish_state.pid(), Instant::now()));
                },
                load_key,
            );
        });
        RunningFixture {
            handle: state,
            supervisor: Supervisor {
                stopping,
                stop,
                thread: Some(thread),
            },
            changed,
            reports,
        }
    }

    fn next_pid(changed: &mpsc::Receiver<(Option<u32>, Instant)>) -> (u32, Instant) {
        loop {
            let (pid, at) = changed.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(pid) = pid {
                return (pid, at);
            }
        }
    }

    #[test]
    fn crashed_child_restarts_with_delay_and_errors_do_not_include_key() {
        let scratch = Scratch::new();
        let config = scratch.script("sleep 0.2\nexit 42");
        let RunningFixture {
            mut supervisor,
            handle,
            changed,
            reports,
        } = run_fixture(config, |_| Ok(Some(ApiKey("dummy-sensitive-key".into()))));
        let (first, at) = next_pid(&changed);
        let (second, later) = next_pid(&changed);
        assert_ne!(first, second);
        assert!(later.duration_since(at) >= Duration::from_secs(1));
        supervisor.shutdown();
        assert_eq!(handle.pid(), None);
        assert_eq!(identity(second), None);
        let reports = reports.lock().unwrap();
        assert_eq!(
            reports.len(),
            1,
            "repeated errors must not flood the banner"
        );
        assert!(!reports[0].contains("dummy-sensitive-key"));
    }

    /// Read-only smoke test against a separately launched, already paired
    /// receiver. Refuse to launch anything if its PID cannot be reused.
    #[test]
    #[ignore = "requires an already running Soloist and SPOTIFAST_SOLOIST_TEST_BINARY / SPOTIFAST_SOLOIST_TEST_DATA_DIR"]
    fn live_receiver_is_reused_and_survives_shutdown() {
        let config = Config {
            binary: std::env::var_os("SPOTIFAST_SOLOIST_TEST_BINARY")
                .unwrap()
                .into(),
            data_dir: std::env::var_os("SPOTIFAST_SOLOIST_TEST_DATA_DIR")
                .unwrap()
                .into(),
            device_name: default_name(),
        };
        let before = existing(&config).expect("live receiver with matching data directory");
        let RunningFixture {
            mut supervisor,
            handle,
            changed,
            ..
        } = run_fixture(config, |_| {
            panic!("an externally owned receiver must not need a key or a new launch")
        });
        let (pid, _) = next_pid(&changed);
        assert_eq!(pid, before.pid);
        supervisor.shutdown();
        assert_eq!(handle.pid(), None);
        assert_eq!(
            identity(pid),
            Some(before),
            "external process survives shutdown"
        );
    }

    #[test]
    fn shutdown_stops_only_the_owned_process() {
        let scratch = Scratch::new();
        let config = scratch.script("exec sleep 60");
        let external = OwnedChild(Command::new("sleep").arg("60").spawn().unwrap());
        let RunningFixture {
            mut supervisor,
            changed,
            ..
        } = run_fixture(config, |_| Ok(Some(ApiKey("dummy-sensitive-key".into()))));
        let (pid, _) = next_pid(&changed);
        supervisor.shutdown();
        assert_eq!(identity(pid), None);
        assert!(identity(external.0.id()).is_some());
    }
}
