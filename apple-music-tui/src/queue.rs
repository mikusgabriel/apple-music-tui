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

    #[test]
    fn add_at_limit_leaves_contents_and_pointer_unchanged() {
        // At exactly MAX_QUEUE_LEN both add_to_queue and play_next are rejected
        // and leave the full Queue contents and current-track pointer unchanged
        // (Requirement 5.12).
        let mut q = Queue::new();
        q.tracks = (0..MAX_QUEUE_LEN).map(|i| track(&i.to_string())).collect();
        q.current = Some(4_200);
        // Snapshot the entire Queue to detect any mutation, not just length.
        let snapshot = q.clone();

        assert_eq!(q.add_to_queue(track("overflow-add")), Err(QueueFullError));
        assert_eq!(q, snapshot);

        assert_eq!(q.play_next(track("overflow-next")), Err(QueueFullError));
        assert_eq!(q, snapshot);
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

    // --- property: play-next insertion (R5.2) ---

    use proptest::prelude::*;

    /// Build a Queue of distinct-id tracks `t0..tn` with the given pointer.
    fn queue_with_len(len: usize, current: Option<usize>) -> Queue {
        let mut q = Queue::new();
        for i in 0..len {
            q.add_to_queue(track(&format!("t{i}"))).unwrap();
        }
        q.current = current;
        q
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 9: Play-next inserts after the current track
        // Validates: Requirements 5.2
        //
        // For any Queue and track, play_next places the inserted track at the
        // position immediately after the current track, keeping the pointer on
        // the same current track and leaving every other track's relative order
        // unchanged. When the Queue has no current track, the inserted track
        // becomes the sole current track (current == 0) without any implied
        // playback state change.
        #[test]
        fn property_play_next_inserts_after_current(
            // Keep lengths well under MAX_QUEUE_LEN so the add always succeeds.
            len in 0usize..30,
            // An arbitrary pointer choice that we clamp into range (or None for
            // an empty queue) below.
            ptr_seed in 0usize..30,
        ) {
            let current = if len == 0 {
                None
            } else {
                Some(ptr_seed % len)
            };
            let mut q = queue_with_len(len, current);

            // A new track whose id cannot collide with the existing t0..tn ids.
            let inserted = track("inserted");
            let before_ids = ids(&q);
            let before_current_id = current_id(&q);

            q.play_next(inserted.clone()).unwrap();

            // Exactly one track was added.
            prop_assert_eq!(q.len(), before_ids.len() + 1);

            match current {
                None => {
                    // Empty queue: the inserted track is now the sole current
                    // track, without changing anything else about the queue.
                    prop_assert_eq!(ids(&q), vec!["inserted".to_string()]);
                    prop_assert_eq!(q.current, Some(0));
                }
                Some(cur) => {
                    // The inserted track sits immediately after the current one.
                    prop_assert_eq!(&q.tracks[cur + 1].track_id, "inserted");

                    // The pointer still names the same current track.
                    prop_assert_eq!(current_id(&q), before_current_id);

                    // Every other track keeps its relative order: removing the
                    // inserted track recovers the original id sequence.
                    let mut without_inserted = ids(&q);
                    without_inserted.remove(cur + 1);
                    prop_assert_eq!(without_inserted, before_ids);
                }
            }
        }
    }
}

