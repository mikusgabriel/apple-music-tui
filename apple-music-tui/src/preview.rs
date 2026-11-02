//! Preview-only playback fallback (task 16.2).
//!
//! When the Playback_Engine cannot start or Widevine is unavailable, the TUI
//! degrades to **preview-only mode**: it plays the 30-second, unencrypted
//! preview clips the Apple Music API exposes without a Music_User_Token, using a
//! lightweight native audio path, and labels every track as a preview (design
//! "Technology Stack", Open Question 3; "Error Handling"). This keeps the
//! application usable without the Chromium runtime.
//!
//! Design choices for this module:
//!
//! - **Lightweight.** Full-length DRM playback lives in the Electron
//!   Playback_Engine; this fallback only has to play short, unencrypted MP3/AAC
//!   preview URLs. We therefore do not pull a heavy media stack into the TUI.
//! - **Audio sink behind a trait.** The actual native audio output is abstracted
//!   behind [`AudioSink`], so the preview *logic* (loading, play/pause, the
//!   30-second cap, preview labelling, position reporting) is fully testable
//!   without real audio hardware. The default [`NullAudioSink`] implements the
//!   trait as a no-op that only records the last command, which is what the test
//!   suite and headless/CI environments use. A real sink (e.g. backed by a
//!   native decoder) can implement the same trait later and be injected without
//!   changing the player.
//!
//! The [`PreviewPlayer`] mirrors the subset of the Playback_Engine transport the
//! TUI drives in this mode (load / play / pause / seek / set-volume) and reports
//! a [`PlaybackStatus`] + position so the Now_Playing_Bar keeps working exactly
//! as it does with the real engine, with the addition of the preview label.

use std::time::Duration;

use crate::domain::PlaybackStatus;

/// The fixed length of an Apple Music preview clip: 30 seconds (design "Open
/// Question 3" / "Error Handling"). Preview playback is capped here regardless
/// of the track's full duration.
pub const PREVIEW_DURATION_MS: u64 = 30_000;

/// The timeout after which a Playback_Engine that has not responded is treated
/// as a *lost connection* (Requirement 4.10: "does not respond to the TUI for 5
/// seconds"). Surfaced as [`EngineFailure::LostConnection`].
pub const ENGINE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The label appended to a track's metadata while preview-only mode is active,
/// so the user can see that only a 30-second preview is playing (design: "label
/// tracks as previews").
pub const PREVIEW_LABEL: &str = "(preview)";

/// A lightweight native audio output abstraction.
///
/// The preview fallback only needs to play short, unencrypted preview clips, so
/// the surface is deliberately small: load a URL, start/stop, seek within the
/// clip, and set the volume. Keeping it a trait means the preview *logic* is
/// testable with the no-op [`NullAudioSink`] and a real, hardware-backed sink
/// can be swapped in later without touching [`PreviewPlayer`].
pub trait AudioSink {
    /// Load the preview clip at `url`, replacing any currently loaded clip. The
    /// clip starts paused at position 0; [`AudioSink::play`] begins playback.
    fn load(&mut self, url: &str);

    /// Begin (or resume) playback of the loaded clip.
    fn play(&mut self);

    /// Pause playback, keeping the current position.
    fn pause(&mut self);

    /// Stop playback and unload the current clip.
    fn stop(&mut self);

    /// Seek within the loaded clip to `position_ms`.
    fn seek(&mut self, position_ms: u64);

    /// Set the output volume level, 0..=100.
    fn set_volume(&mut self, level: u8);
}

/// The last command handed to a [`NullAudioSink`], recorded so tests can assert
/// the preview player drove the sink correctly without any real audio device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkCommand {
    Load(String),
    Play,
    Pause,
    Stop,
    Seek(u64),
    SetVolume(u8),
}

/// A no-op [`AudioSink`] that produces no sound and only records the commands it
/// received.
///
/// This is the default sink: it lets preview-only mode run (and be exercised by
/// tests and headless environments) without a real audio device. The recorded
/// [`SinkCommand`] history is available via [`NullAudioSink::commands`] and the
/// most recent via [`NullAudioSink::last_command`].
#[derive(Debug, Default)]
pub struct NullAudioSink {
    commands: Vec<SinkCommand>,
}

