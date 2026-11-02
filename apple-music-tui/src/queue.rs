//! Queue state and the queue state machine.
//!
//! Defines `Queue` and `QueueTrack` plus the structural operations that edit
//! the ordered track list while preserving the Queue invariant (Requirement
//! 5.9). This module covers add/play-next/add-to-queue, remove, and move along
//! with the 10,000-track limit (Requirements 5.2–5.5, 5.11, 5.12), plus shuffle
//! on/off restoration and repeat-mode cycling (Requirements 5.6–5.8) and the
//! transport transitions over repeat modes (Requirements 4.3, 4.4, 4.5, 4.8).

use crate::domain::{PlaybackStatus, RepeatMode};

/// Maximum number of tracks a Queue may hold (Requirement 5.12).
pub const MAX_QUEUE_LEN: usize = 10_000;

/// Position threshold (in milliseconds) at or below which the previous-track
/// action steps back to the preceding track instead of restarting the current
/// one (Requirements 4.4, 4.5). A position of exactly 3 seconds still steps
/// back.
pub const PREVIOUS_RESTART_THRESHOLD_MS: u64 = 3_000;

/// A single entry in the Queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueTrack {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<u64>,
}

/// The ordered list of tracks plus the current-track pointer.
///
/// Invariant (Requirement 5.9): `current` is `None` iff `tracks` is empty,
/// otherwise `current < tracks.len()`. `pre_shuffle_order` snapshots the track
/// id order so un-shuffle can restore it (Requirement 5.7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    /// Ordered tracks; length is bounded by [`MAX_QUEUE_LEN`].
    pub tracks: Vec<QueueTrack>,
    /// Index into `tracks`, or `None` when the queue is empty.
    pub current: Option<usize>,
    /// Snapshot of track-id order captured on shuffle, used to restore order.
    pub pre_shuffle_order: Option<Vec<String>>,
}

/// Outcome of removing the current track (Requirement 5.11).
///
/// The caller uses this to drive the Playback_Engine: start the new current
/// track when playback was active, or stop playback when no track remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveCurrentOutcome {
    /// The pointer advanced to the track that followed the removed one.
    Advanced { new_current: usize },
    /// The removed track was the last one; the Queue is now empty.
    Emptied,
}

/// Result of a transport transition (next / previous / track-finished).
///
/// These are pure computations over the Queue, the current-track index, the
/// repeat mode, and (for previous) the current position. They report which
/// track should play and at what play/pause status, leaving the actual
/// `load`/`play`/`pause` to the caller driving the Playback_Engine. The
/// `index` always refers to the track the Playback_Engine should load, started
/// from position 0; `status` tells the caller whether to play or pause it
/// (Requirements 4.3, 4.4, 4.5, 4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// Index of the track to load from position 0.
    pub index: usize,
    /// Whether the loaded track should start playing or stay paused.
    pub status: PlaybackStatus,
}

/// Error returned when an add operation would exceed the Queue length limit
/// (Requirement 5.12). The Queue is left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFullError;

impl std::fmt::Display for QueueFullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "queue limit of {} tracks was reached",
            MAX_QUEUE_LEN
        )
    }
}

impl std::error::Error for QueueFullError {}

impl Queue {
    /// Create an empty Queue.
    pub fn new() -> Self {
        Queue::default()
    }

    /// Number of tracks currently in the Queue.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether the Queue holds no tracks.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Insert `track` directly after the current track (Requirement 5.2).
    ///
    /// When the Queue has no current track, `track` becomes the sole entry and
    /// is set as the current track without starting playback. Rejects the add
    /// with [`QueueFullError`] (leaving the Queue unchanged) when it would push
    /// the length past [`MAX_QUEUE_LEN`] (Requirement 5.12).
    pub fn play_next(&mut self, track: QueueTrack) -> Result<(), QueueFullError> {
        if self.tracks.len() >= MAX_QUEUE_LEN {
            return Err(QueueFullError);
        }
        match self.current {
            Some(current) => {
                // Insert immediately after the current track. The pointer index
                // is unaffected because the insertion is after it.
                self.tracks.insert(current + 1, track);
            }
            None => {
                self.tracks.push(track);
                self.current = Some(0);
            }
        }
        Ok(())
    }

