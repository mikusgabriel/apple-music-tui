//! Session_State types and persistence (serialization + atomic write).
//!
//! Defines `Session_State` and its nested `SerializedQueue` / `SerializedTrack`
//! types, serialized as JSON (Requirement 9.3), and the write side of the
//! Session_State store:
//!
//! - [`session_file_path`] resolves the state path
//!   `$XDG_STATE_HOME/apple-music-tui/session.json`, defaulting to
//!   `~/.local/state/apple-music-tui/session.json` when `$XDG_STATE_HOME` is
//!   unset or empty (Requirement 9.1).
//! - [`save_session`] / [`save_session_to`] serialize a `Session_State` to JSON
//!   and write it atomically via temp-file-plus-rename, creating any missing
//!   parent directories first (Requirements 9.1, 9.2). A failed write leaves any
//!   existing file untouched and surfaces a [`SessionSaveError`] so the caller
//!   can print it and still exit (Requirement 9.8).
//! - [`restore_session`] / [`restore_session_from`] read the session file back,
//!   deciding between three startup outcomes (Requirements 9.4–9.7):
//!     - a missing file yields an empty session with no warning (Requirement 9.5);
//!     - a file that cannot be read, cannot be parsed, or holds an
//!       out-of-range value yields an empty session plus a warning while the
//!       existing file is left untouched (Requirement 9.6);
//!     - a valid file is restored as paused with no audio, the position past
//!       the current track's duration clamped to 0 (Requirement 9.4).
//! - [`RestoredSession::prune_unavailable`] drops Queue tracks the API no longer
//!   exposes, keeps the remaining tracks in their saved relative order, advances
//!   the current-track pointer to the next available track (or clears it), and
//!   reports how many tracks were removed so the caller can warn (Requirement
//!   9.7).

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::RepeatMode;

/// A single track as persisted in the session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTrack {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    /// The album name, persisted for the Now_Playing_Bar. Defaults to empty for
    /// session files written before this field existed.
    #[serde(default)]
    pub album: String,
    pub duration_ms: Option<u64>,
}

/// The serialized form of the Queue: track order plus per-track metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedQueue {
    pub tracks: Vec<SerializedTrack>,
}

/// Durable session snapshot, serialized as JSON (Requirement 9.3).
///
/// Round-trip equality is defined over queue order, current pointer, position
/// (to the nearest second), volume, shuffle, and repeat.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session_State {
    pub queue: SerializedQueue,
    pub current_index: Option<usize>,
    /// Playback position persisted to the nearest second (Requirement 9.3).
    pub position_seconds: u64,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

impl Default for Session_State {
    fn default() -> Self {
        Session_State {
            queue: SerializedQueue::default(),
            current_index: None,
            position_seconds: 0,
            volume: 100,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }
}

/// Application directory under the XDG state root.
const APP_DIR: &str = "apple-music-tui";

/// File name of the persisted session within the state directory.
const SESSION_FILE: &str = "session.json";

/// An error produced while saving the Session_State (Requirement 9.8).
///
/// Any of these variants means the session on disk was left unchanged: the new
/// content is first written to a temporary file in the same directory and only
/// renamed into place once fully written, so a failure before the rename never
/// replaces or truncates an existing session file. The caller is expected to
/// print this error and still exit.
#[derive(Debug)]
pub enum SessionSaveError {
    /// The XDG state directory could not be resolved (neither `$XDG_STATE_HOME`
    /// nor `$HOME` is set), so no destination path is known.
    NoStateDir,
    /// Serializing the `Session_State` to JSON failed.
    Serialize(serde_json::Error),
    /// A filesystem operation failed (creating the directory, writing the
    /// temporary file, or renaming it into place). `path` is the file or
    /// directory the operation was acting on.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for SessionSaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionSaveError::NoStateDir => write!(
                f,
                "could not save session: neither $XDG_STATE_HOME nor $HOME is set, \
                 so the session path cannot be determined"
            ),
            SessionSaveError::Serialize(err) => {
                write!(f, "could not save session: failed to serialize state: {err}")
            }
            SessionSaveError::Io { path, source } => write!(
                f,
                "could not save session to {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for SessionSaveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SessionSaveError::NoStateDir => None,
            SessionSaveError::Serialize(err) => Some(err),
            SessionSaveError::Io { source, .. } => Some(source),
        }
    }
}

/// Resolve the directory that holds application state:
/// `$XDG_STATE_HOME/apple-music-tui`, defaulting to
/// `~/.local/state/apple-music-tui` when `$XDG_STATE_HOME` is unset or empty
/// (Requirement 9.1).
///
/// Returns `None` only when neither `$XDG_STATE_HOME` nor `$HOME` is available.
pub fn state_dir() -> Option<PathBuf> {
    state_dir_from(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Resolve the full session file path:
/// `$XDG_STATE_HOME/apple-music-tui/session.json`, defaulting to
/// `~/.local/state/apple-music-tui/session.json` (Requirement 9.1).
pub fn session_file_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(SESSION_FILE))
}

/// Pure resolver for the state directory from the given environment values,
/// so the XDG precedence (Requirement 9.1) can be tested without touching real
/// process environment variables.
///
/// `$XDG_STATE_HOME` is used only when it is set and non-empty; otherwise the
/// default `~/.local/state` is derived from `$HOME`.
fn state_dir_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join(APP_DIR));
        }
    }
    let home = home.filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join(APP_DIR),
    )
}

/// Serialize `state` to JSON and write it to the default session path
/// (Requirement 9.1), replacing any existing file atomically (Requirement 9.2).
///
/// On any failure the existing session file is left unchanged (Requirement 9.8);
/// the returned [`SessionSaveError`] is intended to be printed by the caller,
/// which then still exits.
pub fn save_session(state: &Session_State) -> Result<(), SessionSaveError> {
    let path = session_file_path().ok_or(SessionSaveError::NoStateDir)?;
    save_session_to(state, &path)
}

