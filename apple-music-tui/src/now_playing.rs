//! Now_Playing_Bar state, field rendering, and responsive truncation.
//!
//! Defines `Playback_State`, the TUI's authoritative mirror of the
//! Playback_Engine's state, plus the pure rendering/truncation logic for the
//! Now_Playing_Bar (Requirement 6). The rendering is implemented as pure,
//! testable functions that produce the final text line; the ratatui widget
//! drawing (a later task) only has to paint the string these functions return,
//! which keeps the truncation logic property-testable in isolation (subtask
//! 2.4).

use crate::domain::{PlaybackStatus, RepeatMode};

/// Placeholder shown for a field whose value is unknown (Requirement 6.7).
pub const UNKNOWN_PLACEHOLDER: &str = "Unknown";

/// Message shown in place of the track fields while nothing is loaded
/// (Requirement 6.3).
pub const NOTHING_PLAYING: &str = "Nothing playing";

/// The single-character ellipsis appended to a truncated field (Requirement 6.4).
pub const ELLIPSIS: char = '\u{2026}';

/// The TUI's authoritative mirror of playback state.
///
/// The Playback_Engine's events update `position_ms`, `duration_ms`, and
/// `status`. Title/artist/album hold a placeholder when unknown (Requirement
/// 6.7); `volume` is clamped to `0..=100` (Requirement 4.7).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback_State {
    pub track_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub status: PlaybackStatus,
    /// Volume level, 0..=100.
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

impl Default for Playback_State {
    fn default() -> Self {
        Playback_State {
            track_id: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            position_ms: 0,
            duration_ms: None,
            status: PlaybackStatus::Stopped,
            volume: 100,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }
}

/// The direction of a seek action (Requirement 4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekDirection {
    /// Seek forward, increasing the playback position.
    Forward,
    /// Seek backward, decreasing the playback position.
    Backward,
}

/// The result of applying a seek to the current playback position
/// (Requirement 4.6).
///
/// A seek is clamped to the inclusive range `0..=duration`. A forward seek that
/// reaches the track duration is not reported as a position; instead it is
/// surfaced as [`SeekOutcome::TrackFinished`] so the caller handles it exactly
/// like the current track finishing (Requirement 4.8 / 4.6). Every other seek
/// yields the clamped [`SeekOutcome::Position`] in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekOutcome {
    /// The new, clamped playback position in milliseconds.
    Position(u64),
    /// A forward seek reached the track duration; treat as the track finishing.
    TrackFinished,
}

/// Apply a seek of `step_seconds` to `position_ms` in the given `direction`,
/// clamping the result to the inclusive range `0..=duration` (Requirement 4.6).
///
/// Behavior:
/// - A backward seek subtracts the step and saturates at `0`.
/// - A forward seek adds the step. When `duration_ms` is `Some(d)` and the new
///   position reaches or passes `d`, the result is [`SeekOutcome::TrackFinished`]
///   so the caller handles it as the current track finishing. Otherwise the new
///   position is returned as [`SeekOutcome::Position`].
/// - When `duration_ms` is `None` (unknown duration), there is no finish
///   boundary: a forward seek simply advances the position (saturating at the
///   `u64` max) and never reports `TrackFinished`.
///
/// `step_seconds` is the configured seek step (an integer 1..=300, default 10);
/// the function accepts any value and converts it to milliseconds internally.
pub fn seek(
    position_ms: u64,
    duration_ms: Option<u64>,
    step_seconds: u32,
    direction: SeekDirection,
) -> SeekOutcome {
    let step_ms = (step_seconds as u64).saturating_mul(1000);

    match direction {
        SeekDirection::Backward => {
            SeekOutcome::Position(position_ms.saturating_sub(step_ms))
        }
        SeekDirection::Forward => {
            let target = position_ms.saturating_add(step_ms);
            match duration_ms {
                Some(duration) if target >= duration => SeekOutcome::TrackFinished,
                Some(_) | None => SeekOutcome::Position(target),
            }
        }
    }
}

/// Minimum volume level (Requirement 4.7).
pub const MIN_VOLUME: i64 = 0;

/// Maximum volume level (Requirement 4.7).
pub const MAX_VOLUME: i64 = 100;