    /// Append `track` after the last track in the Queue (Requirement 5.3).
    ///
    /// When the Queue has no current track, the appended track becomes the sole
    /// current track without starting playback. Rejects the add with
    /// [`QueueFullError`] when it would exceed [`MAX_QUEUE_LEN`] (Requirement
    /// 5.12).
    pub fn add_to_queue(&mut self, track: QueueTrack) -> Result<(), QueueFullError> {
        if self.tracks.len() >= MAX_QUEUE_LEN {
            return Err(QueueFullError);
        }
        self.tracks.push(track);
        if self.current.is_none() {
            self.current = Some(0);
        }
        Ok(())
    }

    /// Remove a non-current track at `index` (Requirement 5.4).
    ///
    /// Deletes exactly one track, preserves the relative order of the remaining
    /// tracks, and keeps the current-track pointer on the same track identity
    /// (the stored index shifts down by one when the removed track preceded the
    /// current one). Returns `false` without modifying the Queue when `index`
    /// is out of range or refers to the current track; use
    /// [`Queue::remove_current`] for the current track.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        match self.current {
            Some(current) => {
                if index == current {
                    // Current track removal is handled by remove_current, which
                    // reports the playback consequence.
                    return false;
                }
                self.tracks.remove(index);
                // Keep the pointer on the same track: shift down when the
                // removed track was before the current one.
                if index < current {
                    self.current = Some(current - 1);
                }
                true
            }
            None => false,
        }
    }

    /// Remove the current track and advance the pointer (Requirement 5.11).
    ///
    /// Moves the current-track pointer to the track that followed it, or to no
    /// track when the removed track was the last one. Returns the outcome so
    /// the caller can start the new current track (when playback was active) or
    /// stop playback (when the Queue becomes empty). Returns `None` when there
    /// is no current track to remove.
    pub fn remove_current(&mut self) -> Option<RemoveCurrentOutcome> {
        let current = self.current?;
        self.tracks.remove(current);
        if self.tracks.is_empty() {
            self.current = None;
            Some(RemoveCurrentOutcome::Emptied)
        } else {
            // The following track now occupies `current`; if the removed track
            // was last, clamp to the new final index.
            let new_current = current.min(self.tracks.len() - 1);
            self.current = Some(new_current);
            Some(RemoveCurrentOutcome::Advanced { new_current })
        }
    }

    /// Move the track at `from` to a 1-based `target` position (Requirement
    /// 5.5).
    ///
    /// `target` is clamped to the range `1..=len`. The relative order of the
    /// other tracks is preserved and the current-track pointer stays on the
    /// same track identity. Returns `false` without modifying the Queue when
    /// `from` is out of range.
    pub fn move_track(&mut self, from: usize, target: usize) -> bool {
        let len = self.tracks.len();
        if from >= len {
            return false;
        }
        // Clamp the 1-based target to [1, len] and convert to a 0-based index.
        let dest = target.clamp(1, len) - 1;
        if dest == from {
            return true;
        }

        // Track the moving track's identity through the pointer so we can keep
        // `current` on the same track after reordering.
        let moving_is_current = self.current == Some(from);
        let current_track_index = self.current;

        let track = self.tracks.remove(from);
        self.tracks.insert(dest, track);

        if let Some(cur) = current_track_index {
            if moving_is_current {
                self.current = Some(dest);
            } else {
                // The current track did not move; recompute its index after the
                // removal/insertion shifted surrounding elements.
                let mut new_cur = cur;
                // Removal at `from` shifts down indices greater than `from`.
                if cur > from {
                    new_cur -= 1;
                }
                // Insertion at `dest` shifts up indices at or after `dest`.
                if new_cur >= dest {
                    new_cur += 1;
                }
                self.current = Some(new_cur);
            }
        }
        true
    }

    /// Enable shuffle mode (Requirement 5.6).
    ///
    /// Keeps the current track and every track before it at their positions and
    /// reorders the tracks after the current track into a random permutation of
    /// those same tracks. Leaves the Queue unchanged when one or fewer tracks
    /// follow the current track. Snapshots the pre-shuffle track-id order into
    /// [`Queue::pre_shuffle_order`] so [`Queue::unshuffle`] can restore it
    /// (Requirement 5.7). The current-track pointer is preserved.
    ///
    /// The suffix is permuted with a lightweight internal PRNG seeded from the
    /// system clock; use [`Queue::shuffle_with`] to supply a deterministic
    /// permutation (for tests).
    pub fn shuffle(&mut self) {
        let mut rng = Pcg32::from_time();
        self.shuffle_with(|suffix| fisher_yates(suffix, &mut rng));
    }

    /// Enable shuffle mode using a caller-supplied permutation of the suffix
    /// (Requirement 5.6).
    ///
    /// `permute` receives the mutable slice of tracks that follow the current
    /// track and may reorder it in place. This is the deterministic core used
    /// by [`Queue::shuffle`]; it snapshots the pre-shuffle order, fixes the
    /// current track and its prefix, and keeps the pointer in place.
    ///
    /// When one or fewer tracks follow the current track the Queue is left
    /// unchanged apart from recording the snapshot, so a later
    /// [`Queue::unshuffle`] is still well-defined.
    pub fn shuffle_with<F>(&mut self, permute: F)
    where
        F: FnOnce(&mut [QueueTrack]),
    {
        // Snapshot the order before any reordering so un-shuffle can restore it.
        self.pre_shuffle_order = Some(self.tracks.iter().map(|t| t.track_id.clone()).collect());

        let Some(current) = self.current else {
            // Empty queue: nothing to permute, snapshot already captured.
            return;
        };

        let suffix_start = current + 1;
        // Permute only when 2 or more tracks follow the current track
        // (Requirement 5.6: a suffix of 1 or fewer tracks is left unchanged).
        if self.tracks.len().saturating_sub(suffix_start) < 2 {
            return;
        }
        permute(&mut self.tracks[suffix_start..]);
    }

    /// Disable shuffle mode and restore the pre-shuffle order (Requirement 5.7).
    ///
    /// Restores the track order captured by the last [`Queue::shuffle`],
    /// excluding tracks removed while shuffle mode was enabled, and appends
    /// tracks added while shuffle mode was enabled after the restored tracks in
    /// their current relative order. The current-track pointer is kept on the
    /// same track identity. The pre-shuffle snapshot is cleared afterwards.
    ///
    /// When no snapshot exists (shuffle was never enabled) the Queue is left
    /// unchanged.
    pub fn unshuffle(&mut self) {
        let Some(order) = self.pre_shuffle_order.take() else {
            return;
        };

        // Remember the current track's identity so the pointer can follow it.
        let current_id = self.current.map(|i| self.tracks[i].track_id.clone());

        // Index the present tracks by id so we can pull them in snapshot order.
        // Draining into a map keeps ownership and lets us detect which tracks
        // were added after the snapshot (those not named in `order`).
        let mut by_id: std::collections::HashMap<String, QueueTrack> = std::collections::HashMap::new();
        let mut added_in_order: Vec<QueueTrack> = Vec::new();
        let snapshot_ids: std::collections::HashSet<&String> = order.iter().collect();

        for track in self.tracks.drain(..) {
            if snapshot_ids.contains(&track.track_id) {
                by_id.insert(track.track_id.clone(), track);
            } else {
                // Added while shuffle was enabled; preserve add order.
                added_in_order.push(track);
            }
        }

        // Rebuild: snapshot order first (skipping removed tracks), then the
        // tracks added during shuffle in the order they were added.
        let mut rebuilt: Vec<QueueTrack> = Vec::with_capacity(by_id.len() + added_in_order.len());
        for id in &order {
            if let Some(track) = by_id.remove(id) {
                rebuilt.push(track);
            }
        }
        rebuilt.extend(added_in_order);

        self.tracks = rebuilt;

        // Restore the pointer to the same track identity, or clear it when the
        // queue is empty. The current track is never removed by un-shuffle, so
        // its id is always present when the queue is non-empty.
        self.current = match current_id {
            Some(id) => self.tracks.iter().position(|t| t.track_id == id),
            None => None,
        };
    }

    /// Compute the next-track transition under `repeat` (Requirement 4.3).
    ///
    /// Starts the track after the current one from position 0. When the current
    /// track is the last one, wraps to the first track under
    /// [`RepeatMode::RepeatAll`], and otherwise (off or repeat-one) stays on the
    /// last track in paused status at position 0. Returns `None` when the Queue
    /// is empty. This does not mutate the Queue; the caller applies the result.
    pub fn next_transition(&self, repeat: RepeatMode) -> Option<Transition> {
        let current = self.current?;
        let last = self.tracks.len() - 1;
        if current < last {
            // A following track exists: advance and play it.
            Some(Transition {
                index: current + 1,
                status: PlaybackStatus::Playing,
            })
        } else {
            // Current is the last track.
            match repeat {
                RepeatMode::RepeatAll => Some(Transition {
                    index: 0,
                    status: PlaybackStatus::Playing,
                }),
                RepeatMode::Off | RepeatMode::RepeatOne => Some(Transition {
                    index: last,
                    status: PlaybackStatus::Paused,
                }),
            }
        }
    }

    /// Compute the previous-track transition for a `position_ms` into the
    /// current track (Requirements 4.4, 4.5).
    ///
    /// Restarts the current track from position 0 when the position is beyond
    /// [`PREVIOUS_RESTART_THRESHOLD_MS`]. At or below the threshold, steps back
    /// to the preceding track, except when the current track is the first one,
    /// in which case it restarts the current track. The result always plays
    /// (previous is a user action). Repeat mode does not affect previous
    /// (Requirements 4.4, 4.5). Returns `None` when the Queue is empty.
    pub fn previous_transition(&self, position_ms: u64) -> Option<Transition> {
        let current = self.current?;
        let index = if position_ms > PREVIOUS_RESTART_THRESHOLD_MS || current == 0 {
            // Past the threshold, or already at the first track: restart current.
            current
        } else {
            // Within the threshold and not the first track: step back one.
            current - 1
        };
        Some(Transition {
            index,
            status: PlaybackStatus::Playing,
        })
    }

    /// Compute the track-finished transition under `repeat` (Requirement 4.8).
    ///
    /// Restarts the same track under [`RepeatMode::RepeatOne`]. Otherwise starts
    /// the next track when one exists; when the finished track is the last one,
    /// wraps to the first track under [`RepeatMode::RepeatAll`] and stops in
    /// paused status at the last track under [`RepeatMode::Off`]. Returns `None`
    /// when the Queue is empty. This does not mutate the Queue.
    pub fn track_finished_transition(&self, repeat: RepeatMode) -> Option<Transition> {
        let current = self.current?;
        let last = self.tracks.len() - 1;
        match repeat {
            // Repeat-one always replays the current track, regardless of its
            // position in the Queue.
            RepeatMode::RepeatOne => Some(Transition {
                index: current,
                status: PlaybackStatus::Playing,
            }),
            RepeatMode::RepeatAll => {
                if current < last {
                    Some(Transition {
                        index: current + 1,
                        status: PlaybackStatus::Playing,
                    })
                } else {
                    // Last track finished: wrap to the first.
                    Some(Transition {
                        index: 0,
                        status: PlaybackStatus::Playing,
                    })
                }
            }
            RepeatMode::Off => {
                if current < last {
                    Some(Transition {
                        index: current + 1,
                        status: PlaybackStatus::Playing,
                    })
                } else {
                    // Last track finished with repeat off: stop paused on it.
                    Some(Transition {
                        index: last,
                        status: PlaybackStatus::Paused,
                    })
                }
            }
        }
    }
}

