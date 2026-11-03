//! MPRIS_Adapter: D-Bus registration, command forwarding, and metadata.
//!
//! When MPRIS integration is enabled in the Config_File (Requirement 11, default
//! off), the TUI exposes an MPRIS media player on the D-Bus session bus so
//! desktop media keys and MPRIS clients can drive playback. This module:
//!
//! - registers an `org.mpris.MediaPlayer2.apple_music_tui` bus name serving the
//!   standard `org.mpris.MediaPlayer2` and `org.mpris.MediaPlayer2.Player`
//!   interfaces (Requirement 11.1);
//! - forwards `Play`/`Pause`/`PlayPause`/`Next`/`Previous`/`Seek`/`SetPosition`
//!   commands to the Playback_Engine via the [`crate::ipc`] client, so the
//!   resulting Playback_State matches the equivalent TUI keybinding action
//!   (Requirements 11.2, 11.6);
//! - publishes track metadata and playback status over D-Bus and emits the
//!   `Seeked` signal after seeks (Requirement 11.3);
//! - degrades gracefully when the D-Bus session bus is unavailable: it logs a
//!   warning and lets the TUI continue every non-MPRIS function without retry
//!   (Requirement 11.4);
//! - never registers a bus name when MPRIS is disabled (Requirement 11.5);
//! - releases the bus name on exit (Requirement 11.7).
//!
//! Design split (matching the rest of the crate): the decision logic — turning
//! an incoming MPRIS command into the concrete engine/queue actions, and
//! turning a [`Playback_State`] into an MPRIS metadata map and status string —
//! is pure and unit-testable in [`resolve_command`], [`build_metadata`], and
//! [`mpris_playback_status`]. The thin zbus layer ([`MprisAdapter`],
//! [`MediaPlayer2`], [`Player`]) only wires those pure results onto the bus.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use zbus::zvariant::{ObjectPath, Value};

use crate::domain::{PlaybackStatus, RepeatMode};
use crate::ipc::{IpcSender, RequestMethod};
use crate::now_playing::Playback_State;
use crate::queue::Queue;

/// The well-known bus name the adapter registers under (Requirement 11.1). The
/// `org.mpris.MediaPlayer2.` prefix is mandated by the MPRIS specification; the
/// suffix identifies this application.
pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.apple_music_tui";

/// The object path every MPRIS interface is served on, fixed by the MPRIS spec.
pub const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

/// The identity reported by `org.mpris.MediaPlayer2.Identity`.
pub const IDENTITY: &str = "Apple Music TUI";

// ---------------------------------------------------------------------------
// Pure command resolution (Requirements 11.2, 11.6)
// ---------------------------------------------------------------------------

/// A transport command received from an MPRIS client (Requirement 11.2).
///
/// `Seek` carries a relative micro-second offset (the MPRIS `Seek(Offset)`
/// convention, positive forward / negative backward); `SetPosition` carries an
/// absolute micro-second position (the MPRIS `SetPosition(TrackId, Position)`
/// convention). Both are resolved against the current Playback_State.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisCommand {
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Toggle between playing and paused.
    PlayPause,
    /// Advance to the next track in the Queue.
    Next,
    /// Step to the previous track (or restart the current one).
    Previous,
    /// Seek by a relative offset in microseconds (may be negative).
    Seek { offset_us: i64 },
    /// Seek to an absolute position in microseconds.
    SetPosition { position_us: u64 },
}

/// A single concrete effect the TUI must apply in response to an MPRIS command.
///
/// Returning effects (rather than performing them) keeps [`resolve_command`]
/// pure and testable. The caller applies them in order: `Engine` effects are
/// sent to the Playback_Engine over IPC; `Transition` effects name a Queue
/// index the caller loads from position 0 (Next/Previous reuse the Queue's own
/// transport logic, which the TUI owns — the design keeps queue logic in the
/// TUI rather than the engine). An empty effect list means the command is a
/// no-op (for example, any command with no current track — Requirement 11.6).
///
/// `RequestMethod` derives only `PartialEq` (not `Eq`), so neither does this.
#[derive(Debug, Clone, PartialEq)]
pub enum MprisEffect {
    /// Forward a request directly to the Playback_Engine.
    Engine(RequestMethod),
    /// Load the Queue track at `index` from position 0 and apply `status`.
    ///
    /// Produced by Next/Previous and by a seek past the end of the track. The
    /// caller issues the engine `load`/`play`/`pause` for the resolved track,
    /// exactly as the equivalent keybinding action would (Requirement 11.2).
    Transition {
        index: usize,
        status: PlaybackStatus,
    },
}