/// Clamp a requested integer volume level to the inclusive range `0..=100`
/// (Requirement 4.7).
///
/// Values below 0 clamp to 0 and values above 100 clamp to 100. The requested
/// level is taken as an `i64` so negative requests and values beyond the `u8`
/// range are representable and clamped rather than wrapping.
pub fn clamp_volume(requested: i64) -> u8 {
    requested.clamp(MIN_VOLUME, MAX_VOLUME) as u8
}

/// Format a non-negative duration in whole seconds as `m:ss` for values under
/// one hour and `h:mm:ss` for values of one hour or more, with zero-padded
/// minutes (only in the `h:mm:ss` form) and zero-padded seconds (Requirements
/// 3.4, 6.5).
///
/// This is the agreed shared formatter signature used by the Now_Playing_Bar;
/// it is the exact inverse of [`parse_duration`]: for any non-negative whole
/// second value `n`, `parse_duration(&format_duration(n)) == Some(n)`.
pub fn format_duration(total_seconds: u64) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours >= 1 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

/// Parse a string produced by [`format_duration`] back into whole seconds.
///
/// Accepts the `m:ss` and `h:mm:ss` shapes. Returns `None` when the string is
/// not a well-formed duration (wrong number of components, non-numeric parts,
/// or out-of-range seconds/minutes).
pub fn parse_duration(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [m, sec] => {
            let minutes: u64 = m.parse().ok()?;
            let seconds: u64 = parse_two_digit_field(sec)?;
            if seconds >= 60 {
                return None;
            }
            Some(minutes * 60 + seconds)
        }
        [h, min, sec] => {
            let hours: u64 = h.parse().ok()?;
            let minutes: u64 = parse_two_digit_field(min)?;
            let seconds: u64 = parse_two_digit_field(sec)?;
            if minutes >= 60 || seconds >= 60 {
                return None;
            }
            Some(hours * 3600 + minutes * 60 + seconds)
        }
        _ => None,
    }
}

/// Parse a zero-padded two-digit field, rejecting anything that is not exactly
/// two ASCII digits.
fn parse_two_digit_field(s: &str) -> Option<u64> {
    if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// The resolved field values of the Now_Playing_Bar after applying the
/// "nothing playing" state and missing-field placeholders, before any
/// width-driven truncation.
///
/// `title`, `artist`, and `album` are the (possibly placeholder) texts that may
/// be truncated under a narrow terminal. `elapsed`, `duration`, and `status`
/// are the protected fields that are always kept fully visible (Requirement
/// 6.4). `volume`, `shuffle`, and `repeat` are always shown, including in the
/// "nothing playing" state (Requirement 6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlayingFields {
    pub nothing_playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub elapsed: String,
    pub duration: String,
    pub status: String,
    pub volume: String,
    pub shuffle: String,
    pub repeat: String,
}