/// Cycle a repeat mode one step: Off → RepeatAll → RepeatOne → Off
/// (Requirement 5.8).
pub fn cycle_repeat(mode: RepeatMode) -> RepeatMode {
    match mode {
        RepeatMode::Off => RepeatMode::RepeatAll,
        RepeatMode::RepeatAll => RepeatMode::RepeatOne,
        RepeatMode::RepeatOne => RepeatMode::Off,
    }
}

/// Small self-contained PRNG (PCG-XSH-RR 32-bit) used to shuffle the Queue
/// suffix without pulling in an external random-number dependency.
struct Pcg32 {
    state: u64,
    inc: u64,
}

impl Pcg32 {
    fn new(seed: u64, seq: u64) -> Self {
        let mut rng = Pcg32 {
            state: 0,
            inc: (seq << 1) | 1,
        };
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed);
        rng.next_u32();
        rng
    }

    /// Seed from the system clock. Shuffle does not require cryptographic
    /// randomness, only an unpredictable-enough permutation per invocation.
    fn from_time() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x853c_49e6_748f_ea9b);
        Pcg32::new(nanos, nanos ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform integer in `0..bound` (bound > 0) via rejection sampling.
    fn below(&mut self, bound: u32) -> u32 {
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let r = self.next_u32();
            if r >= threshold {
                return r % bound;
            }
        }
    }
}