/// Resolve an incoming MPRIS `command` against the current `queue` and
/// playback `state` into the ordered effects the TUI should apply
/// (Requirements 11.2, 11.6).
///
/// Rules:
/// - With no current track every command is a no-op and the Playback_State is
///   left unchanged (Requirement 11.6).
/// - `Play`/`Pause` forward directly; `PlayPause` toggles based on the current
///   status (a stopped player resumes).
/// - `Next`/`Previous` compute the Queue transition the TUI would compute for
///   the equivalent keybinding (next / previous over the current repeat mode and
///   position) and return it as a [`MprisEffect::Transition`].
/// - A `Seek`/`SetPosition` that lands before 0 clamps to 0; one that lands at
///   or past the track duration advances to the next track in the Queue
///   (Requirement 11.6). A seek within bounds forwards a `seek` to the engine.
pub fn resolve_command(
    command: MprisCommand,
    queue: &Queue,
    state: &Playback_State,
) -> Vec<MprisEffect> {
    // Requirement 11.6: no current track → leave Playback_State unchanged.
    if queue.current.is_none() {
        return Vec::new();
    }

    match command {
        MprisCommand::Play => vec![MprisEffect::Engine(RequestMethod::Play)],
        MprisCommand::Pause => vec![MprisEffect::Engine(RequestMethod::Pause)],
        MprisCommand::PlayPause => {
            let method = match state.status {
                PlaybackStatus::Playing => RequestMethod::Pause,
                PlaybackStatus::Paused | PlaybackStatus::Stopped => RequestMethod::Play,
            };
            vec![MprisEffect::Engine(method)]
        }
        MprisCommand::Next => queue
            .next_transition(state.repeat)
            .map(|t| {
                vec![MprisEffect::Transition {
                    index: t.index,
                    status: t.status,
                }]
            })
            .unwrap_or_default(),
        MprisCommand::Previous => queue
            .previous_transition(state.position_ms)
            .map(|t| {
                vec![MprisEffect::Transition {
                    index: t.index,
                    status: t.status,
                }]
            })
            .unwrap_or_default(),
        MprisCommand::Seek { offset_us } => {
            let target = apply_offset_ms(state.position_ms, offset_us);
            resolve_seek_to(target, queue, state)
        }
        MprisCommand::SetPosition { position_us } => {
            let target = position_us / 1000;
            resolve_seek_to(target, queue, state)
        }
    }
}

/// Resolve a seek whose clamped/validated target is `target_ms` into effects
/// (Requirement 11.6). Shared by relative `Seek` and absolute `SetPosition`.
///
/// A target at or past the track duration advances to the next track; a target
/// within `0..duration` forwards a `seek` request. Targets below 0 have already
/// been clamped to 0 by [`apply_offset_ms`] / the unsigned absolute position.
fn resolve_seek_to(target_ms: u64, queue: &Queue, state: &Playback_State) -> Vec<MprisEffect> {
    if let Some(duration) = state.duration_ms {
        if target_ms >= duration {
            // Seek past the end advances to the next track (Requirement 11.6),
            // reusing the Queue's track-finished transition so the behaviour
            // matches a forward seek reaching the duration elsewhere.
            return queue
                .track_finished_transition(state.repeat)
                .map(|t| {
                    vec![MprisEffect::Transition {
                        index: t.index,
                        status: t.status,
                    }]
                })
                .unwrap_or_default();
        }
    }
    vec![MprisEffect::Engine(RequestMethod::Seek {
        position_ms: target_ms,
    })]
}

/// Apply a signed micro-second `offset_us` to a milli-second `position_ms`,
/// clamping a result below zero to 0 (Requirement 11.6 seek-before-0 clamp).
fn apply_offset_ms(position_ms: u64, offset_us: i64) -> u64 {
    let offset_ms = offset_us / 1000;
    if offset_ms >= 0 {
        position_ms.saturating_add(offset_ms as u64)
    } else {
        position_ms.saturating_sub(offset_ms.unsigned_abs())
    }
}

// ---------------------------------------------------------------------------
// Pure metadata / status mapping (Requirement 11.3)
// ---------------------------------------------------------------------------

/// Map a [`PlaybackStatus`] to the MPRIS `PlaybackStatus` string published over
/// D-Bus: `Playing`, `Paused`, or `Stopped` (Requirement 11.3).
pub fn mpris_playback_status(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Stopped => "Stopped",
    }
}

/// Map a [`RepeatMode`] to the MPRIS `LoopStatus` string: `None`, `Playlist`,
/// or `Track`.
pub fn mpris_loop_status(repeat: RepeatMode) -> &'static str {
    match repeat {
        RepeatMode::Off => "None",
        RepeatMode::RepeatAll => "Playlist",
        RepeatMode::RepeatOne => "Track",
    }
}

/// A D-Bus track id path for the MPRIS `mpris:trackid` metadata key.
///
/// MPRIS requires a valid object path; Apple Music track ids are arbitrary
/// strings, so they are escaped into a path segment under the player's object
/// path. With no current track the MPRIS "no track" sentinel path is used.
pub fn track_id_path(track_id: Option<&str>) -> String {
    match track_id {
        Some(id) => format!("{OBJECT_PATH}/track/{}", escape_path_segment(id)),
        None => format!("{OBJECT_PATH}/TrackList/NoTrack"),
    }
}