impl NullAudioSink {
    /// Create an empty no-op sink.
    pub fn new() -> Self {
        NullAudioSink::default()
    }

    /// The ordered history of commands this sink received.
    pub fn commands(&self) -> &[SinkCommand] {
        &self.commands
    }

    /// The most recent command this sink received, if any.
    pub fn last_command(&self) -> Option<&SinkCommand> {
        self.commands.last()
    }
}

impl AudioSink for NullAudioSink {
    fn load(&mut self, url: &str) {
        self.commands.push(SinkCommand::Load(url.to_string()));
    }

    fn play(&mut self) {
        self.commands.push(SinkCommand::Play);
    }

    fn pause(&mut self) {
        self.commands.push(SinkCommand::Pause);
    }

    fn stop(&mut self) {
        self.commands.push(SinkCommand::Stop);
    }

    fn seek(&mut self, position_ms: u64) {
        self.commands.push(SinkCommand::Seek(position_ms));
    }

    fn set_volume(&mut self, level: u8) {
        self.commands.push(SinkCommand::SetVolume(level));
    }
}

/// A single preview track: the id of the track and the URL of its 30-second
/// preview clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTrack {
    /// The catalog track id, so the mirrored Playback_State keeps the same
    /// identity it would have with the real engine.
    pub track_id: String,
    /// The unencrypted preview-clip URL (from the API's `previews[0].url`).
    pub preview_url: String,
}

/// The current preview-mode playback state, reported to the caller so it can
/// update the mirrored [`crate::now_playing::Playback_State`] and the
/// Now_Playing_Bar (including the preview label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSnapshot {
    /// The loaded track id, or `None` when nothing is loaded.
    pub track_id: Option<String>,
    /// Play/pause status of the preview clip.
    pub status: PlaybackStatus,
    /// Current position within the clip in milliseconds, clamped to
    /// `0..=PREVIEW_DURATION_MS`.
    pub position_ms: u64,
    /// The preview clip length (always [`PREVIEW_DURATION_MS`] while a track is
    /// loaded), or `None` when nothing is loaded.
    pub duration_ms: Option<u64>,
    /// Whether this is preview-only playback, so the UI can show the preview
    /// label (always `true` for a [`PreviewPlayer`]).
    pub is_preview: bool,
    /// The current volume level, 0..=100.
    pub volume: u8,
}

/// The lightweight native preview player driven while the Playback_Engine is
/// unavailable.
///
/// It mirrors the transport subset the TUI uses in this mode and enforces the
/// 30-second preview cap (Requirement/design: 30-second previews): a forward
/// seek or a position advance that reaches [`PREVIEW_DURATION_MS`] finishes the
/// clip. All audio output goes through the injected [`AudioSink`], so the player
/// is fully testable with [`NullAudioSink`].
pub struct PreviewPlayer<S: AudioSink = NullAudioSink> {
    sink: S,
    track_id: Option<String>,
    status: PlaybackStatus,
    position_ms: u64,
    volume: u8,
}

impl Default for PreviewPlayer<NullAudioSink> {
    fn default() -> Self {
        PreviewPlayer::new(NullAudioSink::new())
    }
}

impl<S: AudioSink> PreviewPlayer<S> {
    /// Create a preview player driving the given audio sink, with nothing loaded
    /// and the volume at full.
    pub fn new(sink: S) -> Self {
        PreviewPlayer {
            sink,
            track_id: None,
            status: PlaybackStatus::Stopped,
            position_ms: 0,
            volume: 100,
        }
    }

    /// Borrow the underlying sink (used by tests to inspect recorded commands).
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Load a preview track at `start_position_ms` (clamped into the clip) and
    /// leave it paused, mirroring the engine's "load … paused" behaviour. The
    /// caller issues [`play`](PreviewPlayer::play) to start audio.
    ///
    /// Loading past the preview length clamps to the clip end (there is no audio
    /// beyond 30 seconds).
    pub fn load(&mut self, track: &PreviewTrack, start_position_ms: u64) {
        self.sink.load(&track.preview_url);
        self.track_id = Some(track.track_id.clone());
        self.position_ms = start_position_ms.min(PREVIEW_DURATION_MS);
        self.status = PlaybackStatus::Paused;
        if self.position_ms > 0 {
            self.sink.seek(self.position_ms);
        }
    }