/// In-place Fisher–Yates shuffle driven by a [`Pcg32`].
fn fisher_yates(slice: &mut [QueueTrack], rng: &mut Pcg32) {
    let len = slice.len();
    if len < 2 {
        return;
    }
    for i in (1..len).rev() {
        let j = rng.below((i + 1) as u32) as usize;
        slice.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> QueueTrack {
        QueueTrack {
            track_id: id.to_string(),
            title: format!("Title {id}"),
            artist: format!("Artist {id}"),
            duration_ms: Some(180_000),
        }
    }

    fn ids(q: &Queue) -> Vec<String> {
        q.tracks.iter().map(|t| t.track_id.clone()).collect()
    }

    fn current_id(q: &Queue) -> Option<String> {
        q.current.map(|i| q.tracks[i].track_id.clone())
    }

    // --- play_next (R5.2) ---

    #[test]
    fn play_next_on_empty_queue_becomes_sole_current() {
        let mut q = Queue::new();
        q.play_next(track("a")).unwrap();
        assert_eq!(ids(&q), ["a"]);
        assert_eq!(q.current, Some(0));
    }

    #[test]
    fn play_next_inserts_after_current() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.add_to_queue(track("b")).unwrap();
        q.add_to_queue(track("c")).unwrap();
        q.current = Some(1); // current is "b"
        q.play_next(track("x")).unwrap();
        assert_eq!(ids(&q), ["a", "b", "x", "c"]);
        // Pointer stays on "b".
        assert_eq!(current_id(&q).as_deref(), Some("b"));
    }

    // --- add_to_queue (R5.3) ---

    #[test]
    fn add_to_queue_on_empty_becomes_sole_current() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        assert_eq!(q.current, Some(0));
    }

    #[test]
    fn add_to_queue_appends_and_keeps_pointer() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.add_to_queue(track("b")).unwrap();
        q.current = Some(0);
        q.add_to_queue(track("c")).unwrap();
        assert_eq!(ids(&q), ["a", "b", "c"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    // --- remove non-current (R5.4) ---

    #[test]
    fn remove_before_current_shifts_pointer_down() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(2); // "c"
        assert!(q.remove(0)); // remove "a"
        assert_eq!(ids(&q), ["b", "c"]);
        assert_eq!(current_id(&q).as_deref(), Some("c"));
    }

    #[test]
    fn remove_after_current_keeps_pointer_index() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // "a"
        assert!(q.remove(2)); // remove "c"
        assert_eq!(ids(&q), ["a", "b"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn remove_current_via_remove_is_rejected() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.current = Some(0);
        assert!(!q.remove(0));
        assert_eq!(ids(&q), ["a"]);
    }

    #[test]
    fn remove_out_of_range_is_noop() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.current = Some(0);
        assert!(!q.remove(5));
        assert_eq!(ids(&q), ["a"]);
    }

    // --- remove current (R5.11) ---

    #[test]
    fn remove_current_advances_to_next() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(1); // "b"
        let outcome = q.remove_current().unwrap();
        assert_eq!(outcome, RemoveCurrentOutcome::Advanced { new_current: 1 });
        assert_eq!(ids(&q), ["a", "c"]);
        assert_eq!(current_id(&q).as_deref(), Some("c"));
    }

    #[test]
    fn remove_current_last_track_clamps_to_new_end() {
        let mut q = Queue::new();
        for id in ["a", "b"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(1); // "b" is last
        let outcome = q.remove_current().unwrap();
        assert_eq!(outcome, RemoveCurrentOutcome::Advanced { new_current: 0 });
        assert_eq!(ids(&q), ["a"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn remove_current_only_track_empties() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.current = Some(0);
        let outcome = q.remove_current().unwrap();
        assert_eq!(outcome, RemoveCurrentOutcome::Emptied);
        assert!(q.is_empty());
        assert_eq!(q.current, None);
    }

    #[test]
    fn remove_current_on_empty_returns_none() {
        let mut q = Queue::new();
        assert_eq!(q.remove_current(), None);
    }

    // --- move (R5.5) ---

    #[test]
    fn move_track_down_preserves_order_and_pointer() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // "a"
        // Move "b" (index 1) to position 4 (end).
        assert!(q.move_track(1, 4));
        assert_eq!(ids(&q), ["a", "c", "d", "b"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn move_track_up_preserves_order_and_pointer() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(3); // "d"
        // Move "c" (index 2) to position 1.
        assert!(q.move_track(2, 1));
        assert_eq!(ids(&q), ["c", "a", "b", "d"]);
        assert_eq!(current_id(&q).as_deref(), Some("d"));
    }

    #[test]
    fn move_current_track_follows_pointer() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // "a"
        assert!(q.move_track(0, 3)); // move current to end
        assert_eq!(ids(&q), ["b", "c", "a"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn move_clamps_target_above_length() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0);
        // Target 99 clamps to length 3.
        assert!(q.move_track(1, 99));
        assert_eq!(ids(&q), ["a", "c", "b"]);
    }

    #[test]
    fn move_clamps_target_below_one() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(2);
        // Target 0 clamps to position 1.
        assert!(q.move_track(2, 0));
        assert_eq!(ids(&q), ["c", "a", "b"]);
        assert_eq!(current_id(&q).as_deref(), Some("c"));
    }

    #[test]
    fn move_out_of_range_source_is_noop() {
        let mut q = Queue::new();
        q.add_to_queue(track("a")).unwrap();
        q.current = Some(0);
        assert!(!q.move_track(5, 1));
        assert_eq!(ids(&q), ["a"]);
    }

    // --- 10k limit (R5.12) ---

    #[test]
    fn add_to_queue_rejected_at_limit() {
        let mut q = Queue::new();
        q.tracks = (0..MAX_QUEUE_LEN).map(|i| track(&i.to_string())).collect();
        q.current = Some(0);
        let before = q.tracks.len();
        assert_eq!(q.add_to_queue(track("overflow")), Err(QueueFullError));
        assert_eq!(q.tracks.len(), before);
        assert_eq!(q.current, Some(0));
    }

    #[test]
    fn play_next_rejected_at_limit() {
        let mut q = Queue::new();
        q.tracks = (0..MAX_QUEUE_LEN).map(|i| track(&i.to_string())).collect();
        q.current = Some(5);
        let before = q.tracks.len();
        assert_eq!(q.play_next(track("overflow")), Err(QueueFullError));
        assert_eq!(q.tracks.len(), before);
        assert_eq!(q.current, Some(5));
    }

    #[test]
    fn add_allowed_just_below_limit() {
        let mut q = Queue::new();
        q.tracks = (0..MAX_QUEUE_LEN - 1)
            .map(|i| track(&i.to_string()))
            .collect();
        q.current = Some(0);
        assert!(q.add_to_queue(track("last")).is_ok());
        assert_eq!(q.tracks.len(), MAX_QUEUE_LEN);
        // Now at the limit, further adds are rejected.
        assert_eq!(q.add_to_queue(track("overflow")), Err(QueueFullError));
    }

    // --- shuffle (R5.6) ---

    /// A deterministic permutation that reverses the suffix, used to assert the
    /// prefix/current/pointer invariants without depending on the PRNG.
    fn reverse_slice(s: &mut [QueueTrack]) {
        s.reverse();
    }

    #[test]
    fn shuffle_fixes_prefix_and_current_permutes_suffix() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d", "e"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(1); // "b"
        q.shuffle_with(reverse_slice);
        // Prefix (a) + current (b) unchanged; suffix c,d,e reversed to e,d,c.
        assert_eq!(ids(&q), ["a", "b", "e", "d", "c"]);
        // Pointer stays on "b".
        assert_eq!(current_id(&q).as_deref(), Some("b"));
    }

    #[test]
    fn shuffle_snapshots_pre_shuffle_order() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0);
        q.shuffle_with(reverse_slice);
        assert_eq!(
            q.pre_shuffle_order.as_deref(),
            Some(["a".to_string(), "b".to_string(), "c".to_string()].as_slice())
        );
    }

    #[test]
    fn shuffle_suffix_is_a_permutation() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d", "e", "f"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0);
        q.shuffle();
        // Current + prefix unchanged.
        assert_eq!(current_id(&q).as_deref(), Some("a"));
        assert_eq!(q.tracks[0].track_id, "a");
        // The whole queue is still a permutation of the original ids.
        let mut got = ids(&q);
        got.sort();
        assert_eq!(got, ["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn shuffle_with_one_following_track_only_snapshots() {
        let mut q = Queue::new();
        for id in ["a", "b"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // one track ("b") follows
        q.shuffle_with(|_| panic!("permute must not run with <=1 following track"));
        assert_eq!(ids(&q), ["a", "b"]);
        assert!(q.pre_shuffle_order.is_some());
    }

    #[test]
    fn shuffle_on_current_last_track_only_snapshots() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(2); // nothing follows
        q.shuffle_with(|_| panic!("permute must not run with no following tracks"));
        assert_eq!(ids(&q), ["a", "b", "c"]);
    }

    // --- un-shuffle (R5.7) ---

    #[test]
    fn unshuffle_restores_pre_shuffle_order_and_pointer() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(1); // "b"
        q.shuffle_with(reverse_slice); // -> a, b, d, c
        assert_eq!(ids(&q), ["a", "b", "d", "c"]);
        q.unshuffle();
        assert_eq!(ids(&q), ["a", "b", "c", "d"]);
        assert_eq!(current_id(&q).as_deref(), Some("b"));
        assert!(q.pre_shuffle_order.is_none());
    }

    #[test]
    fn shuffle_then_unshuffle_round_trip_with_random_permutation() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d", "e"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(2); // "c"
        q.shuffle();
        q.unshuffle();
        assert_eq!(ids(&q), ["a", "b", "c", "d", "e"]);
        assert_eq!(current_id(&q).as_deref(), Some("c"));
    }

    #[test]
    fn unshuffle_excludes_tracks_removed_during_shuffle() {
        let mut q = Queue::new();
        for id in ["a", "b", "c", "d"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // "a"
        q.shuffle_with(reverse_slice); // -> a, d, c, b
        // Remove a non-current track ("c", now at index 2) while shuffled.
        let idx = q.tracks.iter().position(|t| t.track_id == "c").unwrap();
        assert!(q.remove(idx));
        q.unshuffle();
        // Original order minus the removed "c".
        assert_eq!(ids(&q), ["a", "b", "d"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn unshuffle_appends_tracks_added_during_shuffle_in_add_order() {
        let mut q = Queue::new();
        for id in ["a", "b", "c"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0); // "a"
        q.shuffle_with(reverse_slice); // -> a, c, b
        // Add two tracks while shuffled.
        q.add_to_queue(track("x")).unwrap();
        q.add_to_queue(track("y")).unwrap();
        q.unshuffle();
        // Restored a,b,c then appended x,y in add order.
        assert_eq!(ids(&q), ["a", "b", "c", "x", "y"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    #[test]
    fn unshuffle_without_snapshot_is_noop() {
        let mut q = Queue::new();
        for id in ["a", "b"] {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(0);
        q.unshuffle();
        assert_eq!(ids(&q), ["a", "b"]);
        assert_eq!(current_id(&q).as_deref(), Some("a"));
    }

    // --- repeat cycling (R5.8) ---

    #[test]
    fn cycle_repeat_follows_off_all_one_off() {
        assert_eq!(cycle_repeat(RepeatMode::Off), RepeatMode::RepeatAll);
        assert_eq!(cycle_repeat(RepeatMode::RepeatAll), RepeatMode::RepeatOne);
        assert_eq!(cycle_repeat(RepeatMode::RepeatOne), RepeatMode::Off);
    }

    #[test]
    fn cycle_repeat_three_times_returns_to_start() {
        for start in [RepeatMode::Off, RepeatMode::RepeatAll, RepeatMode::RepeatOne] {
            let back = cycle_repeat(cycle_repeat(cycle_repeat(start)));
            assert_eq!(back, start);
        }
    }

    // --- transport: next (R4.3) ---

    fn queue_of(ids: &[&str], current: usize) -> Queue {
        let mut q = Queue::new();
        for id in ids {
            q.add_to_queue(track(id)).unwrap();
        }
        q.current = Some(current);
        q
    }

    #[test]
    fn next_advances_to_following_track_and_plays() {
        let q = queue_of(&["a", "b", "c"], 0);
        for mode in [RepeatMode::Off, RepeatMode::RepeatAll, RepeatMode::RepeatOne] {
            // Mid-queue advance is independent of repeat mode.
            assert_eq!(
                q.next_transition(mode),
                Some(Transition {
                    index: 1,
                    status: PlaybackStatus::Playing
                })
            );
        }
    }

    #[test]
    fn next_on_last_track_wraps_under_repeat_all() {
        let q = queue_of(&["a", "b", "c"], 2);
        assert_eq!(
            q.next_transition(RepeatMode::RepeatAll),
            Some(Transition {
                index: 0,
                status: PlaybackStatus::Playing
            })
        );
    }

    #[test]
    fn next_on_last_track_stops_paused_when_off_or_one() {
        let q = queue_of(&["a", "b", "c"], 2);
        for mode in [RepeatMode::Off, RepeatMode::RepeatOne] {
            assert_eq!(
                q.next_transition(mode),
                Some(Transition {
                    index: 2,
                    status: PlaybackStatus::Paused
                })
            );
        }
    }

    #[test]
    fn next_single_track_last_behaviour() {
        let q = queue_of(&["a"], 0);
        assert_eq!(
            q.next_transition(RepeatMode::RepeatAll),
            Some(Transition {
                index: 0,
                status: PlaybackStatus::Playing
            })
        );
        assert_eq!(
            q.next_transition(RepeatMode::Off),
            Some(Transition {
                index: 0,
                status: PlaybackStatus::Paused
            })
        );
    }

    #[test]
    fn next_on_empty_queue_is_none() {
        let q = Queue::new();
        assert_eq!(q.next_transition(RepeatMode::Off), None);
    }

    // --- transport: previous (R4.4, R4.5) ---

    #[test]
    fn previous_within_threshold_steps_back() {
        let q = queue_of(&["a", "b", "c"], 1);
        // At or below 3 seconds, step to the preceding track.
        for pos in [0, 1_000, PREVIOUS_RESTART_THRESHOLD_MS] {
            assert_eq!(
                q.previous_transition(pos),
                Some(Transition {
                    index: 0,
                    status: PlaybackStatus::Playing
                })
            );
        }
    }

    #[test]
    fn previous_past_threshold_restarts_current() {
        let q = queue_of(&["a", "b", "c"], 1);
        assert_eq!(
            q.previous_transition(PREVIOUS_RESTART_THRESHOLD_MS + 1),
            Some(Transition {
                index: 1,
                status: PlaybackStatus::Playing
            })
        );
    }

    #[test]
    fn previous_at_first_track_restarts_current_within_threshold() {
        let q = queue_of(&["a", "b", "c"], 0);
        assert_eq!(
            q.previous_transition(0),
            Some(Transition {
                index: 0,
                status: PlaybackStatus::Playing
            })
        );
    }

    #[test]
    fn previous_on_empty_queue_is_none() {
        let q = Queue::new();
        assert_eq!(q.previous_transition(0), None);
    }

    // --- transport: track finished (R4.8) ---

    #[test]
    fn finished_repeat_one_replays_same_track() {
        // Replays regardless of position, including the last track.
        for current in [0, 1, 2] {
            let q = queue_of(&["a", "b", "c"], current);
            assert_eq!(
                q.track_finished_transition(RepeatMode::RepeatOne),
                Some(Transition {
                    index: current,
                    status: PlaybackStatus::Playing
                })
            );
        }
    }

    #[test]
    fn finished_advances_to_next_when_not_last() {
        let q = queue_of(&["a", "b", "c"], 1);
        for mode in [RepeatMode::Off, RepeatMode::RepeatAll] {
            assert_eq!(
                q.track_finished_transition(mode),
                Some(Transition {
                    index: 2,
                    status: PlaybackStatus::Playing
                })
            );
        }
    }

    #[test]
    fn finished_last_wraps_under_repeat_all() {
        let q = queue_of(&["a", "b", "c"], 2);
        assert_eq!(
            q.track_finished_transition(RepeatMode::RepeatAll),
            Some(Transition {
                index: 0,
                status: PlaybackStatus::Playing
            })
        );
    }

    #[test]
    fn finished_last_stops_paused_when_off() {
        let q = queue_of(&["a", "b", "c"], 2);
        assert_eq!(
            q.track_finished_transition(RepeatMode::Off),
            Some(Transition {
                index: 2,
                status: PlaybackStatus::Paused
            })
        );
    }

    #[test]
    fn finished_on_empty_queue_is_none() {
        let q = Queue::new();
        assert_eq!(q.track_finished_transition(RepeatMode::Off), None);
    }
}