/// Escape an arbitrary track id into a single valid D-Bus object-path segment.
///
/// A path segment may contain only `[A-Za-z0-9_]`; any other byte is rendered
/// as `_<hex>` so distinct ids map to distinct segments.
fn escape_path_segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("_{byte:02x}"));
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The MPRIS metadata fields derived from a [`Playback_State`] (Requirement
/// 11.3), kept as an owned, comparable value so the mapping is unit-testable
/// without a live bus. [`MprisMetadata::into_dbus`] converts it to the
/// `a{sv}` map D-Bus publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MprisMetadata {
    /// `mpris:trackid` — an object path identifying the track.
    pub track_id_path: String,
    /// `xesam:title` — the track title, when a track is loaded.
    pub title: Option<String>,
    /// `xesam:artist` — the artist name, when present.
    pub artist: Option<String>,
    /// `xesam:album` — the album name, when present.
    pub album: Option<String>,
    /// `mpris:length` — the track length in microseconds, when known.
    pub length_us: Option<u64>,
}

/// Build the MPRIS metadata for a [`Playback_State`] (Requirement 11.3).
///
/// When no track is loaded (`track_id` is `None`) the metadata carries only the
/// MPRIS "no track" sentinel id and no title/artist/album/length, matching the
/// no-current-track case. A blank artist/album (the Now_Playing_Bar placeholder
/// source) is treated as absent so it is simply omitted from the published map.
pub fn build_metadata(state: &Playback_State) -> MprisMetadata {
    if state.track_id.is_none() {
        return MprisMetadata {
            track_id_path: track_id_path(None),
            title: None,
            artist: None,
            album: None,
            length_us: None,
        };
    }

    MprisMetadata {
        track_id_path: track_id_path(state.track_id.as_deref()),
        title: non_empty(&state.title),
        artist: non_empty(&state.artist),
        album: non_empty(&state.album),
        length_us: state.duration_ms.map(|ms| ms.saturating_mul(1000)),
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

impl MprisMetadata {
    /// Convert into the `a{sv}` D-Bus metadata map MPRIS clients read.
    ///
    /// `mpris:trackid` is always present; the title/artist/album/length keys are
    /// included only when they carry a value. `xesam:artist` is an array of
    /// strings per the MPRIS spec.
    pub fn into_dbus(self) -> HashMap<String, Value<'static>> {
        let mut map: HashMap<String, Value<'static>> = HashMap::new();

        // `mpris:trackid` must be a valid object path. The segments we build are
        // always valid, but fall back to the sentinel on the off chance it is
        // not, so conversion never fails.
        let path = ObjectPath::try_from(self.track_id_path)
            .or_else(|_| ObjectPath::try_from(track_id_path(None)))
            .expect("the no-track sentinel is a valid object path");
        map.insert("mpris:trackid".to_string(), Value::from(path));

        if let Some(title) = self.title {
            map.insert("xesam:title".to_string(), Value::from(title));
        }
        if let Some(artist) = self.artist {
            map.insert("xesam:artist".to_string(), Value::from(vec![artist]));
        }
        if let Some(album) = self.album {
            map.insert("xesam:album".to_string(), Value::from(album));
        }
        if let Some(length) = self.length_us {
            map.insert("mpris:length".to_string(), Value::from(length as i64));
        }
        map
    }
}

// ---------------------------------------------------------------------------
// Shared adapter state
// ---------------------------------------------------------------------------

/// The live state the zbus interface reads to answer property gets and to
/// resolve commands. The render loop updates it as the Playback_State and Queue
/// change; the interface methods read it under the mutex.
#[derive(Debug, Default)]
pub struct MprisState {
    /// The current playback state mirrored from the Playback_Engine.
    pub playback: Playback_State,
    /// The current Queue (used to resolve Next/Previous/seek-past-end).
    pub queue: Queue,
    /// Set by the notification pump when the Playback_Engine connection closes
    /// or errors (Requirement 4.10 "lost connection", task 16.2). The render
    /// loop observes this to degrade to preview-only mode and surface the
    /// engine error with a restart action, without the background pump needing
    /// direct access to the UI state.
    pub engine_lost: bool,
    /// Set by the notification pump when a `signInResult` carries a non-empty
    /// Music_User_Token (Requirements 1.3, 1.6). The render loop picks it up on
    /// the next tick so post-login actions can authorize engine `load`
    /// requests, without the background pump needing direct access to the UI
    /// state. `None` until a successful sign-in is observed.
    pub captured_music_user_token: Option<String>,
    /// Set by the notification pump to the track id reported by a
    /// `trackFinished` notification (Requirement 4.8). The render loop consumes
    /// it on the next tick to auto-advance the Queue under the active repeat
    /// mode, then clears it. `None` when no finish is pending. The pump does not
    /// mutate the UI's authoritative Queue directly; it only signals here.
    pub track_finished: Option<String>,
}

/// Shared handle to [`MprisState`], cloned into the zbus interface objects and
/// held by the TUI render loop so both see the same state.
pub type SharedState = Arc<Mutex<MprisState>>;

/// Create a fresh shared adapter state.
pub fn shared_state() -> SharedState {
    Arc::new(Mutex::new(MprisState::default()))
}

// ---------------------------------------------------------------------------
// Graceful-degradation outcome (Requirement 11.4)
// ---------------------------------------------------------------------------

/// A warning produced when the adapter cannot operate over D-Bus (Requirement
/// 11.4). The TUI logs it and continues all non-MPRIS functions without
/// retrying registration until the next start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MprisUnavailable {
    /// A human-readable description of the D-Bus failure, safe to log (it names
    /// the failure, never a token or other credential).
    pub reason: String,
}