/// Resolve the display text for every Now_Playing_Bar field from a
/// `Playback_State`, applying the "nothing playing" message (Requirement 6.3)
/// and the unknown-value placeholders (Requirement 6.7).
///
/// A track is considered loaded when `track_id` is `Some`. While nothing is
/// loaded, the track/time fields are left empty and `nothing_playing` is set so
/// the renderer can substitute the single "nothing playing" message in their
/// place, while volume, shuffle, and repeat continue to be reported.
pub fn resolve_fields(state: &Playback_State) -> NowPlayingFields {
    let loaded = state.track_id.is_some();

    let (nothing_playing, title, artist, album, elapsed, duration) = if loaded {
        (
            false,
            placeholder_if_blank(&state.title),
            placeholder_if_blank(&state.artist),
            placeholder_if_blank(&state.album),
            format_duration(state.position_ms / 1000),
            match state.duration_ms {
                Some(ms) => format_duration(ms / 1000),
                None => UNKNOWN_PLACEHOLDER.to_string(),
            },
        )
    } else {
        (
            true,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };

    NowPlayingFields {
        nothing_playing,
        title,
        artist,
        album,
        elapsed,
        duration,
        status: status_label(state.status).to_string(),
        volume: format!("{}%", state.volume),
        shuffle: format!("shuffle {}", on_off(state.shuffle)),
        repeat: format!("repeat {}", repeat_label(state.repeat)),
    }
}

/// Substitute the unknown placeholder for a blank (missing) text field
/// (Requirement 6.7).
fn placeholder_if_blank(value: &str) -> String {
    if value.is_empty() {
        UNKNOWN_PLACEHOLDER.to_string()
    } else {
        value.to_string()
    }
}

fn status_label(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
        PlaybackStatus::Stopped => "stopped",
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn repeat_label(repeat: RepeatMode) -> &'static str {
    match repeat {
        RepeatMode::Off => "off",
        RepeatMode::RepeatAll => "all",
        RepeatMode::RepeatOne => "one",
    }
}

/// Truncate `text` to at most `max_width` display columns, ending any truncated
/// result with a single ellipsis character (Requirement 6.4).
///
/// Width is measured in `char`s (the Now_Playing_Bar texts are plain track
/// metadata). When the text already fits it is returned unchanged. When
/// `max_width` is 0 the result is empty; when it is 1 the single column holds
/// the ellipsis. Otherwise the result keeps `max_width - 1` leading characters
/// followed by exactly one ellipsis.
pub fn truncate_with_ellipsis(text: &str, max_width: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(max_width - 1).collect();
    format!("{}{}", kept, ELLIPSIS)
}

/// Render the full Now_Playing_Bar line for a given terminal `width`,
/// truncating the album first, then the artist, then the title so the whole
/// line fits, while keeping elapsed time, total duration, and play/pause status
/// fully visible (Requirement 6.4).
///
/// Layout: the protected tail (elapsed/duration/status plus volume/shuffle/
/// repeat) is laid out first and always kept; the remaining columns are given
/// to the track fields, which absorb truncation in album → artist → title
/// order. When the terminal is too narrow even for the protected tail, the tail
/// is returned as-is (it may exceed `width`, but it is never itself truncated).
pub fn render_line(state: &Playback_State, width: usize) -> String {
    let fields = resolve_fields(state);
    render_fields_line(&fields, width)
}

/// Lay out already-resolved [`NowPlayingFields`] into a single line of at most
/// `width` columns (falling back to the un-truncatable protected tail when the
/// terminal cannot even hold that). Separated from [`render_line`] so the
/// truncation behaviour can be exercised directly against field values.
pub fn render_fields_line(fields: &NowPlayingFields, width: usize) -> String {
    const SEP: &str = " | ";

    // The protected segment is always fully visible (Requirement 6.4): the
    // elapsed/duration/status trio plus the always-on volume/shuffle/repeat
    // indicators (Requirement 6.1, 6.3).
    let protected = if fields.nothing_playing {
        format!(
            "{} | {} | {} | {}",
            NOTHING_PLAYING, fields.volume, fields.shuffle, fields.repeat
        )
    } else {
        format!(
            "{}/{} | {} | {} | {} | {}",
            fields.elapsed,
            fields.duration,
            fields.status,
            fields.volume,
            fields.shuffle,
            fields.repeat
        )
    };

    // Nothing-playing: the track fields are replaced by the single message,
    // which lives inside the protected segment, so there is nothing to
    // truncate.
    if fields.nothing_playing {
        return protected;
    }

    let protected_width = protected.chars().count();

    // Columns available for the truncatable track block (title/artist/album),
    // accounting for the separator that joins it to the protected segment.
    let sep_width = SEP.chars().count();
    let budget = width
        .saturating_sub(protected_width)
        .saturating_sub(sep_width);

    let track_block = layout_track_block(&fields.title, &fields.artist, &fields.album, budget);

    if track_block.is_empty() {
        // No room for any track text; show only the protected segment.
        protected
    } else {
        format!("{}{}{}", track_block, SEP, protected)
    }
}

/// Lay out the `title — artist — album` block within `budget` columns,
/// truncating album first, then artist, then title, each truncated field ending
/// in a single ellipsis (Requirement 6.4). Returns an empty string when there
/// is no budget for even a minimal title.
fn layout_track_block(title: &str, artist: &str, album: &str, budget: usize) -> String {
    const SEP: &str = " — ";
    let sep_width = SEP.chars().count();

    let title_w = title.chars().count();
    let artist_w = artist.chars().count();
    let album_w = album.chars().count();

    // Full "title — artist — album" width.
    let full = title_w + sep_width + artist_w + sep_width + album_w;
    if full <= budget {
        return join3(title, artist, album, SEP);
    }

    // Shrink the album first. Keep title, both separators, and artist intact.
    let fixed_without_album = title_w + sep_width + artist_w + sep_width;
    if fixed_without_album < budget {
        let album_budget = budget - fixed_without_album;
        let album_t = truncate_with_ellipsis(album, album_budget);
        return join3(title, artist, &album_t, SEP);
    }

    // Album cannot be shown at all; drop it and shrink the artist next.
    let fixed_without_artist = title_w + sep_width;
    if fixed_without_artist < budget {
        let artist_budget = budget - fixed_without_artist;
        let artist_t = truncate_with_ellipsis(artist, artist_budget);
        return join2(title, &artist_t, SEP);
    }

    // Only room for (part of) the title.
    if budget == 0 {
        return String::new();
    }
    truncate_with_ellipsis(title, budget)
}

fn join2(a: &str, b: &str, sep: &str) -> String {
    format!("{}{}{}", a, sep, b)
}

fn join3(a: &str, b: &str, c: &str, sep: &str) -> String {
    format!("{}{}{}{}{}", a, sep, b, sep, c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlaybackStatus, RepeatMode};
    use proptest::prelude::*;

    proptest! {
        // Feature: apple-music-tui, Property 5: Duration formatting
        // Validates: Requirements 3.4, 6.5
        //
        // For any non-negative whole-second duration, format_duration uses the
        // `m:ss` shape with zero-padded seconds below one hour and the
        // `h:mm:ss` shape with zero-padded minutes and seconds at one hour or
        // more, and parse_duration inverts the formatting back to the original
        // second value.
        #[test]
        fn property_duration_formatting(total_seconds in 0u64..=1_000_000u64) {
            let formatted = format_duration(total_seconds);
            let parts: Vec<&str> = formatted.split(':').collect();

            if total_seconds < 3600 {
                // `m:ss`: minutes unpadded, seconds zero-padded to two digits.
                prop_assert_eq!(parts.len(), 2, "expected m:ss for {}", formatted);
                let minutes = total_seconds / 60;
                let seconds = total_seconds % 60;
                prop_assert_eq!(parts[0], minutes.to_string());
                prop_assert_eq!(parts[1].len(), 2, "seconds must be zero-padded");
                prop_assert_eq!(parts[1], format!("{:02}", seconds));
            } else {
                // `h:mm:ss`: hours unpadded, minutes and seconds zero-padded.
                prop_assert_eq!(parts.len(), 3, "expected h:mm:ss for {}", formatted);
                let hours = total_seconds / 3600;
                let minutes = (total_seconds % 3600) / 60;
                let seconds = total_seconds % 60;
                prop_assert_eq!(parts[0], hours.to_string());
                prop_assert_eq!(parts[1].len(), 2, "minutes must be zero-padded");
                prop_assert_eq!(parts[1], format!("{:02}", minutes));
                prop_assert_eq!(parts[2].len(), 2, "seconds must be zero-padded");
                prop_assert_eq!(parts[2], format!("{:02}", seconds));
            }

            // Parsing the formatted string recovers the original second value.
            prop_assert_eq!(parse_duration(&formatted), Some(total_seconds));
        }
    }

    fn loaded_state() -> Playback_State {
        Playback_State {
            track_id: Some("t1".to_string()),
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            position_ms: 65_000,
            duration_ms: Some(185_000),
            status: PlaybackStatus::Playing,
            volume: 80,
            shuffle: true,
            repeat: RepeatMode::RepeatAll,
        }
    }

    #[test]
    fn format_duration_under_one_hour_uses_m_ss() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(5), "0:05");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3599), "59:59");
    }

    #[test]
    fn format_duration_one_hour_or_more_uses_h_mm_ss() {
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3661), "1:01:01");
        assert_eq!(format_duration(45000), "12:30:00");
    }

    #[test]
    fn parse_duration_is_inverse_of_format() {
        for secs in [0u64, 5, 65, 3599, 3600, 3661, 45_000] {
            let formatted = format_duration(secs);
            assert_eq!(parse_duration(&formatted), Some(secs), "for {formatted}");
        }
    }

    #[test]
    fn parse_duration_rejects_malformed() {
        assert_eq!(parse_duration("1:5"), None); // seconds not zero-padded
        assert_eq!(parse_duration("1:60"), None); // seconds out of range
        assert_eq!(parse_duration("1:00:60"), None); // seconds out of range
        assert_eq!(parse_duration("1:60:00"), None); // minutes out of range
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
    }

    #[test]
    fn resolve_fields_nothing_playing_keeps_volume_shuffle_repeat() {
        let state = Playback_State::default();
        let fields = resolve_fields(&state);
        assert!(fields.nothing_playing);
        assert!(fields.title.is_empty());
        assert!(fields.elapsed.is_empty());
        assert_eq!(fields.volume, "100%");
        assert_eq!(fields.shuffle, "shuffle off");
        assert_eq!(fields.repeat, "repeat off");
    }

    #[test]
    fn resolve_fields_substitutes_unknown_placeholders() {
        let mut state = loaded_state();
        state.artist = String::new();
        state.album = String::new();
        state.duration_ms = None;
        let fields = resolve_fields(&state);
        assert_eq!(fields.artist, UNKNOWN_PLACEHOLDER);
        assert_eq!(fields.album, UNKNOWN_PLACEHOLDER);
        assert_eq!(fields.duration, UNKNOWN_PLACEHOLDER);
        // Title (present) is untouched, and elapsed still renders.
        assert_eq!(fields.title, "Title");
        assert_eq!(fields.elapsed, "1:05");
    }

    #[test]
    fn truncate_with_ellipsis_shorter_than_width_is_unchanged() {
        assert_eq!(truncate_with_ellipsis("abc", 5), "abc");
        assert_eq!(truncate_with_ellipsis("abc", 3), "abc");
    }

    #[test]
    fn truncate_with_ellipsis_applies_single_ellipsis() {
        let out = truncate_with_ellipsis("abcdef", 4);
        assert_eq!(out, "abc\u{2026}");
        assert_eq!(out.chars().count(), 4);
        assert_eq!(out.chars().filter(|&c| c == ELLIPSIS).count(), 1);
    }

    #[test]
    fn truncate_with_ellipsis_edge_widths() {
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
        assert_eq!(truncate_with_ellipsis("abc", 1), "\u{2026}");
    }

    #[test]
    fn render_line_wide_enough_shows_all_fields() {
        let state = loaded_state();
        let line = render_line(&state, 120);
        assert!(line.contains("Title"));
        assert!(line.contains("Artist"));
        assert!(line.contains("Album"));
        assert!(line.contains("1:05/3:05"));
        assert!(line.contains("playing"));
        assert!(line.contains("80%"));
    }

    #[test]
    fn render_line_nothing_playing_shows_message_and_indicators() {
        let state = Playback_State::default();
        let line = render_line(&state, 80);
        assert!(line.contains(NOTHING_PLAYING));
        assert!(line.contains("100%"));
        assert!(line.contains("shuffle off"));
        assert!(line.contains("repeat off"));
    }

    #[test]
    fn render_line_truncates_album_before_artist_before_title() {
        let mut state = loaded_state();
        state.title = "TheTitle".to_string();
        state.artist = "TheArtist".to_string();
        state.album = "TheAlbumNameIsQuiteLong".to_string();

        // Pick a width that forces truncation of the album only.
        let protected = "1:05/3:05 | playing | 80% | shuffle on | repeat all";
        let protected_w = protected.chars().count();
        // Enough budget for title + artist + separators + a few album chars.
        let width = protected_w + 3 /* " | " */ + 8 /* title */ + 3 + 9 /* artist */ + 3 + 6;
        let line = render_line(&state, width);

        assert!(line.contains("TheTitle"));
        assert!(line.contains("TheArtist"));
        // Album is the only field that should carry the ellipsis.
        assert!(line.contains(ELLIPSIS));
        assert!(line.starts_with("TheTitle"));
        // Protected tail intact.
        assert!(line.contains("1:05/3:05"));
        assert!(line.contains("playing"));
    }

    #[test]
    fn render_line_keeps_protected_fields_when_very_narrow() {
        let state = loaded_state();
        let line = render_line(&state, 10);
        // Elapsed/duration/status are never dropped.
        assert!(line.contains("1:05/3:05"));
        assert!(line.contains("playing"));
    }

    #[test]
    fn seek_backward_subtracts_and_clamps_to_zero() {
        // Normal backward seek.
        assert_eq!(
            seek(30_000, Some(180_000), 10, SeekDirection::Backward),
            SeekOutcome::Position(20_000)
        );
        // Clamps at 0 instead of going negative.
        assert_eq!(
            seek(5_000, Some(180_000), 10, SeekDirection::Backward),
            SeekOutcome::Position(0)
        );
        assert_eq!(
            seek(0, Some(180_000), 10, SeekDirection::Backward),
            SeekOutcome::Position(0)
        );
    }

    #[test]
    fn seek_forward_advances_within_duration() {
        assert_eq!(
            seek(30_000, Some(180_000), 10, SeekDirection::Forward),
            SeekOutcome::Position(40_000)
        );
    }

    #[test]
    fn seek_forward_reaching_duration_finishes_track() {
        // Exactly reaching the duration finishes the track.
        assert_eq!(
            seek(170_000, Some(180_000), 10, SeekDirection::Forward),
            SeekOutcome::TrackFinished
        );
        // Overshooting the duration also finishes the track.
        assert_eq!(
            seek(175_000, Some(180_000), 10, SeekDirection::Forward),
            SeekOutcome::TrackFinished
        );
    }

    #[test]
    fn seek_forward_without_known_duration_just_advances() {
        assert_eq!(
            seek(30_000, None, 10, SeekDirection::Forward),
            SeekOutcome::Position(40_000)
        );
    }

    #[test]
    fn seek_saturates_rather_than_overflowing() {
        // Huge step with unknown duration saturates at u64::MAX, no finish.
        assert_eq!(
            seek(u64::MAX - 1, None, 300, SeekDirection::Forward),
            SeekOutcome::Position(u64::MAX)
        );
    }

    #[test]
    fn seek_result_is_always_within_duration_bounds() {
        let duration = 120_000u64;
        for pos in [0u64, 1_000, 60_000, 119_000, 120_000] {
            for step in [1u32, 10, 300] {
                for dir in [SeekDirection::Forward, SeekDirection::Backward] {
                    if let SeekOutcome::Position(p) = seek(pos, Some(duration), step, dir) {
                        assert!(p <= duration, "pos {p} exceeds duration {duration}");
                    }
                }
            }
        }
    }

    #[test]
    fn clamp_volume_clamps_below_zero_to_zero() {
        assert_eq!(clamp_volume(-1), 0);
        assert_eq!(clamp_volume(-1000), 0);
    }

    #[test]
    fn clamp_volume_clamps_above_hundred_to_hundred() {
        assert_eq!(clamp_volume(101), 100);
        assert_eq!(clamp_volume(100_000), 100);
    }

    #[test]
    fn clamp_volume_passes_through_in_range_values() {
        assert_eq!(clamp_volume(0), 0);
        assert_eq!(clamp_volume(50), 50);
        assert_eq!(clamp_volume(100), 100);
    }

    proptest! {
        // Feature: apple-music-tui, Property 7: Volume clamping
        // Validates: Requirements 4.7
        //
        // For any requested integer volume level, the resulting volume equals
        // that level clamped to the inclusive range 0..=100: requests below 0
        // yield 0, requests above 100 yield 100, and in-range requests pass
        // through unchanged.
        #[test]
        fn property_volume_clamping(requested in any::<i64>()) {
            let result = clamp_volume(requested);
            let expected = requested.clamp(MIN_VOLUME, MAX_VOLUME);

            // The clamped result matches the mathematical clamp of the request.
            prop_assert_eq!(i64::from(result), expected);

            // The result is always within the inclusive 0..=100 range.
            prop_assert!(i64::from(result) >= MIN_VOLUME);
            prop_assert!(i64::from(result) <= MAX_VOLUME);

            // Below-range requests clamp to 0; above-range requests clamp to
            // 100; in-range requests are returned unchanged.
            if requested < MIN_VOLUME {
                prop_assert_eq!(result, 0);
            } else if requested > MAX_VOLUME {
                prop_assert_eq!(result, 100);
            } else {
                prop_assert_eq!(i64::from(result), requested);
            }
        }
    }

    #[test]
    fn render_line_fits_within_width_when_room_for_track_block() {
        let mut state = loaded_state();
        state.title = "A Reasonably Long Title".to_string();
        state.artist = "Some Artist".to_string();
        state.album = "Some Album".to_string();
        // Width comfortably larger than the protected tail so the track block
        // is included and must fit.
        let width = 90;
        let line = render_line(&state, width);
        assert!(line.chars().count() <= width);
    }

    mod property_tests {
        use super::super::*;
        use proptest::prelude::{prop_assert, prop_assert_eq, proptest, ProptestConfig};

        /// Count how many of `text`'s characters are the ellipsis character.
        fn ellipsis_count(text: &str) -> usize {
            text.chars().filter(|&c| c == ELLIPSIS).count()
        }

        /// Build a loaded [`Playback_State`] from generated field values. A
        /// loaded track (`track_id` is `Some`) means the Now_Playing_Bar renders
        /// its track/time fields and so can be forced to truncate.
        fn make_state(
            title: String,
            artist: String,
            album: String,
            position_ms: u64,
            duration_ms: u64,
        ) -> Playback_State {
            Playback_State {
                track_id: Some("track".to_string()),
                title,
                artist,
                album,
                position_ms,
                duration_ms: Some(duration_ms),
                status: PlaybackStatus::Playing,
                volume: 75,
                shuffle: false,
                repeat: RepeatMode::Off,
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(400))]

            // Feature: apple-music-tui, Property 18: Now_Playing_Bar truncation under narrow width
            // Validates: Requirements 6.4
            //
            // For any terminal width and set of Now_Playing_Bar field values,
            // the rendered line fits within the width (whenever the always-
            // visible protected segment itself fits), the elapsed time, total
            // duration, and play/pause status remain fully visible, and
            // truncation is applied to the album first, then the artist, then
            // the title, each truncated field ending with a single ellipsis.
            #[test]
            fn property_now_playing_bar_truncation(
                // Non-ellipsis, non-separator text so that any ellipsis found in
                // the output comes from truncation, and so source text never
                // collides with the layout separators.
                title in "[A-Za-z0-9 ]{0,40}",
                artist in "[A-Za-z0-9 ]{0,40}",
                album in "[A-Za-z0-9 ]{0,40}",
                position_s in 0u64..7200,
                extra_s in 0u64..3600,
                width in 0usize..200,
            ) {
                let position_ms = position_s * 1000;
                // Ensure the duration is at least the elapsed position so the
                // formatted elapsed/duration pair is realistic.
                let duration_ms = (position_s + extra_s) * 1000;
                let state = make_state(
                    title.clone(),
                    artist.clone(),
                    album.clone(),
                    position_ms,
                    duration_ms,
                );

                let fields = resolve_fields(&state);
                let line = render_fields_line(&fields, width);

                // The protected segment that must always stay fully visible:
                // elapsed/duration plus the play/pause status (Requirement 6.4).
                let elapsed = &fields.elapsed;
                let duration = &fields.duration;
                let status = &fields.status;
                let elapsed_duration = format!("{}/{}", elapsed, duration);

                // 1. Elapsed time, total duration, and play/pause status remain
                //    fully visible regardless of how narrow the terminal is.
                prop_assert!(
                    line.contains(&elapsed_duration),
                    "elapsed/duration `{}` missing from line `{}` at width {}",
                    elapsed_duration, line, width
                );
                prop_assert!(
                    line.contains(status.as_str()),
                    "status `{}` missing from line `{}` at width {}",
                    status, line, width
                );

                // The width needed just for the protected segment.
                let protected_width = line
                    .chars()
                    .count()
                    .min(format!(
                        "{}/{} | {} | {} | {} | {}",
                        fields.elapsed, fields.duration, fields.status,
                        fields.volume, fields.shuffle, fields.repeat
                    ).chars().count());

                // 2. The rendered line fits within the width whenever the width
                //    can hold the always-visible protected segment. (When the
                //    terminal is too narrow even for that segment, the segment
                //    is still shown rather than dropped, so the line may exceed
                //    the width by design.)
                if width >= protected_width {
                    prop_assert!(
                        line.chars().count() <= width,
                        "line `{}` ({} cols) exceeds width {}",
                        line, line.chars().count(), width
                    );
                }

                // Split the rendered track block (everything before the " | "
                // that introduces the protected segment) back into its
                // title/artist/album parts so we can reason about which fields
                // were truncated. The track block is joined with " — ".
                let protected_sep = " | ";
                // The protected segment always starts with the elapsed/duration
                // pair, so locate it to recover the track block prefix.
                if let Some(idx) = line.find(&format!("{}{}", protected_sep, elapsed_duration)) {
                    let track_block = &line[..idx];
                    if !track_block.is_empty() {
                        let parts: Vec<&str> = track_block.split(" — ").collect();
                        // The rendered track block is some prefix of
                        // title/artist/album in that order (album may be
                        // dropped entirely when there is no room, then artist).
                        prop_assert!(
                            (1..=3).contains(&parts.len()),
                            "unexpected track block `{}` -> {:?}",
                            track_block, parts
                        );

                        // 3 & 4. Truncation order and single-ellipsis rule.
                        //
                        // Walk the present parts in render order (title, then
                        // artist, then album). A field is "fully shown" when it
                        // equals its source text; otherwise it is truncated and
                        // must end with exactly one ellipsis. Once any earlier
                        // field is truncated, no later field may be present —
                        // i.e. album is sacrificed before artist, and artist
                        // before title.
                        //
                        // The sources are the *resolved* field values, which
                        // already apply the unknown-value placeholder for blank
                        // inputs (Requirement 6.7); truncation operates on those
                        // resolved texts, not the raw generated strings.
                        let sources = [
                            fields.title.as_str(),
                            fields.artist.as_str(),
                            fields.album.as_str(),
                        ];
                        let mut earlier_truncated = false;
                        for (i, part) in parts.iter().enumerate() {
                            let source = sources[i];
                            let truncated = *part != source;

                            if truncated {
                                // Exactly one ellipsis, placed at the very end.
                                prop_assert_eq!(
                                    ellipsis_count(part), 1,
                                    "field `{}` must carry exactly one ellipsis (line `{}`)",
                                    part, line
                                );
                                prop_assert!(
                                    part.ends_with(ELLIPSIS),
                                    "truncated field `{}` must end with an ellipsis (line `{}`)",
                                    part, line
                                );
                                // Truncated text is a prefix of the source plus
                                // the ellipsis.
                                let without_ellipsis = &part[..part.len() - ELLIPSIS.len_utf8()];
                                prop_assert!(
                                    source.starts_with(without_ellipsis),
                                    "truncated field `{}` is not a prefix of source `{}`",
                                    part, source
                                );
                            } else {
                                // A fully shown field never carries an ellipsis
                                // (the generator excludes it from source text).
                                prop_assert_eq!(
                                    ellipsis_count(part), 0,
                                    "fully shown field `{}` should have no ellipsis",
                                    part
                                );
                                // Truncation must proceed album-first: a later
                                // field cannot be fully present once an earlier
                                // one was truncated.
                                prop_assert!(
                                    !earlier_truncated,
                                    "field `{}` present after an earlier field was truncated (parts {:?})",
                                    part, parts
                                );
                            }

                            earlier_truncated = earlier_truncated || truncated;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod seek_property_tests {
    //! Property-based test for the pure seek-clamping logic (task 7.1). Runs the
    //! single design correctness property at ≥100 iterations.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // Feature: apple-music-tui, Property 6: Seek clamping
        // Validates: Requirements 4.6
        //
        // For any current position, seek step, and track duration, a seek
        // produces a position within the inclusive range 0..=duration, and a
        // forward seek that reaches the duration is handled as the current
        // track finishing (TrackFinished) rather than a position.
        //
        // The generator constrains `position_ms` to a valid position within the
        // track (`0..=duration_ms`), matching the real input space: the
        // Playback_Engine only ever reports positions inside the current
        // track's duration.
        #[test]
        fn property_seek_clamping(
            duration_ms in 1u64..=600_000,
            step_seconds in 1u32..=300,
            forward in any::<bool>(),
            position_frac in 0.0f64..=1.0,
        ) {
            let position_ms = (position_frac * duration_ms as f64) as u64;

            let direction = if forward {
                SeekDirection::Forward
            } else {
                SeekDirection::Backward
            };

            let step_ms = (step_seconds as u64) * 1000;
            let outcome = seek(position_ms, Some(duration_ms), step_seconds, direction);

            match outcome {
                SeekOutcome::Position(p) => {
                    // The clamped position always lies within 0..=duration.
                    prop_assert!(
                        p <= duration_ms,
                        "position {} exceeds duration {}",
                        p,
                        duration_ms
                    );

                    match direction {
                        SeekDirection::Backward => {
                            // Backward seek subtracts the step, saturating at 0.
                            prop_assert_eq!(p, position_ms.saturating_sub(step_ms));
                        }
                        SeekDirection::Forward => {
                            // A forward seek that yields a position must not
                            // have reached the duration (otherwise it would be
                            // TrackFinished), and it advances by the step.
                            let target = position_ms.saturating_add(step_ms);
                            prop_assert!(target < duration_ms);
                            prop_assert_eq!(p, target);
                        }
                    }
                }
                SeekOutcome::TrackFinished => {
                    // Only a forward seek that reaches the duration finishes.
                    prop_assert_eq!(direction, SeekDirection::Forward);
                    let target = position_ms.saturating_add(step_ms);
                    prop_assert!(target >= duration_ms);
                }
            }
        }
    }
}
