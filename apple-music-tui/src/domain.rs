//! Domain_Object types (Song/Album/Artist/Playlist) and shared playback enums.
//!
//! This module defines the in-memory representations of Apple Music resources
//! (Requirement 12) plus the shared enums used across playback, queue, session,
//! and configuration (playback status, repeat mode). Behavior/mapping logic is
//! implemented in later tasks; this module is type definitions only.

use serde::{Deserialize, Serialize};

/// A tagged union of the four Apple Music resource types.
///
/// `id` and `name` are always required (Requirement 12.3: a resource lacking
/// either is omitted and logged). Missing text attributes map to empty strings
/// and a missing duration maps to `None` (Requirement 12.1–12.2).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Domain_Object {
    Song {
        id: String,
        name: String,
        artist_name: String,
        album_name: String,
        duration_ms: Option<u64>,
        artwork_url: Option<String>,
        is_playable: bool,
    },
    Album {
        id: String,
        name: String,
        artist_name: String,
        artwork_url: Option<String>,
    },
    Artist {
        id: String,
        name: String,
        artwork_url: Option<String>,
    },
    Playlist {
        id: String,
        name: String,
        artwork_url: Option<String>,
    },
}

/// Playback status reported by the Playback_Engine and mirrored by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

/// Repeat mode for the Queue (Requirement 5.8 cycles Off → RepeatAll → RepeatOne).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    RepeatAll,
    RepeatOne,
}