impl std::fmt::Display for MprisUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MPRIS disabled for this session: the D-Bus session bus is \
             unavailable ({}); continuing without media-key integration",
            self.reason
        )
    }
}

/// The outcome of attempting to register the MPRIS adapter at startup.
///
/// `Disabled` is returned when MPRIS is off in the config (Requirement 11.5);
/// no bus name is touched. `Registered` carries the live [`MprisAdapter`] whose
/// bus name is released on drop / [`MprisAdapter::shutdown`] (Requirement 11.7).
/// `Unavailable` carries the degradation warning so the caller logs it and
/// continues (Requirement 11.4).
#[derive(Debug)]
pub enum RegistrationOutcome {
    /// MPRIS is disabled in the Config_File; nothing was registered.
    Disabled,
    /// The adapter registered successfully on the session bus.
    Registered(MprisAdapter),
    /// The session bus was unavailable; the TUI continues without MPRIS.
    Unavailable(MprisUnavailable),
}

// ---------------------------------------------------------------------------
// zbus interfaces (Requirements 11.1, 11.2, 11.3)
// ---------------------------------------------------------------------------

/// Context shared by both interface objects: the engine sender used to forward
/// commands and the live adapter state used to resolve them and answer gets.
#[derive(Clone)]
struct InterfaceContext {
    sender: IpcSender,
    state: SharedState,
}

impl InterfaceContext {
    /// Apply the resolved effects of an MPRIS `command` to the Playback_Engine
    /// (Requirement 11.2). `Transition` effects load the resolved Queue track
    /// from position 0, mirroring the equivalent keybinding action; the
    /// Music_User_Token is not known to the adapter, so loads carry an empty
    /// token and rely on the engine's already-authorized session (the TUI's
    /// keybinding path uses the same engine session).
    async fn apply(&self, command: MprisCommand) {
        let effects = {
            let guard = self.state.lock().await;
            resolve_command(command, &guard.queue, &guard.playback)
        };
        for effect in effects {
            match effect {
                MprisEffect::Engine(method) => {
                    let _ = self.sender.send(&method).await;
                }
                MprisEffect::Transition { index, status } => {
                    let load = {
                        let guard = self.state.lock().await;
                        guard.queue.tracks.get(index).map(|t| RequestMethod::Load {
                            track_id: t.track_id.clone(),
                            position_ms: 0,
                            music_user_token: String::new(),
                            developer_token: String::new(),
                        })
                    };
                    if let Some(load) = load {
                        let _ = self.sender.send(&load).await;
                        let follow = match status {
                            PlaybackStatus::Playing => RequestMethod::Play,
                            PlaybackStatus::Paused | PlaybackStatus::Stopped => {
                                RequestMethod::Pause
                            }
                        };
                        let _ = self.sender.send(&follow).await;
                    }
                }
            }
        }
    }
}

/// The root `org.mpris.MediaPlayer2` interface (Requirement 11.1).
pub struct MediaPlayer2;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2 {
    /// MPRIS clients may ask the app to raise its window; the TUI has no window
    /// to raise, so this is a no-op (and `CanRaise` is false).
    async fn raise(&self) {}

    /// MPRIS `Quit` is not honoured from the bus; the TUI owns its own quit
    /// keybinding and lifecycle, so this is a no-op (and `CanQuit` is false).
    async fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        IDENTITY
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The `org.mpris.MediaPlayer2.Player` interface: transport control and
/// metadata/status (Requirements 11.2, 11.3).
pub struct Player {
    ctx: InterfaceContext,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    /// Forward `Play` (Requirement 11.2).
    async fn play(&self) {
        self.ctx.apply(MprisCommand::Play).await;
    }

    /// Forward `Pause` (Requirement 11.2).
    async fn pause(&self) {
        self.ctx.apply(MprisCommand::Pause).await;
    }