    /// Resume playback of the loaded clip. A no-op when nothing is loaded.
    pub fn play(&mut self) {
        if self.track_id.is_some() {
            self.sink.play();
            self.status = PlaybackStatus::Playing;
        }
    }

    /// Pause playback, keeping the position. A no-op when nothing is loaded.
    pub fn pause(&mut self) {
        if self.track_id.is_some() {
            self.sink.pause();
            self.status = PlaybackStatus::Paused;
        }
    }

    /// Toggle play/pause. A no-op when nothing is loaded.
    pub fn toggle(&mut self) {
        match self.status {
            PlaybackStatus::Playing => self.pause(),
            PlaybackStatus::Paused | PlaybackStatus::Stopped => self.play(),
        }
    }

    /// Stop playback and unload the current clip.
    pub fn stop(&mut self) {
        self.sink.stop();
        self.track_id = None;
        self.status = PlaybackStatus::Stopped;
        self.position_ms = 0;
    }

    /// Seek to an absolute `position_ms` within the clip, clamped to
    /// `0..=PREVIEW_DURATION_MS`. Reaching the clip end is reported as the
    /// preview finishing so the caller can advance the Queue, exactly as a
    /// forward seek that reaches the duration does with the real engine.
    ///
    /// Returns `true` when the preview finished (reached the cap), `false`
    /// otherwise. A seek with nothing loaded is a no-op returning `false`.
    pub fn seek(&mut self, position_ms: u64) -> bool {
        if self.track_id.is_none() {
            return false;
        }
        if position_ms >= PREVIEW_DURATION_MS {
            self.finish();
            return true;
        }
        self.position_ms = position_ms;
        self.sink.seek(position_ms);
        false
    }

    /// Advance the elapsed position by `elapsed_ms` while playing, enforcing the
    /// 30-second cap. Called by the caller's tick (mirrors how the engine
    /// reports position at least once per second).
    ///
    /// Returns `true` when the advance reached the preview cap and the clip
    /// finished, `false` otherwise. Advancing while paused/stopped or with
    /// nothing loaded is a no-op returning `false`.
    pub fn advance(&mut self, elapsed_ms: u64) -> bool {
        if self.track_id.is_none() || self.status != PlaybackStatus::Playing {
            return false;
        }
        let next = self.position_ms.saturating_add(elapsed_ms);
        if next >= PREVIEW_DURATION_MS {
            self.finish();
            return true;
        }
        self.position_ms = next;
        false
    }

    /// Set the output volume, clamped to 0..=100, and forward it to the sink.
    pub fn set_volume(&mut self, level: u8) {
        self.volume = level.min(100);
        self.sink.set_volume(self.volume);
    }

    /// A snapshot of the current preview state for the Now_Playing_Bar mirror.
    pub fn snapshot(&self) -> PreviewSnapshot {
        PreviewSnapshot {
            track_id: self.track_id.clone(),
            status: self.status,
            position_ms: self.position_ms,
            duration_ms: self.track_id.as_ref().map(|_| PREVIEW_DURATION_MS),
            is_preview: true,
            volume: self.volume,
        }
    }

    /// Handle the clip reaching its cap: stop audio, pin the position at the cap,
    /// and mark it paused (the caller then advances the Queue). The track id is
    /// kept so the caller knows which track finished.
    fn finish(&mut self) {
        self.sink.stop();
        self.position_ms = PREVIEW_DURATION_MS;
        self.status = PlaybackStatus::Paused;
    }
}

/// Decorate a track title with the preview label so the Now_Playing_Bar shows
/// that only a 30-second preview is playing (design: "label tracks as
/// previews").
///
/// A blank title yields just the label so the bar is never empty while a
/// preview is loaded; a present title gets the label appended with a space.
pub fn label_as_preview(title: &str) -> String {
    if title.is_empty() {
        PREVIEW_LABEL.to_string()
    } else {
        format!("{title} {PREVIEW_LABEL}")
    }
}