/// Serialize `state` to JSON and write it to `path`, creating any missing parent
/// directories (Requirement 9.1) and replacing the destination atomically via a
/// temp-file-plus-rename so the file on disk always holds a complete state
/// (Requirement 9.2).
///
/// The temporary file is created in the same directory as `path` (so the final
/// `rename` stays within one filesystem and is atomic) and is removed on a
/// write/flush failure. Because the destination is only touched by the final
/// rename, any earlier failure leaves an existing file unchanged
/// (Requirement 9.8).
pub fn save_session_to(state: &Session_State, path: &Path) -> Result<(), SessionSaveError> {
    // Serialize first: a serialization failure must not disturb the directory or
    // any existing file.
    let json = serde_json::to_vec_pretty(state).map_err(SessionSaveError::Serialize)?;

    // Ensure the parent directory exists (Requirement 9.1).
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| SessionSaveError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    // Write to a sibling temp file, then rename over the destination. The temp
    // file lives in the same directory as the destination so the rename is a
    // same-filesystem, atomic operation.
    let tmp_path = temp_path_for(path);

    write_all_synced(&tmp_path, &json).map_err(|source| {
        // Best-effort cleanup: never leave a partial temp file behind, and never
        // touch the real destination.
        let _ = fs::remove_file(&tmp_path);
        SessionSaveError::Io {
            path: tmp_path.clone(),
            source,
        }
    })?;

    fs::rename(&tmp_path, path).map_err(|source| {
        let _ = fs::remove_file(&tmp_path);
        SessionSaveError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;

    Ok(())
}

/// Build the temporary file path used while writing `path` atomically. It sits
/// in the same directory as the destination and carries a distinctive suffix so
/// it never collides with the real session file.
fn temp_path_for(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from(SESSION_FILE));
    // Disambiguate concurrent writers within the same process run.
    file_name.push(format!(".tmp.{}", std::process::id()));
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

/// Write `bytes` to `tmp_path`, flushing and fsyncing before returning so the
/// data is durable on disk prior to the rename (Requirement 9.2).
fn write_all_synced(tmp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(tmp_path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// The supported volume range for a restored session (Requirement 9.6).
///
/// The persisted `volume` is a `u8`, so values above 100 are the only way it
/// can fall outside the TUI's 0–100 percent range; a value in this range is
/// accepted as-is.
const MAX_VOLUME: u8 = 100;

/// Why a restore fell back to an empty session (Requirement 9.6).
///
/// Every variant means the on-disk file was left untouched; the caller surfaces
/// the attached message as a warning. [`RestoreWarning::reason`] returns a
/// human-readable description with no token or credential content.
#[derive(Debug)]
pub enum RestoreWarning {
    /// The session file exists but could not be read (for example a permission
    /// error or an I/O failure). `source` is the underlying error.
    Unreadable(io::Error),
    /// The session file exists but does not parse as a `Session_State`.
    Unparseable(serde_json::Error),
    /// The session parsed but a field value was outside its allowed range, such
    /// as a current pointer outside the Queue bounds or a volume above 100.
    OutOfRange(String),
}

impl RestoreWarning {
    /// A user-facing description of why the previous session could not be
    /// restored (Requirement 9.6), free of any token or credential content.
    pub fn reason(&self) -> String {
        match self {
            RestoreWarning::Unreadable(err) => {
                format!("the previous session could not be read: {err}")
            }
            RestoreWarning::Unparseable(err) => {
                format!("the previous session file was not valid: {err}")
            }
            RestoreWarning::OutOfRange(detail) => {
                format!("the previous session contained an invalid value: {detail}")
            }
        }
    }
}

impl fmt::Display for RestoreWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not restore the previous session, starting fresh: {}",
            self.reason()
        )
    }
}

impl Error for RestoreWarning {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RestoreWarning::Unreadable(err) => Some(err),
            RestoreWarning::Unparseable(err) => Some(err),
            RestoreWarning::OutOfRange(_) => None,
        }
    }
}

/// The result of restoring a session (Requirements 9.4–9.6).
///
/// The restored `state` is always paused with no audio output until the user
/// starts playback; this type carries data only, so "paused with no audio" is a
/// property of how the caller applies it rather than a stored flag. An optional
/// `warning` is present only when the restore fell back to defaults because the
/// existing file was unreadable, unparseable, or out of range (Requirement
/// 9.6); a missing file produces an empty session with no warning (Requirement
/// 9.5).
#[derive(Debug)]
pub struct RestoredSession {
    /// The session to apply at startup. Defaults to [`Session_State::default`]
    /// on a missing file or a fallback.
    pub state: Session_State,
    /// Present only on a fallback; `None` for both a clean restore and a
    /// missing file.
    pub warning: Option<RestoreWarning>,
}

impl RestoredSession {
    /// An empty, warning-free restore used when no session file exists
    /// (Requirement 9.5).
    fn empty() -> Self {
        RestoredSession {
            state: Session_State::default(),
            warning: None,
        }
    }

    /// A fallback restore: an empty session paired with the warning explaining
    /// why the saved session was discarded (Requirement 9.6).
    fn fallback(warning: RestoreWarning) -> Self {
        RestoredSession {
            state: Session_State::default(),
            warning: Some(warning),
        }
    }