    /// Forward `PlayPause`, toggling on the current status (Requirement 11.2).
    async fn play_pause(&self) {
        self.ctx.apply(MprisCommand::PlayPause).await;
    }

    /// `Stop` maps to a pause; the TUI has no distinct stopped transport beyond
    /// the engine's pause, so it forwards a pause rather than tearing down the
    /// Queue.
    async fn stop(&self) {
        self.ctx.apply(MprisCommand::Pause).await;
    }

    /// Forward `Next`, computing the Queue transition the TUI would
    /// (Requirement 11.2).
    async fn next(&self) {
        self.ctx.apply(MprisCommand::Next).await;
    }

    /// Forward `Previous`, computing the Queue transition the TUI would
    /// (Requirement 11.2).
    async fn previous(&self) {
        self.ctx.apply(MprisCommand::Previous).await;
    }

    /// Relative `Seek(Offset)` in microseconds; clamps before 0 and advances
    /// past the end (Requirement 11.6).
    async fn seek(&self, offset: i64) {
        self.ctx.apply(MprisCommand::Seek { offset_us: offset }).await;
    }

    /// Absolute `SetPosition(TrackId, Position)` in microseconds; clamps before
    /// 0 and advances past the end (Requirement 11.6). The `track_id` argument
    /// is accepted for spec conformance; the TUI always seeks within the
    /// current track.
    async fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        let position_us = position.max(0) as u64;
        self.ctx
            .apply(MprisCommand::SetPosition { position_us })
            .await;
    }