/// Why the Playback_Engine is unavailable, named so the TUI can show the exact
/// error Requirement 4.10 requires ("start failure or lost connection").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineFailure {
    /// The engine failed to start within its 10-second budget, or could not be
    /// spawned/connected at all (Requirement 4.10 "start failure").
    StartFailure,
    /// The engine stopped responding for 5 seconds / the connection was lost
    /// (Requirement 4.10 "lost connection").
    LostConnection,
}

impl EngineFailure {
    /// The short failure name shown in the error (Requirement 4.10).
    pub fn name(self) -> &'static str {
        match self {
            EngineFailure::StartFailure => "start failure",
            EngineFailure::LostConnection => "lost connection",
        }
    }
}

/// The user-facing Playback_Engine error state surfaced with a restart action
/// (Requirements 4.10, 4.14).
///
/// While present, the TUI shows the named failure and offers a restart; the
/// Queue and the current track are kept unchanged (Requirement 4.10). The
/// [`last_position_ms`] is the engine's last reported position for the current
/// track so that, on a successful restart+reconnect, the current track is loaded
/// at that position in paused status (Requirement 4.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineErrorState {
    /// Which failure occurred (Requirement 4.10).
    pub failure: EngineFailure,
    /// The current track id to reload on reconnect, if any (Requirement 4.14).
    pub current_track_id: Option<String>,
    /// The last reported position of the current track, reloaded paused on
    /// reconnect (Requirement 4.14).
    pub last_position_ms: u64,
}

impl EngineErrorState {
    /// Build the error state for a failure, capturing the current track and its
    /// last position so a restart can resume at exactly that point
    /// (Requirements 4.10, 4.14). Preserving the Queue is the caller's job (the
    /// Queue is never mutated here).
    pub fn new(failure: EngineFailure, current_track_id: Option<String>, last_position_ms: u64) -> Self {
        EngineErrorState {
            failure,
            current_track_id,
            last_position_ms,
        }
    }

    /// The error message shown to the user, naming the failure and the restart
    /// action (Requirement 4.10). The Queue and current track are preserved, so
    /// the message reassures the user they are kept.
    pub fn message(&self) -> String {
        format!(
            "Playback_Engine {} — press restart to try again (Queue and current track kept)",
            self.failure.name()
        )
    }

    /// The reconnect plan after a successful restart: load the current track at
    /// its last reported position, paused (Requirement 4.14). Returns `None`
    /// when there is no current track to reload.
    pub fn reconnect_load(&self) -> Option<ReconnectLoad> {
        self.current_track_id.as_ref().map(|track_id| ReconnectLoad {
            track_id: track_id.clone(),
            position_ms: self.last_position_ms,
        })
    }
}