#[cfg(test)]
mod remove_property_tests {
    //! Property-based test for removing a non-current track (task 5.6). Runs the
    //! single design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// Build a Queue of `n` tracks with ids `t0..t{n-1}` and the given current
    /// pointer. Ids are unique so track identity is unambiguous.
    fn queue_with(n: usize, current: usize) -> Queue {
        let mut q = Queue::new();
        for i in 0..n {
            let id = format!("t{i}");
            q.add_to_queue(QueueTrack {
                track_id: id.clone(),
                title: format!("Title {id}"),
                artist: format!("Artist {id}"),
                duration_ms: Some(180_000),
            })
            .unwrap();
        }
        q.current = Some(current);
        q
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 11: Removing a non-current track preserves order and pointer
        //
        // Validates: Requirements 5.4
        //
        // For any Queue and any index other than the current one, removing that
        // track reduces the length by one, preserves the relative order of the
        // remaining tracks, and keeps the current-track pointer on the same
        // track identity.
        //
        // The generator builds a Queue of 2..=50 tracks (at least two so a
        // non-current index always exists), picks a current pointer inside the
        // queue, and picks a removal index that is guaranteed distinct from the
        // current one. This matches the real input space of `remove`: it rejects
        // out-of-range and current-track indices, so the property targets the
        // accepted non-current removals.
        #[test]
        fn property_remove_non_current_preserves_order_and_pointer(
            len in 2usize..=50,
            current_frac in 0.0f64..1.0,
            remove_offset_frac in 0.0f64..1.0,
        ) {
            let current = ((current_frac * len as f64) as usize).min(len - 1);

            // Choose a removal index in 0..len-1 then skip past `current`, so it
            // lands uniformly on one of the `len - 1` non-current indices.
            let raw = ((remove_offset_frac * (len - 1) as f64) as usize).min(len - 2);
            let remove_index = if raw >= current { raw + 1 } else { raw };

            let mut q = queue_with(len, current);

            // Snapshot pre-removal state.
            let ids_before: Vec<String> =
                q.tracks.iter().map(|t| t.track_id.clone()).collect();
            let current_id_before = q.tracks[current].track_id.clone();
            let removed_id = q.tracks[remove_index].track_id.clone();

            let removed = q.remove(remove_index);

            // Removing a valid non-current index always succeeds.
            prop_assert!(removed);

            // Length reduced by exactly one.
            prop_assert_eq!(q.len(), len - 1);

            // Relative order of the remaining tracks is preserved: the surviving
            // ids equal the original ids with the removed one deleted.
            let expected_ids: Vec<String> = ids_before
                .iter()
                .filter(|id| **id != removed_id)
                .cloned()
                .collect();
            let ids_after: Vec<String> =
                q.tracks.iter().map(|t| t.track_id.clone()).collect();
            prop_assert_eq!(ids_after, expected_ids);

            // The pointer still refers to the same track identity.
            let current_after = q.current.expect("non-empty queue keeps a pointer");
            prop_assert_eq!(&q.tracks[current_after].track_id, &current_id_before);
        }
    }
}

#[cfg(test)]
mod property_tests {
    //! Property-based tests for the structural Queue operations.
    //!
    //! Each test exercises one design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// Strategy for a single `QueueTrack` with arbitrary-but-bounded metadata.
    /// Track ids are drawn from a small alphabet so generated queues can hold
    /// repeated ids; the add-to-queue property does not rely on id uniqueness.
    fn queue_track() -> impl Strategy<Value = QueueTrack> {
        (
            "[a-z0-9]{1,8}",
            ".{0,16}",
            ".{0,16}",
            proptest::option::of(0u64..600_000),
        )
            .prop_map(|(track_id, title, artist, duration_ms)| QueueTrack {
                track_id,
                title,
                artist,
                duration_ms,
            })
    }