    /// Remove Queue tracks that the Apple Music API no longer exposes
    /// (Requirement 9.7).
    ///
    /// `is_available` is called once per track id and returns whether that
    /// track is still available. Unavailable tracks are dropped, the remaining
    /// tracks keep their saved relative order, and the current-track pointer is
    /// moved to the next available track at or after its old position (or
    /// cleared when none remain). Returns the number of tracks removed so the
    /// caller can warn when it is non-zero.
    pub fn prune_unavailable<F>(&mut self, mut is_available: F) -> usize
    where
        F: FnMut(&str) -> bool,
    {
        let original_len = self.state.queue.tracks.len();
        if original_len == 0 {
            return 0;
        }

        // Mark availability up front so the pointer remap and the retain pass
        // agree on the same decision for each track.
        let keep: Vec<bool> = self
            .state
            .queue
            .tracks
            .iter()
            .map(|t| is_available(&t.track_id))
            .collect();

        // Remap the current pointer to the next surviving track at or after its
        // old index; if none survive from there, clear it (Requirement 9.7).
        let new_current = self.state.current_index.and_then(|old| {
            if old >= original_len {
                // Out-of-range pointers are rejected before pruning, but stay
                // defensive: treat as "no surviving track from here".
                return None;
            }
            keep.iter()
                .enumerate()
                .skip(old)
                .find(|(_, &kept)| kept)
                .map(|(idx, _)| {
                    // The new index is the count of kept tracks strictly before
                    // `idx`.
                    keep[..idx].iter().filter(|&&k| k).count()
                })
        });

        // Keep survivors in their saved relative order.
        let mut kept_iter = keep.iter();
        self.state
            .queue
            .tracks
            .retain(|_| *kept_iter.next().unwrap());

        self.state.current_index = new_current;

        original_len - self.state.queue.tracks.len()
    }
}

/// Restore the session from the default path (Requirements 9.4–9.6).
///
/// Resolving the path can fail only when neither `$XDG_STATE_HOME` nor `$HOME`
/// is set; that is treated like a missing file (an empty, warning-free session),
/// since there is no file to read and nothing to warn about.
pub fn restore_session() -> RestoredSession {
    match session_file_path() {
        Some(path) => restore_session_from(&path),
        None => RestoredSession::empty(),
    }
}

/// Restore the session from `path` (Requirements 9.4–9.6).
///
/// - A missing file yields an empty session with no warning (Requirement 9.5).
/// - A file that cannot be read, cannot be parsed, or carries an out-of-range
///   value yields an empty session plus a [`RestoreWarning`]; the file on disk
///   is never modified here (Requirement 9.6).
/// - A valid file is restored as-is except that a saved position greater than
///   the current track's duration is clamped to 0 seconds (Requirement 9.4).
///
/// The returned state carries no "playing" flag; the caller applies it as
/// paused with no audio until the user starts playback (Requirement 9.4).
pub fn restore_session_from(path: &Path) -> RestoredSession {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // No saved session: start empty without a warning (Requirement 9.5).
            return RestoredSession::empty();
        }
        Err(err) => {
            // The file exists but could not be read (Requirement 9.6).
            return RestoredSession::fallback(RestoreWarning::Unreadable(err));
        }
    };

    let mut state: Session_State = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(err) => return RestoredSession::fallback(RestoreWarning::Unparseable(err)),
    };

    if let Err(detail) = validate_restored(&state) {
        return RestoredSession::fallback(RestoreWarning::OutOfRange(detail));
    }

    clamp_position(&mut state);

    RestoredSession {
        state,
        warning: None,
    }
}

/// Validate the ranges of a freshly parsed `Session_State` (Requirement 9.6).
///
/// Returns `Err(detail)` naming the first offending field when the current
/// pointer is outside the Queue bounds or the volume is above the supported
/// maximum. The position is not validated here: a position past the current
/// track's duration is clamped rather than rejected (Requirement 9.4).
fn validate_restored(state: &Session_State) -> Result<(), String> {
    if let Some(current) = state.current_index {
        let len = state.queue.tracks.len();
        if current >= len {
            return Err(format!(
                "current track pointer {current} is outside the queue of {len} track(s)"
            ));
        }
    } else if !state.queue.tracks.is_empty() {
        // A non-empty queue with no current pointer cannot identify a current
        // track; treat it as out of range so we fall back cleanly.
        return Err(format!(
            "no current track pointer for a queue of {} track(s)",
            state.queue.tracks.len()
        ));
    }

    if state.volume > MAX_VOLUME {
        return Err(format!(
            "volume {} is outside the supported range 0..={MAX_VOLUME}",
            state.volume
        ));
    }

    Ok(())
}