/// The load the TUI issues to a freshly reconnected Playback_Engine: the current
/// track at its last position, which the engine loads in paused status
/// (Requirement 4.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectLoad {
    /// The current track id to load.
    pub track_id: String,
    /// The position to load it at, in milliseconds; the engine loads it paused.
    pub position_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> PreviewTrack {
        PreviewTrack {
            track_id: id.to_string(),
            preview_url: format!("https://example.com/{id}.m4a"),
        }
    }

    #[test]
    fn load_sets_paused_at_clamped_position_and_loads_sink() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        let snap = player.snapshot();
        assert_eq!(snap.track_id.as_deref(), Some("t1"));
        assert_eq!(snap.status, PlaybackStatus::Paused);
        assert_eq!(snap.position_ms, 0);
        assert_eq!(snap.duration_ms, Some(PREVIEW_DURATION_MS));
        assert!(snap.is_preview);
        assert_eq!(
            player.sink().last_command(),
            Some(&SinkCommand::Load("https://example.com/t1.m4a".to_string()))
        );
    }

    #[test]
    fn load_past_cap_clamps_to_preview_length() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), PREVIEW_DURATION_MS + 10_000);
        assert_eq!(player.snapshot().position_ms, PREVIEW_DURATION_MS);
    }

    #[test]
    fn play_then_pause_toggles_status_and_drives_sink() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        player.play();
        assert_eq!(player.snapshot().status, PlaybackStatus::Playing);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::Play));
        player.pause();
        assert_eq!(player.snapshot().status, PlaybackStatus::Paused);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::Pause));
    }

    #[test]
    fn toggle_switches_between_play_and_pause() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        player.toggle(); // paused -> playing
        assert_eq!(player.snapshot().status, PlaybackStatus::Playing);
        player.toggle(); // playing -> paused
        assert_eq!(player.snapshot().status, PlaybackStatus::Paused);
    }

    #[test]
    fn transport_is_noop_when_nothing_loaded() {
        let mut player = PreviewPlayer::default();
        player.play();
        player.pause();
        assert_eq!(player.snapshot().status, PlaybackStatus::Stopped);
        assert_eq!(player.snapshot().track_id, None);
        assert!(!player.seek(5_000));
        assert!(!player.advance(1_000));
    }

    #[test]
    fn advance_while_playing_moves_position() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        player.play();
        let finished = player.advance(1_000);
        assert!(!finished);
        assert_eq!(player.snapshot().position_ms, 1_000);
    }

    #[test]
    fn advance_while_paused_does_not_move() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 5_000);
        // Not playing: advance is a no-op.
        assert!(!player.advance(1_000));
        assert_eq!(player.snapshot().position_ms, 5_000);
    }

    #[test]
    fn advance_reaching_cap_finishes_preview() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), PREVIEW_DURATION_MS - 500);
        player.play();
        let finished = player.advance(1_000);
        assert!(finished);
        let snap = player.snapshot();
        assert_eq!(snap.position_ms, PREVIEW_DURATION_MS);
        assert_eq!(snap.status, PlaybackStatus::Paused);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::Stop));
    }

    #[test]
    fn seek_within_clip_sets_position() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        let finished = player.seek(10_000);
        assert!(!finished);
        assert_eq!(player.snapshot().position_ms, 10_000);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::Seek(10_000)));
    }

    #[test]
    fn seek_to_or_past_cap_finishes_preview() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 0);
        assert!(player.seek(PREVIEW_DURATION_MS));
        assert_eq!(player.snapshot().position_ms, PREVIEW_DURATION_MS);
        assert_eq!(player.snapshot().status, PlaybackStatus::Paused);
    }

    #[test]
    fn set_volume_clamps_and_forwards_to_sink() {
        let mut player = PreviewPlayer::default();
        player.set_volume(150);
        assert_eq!(player.snapshot().volume, 100);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::SetVolume(100)));
        player.set_volume(40);
        assert_eq!(player.snapshot().volume, 40);
    }

    #[test]
    fn stop_unloads_and_resets() {
        let mut player = PreviewPlayer::default();
        player.load(&track("t1"), 5_000);
        player.play();
        player.stop();
        let snap = player.snapshot();
        assert_eq!(snap.track_id, None);
        assert_eq!(snap.status, PlaybackStatus::Stopped);
        assert_eq!(snap.position_ms, 0);
        assert_eq!(player.sink().last_command(), Some(&SinkCommand::Stop));
    }

    #[test]
    fn label_as_preview_appends_label() {
        assert_eq!(label_as_preview("Clair de Lune"), "Clair de Lune (preview)");
        assert_eq!(label_as_preview(""), "(preview)");
    }

    #[test]
    fn engine_failure_names_match_requirement() {
        assert_eq!(EngineFailure::StartFailure.name(), "start failure");
        assert_eq!(EngineFailure::LostConnection.name(), "lost connection");
    }

    #[test]
    fn engine_error_message_names_failure_and_restart() {
        let state = EngineErrorState::new(EngineFailure::StartFailure, Some("t1".into()), 12_000);
        let msg = state.message();
        assert!(msg.contains("start failure"));
        assert!(msg.to_lowercase().contains("restart"));
    }

    #[test]
    fn reconnect_load_uses_current_track_and_last_position() {
        let state = EngineErrorState::new(EngineFailure::LostConnection, Some("t9".into()), 7_500);
        let load = state.reconnect_load().expect("a current track to reload");
        assert_eq!(load.track_id, "t9");
        // Requirement 4.14: load at the last reported position (paused by the
        // engine).
        assert_eq!(load.position_ms, 7_500);
    }

    #[test]
    fn reconnect_load_is_none_without_current_track() {
        let state = EngineErrorState::new(EngineFailure::StartFailure, None, 0);
        assert!(state.reconnect_load().is_none());
    }
}