    /// Strategy for an arbitrary (and internally consistent) `Queue`: either an
    /// empty queue, or a non-empty list with a current index somewhere inside
    /// it. This matches the Queue invariant (`current` is `None` iff empty).
    fn arb_queue() -> impl Strategy<Value = Queue> {
        proptest::collection::vec(queue_track(), 0..32).prop_flat_map(|tracks| {
            if tracks.is_empty() {
                Just(Queue {
                    tracks,
                    current: None,
                    pre_shuffle_order: None,
                })
                .boxed()
            } else {
                let len = tracks.len();
                (Just(tracks), 0..len)
                    .prop_map(|(tracks, current)| Queue {
                        tracks,
                        current: Some(current),
                        pre_shuffle_order: None,
                    })
                    .boxed()
            }
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 10: Add-to-queue appends
        // Validates: Requirements 5.3
        #[test]
        fn add_to_queue_appends(queue in arb_queue(), new_track in queue_track()) {
            let mut q = queue.clone();
            let had_current = q.current;
            let old_tracks = q.tracks.clone();

            q.add_to_queue(new_track.clone()).unwrap();

            // The track is placed after the last existing track: the list grew
            // by exactly one and the new last entry is the added track, while
            // every prior track keeps its position and identity.
            prop_assert_eq!(q.tracks.len(), old_tracks.len() + 1);
            prop_assert_eq!(q.tracks.last(), Some(&new_track));
            prop_assert_eq!(&q.tracks[..old_tracks.len()], &old_tracks[..]);

            match had_current {
                // Non-empty queue: the current-track pointer is untouched, so no
                // playback starts on a different track.
                Some(current) => prop_assert_eq!(q.current, Some(current)),
                // Empty queue: the appended track becomes the sole current track
                // (index 0) without starting playback (the Queue state carries
                // no playback flag; it is simply made current).
                None => prop_assert_eq!(q.current, Some(0)),
            }
        }
    }

    /// Strategy for a non-empty `Queue` with unique, sequential track ids
    /// (`t0..tN`) plus a current index inside it. Unique ids let the move
    /// property track the moved track and the other tracks by identity.
    fn arb_nonempty_queue_unique_ids() -> impl Strategy<Value = Queue> {
        (1usize..32).prop_flat_map(|len| {
            let tracks: Vec<QueueTrack> = (0..len)
                .map(|i| QueueTrack {
                    track_id: format!("t{i}"),
                    title: format!("Title {i}"),
                    artist: format!("Artist {i}"),
                    duration_ms: Some(180_000),
                })
                .collect();
            (Just(tracks), 0..len).prop_map(|(tracks, current)| Queue {
                tracks,
                current: Some(current),
                pre_shuffle_order: None,
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 12: Move clamps target and preserves other tracks
        // Validates: Requirements 5.5
        //
        // For any non-empty Queue, source position `from`, and target position
        // `target`, moving the track at `from` places it at the target clamped
        // to the inclusive 1-based range 1..=len (0-based destination
        // `target.clamp(1, len) - 1`), preserves the relative order of all
        // other tracks, and keeps the current-track pointer on the same track
        // identity. `target` is drawn from 0..len+3 so below-range (0) and
        // above-range (>len) targets are exercised alongside in-range ones.
        #[test]
        fn move_clamps_target_and_preserves_other_tracks(
            queue in arb_nonempty_queue_unique_ids(),
            from_sel in any::<usize>(),
            target_sel in any::<usize>(),
        ) {
            let len = queue.tracks.len();
            let from = from_sel % len;
            let target = target_sel % (len + 3);

            let mut q = queue.clone();
            let ids = |q: &Queue| -> Vec<String> {
                q.tracks.iter().map(|t| t.track_id.clone()).collect()
            };
            let current_id = |q: &Queue| -> Option<String> {
                q.current.map(|i| q.tracks[i].track_id.clone())
            };

            let original_ids = ids(&q);
            let moved_id = original_ids[from].clone();
            let current_identity = current_id(&q);
            let others_before: Vec<String> = original_ids
                .iter()
                .filter(|id| *id != &moved_id)
                .cloned()
                .collect();

            // A valid source position is always accepted.
            prop_assert!(q.move_track(from, target));

            // Length and the full set of tracks are preserved.
            prop_assert_eq!(q.tracks.len(), len);
            let mut after_sorted = ids(&q);
            after_sorted.sort();
            let mut before_sorted = original_ids.clone();
            before_sorted.sort();
            prop_assert_eq!(after_sorted, before_sorted);

            // The moved track lands at the clamped 0-based destination.
            let dest = target.clamp(1, len) - 1;
            let actual_pos = q
                .tracks
                .iter()
                .position(|t| t.track_id == moved_id)
                .expect("moved track is still present");
            prop_assert_eq!(actual_pos, dest);

            // The relative order of every other track is unchanged.
            let others_after: Vec<String> =
                ids(&q).into_iter().filter(|id| id != &moved_id).collect();
            prop_assert_eq!(others_after, others_before);

            // The current-track pointer still rests on the same track identity.
            prop_assert_eq!(current_id(&q), current_identity);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: apple-music-tui, Property 12: Move clamps target and preserves other tracks
        // Validates: Requirements 5.5
        //
        // An out-of-range source position leaves the Queue entirely unchanged
        // (tracks and current-track pointer) and reports no change.
        #[test]
        fn move_out_of_range_source_is_noop(
            queue in arb_queue(),
            from_extra in 0usize..50,
            target in any::<usize>(),
        ) {
            let mut q = queue.clone();
            // `from` is guaranteed to be at or beyond the end of the list.
            let from = q.tracks.len() + from_extra;

            prop_assert!(!q.move_track(from, target));
            prop_assert_eq!(q, queue);
        }
    }
}

#[cfg(test)]
mod shuffle_roundtrip_property_tests {
    //! Property-based test for the shuffle/un-shuffle round-trip (task 5.9).
    //! Runs the single design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// Strategy for a non-empty `Queue` with unique, sequential track ids
    /// (`t0..t{n-1}`) plus a current index inside it. Unique ids let the
    /// round-trip compare order and pointer by identity, and shuffle's real
    /// snapshot/restore relies on distinct ids.
    fn arb_nonempty_queue_unique_ids() -> impl Strategy<Value = Queue> {
        (1usize..32).prop_flat_map(|len| {
            let tracks: Vec<QueueTrack> = (0..len)
                .map(|i| QueueTrack {
                    track_id: format!("t{i}"),
                    title: format!("Title {i}"),
                    artist: format!("Artist {i}"),
                    duration_ms: Some(180_000),
                })
                .collect();
            (Just(tracks), 0..len).prop_map(|(tracks, current)| Queue {
                tracks,
                current: Some(current),
                pre_shuffle_order: None,
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 14: Shuffle then un-shuffle round-trip
        // Validates: Requirements 5.7
        //
        // For any Queue, enabling shuffle and then disabling it with no
        // intervening edits restores the original track order and keeps the
        // current-track pointer on the same track. The generator builds
        // non-empty queues with unique ids and an arbitrary current pointer so
        // the prefix-fixed / suffix-permuted shuffle and the id-based unshuffle
        // restore are exercised across the whole input space.
        #[test]
        fn shuffle_then_unshuffle_round_trip(queue in arb_nonempty_queue_unique_ids()) {
            let ids = |q: &Queue| -> Vec<String> {
                q.tracks.iter().map(|t| t.track_id.clone()).collect()
            };
            let current_id = |q: &Queue| -> Option<String> {
                q.current.map(|i| q.tracks[i].track_id.clone())
            };

            let original_ids = ids(&queue);
            let original_current_id = current_id(&queue);

            let mut q = queue.clone();
            // Enable shuffle (uses the real clock-seeded permutation) then
            // immediately disable it with no intervening edits.
            q.shuffle();
            q.unshuffle();

            // The original track order is restored exactly.
            prop_assert_eq!(ids(&q), original_ids);
            // The current-track pointer rests on the same track.
            prop_assert_eq!(current_id(&q), original_current_id);
            // The snapshot is cleared after un-shuffle.
            prop_assert!(q.pre_shuffle_order.is_none());
        }
    }
}

#[cfg(test)]
mod repeat_cycle_property_tests {
    //! Property-based test for repeat-mode cycling (task 5.10). Runs the single
    //! design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// Strategy over the three repeat modes.
    fn arb_repeat_mode() -> impl Strategy<Value = RepeatMode> {
        prop_oneof![
            Just(RepeatMode::Off),
            Just(RepeatMode::RepeatAll),
            Just(RepeatMode::RepeatOne),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 15: Repeat mode cycling
        // Validates: Requirements 5.8
        //
        // For any starting repeat mode, one cycle maps Off -> RepeatAll,
        // RepeatAll -> RepeatOne, and RepeatOne -> Off, and three consecutive
        // cycles return to the starting mode.
        #[test]
        fn repeat_cycling(mode in arb_repeat_mode()) {
            // One cycle follows the Off -> RepeatAll -> RepeatOne -> Off order.
            let once = cycle_repeat(mode);
            let expected_once = match mode {
                RepeatMode::Off => RepeatMode::RepeatAll,
                RepeatMode::RepeatAll => RepeatMode::RepeatOne,
                RepeatMode::RepeatOne => RepeatMode::Off,
            };
            prop_assert_eq!(once, expected_once);

            // Three consecutive cycles return to the starting mode.
            let thrice = cycle_repeat(cycle_repeat(cycle_repeat(mode)));
            prop_assert_eq!(thrice, mode);
        }
    }
}

#[cfg(test)]
mod shuffle_property_tests {
    //! Property-based test for enabling shuffle (task 5.8). Runs the single
    //! design correctness property at >=100 iterations in a uniquely-named
    //! module so it does not collide with the other `property_tests` modules.
    use super::*;
    use proptest::prelude::*;

    /// Strategy for a non-empty `Queue` with unique, sequential track ids
    /// (`t0..t{n-1}`) and a current index inside it. Unique ids let the
    /// property distinguish the prefix, the current track, and the suffix by
    /// identity and confirm the suffix is a permutation of itself.
    fn arb_nonempty_queue_unique_ids() -> impl Strategy<Value = Queue> {
        (1usize..32).prop_flat_map(|len| {
            let tracks: Vec<QueueTrack> = (0..len)
                .map(|i| QueueTrack {
                    track_id: format!("t{i}"),
                    title: format!("Title {i}"),
                    artist: format!("Artist {i}"),
                    duration_ms: Some(180_000),
                })
                .collect();
            (Just(tracks), 0..len).prop_map(|(tracks, current)| Queue {
                tracks,
                current: Some(current),
                pre_shuffle_order: None,
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 13: Shuffle fixes the prefix and permutes the suffix
        // Validates: Requirements 5.6
        //
        // For any Queue, enabling shuffle leaves the current track and every
        // track before it unchanged in place and reorders the tracks after the
        // current track into a permutation of those same tracks (unchanged when
        // one or fewer tracks follow the current track).
        //
        // The generator builds a non-empty Queue with unique ids and a pointer
        // inside it, then drives shuffle through `shuffle_with` using a
        // deterministic permutation (a rotation by one) so the assertions do
        // not depend on the system-seeded PRNG. A rotation reorders every
        // suffix of two or more tracks, which also lets us confirm that a
        // suffix of two or more actually changes.
        #[test]
        fn shuffle_fixes_prefix_and_permutes_suffix(
            queue in arb_nonempty_queue_unique_ids(),
        ) {
            let mut q = queue.clone();
            let current = q.current.expect("non-empty queue has a pointer");
            let len = q.tracks.len();

            let ids = |q: &Queue| -> Vec<String> {
                q.tracks.iter().map(|t| t.track_id.clone()).collect()
            };

            let before_ids = ids(&queue);
            // The prefix is the current track and everything before it.
            let prefix_end = current + 1;
            let prefix_before = before_ids[..prefix_end].to_vec();
            let suffix_before = before_ids[prefix_end..].to_vec();
            let suffix_len = suffix_before.len();

            // Deterministic permutation: rotate the suffix left by one. This is
            // a permutation for any length and reorders every suffix of length
            // >= 2.
            q.shuffle_with(|suffix| {
                if suffix.len() >= 2 {
                    suffix.rotate_left(1);
                }
            });

            let after_ids = ids(&q);

            // Length never changes.
            prop_assert_eq!(after_ids.len(), len);

            // The pointer stays on the same track position (and so the same
            // track identity, since the prefix is fixed).
            prop_assert_eq!(q.current, Some(current));

            // The current track and every track before it are unchanged in
            // place.
            prop_assert_eq!(&after_ids[..prefix_end], &prefix_before[..]);

            let suffix_after = after_ids[prefix_end..].to_vec();

            // The suffix is a permutation of the original suffix tracks.
            let mut sorted_before = suffix_before.clone();
            sorted_before.sort();
            let mut sorted_after = suffix_after.clone();
            sorted_after.sort();
            prop_assert_eq!(&sorted_after, &sorted_before);

            if suffix_len <= 1 {
                // One or fewer following tracks: the Queue is left unchanged.
                prop_assert_eq!(&suffix_after, &suffix_before);
            }
        }
    }
}

#[cfg(test)]
mod transport_property_tests {
    //! Property-based test for the transport transitions (task 5.13).
    //!
    //! Exercises the single design correctness property (Property 8) at >=100
    //! iterations against `next_transition`, `previous_transition`, and
    //! `track_finished_transition`.
    use super::*;
    use proptest::prelude::*;

    /// Build a non-empty Queue of `len` tracks (ids `t0..t{len-1}`) with the
    /// given current pointer. Distinct ids are not required here because the
    /// transitions reason purely over indices, but they keep failures readable.
    fn queue_with(len: usize, current: usize) -> Queue {
        let mut q = Queue::new();
        for i in 0..len {
            let id = format!("t{i}");
            q.add_to_queue(QueueTrack {
                track_id: id.clone(),
                title: format!("Title {id}"),
                artist: format!("Artist {id}"),
                duration_ms: Some(180_000),
            })
            .unwrap();
        }
        q.current = Some(current);
        q
    }

    /// Enumerate the three repeat modes so each property run covers all of them.
    fn repeat_mode() -> impl Strategy<Value = RepeatMode> {
        prop_oneof![
            Just(RepeatMode::Off),
            Just(RepeatMode::RepeatAll),
            Just(RepeatMode::RepeatOne),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 8: Transport transitions over repeat modes
        // Validates: Requirements 4.3, 4.4, 4.5, 4.8
        //
        // For any Queue, current-track index, repeat mode, and (for previous)
        // current position, the next-track, previous-track, and track-finished
        // transitions produce the current index and play/pause status defined
        // by the transition rules:
        //   * next (R4.3): advance and play when a following track exists; at
        //     the last track, wrap to index 0 playing under repeat all,
        //     otherwise (off / repeat one) stay on the last track paused.
        //   * previous (R4.4, R4.5): restart the current track playing when the
        //     position is past 3 seconds or the current track is the first one;
        //     otherwise step back one track playing. Repeat mode is irrelevant.
        //   * track-finished (R4.8): replay the current track playing under
        //     repeat one; otherwise advance and play when a following track
        //     exists; at the last track wrap to index 0 playing under repeat
        //     all and stop paused on the last track under repeat off.
        #[test]
        fn property_transport_transitions(
            len in 1usize..32,
            current_frac in 0.0f64..1.0,
            repeat in repeat_mode(),
            position_ms in 0u64..600_000,
        ) {
            let current = ((current_frac * len as f64) as usize).min(len - 1);
            let q = queue_with(len, current);
            let last = len - 1;

            // --- next-track transition (Requirement 4.3) ---
            let expected_next = if current < last {
                Transition { index: current + 1, status: PlaybackStatus::Playing }
            } else {
                match repeat {
                    RepeatMode::RepeatAll => {
                        Transition { index: 0, status: PlaybackStatus::Playing }
                    }
                    RepeatMode::Off | RepeatMode::RepeatOne => {
                        Transition { index: last, status: PlaybackStatus::Paused }
                    }
                }
            };
            prop_assert_eq!(q.next_transition(repeat), Some(expected_next));

            // --- previous-track transition (Requirements 4.4, 4.5) ---
            // Previous always plays and ignores repeat mode.
            let expected_prev_index =
                if position_ms > PREVIOUS_RESTART_THRESHOLD_MS || current == 0 {
                    current
                } else {
                    current - 1
                };
            let expected_prev = Transition {
                index: expected_prev_index,
                status: PlaybackStatus::Playing,
            };
            // The result is independent of repeat mode, so assert it holds for
            // the drawn mode and that every mode agrees.
            prop_assert_eq!(q.previous_transition(position_ms), Some(expected_prev));
            for mode_invariant_check in
                [RepeatMode::Off, RepeatMode::RepeatAll, RepeatMode::RepeatOne]
            {
                let _ = mode_invariant_check; // repeat does not alter previous
                prop_assert_eq!(
                    q.previous_transition(position_ms),
                    Some(expected_prev)
                );
            }

            // --- track-finished transition (Requirement 4.8) ---
            let expected_finished = match repeat {
                RepeatMode::RepeatOne => {
                    Transition { index: current, status: PlaybackStatus::Playing }
                }
                RepeatMode::RepeatAll => {
                    if current < last {
                        Transition {
                            index: current + 1,
                            status: PlaybackStatus::Playing,
                        }
                    } else {
                        Transition { index: 0, status: PlaybackStatus::Playing }
                    }
                }
                RepeatMode::Off => {
                    if current < last {
                        Transition {
                            index: current + 1,
                            status: PlaybackStatus::Playing,
                        }
                    } else {
                        Transition { index: last, status: PlaybackStatus::Paused }
                    }
                }
            };
            prop_assert_eq!(
                q.track_finished_transition(repeat),
                Some(expected_finished)
            );
        }
    }
}

#[cfg(test)]
mod remove_current_property_tests {
    //! Property-based test for removing the current track (task 5.11). Runs the
    //! single design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// Strategy for a non-empty `Queue` with unique, sequential track ids
    /// (`t0..t{n-1}`) plus a current index inside it. Unique ids let the
    /// property track the following track and the surviving tracks by identity.
    fn arb_nonempty_queue_unique_ids() -> impl Strategy<Value = Queue> {
        (1usize..32).prop_flat_map(|len| {
            let tracks: Vec<QueueTrack> = (0..len)
                .map(|i| QueueTrack {
                    track_id: format!("t{i}"),
                    title: format!("Title {i}"),
                    artist: format!("Artist {i}"),
                    duration_ms: Some(180_000),
                })
                .collect();
            (Just(tracks), 0..len).prop_map(|(tracks, current)| Queue {
                tracks,
                current: Some(current),
                pre_shuffle_order: None,
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 17: Removing the current track advances the pointer
        // Validates: Requirements 5.11
        //
        // For any Queue with a current track, removing the current track moves
        // the pointer to the track that followed it, or to no track when the
        // removed track was last. We capture the removed track's identity, the
        // following track's identity (if any), and the surviving ids before the
        // operation, then assert the outcome, the new pointer, and that exactly
        // the removed track is gone while the rest keep their relative order.
        #[test]
        fn remove_current_advances_pointer(queue in arb_nonempty_queue_unique_ids()) {
            let ids = |q: &Queue| -> Vec<String> {
                q.tracks.iter().map(|t| t.track_id.clone()).collect()
            };
            let current_id = |q: &Queue| -> Option<String> {
                q.current.map(|i| q.tracks[i].track_id.clone())
            };

            let original_ids = ids(&queue);
            let current = queue.current.expect("strategy yields a current track");
            let was_last = current + 1 == original_ids.len();
            let removed_id = original_ids[current].clone();
            // The track that followed the current one, if the current was not last.
            let following_id = original_ids.get(current + 1).cloned();
            // Every track except the removed one, in original relative order.
            let survivors: Vec<String> = original_ids
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != current)
                .map(|(_, id)| id.clone())
                .collect();

            let mut q = queue.clone();
            let outcome = q
                .remove_current()
                .expect("a queue with a current track removes it");

            // The removed track is gone and every other track keeps its order.
            prop_assert_eq!(ids(&q), survivors.clone());

            if was_last {
                if survivors.is_empty() {
                    // The removed track was the only track: the queue empties and
                    // the pointer clears (points at no track).
                    prop_assert_eq!(outcome, RemoveCurrentOutcome::Emptied);
                    prop_assert!(q.is_empty());
                    prop_assert_eq!(q.current, None);
                } else {
                    // The removed track was last among several: no following
                    // track existed, so the pointer rests on the new final track.
                    let new_current = q.tracks.len() - 1;
                    prop_assert_eq!(outcome, RemoveCurrentOutcome::Advanced { new_current });
                    prop_assert_eq!(q.current, Some(new_current));
                    prop_assert_eq!(current_id(&q), survivors.last().cloned());
                }
            } else {
                // A following track existed: the pointer advances to it, keeping
                // the same index the follower now occupies (`current`).
                prop_assert_eq!(outcome, RemoveCurrentOutcome::Advanced { new_current: current });
                prop_assert_eq!(q.current, Some(current));
                prop_assert_eq!(current_id(&q), following_id);
            }

            // The removed id never lingers in the queue.
            prop_assert!(!ids(&q).contains(&removed_id));
        }
    }
}

#[cfg(test)]
mod queue_invariant_stateful_property_tests {
    //! Stateful property-based test for the Queue invariant (task 5.12). Runs a
    //! randomly generated sequence of add / remove / reorder / shuffle
    //! operations against a real `Queue` and checks the invariant after every
    //! step, at >=100 iterations, in a uniquely-named module so it does not
    //! collide with the other `property_tests` / `*_property_tests` modules.
    use super::*;
    use proptest::prelude::*;

    /// The structural operations the state machine can apply to the Queue. The
    /// embedded selectors (`usize` / `f64` fractions) are resolved against the
    /// live Queue length when the step runs, so every generated op is valid to
    /// attempt even as the Queue grows and shrinks across the sequence.
    #[derive(Debug, Clone)]
    enum Op {
        /// Insert a fresh track immediately after the current track.
        PlayNext,
        /// Append a fresh track after the last track.
        AddToQueue,
        /// Remove a non-current track chosen by a fraction of the length.
        RemoveNonCurrent(f64),
        /// Remove the current track.
        RemoveCurrent,
        /// Move a track (chosen by fraction) to a target position (chosen by
        /// fraction over a slightly widened range so clamping is exercised).
        Move(f64, f64),
        /// Enable shuffle (real clock-seeded permutation).
        Shuffle,
        /// Disable shuffle, restoring the pre-shuffle order.
        Unshuffle,
    }

    /// Strategy over a single operation. Each variant is weighted so adds are
    /// common enough to let the Queue grow before removals shrink it.
    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::PlayNext),
            3 => Just(Op::AddToQueue),
            2 => (0.0f64..1.0).prop_map(Op::RemoveNonCurrent),
            1 => Just(Op::RemoveCurrent),
            2 => (0.0f64..1.0, 0.0f64..1.0).prop_map(|(a, b)| Op::Move(a, b)),
            1 => Just(Op::Shuffle),
            1 => Just(Op::Unshuffle),
        ]
    }

    /// The Queue invariant from Requirement 5.9: the current-track pointer names
    /// a track that exists in the Queue, or names no track exactly when the
    /// Queue is empty.
    fn pointer_invariant_holds(q: &Queue) -> bool {
        match q.current {
            None => q.tracks.is_empty(),
            Some(i) => !q.tracks.is_empty() && i < q.tracks.len(),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 16: Queue invariant across operation sequences
        // Validates: Requirements 5.9
        //
        // For any sequence of add, remove, reorder, and shuffle operations,
        // after each operation the current-track pointer references a track that
        // exists in the Queue (or references no track exactly when the Queue is
        // empty), and the Queue length equals the starting length plus the
        // number of added tracks minus the number of removed tracks.
        //
        // The test is state-machine style: it seeds a Queue of 0..=8 tracks,
        // then applies a generated sequence of up to 40 operations one at a
        // time. Each step resolves its selectors against the live Queue, applies
        // the real `Queue` method, and counts a track as added only when an add
        // actually grew the Queue and as removed only when a remove actually
        // shrank it (rejected adds at the limit, out-of-range removes, and
        // current-track removes via `remove` are no-ops and are not counted).
        // After every step both the pointer invariant and the
        // length == start + added - removed accounting must hold; reorder and
        // shuffle move tracks around without changing the count.
        #[test]
        fn queue_invariant_across_operation_sequences(
            start_len in 0usize..=8,
            start_ptr in 0.0f64..1.0,
            ops in proptest::collection::vec(arb_op(), 0..40),
        ) {
            // Seed a Queue with unique ids so track identity is unambiguous.
            let mut q = Queue::new();
            for i in 0..start_len {
                let id = format!("s{i}");
                q.add_to_queue(QueueTrack {
                    track_id: id.clone(),
                    title: format!("Title {id}"),
                    artist: format!("Artist {id}"),
                    duration_ms: Some(180_000),
                })
                .unwrap();
            }
            // `add_to_queue` on an empty queue sets current to 0; move it to an
            // arbitrary in-range position so the sequence starts from a varied
            // pointer. (No-op when the queue is empty.)
            if start_len > 0 {
                q.current = Some(((start_ptr * start_len as f64) as usize).min(start_len - 1));
            }

            let starting_len = start_len;
            let mut added: usize = 0;
            let mut removed: usize = 0;
            // Counter for fresh, collision-free ids on every add.
            let mut next_id: usize = 0;

            // The invariant must already hold on the seeded Queue.
            prop_assert!(pointer_invariant_holds(&q), "seed violates pointer invariant: {:?}", q);
            prop_assert_eq!(q.len(), starting_len);

            for op in ops {
                let len_before = q.len();

                match op {
                    Op::PlayNext => {
                        let id = format!("add{next_id}");
                        next_id += 1;
                        let res = q.play_next(QueueTrack {
                            track_id: id.clone(),
                            title: id.clone(),
                            artist: id.clone(),
                            duration_ms: None,
                        });
                        // Count the add only when it actually grew the Queue.
                        if res.is_ok() {
                            added += 1;
                            prop_assert_eq!(q.len(), len_before + 1);
                        } else {
                            // Rejected at the limit: Queue unchanged.
                            prop_assert_eq!(q.len(), len_before);
                        }
                    }
                    Op::AddToQueue => {
                        let id = format!("add{next_id}");
                        next_id += 1;
                        let res = q.add_to_queue(QueueTrack {
                            track_id: id.clone(),
                            title: id.clone(),
                            artist: id.clone(),
                            duration_ms: None,
                        });
                        if res.is_ok() {
                            added += 1;
                            prop_assert_eq!(q.len(), len_before + 1);
                        } else {
                            prop_assert_eq!(q.len(), len_before);
                        }
                    }
                    Op::RemoveNonCurrent(frac) => {
                        if len_before == 0 {
                            // Nothing to remove.
                            prop_assert_eq!(q.len(), 0);
                        } else {
                            let index = ((frac * len_before as f64) as usize).min(len_before - 1);
                            let did_remove = q.remove(index);
                            if did_remove {
                                removed += 1;
                                prop_assert_eq!(q.len(), len_before - 1);
                            } else {
                                // Current-track index (rejected by `remove`): no change.
                                prop_assert_eq!(q.len(), len_before);
                            }
                        }
                    }
                    Op::RemoveCurrent => {
                        match q.remove_current() {
                            Some(_) => {
                                removed += 1;
                                prop_assert_eq!(q.len(), len_before - 1);
                            }
                            None => {
                                // Empty queue: no current track to remove.
                                prop_assert_eq!(q.len(), len_before);
                            }
                        }
                    }
                    Op::Move(from_frac, target_frac) => {
                        if len_before == 0 {
                            prop_assert_eq!(q.len(), 0);
                        } else {
                            let from = ((from_frac * len_before as f64) as usize)
                                .min(len_before - 1);
                            // Widen the target range so below-range (0) and
                            // above-range (>len) targets exercise clamping.
                            let target = (target_frac * (len_before as f64 + 3.0)) as usize;
                            q.move_track(from, target);
                            // Reorder never changes the count.
                            prop_assert_eq!(q.len(), len_before);
                        }
                    }
                    Op::Shuffle => {
                        q.shuffle();
                        prop_assert_eq!(q.len(), len_before);
                    }
                    Op::Unshuffle => {
                        q.unshuffle();
                        prop_assert_eq!(q.len(), len_before);
                    }
                }

                // After every operation the pointer invariant holds.
                prop_assert!(
                    pointer_invariant_holds(&q),
                    "pointer invariant violated after {:?}: {:?}",
                    op,
                    q
                );

                // And the length accounting holds: start + added - removed.
                let expected_len = starting_len + added - removed;
                prop_assert_eq!(q.len(), expected_len);
            }
        }
    }
}