    /// `OpenUri` is unsupported (no supported URI schemes); it is a no-op.
    async fn open_uri(&self, _uri: String) {}

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        let guard = self.ctx.state.lock().await;
        mpris_playback_status(guard.playback.status).to_string()
    }

    #[zbus(property)]
    async fn loop_status(&self) -> String {
        let guard = self.ctx.state.lock().await;
        mpris_loop_status(guard.playback.repeat).to_string()
    }

    #[zbus(property)]
    async fn shuffle(&self) -> bool {
        let guard = self.ctx.state.lock().await;
        guard.playback.shuffle
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, Value<'static>> {
        let guard = self.ctx.state.lock().await;
        build_metadata(&guard.playback).into_dbus()
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        let guard = self.ctx.state.lock().await;
        guard.playback.volume as f64 / 100.0
    }

    #[zbus(property)]
    async fn position(&self) -> i64 {
        let guard = self.ctx.state.lock().await;
        (guard.playback.position_ms.saturating_mul(1000)) as i64
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    /// MPRIS `Seeked` signal, emitted with the new absolute position in
    /// microseconds after a seek (Requirement 11.3). The macro generates the
    /// emitter as an associated function taking a [`SignalContext`].
    #[zbus(signal)]
    async fn seeked(ctx: &zbus::object_server::SignalContext<'_>, position: i64)
        -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// The live adapter (registration, publishing, release)
// ---------------------------------------------------------------------------

/// A registered MPRIS adapter holding its D-Bus connection and the shared state
/// (Requirements 11.1, 11.3, 11.7).
///
/// While this value is alive the `org.mpris.MediaPlayer2.apple_music_tui` bus
/// name is held. [`MprisAdapter::shutdown`] releases it explicitly before exit
/// (Requirement 11.7); dropping the connection also releases it, so an abrupt
/// teardown still frees the name.
#[derive(Debug)]
pub struct MprisAdapter {
    connection: zbus::Connection,
    state: SharedState,
}

impl MprisAdapter {
    /// Register the MPRIS adapter on the session bus when enabled (Requirements
    /// 11.1, 11.4, 11.5).
    ///
    /// Returns:
    /// - [`RegistrationOutcome::Disabled`] when `mpris_enabled` is false,
    ///   without touching the bus (Requirement 11.5);
    /// - [`RegistrationOutcome::Registered`] with the live adapter on success;
    /// - [`RegistrationOutcome::Unavailable`] carrying a loggable warning when
    ///   the session bus cannot be reached or the name cannot be served, so the
    ///   caller logs it and continues every non-MPRIS function (Requirement
    ///   11.4). No internal retry is attempted (Requirement 11.4).
    ///
    /// `sender` forwards transport commands to the Playback_Engine; `state` is
    /// the shared handle the render loop updates so published properties track
    /// the Playback_State.
    pub async fn register(
        mpris_enabled: bool,
        sender: IpcSender,
        state: SharedState,
    ) -> RegistrationOutcome {
        if !mpris_enabled {
            // Requirement 11.5: never touch the bus when disabled.
            return RegistrationOutcome::Disabled;
        }

        match Self::try_register(sender, state.clone()).await {
            Ok(adapter) => RegistrationOutcome::Registered(adapter),
            Err(err) => RegistrationOutcome::Unavailable(MprisUnavailable {
                reason: err.to_string(),
            }),
        }
    }

    /// Attempt the full registration against the session bus, serving both
    /// interfaces at [`OBJECT_PATH`] and requesting [`BUS_NAME`].
    async fn try_register(
        sender: IpcSender,
        state: SharedState,
    ) -> zbus::Result<MprisAdapter> {
        let ctx = InterfaceContext {
            sender,
            state: state.clone(),
        };

        let connection = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, MediaPlayer2)?
            .serve_at(OBJECT_PATH, Player { ctx })?
            .build()
            .await?;

        Ok(MprisAdapter { connection, state })
    }

    /// The shared state handle, so the render loop can update the mirrored
    /// Playback_State and Queue the published properties read from.
    pub fn state(&self) -> SharedState {
        self.state.clone()
    }

    /// Publish the current Playback_State's metadata and status change over
    /// D-Bus (Requirement 11.3).
    ///
    /// Emits `PropertiesChanged` for `Metadata`, `PlaybackStatus`, `LoopStatus`,
    /// `Shuffle`, and `Volume` so MPRIS clients see track/status changes within
    /// the required window. Call this after mutating the shared state.
    pub async fn publish_state(&self) -> zbus::Result<()> {
        let iface_ref = self
            .connection
            .object_server()
            .interface::<_, Player>(OBJECT_PATH)
            .await?;
        let ctx = iface_ref.signal_context();
        let iface = iface_ref.get().await;
        iface.playback_status_changed(ctx).await?;
        iface.loop_status_changed(ctx).await?;
        iface.shuffle_changed(ctx).await?;
        iface.metadata_changed(ctx).await?;
        iface.volume_changed(ctx).await?;
        Ok(())
    }

    /// Emit the MPRIS `Seeked` signal with the new absolute position after a
    /// seek (Requirement 11.3).
    pub async fn publish_seek(&self, position_ms: u64) -> zbus::Result<()> {
        let iface_ref = self
            .connection
            .object_server()
            .interface::<_, Player>(OBJECT_PATH)
            .await?;
        let ctx = iface_ref.signal_context();
        Player::seeked(ctx, (position_ms.saturating_mul(1000)) as i64).await
    }

    /// Release the bus name before the TUI exits (Requirement 11.7).
    ///
    /// Releasing the well-known name and closing the connection frees the MPRIS
    /// name so another instance can claim it. Dropping the adapter closes the
    /// connection too, so this is the explicit, awaited path used on clean exit.
    pub async fn shutdown(self) -> zbus::Result<()> {
        let _ = self.connection.release_name(BUS_NAME).await;
        // Closing the connection drops the served interfaces and the unique
        // connection, fully releasing the name on the bus.
        self.connection.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlaybackStatus, RepeatMode};
    use crate::queue::QueueTrack;

    fn track(id: &str) -> QueueTrack {
        QueueTrack { track_id: id.to_string(), title: format!("Title {id}"), artist: format!("Artist {id}"), album: String::new(), duration_ms: Some(180_000) }
    }

    fn queue_with(ids: &[&str], current: Option<usize>) -> Queue {
        let mut q = Queue::new();
        for id in ids {
            q.tracks.push(track(id));
        }
        q.current = current;
        q
    }

    fn state_for(queue: &Queue, position_ms: u64, duration_ms: Option<u64>) -> Playback_State {
        let mut state = Playback_State::default();
        if let Some(cur) = queue.current {
            let t = &queue.tracks[cur];
            state.track_id = Some(t.track_id.clone());
            state.title = t.title.clone();
            state.artist = t.artist.clone();
        }
        state.position_ms = position_ms;
        state.duration_ms = duration_ms;
        state
    }

    // --- command resolution (R11.2, R11.6) ---

    #[test]
    fn no_current_track_makes_every_command_a_noop() {
        let q = queue_with(&[], None);
        let state = Playback_State::default();
        for command in [
            MprisCommand::Play,
            MprisCommand::Pause,
            MprisCommand::PlayPause,
            MprisCommand::Next,
            MprisCommand::Previous,
            MprisCommand::Seek { offset_us: 5_000_000 },
            MprisCommand::SetPosition { position_us: 1_000_000 },
        ] {
            assert!(
                resolve_command(command, &q, &state).is_empty(),
                "{command:?} should be a no-op with no current track"
            );
        }
    }

    #[test]
    fn play_and_pause_forward_directly() {
        let q = queue_with(&["a"], Some(0));
        let state = state_for(&q, 0, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Play, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Play)]
        );
        assert_eq!(
            resolve_command(MprisCommand::Pause, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Pause)]
        );
    }

    #[test]
    fn play_pause_toggles_on_status() {
        let q = queue_with(&["a"], Some(0));
        let mut playing = state_for(&q, 0, Some(180_000));
        playing.status = PlaybackStatus::Playing;
        assert_eq!(
            resolve_command(MprisCommand::PlayPause, &q, &playing),
            vec![MprisEffect::Engine(RequestMethod::Pause)]
        );

        let mut paused = state_for(&q, 0, Some(180_000));
        paused.status = PlaybackStatus::Paused;
        assert_eq!(
            resolve_command(MprisCommand::PlayPause, &q, &paused),
            vec![MprisEffect::Engine(RequestMethod::Play)]
        );

        let mut stopped = state_for(&q, 0, Some(180_000));
        stopped.status = PlaybackStatus::Stopped;
        assert_eq!(
            resolve_command(MprisCommand::PlayPause, &q, &stopped),
            vec![MprisEffect::Engine(RequestMethod::Play)]
        );
    }

    #[test]
    fn next_resolves_to_following_track_transition() {
        let q = queue_with(&["a", "b", "c"], Some(0));
        let state = state_for(&q, 10_000, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Next, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    #[test]
    fn next_at_end_with_repeat_off_stops_paused_on_last() {
        let q = queue_with(&["a", "b"], Some(1));
        let state = state_for(&q, 10_000, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Next, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Paused,
            }]
        );
    }

    #[test]
    fn previous_within_threshold_steps_back() {
        let q = queue_with(&["a", "b", "c"], Some(2));
        let state = state_for(&q, 1_000, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Previous, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    #[test]
    fn previous_past_threshold_restarts_current() {
        let q = queue_with(&["a", "b", "c"], Some(2));
        let state = state_for(&q, 10_000, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Previous, &q, &state),
            vec![MprisEffect::Transition {
                index: 2,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    // --- seek handling (R11.6) ---

    #[test]
    fn seek_within_bounds_forwards_absolute_position() {
        let q = queue_with(&["a"], Some(0));
        let state = state_for(&q, 30_000, Some(180_000));
        // +10s from 30s = 40s.
        assert_eq!(
            resolve_command(MprisCommand::Seek { offset_us: 10_000_000 }, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Seek { position_ms: 40_000 })]
        );
    }

    #[test]
    fn seek_before_zero_clamps_to_zero() {
        let q = queue_with(&["a"], Some(0));
        let state = state_for(&q, 5_000, Some(180_000));
        // -10s from 5s clamps to 0.
        assert_eq!(
            resolve_command(MprisCommand::Seek { offset_us: -10_000_000 }, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Seek { position_ms: 0 })]
        );
    }

    #[test]
    fn seek_past_end_advances_to_next_track() {
        let q = queue_with(&["a", "b"], Some(0));
        let state = state_for(&q, 170_000, Some(180_000));
        // +30s from 170s = 200s >= 180s duration → advance.
        assert_eq!(
            resolve_command(MprisCommand::Seek { offset_us: 30_000_000 }, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    #[test]
    fn seek_exactly_to_duration_advances() {
        let q = queue_with(&["a", "b"], Some(0));
        let state = state_for(&q, 170_000, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::Seek { offset_us: 10_000_000 }, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    #[test]
    fn set_position_within_bounds_forwards_absolute() {
        let q = queue_with(&["a"], Some(0));
        let state = state_for(&q, 0, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::SetPosition { position_us: 60_000_000 }, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Seek { position_ms: 60_000 })]
        );
    }

    #[test]
    fn set_position_past_end_advances() {
        let q = queue_with(&["a", "b"], Some(0));
        let state = state_for(&q, 0, Some(180_000));
        assert_eq!(
            resolve_command(MprisCommand::SetPosition { position_us: 200_000_000 }, &q, &state),
            vec![MprisEffect::Transition {
                index: 1,
                status: PlaybackStatus::Playing,
            }]
        );
    }

    #[test]
    fn seek_with_unknown_duration_never_advances() {
        let q = queue_with(&["a"], Some(0));
        let state = state_for(&q, 10_000, None);
        assert_eq!(
            resolve_command(MprisCommand::Seek { offset_us: 999_000_000 }, &q, &state),
            vec![MprisEffect::Engine(RequestMethod::Seek {
                position_ms: 10_000 + 999_000
            })]
        );
    }

    // --- status / loop mapping (R11.3) ---

    #[test]
    fn playback_status_maps_to_mpris_strings() {
        assert_eq!(mpris_playback_status(PlaybackStatus::Playing), "Playing");
        assert_eq!(mpris_playback_status(PlaybackStatus::Paused), "Paused");
        assert_eq!(mpris_playback_status(PlaybackStatus::Stopped), "Stopped");
    }

    #[test]
    fn loop_status_maps_to_mpris_strings() {
        assert_eq!(mpris_loop_status(RepeatMode::Off), "None");
        assert_eq!(mpris_loop_status(RepeatMode::RepeatAll), "Playlist");
        assert_eq!(mpris_loop_status(RepeatMode::RepeatOne), "Track");
    }

    // --- metadata (R11.3) ---

    #[test]
    fn metadata_without_a_track_carries_only_the_no_track_sentinel() {
        let state = Playback_State::default();
        let meta = build_metadata(&state);
        assert_eq!(meta.track_id_path, track_id_path(None));
        assert_eq!(meta.title, None);
        assert_eq!(meta.artist, None);
        assert_eq!(meta.album, None);
        assert_eq!(meta.length_us, None);
    }

    #[test]
    fn metadata_with_a_track_includes_fields_and_length_in_microseconds() {
        let state = Playback_State {
            track_id: Some("abc".to_string()),
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration_ms: Some(180_000),
            ..Default::default()
        };

        let meta = build_metadata(&state);
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.artist.as_deref(), Some("Artist"));
        assert_eq!(meta.album.as_deref(), Some("Album"));
        assert_eq!(meta.length_us, Some(180_000_000));
        assert!(meta.track_id_path.ends_with("/track/abc"));
    }

    #[test]
    fn metadata_blank_fields_are_omitted() {
        let state = Playback_State {
            track_id: Some("abc".to_string()),
            title: "Song".to_string(),
            artist: String::new(),
            album: String::new(),
            duration_ms: None,
            ..Default::default()
        };

        let meta = build_metadata(&state);
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.artist, None);
        assert_eq!(meta.album, None);
        assert_eq!(meta.length_us, None);
    }

    #[test]
    fn metadata_into_dbus_includes_trackid_and_present_fields() {
        let state = Playback_State {
            track_id: Some("abc".to_string()),
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: String::new(),
            duration_ms: Some(180_000),
            ..Default::default()
        };

        let map = build_metadata(&state).into_dbus();
        assert!(map.contains_key("mpris:trackid"));
        assert!(map.contains_key("xesam:title"));
        assert!(map.contains_key("xesam:artist"));
        assert!(map.contains_key("mpris:length"));
        // Blank album omitted.
        assert!(!map.contains_key("xesam:album"));
    }

    #[test]
    fn track_id_path_escapes_non_path_characters() {
        let path = track_id_path(Some("a.b:c/d"));
        // Dots, colons and slashes are escaped into `_hh` segments so the whole
        // thing is a single valid object-path segment under /track/.
        assert!(path.starts_with(&format!("{OBJECT_PATH}/track/")));
        assert!(!path.trim_start_matches(&format!("{OBJECT_PATH}/track/")).contains('.'));
        // The escaped form must build a valid object path.
        assert!(ObjectPath::try_from(path).is_ok());
    }

    #[test]
    fn apply_offset_clamps_and_saturates() {
        assert_eq!(apply_offset_ms(5_000, -10_000_000), 0);
        assert_eq!(apply_offset_ms(5_000, 10_000_000), 15_000);
        assert_eq!(apply_offset_ms(0, -1_000), 0);
    }

    // --- graceful degradation (R11.4, R11.5) ---

    #[tokio::test]
    async fn disabled_config_never_registers() {
        // A dummy sender is required; it is never used because registration
        // short-circuits on the disabled flag before any bus or IPC work.
        let (sender, _server) = test_sender().await;
        let outcome = MprisAdapter::register(false, sender, shared_state()).await;
        assert!(matches!(outcome, RegistrationOutcome::Disabled));
    }

    #[tokio::test]
    async fn unavailable_bus_degrades_with_a_warning() {
        // Point D-Bus at an address that cannot be reached so registration fails
        // and the adapter degrades rather than erroring out (Requirement 11.4).
        let previous = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/apple-music-tui-test-bus",
        );

        let (sender, _server) = test_sender().await;
        let outcome = MprisAdapter::register(true, sender, shared_state()).await;

        match previous {
            Some(v) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v),
            None => std::env::remove_var("DBUS_SESSION_BUS_ADDRESS"),
        }

        match outcome {
            RegistrationOutcome::Unavailable(warning) => {
                // The warning names a D-Bus failure and carries no credentials.
                assert!(!warning.reason.is_empty());
                let shown = warning.to_string();
                assert!(shown.contains("D-Bus session bus is"));
            }
            other => panic!("expected graceful degradation, got {other:?}"),
        }
    }

    /// Build an [`IpcSender`] backed by a throwaway Unix socket with an accept
    /// loop, so tests can construct the adapter without a real engine. The
    /// returned guard keeps the listener task alive.
    async fn test_sender() -> (IpcSender, tokio::task::JoinHandle<()>) {
        use crate::ipc::IpcClient;
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!(
            "amt-mpris-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("engine.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            // Accept and then drain; the tests that use the sender do not rely
            // on responses.
            if let Ok((stream, _)) = listener.accept().await {
                let (mut read, _write) = stream.into_split();
                let mut buf = [0u8; 1024];
                use tokio::io::AsyncReadExt;
                while let Ok(n) = read.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                }
            }
        });

        let client = IpcClient::connect_to(&socket).await.unwrap();
        (client.sender(), server)
    }
}