/// Clamp a saved position that exceeds the current track's duration to 0
/// (Requirement 9.4).
///
/// Durations are stored in milliseconds while the position is persisted in
/// whole seconds, so the comparison converts the duration down to whole seconds
/// first. Tracks without a known duration leave the position untouched.
fn clamp_position(state: &mut Session_State) {
    let Some(current) = state.current_index else {
        return;
    };
    let Some(track) = state.queue.tracks.get(current) else {
        return;
    };
    if let Some(duration_ms) = track.duration_ms {
        let duration_seconds = duration_ms / 1000;
        if state.position_seconds > duration_seconds {
            state.position_seconds = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn sample_state() -> Session_State {
        Session_State {
            queue: SerializedQueue {
                tracks: vec![
                    SerializedTrack {
                        track_id: "1".to_string(),
                        title: "First".to_string(),
                        artist: "Artist A".to_string(),
                        album: String::new(),
                        duration_ms: Some(210_000),
                    },
                    SerializedTrack {
                        track_id: "2".to_string(),
                        title: "Second".to_string(),
                        artist: "Artist B".to_string(),
                        album: String::new(),
                        duration_ms: None,
                    },
                ],
            },
            current_index: Some(1),
            position_seconds: 42,
            volume: 70,
            shuffle: true,
            repeat: RepeatMode::RepeatAll,
        }
    }

    /// A unique temp directory for a test, created under the OS temp dir.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "amtui-session-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        fs::create_dir_all(&dir).expect("create test temp dir");
        dir
    }

    #[test]
    fn json_round_trip_preserves_all_fields() {
        let state = sample_state();
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: Session_State = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, state);
    }

    #[test]
    fn default_state_round_trips() {
        let state = Session_State::default();
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: Session_State = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, state);
    }

    #[test]
    fn save_writes_readable_json_at_path() {
        let dir = unique_temp_dir("write");
        let path = dir.join(SESSION_FILE);
        let state = sample_state();

        save_session_to(&state, &path).expect("save succeeds");

        let bytes = fs::read(&path).expect("read back");
        let restored: Session_State = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(restored, state);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = unique_temp_dir("mkdir");
        // Destination is several levels below an existing directory.
        let path = dir.join("a").join("b").join("c").join(SESSION_FILE);

        save_session_to(&sample_state(), &path).expect("save creates parents");
        assert!(path.exists(), "session file should exist after save");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = unique_temp_dir("notemp");
        let path = dir.join(SESSION_FILE);

        save_session_to(&sample_state(), &path).expect("save succeeds");

        // Only the final session file should remain in the directory.
        let entries: Vec<OsString> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from(SESSION_FILE)]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overwriting_replaces_previous_content_atomically() {
        let dir = unique_temp_dir("overwrite");
        let path = dir.join(SESSION_FILE);

        let first = Session_State::default();
        save_session_to(&first, &path).expect("first save");

        let second = sample_state();
        save_session_to(&second, &path).expect("second save");

        let restored: Session_State =
            serde_json::from_slice(&fs::read(&path).unwrap()).expect("parse");
        assert_eq!(restored, second);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_write_leaves_existing_file_unchanged() {
        let dir = unique_temp_dir("fail");
        let path = dir.join(SESSION_FILE);

        // Seed an existing, valid session file.
        let original = sample_state();
        save_session_to(&original, &path).expect("seed save");
        let original_bytes = fs::read(&path).expect("read seed");

        // Make the directory read-only so creating the temp file fails. The
        // existing session file must be left untouched (Requirement 9.8).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(0o500); // r-x------ : no write, so File::create fails
            fs::set_permissions(&dir, perms).unwrap();

            let result = save_session_to(&Session_State::default(), &path);

            // Restore permissions before asserting so cleanup always works.
            let mut restore = fs::metadata(&dir).unwrap().permissions();
            restore.set_mode(0o700);
            fs::set_permissions(&dir, restore).unwrap();

            assert!(result.is_err(), "write into read-only dir should fail");
            match result.unwrap_err() {
                SessionSaveError::Io { .. } => {}
                other => panic!("expected an I/O error, got {other:?}"),
            }

            let after = fs::read(&path).expect("existing file still present");
            assert_eq!(after, original_bytes, "existing session must be unchanged");
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn temp_path_is_sibling_of_destination() {
        let path = Path::new("/var/state/apple-music-tui/session.json");
        let tmp = temp_path_for(path);
        assert_eq!(tmp.parent(), path.parent());
        assert_ne!(tmp, path.to_path_buf());
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("session.json.tmp."));
    }

    #[test]
    fn state_dir_prefers_xdg_state_home_when_set() {
        let dir = state_dir_from(
            Some(OsString::from("/custom/state")),
            Some(OsString::from("/home/user")),
        )
        .expect("resolves");
        assert_eq!(dir, PathBuf::from("/custom/state").join(APP_DIR));
    }

    #[test]
    fn state_dir_falls_back_to_home_when_xdg_unset_or_empty() {
        let expected = PathBuf::from("/home/user")
            .join(".local")
            .join("state")
            .join(APP_DIR);

        let from_unset = state_dir_from(None, Some(OsString::from("/home/user")))
            .expect("resolves from home");
        assert_eq!(from_unset, expected);

        let from_empty = state_dir_from(
            Some(OsString::from("")),
            Some(OsString::from("/home/user")),
        )
        .expect("empty xdg falls back to home");
        assert_eq!(from_empty, expected);
    }

    #[test]
    fn state_dir_is_none_without_xdg_or_home() {
        assert!(state_dir_from(None, None).is_none());
        assert!(state_dir_from(Some(OsString::from("")), Some(OsString::from(""))).is_none());
    }

    #[test]
    fn session_file_path_ends_with_app_dir_and_file() {
        let dir = state_dir_from(Some(OsString::from("/s")), None).unwrap();
        let path = dir.join(SESSION_FILE);
        assert_eq!(path, PathBuf::from("/s").join(APP_DIR).join(SESSION_FILE));
    }

    // --- restore: missing file (R9.5) ---

    #[test]
    fn restore_missing_file_is_empty_without_warning() {
        let dir = unique_temp_dir("restore-missing");
        let path = dir.join(SESSION_FILE); // never created

        let restored = restore_session_from(&path);

        assert_eq!(restored.state, Session_State::default());
        assert!(restored.warning.is_none(), "missing file must not warn");

        fs::remove_dir_all(&dir).ok();
    }

    // --- restore: valid file as paused, position clamping (R9.4) ---

    #[test]
    fn restore_valid_file_round_trips_state() {
        let dir = unique_temp_dir("restore-valid");
        let path = dir.join(SESSION_FILE);
        let state = sample_state(); // position 42s, current duration None
        save_session_to(&state, &path).expect("seed");

        let restored = restore_session_from(&path);

        assert!(restored.warning.is_none());
        assert_eq!(restored.state, state);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_clamps_position_past_duration_to_zero() {
        let dir = unique_temp_dir("restore-clamp");
        let path = dir.join(SESSION_FILE);

        let mut state = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "1".to_string(),
                    title: "Short".to_string(),
                    artist: "A".to_string(),
                    album: String::new(),
                    duration_ms: Some(30_000), // 30 seconds
                }],
            },
            current_index: Some(0),
            position_seconds: 120, // well past the 30s duration
            volume: 50,
            shuffle: false,
            repeat: RepeatMode::Off,
        };
        save_session_to(&state, &path).expect("seed");

        let restored = restore_session_from(&path);

        state.position_seconds = 0;
        assert!(restored.warning.is_none());
        assert_eq!(restored.state, state, "position should clamp to 0");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_keeps_position_within_duration() {
        let dir = unique_temp_dir("restore-within");
        let path = dir.join(SESSION_FILE);

        let state = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "1".to_string(),
                    title: "Long".to_string(),
                    artist: "A".to_string(),
                    album: String::new(),
                    duration_ms: Some(300_000), // 300 seconds
                }],
            },
            current_index: Some(0),
            position_seconds: 120,
            volume: 50,
            shuffle: false,
            repeat: RepeatMode::Off,
        };
        save_session_to(&state, &path).expect("seed");

        let restored = restore_session_from(&path);
        assert_eq!(restored.state.position_seconds, 120);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_without_duration_leaves_position_untouched() {
        let dir = unique_temp_dir("restore-noduration");
        let path = dir.join(SESSION_FILE);

        let state = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "1".to_string(),
                    title: "Unknown".to_string(),
                    artist: "A".to_string(),
                    album: String::new(),
                    duration_ms: None,
                }],
            },
            current_index: Some(0),
            position_seconds: 999,
            volume: 50,
            shuffle: false,
            repeat: RepeatMode::Off,
        };
        save_session_to(&state, &path).expect("seed");

        let restored = restore_session_from(&path);
        assert_eq!(restored.state.position_seconds, 999);

        fs::remove_dir_all(&dir).ok();
    }

    // --- restore: corrupt / out-of-range fallback (R9.6) ---

    #[test]
    fn restore_unparseable_falls_back_with_warning_and_keeps_file() {
        let dir = unique_temp_dir("restore-corrupt");
        let path = dir.join(SESSION_FILE);
        let garbage = b"{ this is not valid json";
        fs::write(&path, garbage).expect("write garbage");

        let restored = restore_session_from(&path);

        assert_eq!(restored.state, Session_State::default());
        assert!(matches!(
            restored.warning,
            Some(RestoreWarning::Unparseable(_))
        ));
        // The corrupt file must be left untouched (Requirement 9.6).
        assert_eq!(fs::read(&path).unwrap(), garbage);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_pointer_out_of_bounds_falls_back() {
        let dir = unique_temp_dir("restore-ptr");
        let path = dir.join(SESSION_FILE);

        // current_index past the end of a one-track queue.
        let state = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "1".to_string(),
                    title: "T".to_string(),
                    artist: "A".to_string(),
                    album: String::new(),
                    duration_ms: Some(1000),
                }],
            },
            current_index: Some(5),
            position_seconds: 0,
            volume: 50,
            shuffle: false,
            repeat: RepeatMode::Off,
        };
        // Bypass save (which never validates) by writing JSON directly.
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let restored = restore_session_from(&path);
        assert_eq!(restored.state, Session_State::default());
        assert!(matches!(
            restored.warning,
            Some(RestoreWarning::OutOfRange(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_volume_out_of_range_falls_back() {
        let dir = unique_temp_dir("restore-vol");
        let path = dir.join(SESSION_FILE);

        // Write JSON with a volume above 100 directly (the struct is a u8 so we
        // craft the JSON text rather than the typed value).
        let json = r#"{
            "queue": { "tracks": [] },
            "current_index": null,
            "position_seconds": 0,
            "volume": 150,
            "shuffle": false,
            "repeat": "Off"
        }"#;
        fs::write(&path, json).unwrap();

        let restored = restore_session_from(&path);
        assert_eq!(restored.state, Session_State::default());
        assert!(matches!(
            restored.warning,
            Some(RestoreWarning::OutOfRange(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_nonempty_queue_without_pointer_falls_back() {
        let dir = unique_temp_dir("restore-noptr");
        let path = dir.join(SESSION_FILE);

        let state = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "1".to_string(),
                    title: "T".to_string(),
                    artist: "A".to_string(),
                    album: String::new(),
                    duration_ms: Some(1000),
                }],
            },
            current_index: None,
            position_seconds: 0,
            volume: 50,
            shuffle: false,
            repeat: RepeatMode::Off,
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let restored = restore_session_from(&path);
        assert!(matches!(
            restored.warning,
            Some(RestoreWarning::OutOfRange(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_warning_reason_excludes_sensitive_wording() {
        let warning = RestoreWarning::OutOfRange("volume 150 is out of range".to_string());
        let reason = warning.reason();
        assert!(reason.contains("invalid value"));
        assert!(reason.contains("volume 150"));
    }

    // --- prune unavailable tracks (R9.7) ---

    fn strack(id: &str) -> SerializedTrack {
        SerializedTrack { track_id: id.to_string(), title: format!("Title {id}"), artist: format!("Artist {id}"), album: String::new(), duration_ms: Some(180_000) }
    }

    fn restored_with(tracks: &[&str], current: Option<usize>) -> RestoredSession {
        RestoredSession {
            state: Session_State {
                queue: SerializedQueue {
                    tracks: tracks.iter().map(|id| strack(id)).collect(),
                },
                current_index: current,
                position_seconds: 0,
                volume: 80,
                shuffle: false,
                repeat: RepeatMode::Off,
            },
            warning: None,
        }
    }

    fn track_ids(r: &RestoredSession) -> Vec<String> {
        r.state
            .queue
            .tracks
            .iter()
            .map(|t| t.track_id.clone())
            .collect()
    }

    #[test]
    fn prune_removes_unavailable_and_keeps_relative_order() {
        let mut r = restored_with(&["a", "b", "c", "d"], Some(0));
        let available: &[&str] = &["a", "c"];
        let removed = r.prune_unavailable(|id| available.contains(&id));

        assert_eq!(removed, 2);
        assert_eq!(track_ids(&r), ["a", "c"]);
        // "a" survives at index 0, so the pointer stays on it.
        assert_eq!(r.state.current_index, Some(0));
    }

    #[test]
    fn prune_advances_pointer_when_current_removed() {
        // current is "b" (index 1); "b" is unavailable, so the pointer should
        // advance to the next surviving track after it ("d").
        let mut r = restored_with(&["a", "b", "c", "d"], Some(1));
        let available: &[&str] = &["a", "d"];
        let removed = r.prune_unavailable(|id| available.contains(&id));

        assert_eq!(removed, 2);
        assert_eq!(track_ids(&r), ["a", "d"]);
        // "d" is now at index 1.
        assert_eq!(r.state.current_index, Some(1));
    }

    #[test]
    fn prune_clears_pointer_when_no_track_survives_from_current() {
        // current is "c" (index 2); nothing from index 2 onward survives.
        let mut r = restored_with(&["a", "b", "c", "d"], Some(2));
        let available: &[&str] = &["a"];
        let removed = r.prune_unavailable(|id| available.contains(&id));

        assert_eq!(removed, 3);
        assert_eq!(track_ids(&r), ["a"]);
        assert_eq!(r.state.current_index, None);
    }

    #[test]
    fn prune_clears_pointer_when_all_removed() {
        let mut r = restored_with(&["a", "b"], Some(0));
        let removed = r.prune_unavailable(|_| false);

        assert_eq!(removed, 2);
        assert!(r.state.queue.tracks.is_empty());
        assert_eq!(r.state.current_index, None);
    }

    #[test]
    fn prune_keeps_pointer_when_current_available_after_earlier_removals() {
        // current is "d" (index 3). "a" and "c" removed; "d" survives and
        // becomes index 1 (only "b" precedes it among survivors).
        let mut r = restored_with(&["a", "b", "c", "d"], Some(3));
        let available: &[&str] = &["b", "d"];
        let removed = r.prune_unavailable(|id| available.contains(&id));

        assert_eq!(removed, 2);
        assert_eq!(track_ids(&r), ["b", "d"]);
        assert_eq!(r.state.current_index, Some(1));
    }

    #[test]
    fn prune_all_available_is_a_noop() {
        let mut r = restored_with(&["a", "b", "c"], Some(1));
        let removed = r.prune_unavailable(|_| true);

        assert_eq!(removed, 0);
        assert_eq!(track_ids(&r), ["a", "b", "c"]);
        assert_eq!(r.state.current_index, Some(1));
    }

    #[test]
    fn prune_empty_queue_returns_zero() {
        let mut r = restored_with(&[], None);
        let removed = r.prune_unavailable(|_| false);
        assert_eq!(removed, 0);
        assert_eq!(r.state.current_index, None);
    }

    // --- property: corrupt / out-of-range fallback (R9.6) ---

    mod property_tests {
        use super::super::*;
        use super::unique_temp_dir;
        use proptest::prelude::*;
        use std::fs;

        /// Build a serialized track from a generated id and optional duration.
        fn make_track(id: u64, duration_ms: Option<u64>) -> SerializedTrack {
            SerializedTrack {
                track_id: format!("track-{id}"),
                title: format!("Title {id}"),
                artist: format!("Artist {id}"),
                album: String::new(),
                duration_ms,
            }
        }

        /// Serialize a `Session_State` to JSON bytes and write them to a fresh
        /// session file, returning the path and the bytes on disk. The write
        /// goes straight to disk (bypassing `save_session_to`, which never
        /// validates ranges) so out-of-range values survive to the restore.
        fn seed_json(dir: &std::path::Path, state: &Session_State) -> (std::path::PathBuf, Vec<u8>) {
            let path = dir.join(SESSION_FILE);
            let bytes = serde_json::to_vec(state).expect("serialize seed");
            fs::write(&path, &bytes).expect("write seed");
            (path, bytes)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: apple-music-tui, Property 23: Corrupt or out-of-range session falls back to defaults
            // Validates: Requirements 9.6
            //
            // For any session file that is unreadable/unparseable, or that holds
            // a current-track pointer outside the Queue bounds or a volume above
            // the supported maximum, restoring at startup yields an empty Queue
            // with the default Playback_State, raises a warning, and leaves the
            // session file byte-for-byte unmodified.
            #[test]
            fn property_corrupt_or_out_of_range_falls_back(
                // Arbitrary bytes that are (almost always) not valid session
                // JSON. A leading byte that cannot start JSON keeps it
                // unparseable even if proptest produces otherwise-structured
                // bytes.
                garbage in proptest::collection::vec(any::<u8>(), 0..64),
                // A queue of 0..=8 tracks with arbitrary durations.
                durations in proptest::collection::vec(
                    proptest::option::of(0u64..600_000),
                    0..9usize,
                ),
                // A pointer that we force to be out of bounds for the queue.
                pointer_offset in 0usize..5,
                position_seconds in 0u64..100_000,
                // A volume strictly above the supported maximum (0..=100).
                // The band spans both representable-u8 values (101..=255,
                // rejected as out-of-range) and values that overflow a u8
                // (256.., rejected while parsing); either way the restore must
                // fall back with a warning.
                over_volume in 101u16..=1000,
                shuffle in any::<bool>(),
            ) {
                // --- Case 1: unparseable bytes ---
                {
                    let dir = unique_temp_dir("prop-unparseable");
                    let path = dir.join(SESSION_FILE);
                    // Prefix with a byte that cannot begin a JSON document so
                    // the content is always unparseable as a Session_State.
                    let mut bytes = vec![b'\x7f'];
                    bytes.extend_from_slice(&garbage);
                    fs::write(&path, &bytes).expect("write garbage");

                    let restored = restore_session_from(&path);

                    prop_assert_eq!(&restored.state, &Session_State::default());
                    prop_assert!(restored.warning.is_some(), "corrupt file must warn");
                    // The file on disk is left untouched (Requirement 9.6).
                    prop_assert_eq!(fs::read(&path).expect("file present"), bytes);

                    fs::remove_dir_all(&dir).ok();
                }

                // --- Case 2: current-track pointer outside the queue bounds ---
                {
                    let dir = unique_temp_dir("prop-pointer");
                    let tracks: Vec<SerializedTrack> = durations
                        .iter()
                        .enumerate()
                        .map(|(i, &d)| make_track(i as u64, d))
                        .collect();
                    let len = tracks.len();
                    // Always point at or past the end of the queue (len..len+5),
                    // which is outside the valid 0..len bounds.
                    let bad_pointer = len + pointer_offset;
                    let state = Session_State {
                        queue: SerializedQueue { tracks },
                        current_index: Some(bad_pointer),
                        position_seconds,
                        volume: 50,
                        shuffle,
                        repeat: RepeatMode::Off,
                    };
                    let (path, bytes) = seed_json(&dir, &state);

                    let restored = restore_session_from(&path);

                    prop_assert_eq!(&restored.state, &Session_State::default());
                    prop_assert!(
                        matches!(restored.warning, Some(RestoreWarning::OutOfRange(_))),
                        "an out-of-range pointer must fall back with an out-of-range warning"
                    );
                    prop_assert_eq!(fs::read(&path).expect("file present"), bytes);

                    fs::remove_dir_all(&dir).ok();
                }

                // --- Case 3: volume above the supported maximum ---
                {
                    let dir = unique_temp_dir("prop-volume");
                    let path = dir.join(SESSION_FILE);
                    // The struct's volume is a u8, so an over-range value can
                    // only reach restore via crafted JSON text.
                    let json = format!(
                        "{{\"queue\":{{\"tracks\":[]}},\"current_index\":null,\
                         \"position_seconds\":{position_seconds},\"volume\":{over_volume},\
                         \"shuffle\":{shuffle},\"repeat\":\"Off\"}}"
                    );
                    fs::write(&path, &json).expect("write over-volume json");

                    let restored = restore_session_from(&path);

                    prop_assert_eq!(&restored.state, &Session_State::default());
                    // A volume in 101..=255 is a valid u8 rejected as out of
                    // range; 256.. overflows the u8 and is rejected while
                    // parsing. Both are valid fallbacks under Requirement 9.6,
                    // so only require a warning of the expected kind.
                    match &restored.warning {
                        Some(RestoreWarning::OutOfRange(_)) => {
                            prop_assert!(
                                over_volume <= u16::from(u8::MAX),
                                "out-of-range only applies to representable u8 volumes"
                            );
                        }
                        Some(RestoreWarning::Unparseable(_)) => {
                            prop_assert!(
                                over_volume > u16::from(u8::MAX),
                                "unparseable only applies to volumes that overflow a u8"
                            );
                        }
                        other => prop_assert!(
                            false,
                            "an over-range volume must fall back with a warning, got {:?}",
                            other
                        ),
                    }
                    prop_assert_eq!(fs::read(&path).expect("file present"), json.into_bytes());

                    fs::remove_dir_all(&dir).ok();
                }
            }
        }

        /// Build a `RestoredSession` from a list of track ids and an optional
        /// current-track pointer, mirroring the unit-test helper so the
        /// property exercises the same shape of input.
        fn restored_from_ids(ids: &[u64], current: Option<usize>) -> RestoredSession {
            RestoredSession {
                state: Session_State {
                    queue: SerializedQueue {
                        tracks: ids.iter().map(|&id| make_track(id, Some(180_000))).collect(),
                    },
                    current_index: current,
                    position_seconds: 0,
                    volume: 80,
                    shuffle: false,
                    repeat: RepeatMode::Off,
                },
                warning: None,
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: apple-music-tui, Property 24: Pruning unavailable tracks on restore
            // Validates: Requirements 9.7
            //
            // For any restored Queue and any subset of its tracks marked
            // unavailable, the pruned Queue contains exactly the available
            // tracks in their original relative order and moves the
            // current-track pointer to the next available track at or after its
            // old position, or clears it when no available track remains.
            #[test]
            fn property_prune_unavailable_keeps_available_in_order(
                // Distinct track ids (0..count) keep availability lookups
                // unambiguous; order is the identity of the queue here.
                count in 0usize..16,
                // One availability bit per possible track position; only the
                // first `count` entries are consulted.
                availability in proptest::collection::vec(any::<bool>(), 16),
                // A fraction mapped into the queue bounds to pick a valid
                // current-track pointer (or None when the queue is empty).
                has_current in any::<bool>(),
                current_frac in 0.0f64..1.0,
            ) {
                let ids: Vec<u64> = (0..count as u64).collect();
                let current = if count == 0 || !has_current {
                    None
                } else {
                    let idx = (current_frac * count as f64) as usize;
                    Some(idx.min(count - 1))
                };

                // `available[id as usize]` decides each track; capture it before
                // the move so the predicate and the expectations agree.
                let avail: Vec<bool> = (0..count).map(|i| availability[i]).collect();

                let mut r = restored_from_ids(&ids, current);
                let removed = r.prune_unavailable(|id| {
                    let n: u64 = id
                        .strip_prefix("track-")
                        .expect("generated id prefix")
                        .parse()
                        .expect("generated id number");
                    avail[n as usize]
                });

                // Expected survivors: available track ids in their original
                // relative order.
                let expected_ids: Vec<u64> = ids
                    .iter()
                    .copied()
                    .filter(|&id| avail[id as usize])
                    .collect();
                let actual_ids: Vec<u64> = r
                    .state
                    .queue
                    .tracks
                    .iter()
                    .map(|t| {
                        t.track_id
                            .strip_prefix("track-")
                            .unwrap()
                            .parse::<u64>()
                            .unwrap()
                    })
                    .collect();

                // Exactly the available tracks, in their original order.
                prop_assert_eq!(&actual_ids, &expected_ids);

                // The reported count equals how many were dropped.
                prop_assert_eq!(removed, count - expected_ids.len());

                // The pointer moves to the next available track at or after its
                // old position, mapped into the pruned queue's indices; it is
                // cleared when none survive from there (or there was none).
                let expected_current = current.and_then(|old| {
                    // First kept index at or after `old` in the original queue.
                    (old..count).find(|&i| avail[i]).map(|surviving_original| {
                        // Its new index is the number of survivors strictly
                        // before it.
                        (0..surviving_original).filter(|&i| avail[i]).count()
                    })
                });
                prop_assert_eq!(r.state.current_index, expected_current);

                // When the pointer is set, it addresses the expected track id.
                if let (Some(new_idx), Some(old)) = (r.state.current_index, current) {
                    let expected_surviving_id =
                        (old..count).find(|&i| avail[i]).map(|i| ids[i]);
                    prop_assert_eq!(Some(actual_ids[new_idx]), expected_surviving_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod round_trip_property_tests {
    //! Property-based test for Session_State JSON serialization round-trip
    //! (task 4.3). Runs the single design correctness property at ≥100
    //! iterations.
    use super::*;
    use proptest::prelude::*;

    /// Strategy for a single serialized track with arbitrary metadata and an
    /// optional duration.
    fn track_strategy() -> impl Strategy<Value = SerializedTrack> {
        (
            ".{0,32}",
            ".{0,64}",
            ".{0,64}",
            proptest::option::of(0u64..1_000_000_000),
        )
            .prop_map(|(track_id, title, artist, duration_ms)| SerializedTrack {
                track_id,
                title,
                artist,
                album: String::new(),
                duration_ms,
            })
    }

    /// Strategy for a `RepeatMode` across all three variants.
    fn repeat_strategy() -> impl Strategy<Value = RepeatMode> {
        prop_oneof![
            Just(RepeatMode::Off),
            Just(RepeatMode::RepeatAll),
            Just(RepeatMode::RepeatOne),
        ]
    }

    /// Strategy for an arbitrary valid `Session_State`.
    ///
    /// The current-track pointer is generated as a fraction and mapped into the
    /// queue's bounds so it always refers to a real track (or `None` when the
    /// queue is empty), matching the input space of a persisted session. The
    /// volume stays within the supported 0..=100 range and the position is a
    /// whole number of seconds, as it is persisted to the nearest second.
    fn session_state_strategy() -> impl Strategy<Value = Session_State> {
        (
            prop::collection::vec(track_strategy(), 0..8),
            any::<bool>(),
            0.0f64..=1.0,
            0u64..=1_000_000,
            0u8..=100,
            any::<bool>(),
            repeat_strategy(),
        )
            .prop_map(
                |(tracks, has_current, current_frac, position_seconds, volume, shuffle, repeat)| {
                    let current_index = if tracks.is_empty() || !has_current {
                        None
                    } else {
                        let idx = (current_frac * tracks.len() as f64) as usize;
                        Some(idx.min(tracks.len() - 1))
                    };
                    Session_State {
                        queue: SerializedQueue { tracks },
                        current_index,
                        position_seconds,
                        volume,
                        shuffle,
                        repeat,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 22: Session_State serialize then deserialize round-trip
        // Validates: Requirements 9.3
        //
        // For any valid Session_State, deserializing its serialization produces
        // a Session_State equal to the original in queue track order,
        // current-track pointer, current position (to the nearest second),
        // volume, shuffle mode, and repeat mode.
        #[test]
        fn property_session_state_round_trip(state in session_state_strategy()) {
            let json = serde_json::to_string(&state).expect("serialize");
            let restored: Session_State =
                serde_json::from_str(&json).expect("deserialize");

            // Whole-struct equality covers queue track order, the current-track
            // pointer, position (already whole seconds), volume, shuffle, and
            // repeat mode.
            prop_assert_eq!(&restored, &state);

            // Spell out the per-field guarantees the property names so a
            // regression points at the exact field.
            prop_assert_eq!(
                restored
                    .queue
                    .tracks
                    .iter()
                    .map(|t| &t.track_id)
                    .collect::<Vec<_>>(),
                state
                    .queue
                    .tracks
                    .iter()
                    .map(|t| &t.track_id)
                    .collect::<Vec<_>>()
            );
            prop_assert_eq!(restored.current_index, state.current_index);
            prop_assert_eq!(restored.position_seconds, state.position_seconds);
            prop_assert_eq!(restored.volume, state.volume);
            prop_assert_eq!(restored.shuffle, state.shuffle);
            prop_assert_eq!(restored.repeat, state.repeat);
        }
    }
}
