# Implementation Plan: Apple Music TUI

## Overview

This plan implements the two-process architecture from the design: a native **Rust/ratatui TUI process** that owns all durable state (Queue, Config, Session, tokens, API access, MPRIS) and a disposable **Electron/Chromium Playback_Engine process** that runs MusicKit JS for DRM playback and hosts the Sign_In_Window. The two communicate over a length-prefixed JSON-RPC Unix-domain-socket channel.

Tasks start with scaffolding and pure data models, then build the independently-testable logic (Config/Session/Queue with their round-trip and invariant property tests, Domain_Object mapping, formatters), then the networked Auth_Manager/API_Client/Token_Store, then the IPC layer, then the Electron Playback_Engine and Sign_In_Window, then the TUI views, Now_Playing_Bar, keybindings, MPRIS, and finally the integration/smoke/resource tests. Each step builds on the previous and wires into the running application.

Property-based tests (`proptest`) implement the 27 correctness properties from the design, one test per property. DRM, web-view, D-Bus, external-asset, and resource-budget behavior is covered by integration/smoke tests rather than property tests, per the design's testing strategy.

## Tasks

- [x] 1. Scaffold the two-process workspace and core types
  - [x] 1.1 Set up the Rust TUI crate and Cargo workspace
    - Create the Cargo workspace with the `apple-music-tui` TUI crate
    - Add dependencies: `ratatui`, `crossterm`, `tokio`, `reqwest`, `serde`, `serde_json`, `toml`, `zbus`, `proptest` (dev)
    - Create module files (`config`, `session`, `queue`, `domain`, `api`, `auth`, `token_store`, `ipc`, `ui`, `now_playing`, `keybindings`, `mpris`) as empty stubs
    - _Requirements: 10.1, 10.2_
  - [x] 1.2 Define Domain_Object, Playback_State, Queue, Session_State, and Configuration types
    - Implement the `Domain_Object` tagged union (Song/Album/Artist/Playlist) with required `id`/`name` and optional `artistName`/`durationMs`/`artworkUrl`/`isPlayable`
    - Implement `Playback_State`, `QueueTrack`, `Queue`, `Session_State`, and `Configuration` structs with their fields and enums (status, repeat mode, token-store backend)
    - Derive `serde` traits where serialization is needed
    - _Requirements: 12.1, 5.9, 6.1, 9.3, 8.1_
    - _Design: Data Models_
  - [x] 1.3 Scaffold the Electron Playback_Engine project
    - Create the Node/Electron project with a main process and a package manifest pinning Electron
    - Add launch flags `--disable-gpu`, `--disable-extensions`, single renderer, audio-only features
    - Create placeholder modules for the IPC server, the MusicKit player window, and the Sign_In_Window
    - _Requirements: 10.3, 10.8_
    - _Design: Technology Stack and Rationale, Playback_Engine_

- [x] 2. Implement duration formatting and the Now_Playing_Bar layout logic
  - [x] 2.1 Implement the duration formatter
    - Format non-negative durations as `m:ss` under 1 hour and `h:mm:ss` for 1 hour or more, with zero-padding
    - Provide an inverse parse of the formatted string back to whole seconds
    - _Requirements: 3.4, 6.5_
  - [x] 2.2 Write property test for duration formatting
    - **Property 5: Duration formatting**
    - **Validates: Requirements 3.4, 6.5**
  - [x] 2.3 Implement Now_Playing_Bar field rendering and responsive truncation
    - Render all Playback_State fields; apply "nothing playing" and missing-field placeholders
    - Truncate album, then artist, then title with a single ellipsis while keeping elapsed/duration/status visible
    - _Requirements: 6.1, 6.3, 6.4, 6.7_
    - _Design: Now_Playing_Bar_
  - [x] 2.4 Write property test for Now_Playing_Bar truncation
    - **Property 18: Now_Playing_Bar truncation under narrow width**
    - **Validates: Requirements 6.4**

- [x] 3. Implement the Configuration subsystem (parser, printer, defaults)
  - [x] 3.1 Implement Config_Parser validation and typed/positioned errors
    - Parse TOML into a Configuration; validate types and value ranges (seek step 1-300, Storefront as ISO 3166-1 alpha-2)
    - Return errors naming the setting and 1-based line number (line only for untieable syntax errors)
    - Apply defaults for absent settings without rewriting; warn and ignore unknown settings with their line number
    - _Requirements: 8.1, 8.3, 8.6, 8.8_
    - _Design: Config_Parser / Config_Printer_
  - [x] 3.2 Implement Config_Printer and default-file creation
    - Serialize a Configuration to TOML the parser accepts, emitting no credentials
    - Create the Config_File and parent directories with default content when absent; warn and continue on write failure
    - _Requirements: 8.2, 8.4, 8.7_
    - _Design: Config_Parser / Config_Printer_
  - [x] 3.3 Write property test for Config validation
    - **Property 19: Config validation**
    - **Validates: Requirements 8.3**
  - [x] 3.4 Write property test for Config print/parse round-trip
    - **Property 20: Config print then parse round-trip**
    - **Validates: Requirements 8.5**
  - [x] 3.5 Write property test for Config defaults on absent settings
    - **Property 21: Config applies defaults for absent settings**
    - **Validates: Requirements 8.6**
  - [x] 3.6 Write unit test for exact default-config content
    - Assert the concrete default TOML produced when the file is absent
    - _Requirements: 8.2_

- [x] 4. Implement the Session_State persistence subsystem
  - [x] 4.1 Implement Session_State serialization and atomic write
    - Serialize/deserialize Session_State as JSON at the XDG state path, creating parent directories
    - Write atomically via temp-file-plus-rename so the file always holds a complete state
    - On write failure, surface/print an error and leave any existing file unchanged while still exiting
    - _Requirements: 9.1, 9.2, 9.8_
    - _Design: Session_State store_
  - [x] 4.2 Implement Session_State restore with validation, clamping, and pruning
    - Restore Queue/current/position/volume/shuffle/repeat as paused with no audio; clamp position past duration to 0
    - On missing file start empty with no warning; on corrupt/out-of-range file start empty with a warning and leave the file untouched
    - Prune Queue tracks unavailable from the API, keep relative order, advance pointer, warn with the removed count
    - _Requirements: 9.4, 9.5, 9.6, 9.7_
    - _Design: Session_State store_
  - [x] 4.3 Write property test for Session_State round-trip
    - **Property 22: Session_State serialize then deserialize round-trip**
    - **Validates: Requirements 9.3**
  - [x] 4.4 Write property test for corrupt/out-of-range session fallback
    - **Property 23: Corrupt or out-of-range session falls back to defaults**
    - **Validates: Requirements 9.6**
  - [x] 4.5 Write property test for pruning unavailable tracks on restore
    - **Property 24: Pruning unavailable tracks on restore**
    - **Validates: Requirements 9.7**

- [x] 5. Implement the Queue state machine
  - [x] 5.1 Implement add, play-next, add-to-queue, remove, and move operations
    - Play-next inserts after current (or sole current without playback); add-to-queue appends (or sole current without playback)
    - Remove non-current preserves order and pointer; remove current advances pointer to next or none
    - Move clamps the target to 1..length and preserves other tracks; enforce the 10,000-track limit rejecting excess adds
    - _Requirements: 5.2, 5.3, 5.4, 5.5, 5.11, 5.12_
    - _Design: Queue_
  - [x] 5.2 Implement shuffle, un-shuffle restoration, and repeat cycling
    - Shuffle fixes the current track and its prefix and permutes the suffix; snapshot pre-shuffle order
    - Un-shuffle restores the pre-shuffle order excluding removed tracks and appends tracks added during shuffle; keep pointer
    - Cycle repeat off -> all -> one -> off
    - _Requirements: 5.6, 5.7, 5.8_
    - _Design: Queue_
  - [x] 5.3 Implement transport transitions over repeat modes
    - Compute next/previous/track-finished target index and play/pause status under each repeat mode
    - Previous restarts current within 3 seconds or at list start; next/finish wrap under repeat all and stop paused at the last track when off
    - _Requirements: 4.3, 4.4, 4.5, 4.8_
    - _Design: Playback-Start Sequence, IPC Protocol_
  - [x] 5.4 Write property test for play-next insertion
    - **Property 9: Play-next inserts after the current track**
    - **Validates: Requirements 5.2**
  - [x] 5.5 Write property test for add-to-queue append
    - **Property 10: Add-to-queue appends**
    - **Validates: Requirements 5.3**
  - [x] 5.6 Write property test for removing a non-current track
    - **Property 11: Removing a non-current track preserves order and pointer**
    - **Validates: Requirements 5.4**
  - [x] 5.7 Write property test for move clamping
    - **Property 12: Move clamps target and preserves other tracks**
    - **Validates: Requirements 5.5**
  - [x] 5.8 Write property test for shuffle prefix/suffix
    - **Property 13: Shuffle fixes the prefix and permutes the suffix**
    - **Validates: Requirements 5.6**
  - [x] 5.9 Write property test for shuffle/un-shuffle round-trip
    - **Property 14: Shuffle then un-shuffle round-trip**
    - **Validates: Requirements 5.7**
  - [x] 5.10 Write property test for repeat cycling
    - **Property 15: Repeat mode cycling**
    - **Validates: Requirements 5.8**
  - [x] 5.11 Write property test for removing the current track
    - **Property 17: Removing the current track advances the pointer**
    - **Validates: Requirements 5.11**
  - [x] 5.12 Write stateful property test for the Queue invariant
    - **Property 16: Queue invariant across operation sequences** (proptest state-machine style over add/remove/reorder/shuffle sequences)
    - **Validates: Requirements 5.9**
  - [x] 5.13 Write property test for transport transitions
    - **Property 8: Transport transitions over repeat modes**
    - **Validates: Requirements 4.3, 4.4, 4.5, 4.8**
  - [x] 5.14 Write unit test for the 10,000-track boundary
    - Queue at exactly 10,000 tracks rejects further adds with state unchanged
    - _Requirements: 5.12_

- [x] 6. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Implement seek/volume clamping and the Domain_Object mapper
  - [x] 7.1 Implement seek and volume clamping
    - Clamp seek to 0..duration and treat a forward seek reaching the duration as the track finishing
    - Clamp requested volume to 0..100
    - _Requirements: 4.6, 4.7_
  - [x] 7.2 Write property test for seek clamping
    - **Property 6: Seek clamping**
    - **Validates: Requirements 4.6**
  - [x] 7.3 Write property test for volume clamping
    - **Property 7: Volume clamping**
    - **Validates: Requirements 4.7**
  - [x] 7.4 Implement Domain_Object mapping from API responses
    - Map each 2xx resource with id and name to a Domain_Object, preserving present optional attributes
    - Substitute empty string for missing text and no value for missing duration; drop resources missing id or name and log them
    - _Requirements: 12.1, 12.2, 12.3_
    - _Design: API_Client, Domain_Object_
  - [x] 7.5 Write property test for mapping preserving present attributes
    - **Property 25: Domain_Object mapping preserves present attributes**
    - **Validates: Requirements 12.1, 12.2**
  - [x] 7.6 Write property test for dropping malformed resources
    - **Property 26: Malformed resources are dropped, the rest returned**
    - **Validates: Requirements 12.3**

- [x] 8. Implement the Token_Store and Auth_Manager
  - [x] 8.1 Implement the Token_Store with file (0600) and keyring backends
    - Persist Developer_Token and Music_User_Token for the current Linux user with mode 0600, plus an optional secret-service keyring backend
    - Exclude token values from logs, stdout, stderr, and TUI error text
    - On save failure surface a TUI error while keeping the in-memory token for the session
    - _Requirements: 1.5, 1.13, 1.15_
    - _Design: Token_Store_
  - [x] 8.2 Implement Developer_Token fetch and JWT validity decision
    - Fetch the Developer_Token from the music.apple.com web player assets with a 10-second timeout and cache it
    - Accept a cached token only if it decodes as a JWT with an `exp` at least 60 seconds ahead; otherwise discard and refetch
    - On fetch failure show the web-player error, keep the Music_User_Token, and offer retry
    - _Requirements: 1.7, 1.8, 1.10_
    - _Design: Auth_Manager_
  - [x] 8.3 Implement Music_User_Token lifecycle and 401/403 refresh-and-resend
    - Attach a stored non-empty Music_User_Token to requests without opening sign-in
    - On catalog 401/403 refresh the Developer_Token and resend exactly once; on second failure show the error and stop
    - On a Music_User_Token 401/403 still failing after refresh+resend, delete the token, show "session expired", trigger re-sign-in
    - Delete the Music_User_Token on sign-out while keeping the cached Developer_Token
    - _Requirements: 1.6, 1.9, 1.11, 1.12, 3.9_
    - _Design: Auth_Manager_
  - [x] 8.4 Write property test for Developer_Token validity decision
    - **Property 1: Developer_Token validity decision**
    - **Validates: Requirements 1.8**

- [x] 9. Implement the API_Client (search, library, pagination, error handling)
  - [x] 9.1 Implement catalog search with query validation and section caps
    - Request up to 25 songs/albums/artists/playlists for the Storefront when the trimmed query is 1-200 chars; reject otherwise with a retained query and message
    - Apply the latest-query-wins rule for overlapping in-flight searches
    - _Requirements: 2.1, 2.2, 2.6_
    - _Design: API_Client_
  - [x] 9.2 Implement search pagination and next-page error handling
    - Request the next 25-item page when the selection reaches a section's last item with more available; append without moving selection
    - On next-page failure keep existing results, show a load-more error, and retry on the next end-of-list navigation
    - _Requirements: 2.10, 2.11_
    - _Design: API_Client_
  - [x] 9.3 Implement library browsing and pagination
    - Retrieve library sections (100/page) and album/playlist tracks (100/page) in stored order; re-fetch on refresh and reset selection
    - On library request failure or timeout keep prior items and leave Queue/playback unchanged; stop sections and prompt re-sign-in on token rejection
    - _Requirements: 3.1, 3.2, 3.3, 3.6, 3.7, 3.9_
    - _Design: API_Client_
  - [x] 9.4 Implement API transport error handling (timeout, 429, 5xx/4xx, retry)
    - Enforce the 10-second timeout with retry; back off on 429 per Retry-After (default 1s, cap 60s, max 3 attempts); show status/request for 5xx and other 4xx; resend once per retry selection
    - _Requirements: 2.5, 12.4, 12.5, 12.6, 12.7, 12.8_
    - _Design: API_Client, Error Handling_
  - [x] 9.5 Write property test for rate-limit backoff schedule
    - **Property 27: Rate-limit backoff schedule**
    - **Validates: Requirements 12.6**

- [x] 10. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 11. Implement the IPC layer (JSON-RPC over Unix domain socket)
  - [x] 11.1 Implement the Rust IPC client with length-prefixed JSON-RPC framing
    - Connect to the Unix socket at the XDG runtime path (temp-dir fallback); frame messages with a 4-byte big-endian length prefix
    - Encode the request methods (load/play/pause/seek/setVolume/openSignIn/closeSignIn/shutdown) and decode notifications (stateChanged/trackFinished/playbackError/trackUnavailable/signInResult/signInError)
    - _Requirements: 4.2, 4.9_
    - _Design: IPC Client / Server, IPC Protocol_
  - [x] 11.2 Implement the Electron IPC server side
    - Accept the socket connection, decode framed JSON-RPC requests, dispatch to player/sign-in handlers, and emit notifications
    - _Requirements: 4.2_
    - _Design: Playback_Engine, IPC Protocol_
  - [x] 11.3 Write integration test for IPC protocol conformance
    - Verify request/notification schema between the Rust client and a stub Playback_Engine
    - _Requirements: 4.2_
    - _Design: Testing Strategy (Integration and Smoke Tests)_

- [x] 12. Implement the Electron Playback_Engine and Sign_In_Window
  - [x] 12.1 Implement MusicKit JS playback in the hidden window
    - Load and play tracks, authorize with the Music_User_Token, request the DRM license, and report position at least once per second
    - Emit stateChanged/trackFinished/playbackError(network|drm)/trackUnavailable notifications
    - _Requirements: 4.1, 4.11, 4.12, 4.13_
    - _Design: Playback_Engine, Playback-Start Sequence_
  - [x] 12.2 Implement the Sign_In_Window and Music_User_Token capture
    - Open the MusicKit sign-in web view, capture a non-empty Music_User_Token, and report signInResult (ok/cancelled) or signInError(reason)
    - Terminate all sign-in processes when the window closes while keeping the obtained token
    - _Requirements: 1.3, 1.4, 1.14, 10.8_
    - _Design: Sign-In Sequence, Playback_Engine_
  - [x] 12.3 Implement parent-loss detection and self-termination
    - Detect loss of the parent TUI process and terminate the engine and its child processes within 5 seconds
    - _Requirements: 10.7_
    - _Design: Playback_Engine, Error Handling_
  - [x] 12.4 Write integration/smoke tests for playback and sign-in orchestration
    - Playback start latency, stream/DRM failure handling, and engine restart/reconnect at last position (mock MusicKit where possible)
    - Sign_In_Window open/cancel/complete and token capture (mock the Apple-hosted page)
    - _Requirements: 4.1, 4.10, 4.13, 4.14, 1.1, 1.2, 1.3, 1.4, 1.14_
    - _Design: Testing Strategy (Integration and Smoke Tests)_

- [x] 13. Implement the TUI render/input loop, views, and Now_Playing_Bar wiring
  - [x] 13.1 Implement the terminal render/input loop and lifecycle
    - Run the ratatui render loop and crossterm input loop; render the main view layout within 2s; handle resize redraw within 100ms and key latency under 100ms
    - Save Session_State, stop playback, terminate the Playback_Engine, and restore terminal mode on quit/SIGINT/SIGTERM/SIGHUP; restore terminal and log on unrecoverable error with tokens excluded
    - _Requirements: 10.1, 10.5, 12.9, 12.10, 12.11, 7.6, 7.9_
    - _Design: TUI (view + input loop)_
  - [x] 13.2 Implement search and library views with empty states
    - Render grouped search sections and selection actions (song replaces Queue and plays; album/playlist/artist drill-down); render library sections, drill-down, and loading indicators
    - Selecting a track in an album/playlist replaces the Queue in stored order and starts at the selected position
    - Show empty-state and zero-result messages; omit zero-result sections
    - _Requirements: 2.3, 2.4, 2.7, 2.8, 2.9, 3.4, 3.5, 3.8_
    - _Design: TUI (view + input loop)_
  - [x] 13.3 Implement the Queue view and the live Now_Playing_Bar
    - Render the Queue with 1-based positions and the current track highlighted; show the empty-Queue message; wire queue edits to the Queue operations
    - Drive the Now_Playing_Bar from Playback_State so elapsed time updates at least once per second and field changes show within 100ms; keep playback across view switches
    - _Requirements: 5.1, 5.10, 6.2, 6.6, 4.9_
    - _Design: Now_Playing_Bar, TUI (view + input loop)_
  - [x] 13.4 Write unit tests for empty-state and zero-result messages
    - Assert the specific messages for zero search results, empty library sections, and empty Queue
    - _Requirements: 2.4, 3.8, 5.10_

- [x] 14. Implement keybinding resolution and the help overlay
  - [x] 14.1 Implement the Keybinding resolver with defaults, overrides, and conflict/unknown warnings
    - Build the action map from defaults overlaid with config bindings; provide list-navigation defaults with edge clamping; keep printable keys as text input in fields with Esc to exit
    - Warn at startup on duplicate bindings (including conflicts with unlisted defaults) and on unknown action/key names, falling back to defaults
    - _Requirements: 7.1, 7.2, 7.4, 7.5, 7.7, 7.8_
    - _Design: Keybinding Resolver_
  - [x] 14.2 Implement the help overlay
    - Display active view bindings and global bindings with action names and custom bindings; scroll with list-navigation keys; close and restore the prior view on Esc/`?`
    - _Requirements: 7.3_
    - _Design: Keybinding Resolver_
  - [x] 14.3 Write unit tests for keybinding warnings and help overlay
    - Conflict and unknown-action/unknown-key warnings from config fixtures; help overlay contents for a representative view
    - _Requirements: 7.5, 7.8, 7.3_

- [x] 15. Implement the MPRIS_Adapter
  - [x] 15.1 Implement MPRIS registration, command forwarding, and metadata publishing
    - When enabled, register on the D-Bus session bus within 2s; forward Play/Pause/PlayPause/Next/Previous/Seek to the Playback_Engine so state matches the keybinding action within 500ms
    - Publish track metadata and status within 1s of changes and position after seeks; handle no-current-track, seek-before-0 clamp, and seek-past-end advance; release the bus name on exit; do not register when disabled
    - Degrade gracefully when D-Bus is unavailable, logging a warning and continuing non-MPRIS functions without retry
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5, 11.6, 11.7_
    - _Design: MPRIS_Adapter_
  - [x] 15.2 Write integration tests for MPRIS against a session D-Bus
    - Registration, command forwarding, metadata publishing, and bus-name release
    - _Requirements: 11.1, 11.2, 11.3, 11.7_
    - _Design: Testing Strategy (Integration and Smoke Tests)_

- [x] 16. Wire startup, preview-only fallback, and full application integration
  - [x] 16.1 Wire the end-to-end startup and shutdown flow
    - On start: parse config, restore session, resolve keybindings, read tokens, spawn/connect the Playback_Engine, open the Sign_In_Window when no token, and show the main view within the required deadlines
    - Connect views, Queue, API_Client, Auth_Manager, Now_Playing_Bar, IPC client, and MPRIS into the running loop
    - _Requirements: 1.1, 1.2, 10.1_
    - _Design: Architecture, Sign-In Sequence_
  - [x] 16.2 Implement preview-only fallback when the Playback_Engine is unavailable
    - When the engine cannot start or Widevine is unavailable, degrade to 30-second unencrypted preview playback via a lightweight native path and label tracks as previews
    - Surface the engine start-failure/lost-connection error with a restart action keeping Queue and current track; load current track at last position paused on reconnect
    - _Requirements: 4.10, 4.14_
    - _Design: Technology Stack (preview-only decision), Error Handling_
  - [x] 16.3 Write integration/smoke tests for token fetch, resize, and offline input latency
    - Developer_Token fetch from live web-player assets and the 401/403 refresh-and-resend flow against a mock API; Token_Store 0600/keyring permissions smoke check
    - Terminal resize redraw and input latency under an unreachable network
    - _Requirements: 1.7, 1.9, 1.5, 12.9, 12.10_
    - _Design: Testing Strategy (Integration and Smoke Tests)_
  - [x] 16.4 Write resource-footprint profiling tests
    - Measure idle/playback memory and CPU budgets and process-teardown guarantees as a periodic profiling job
    - _Requirements: 10.2, 10.3, 10.4, 10.5, 10.6, 10.7, 10.8_
    - _Design: Testing Strategy (Integration and Smoke Tests)_

- [x] 17. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional test sub-tasks and can be skipped for a faster MVP; core implementation tasks are never optional.
- Each of the 27 correctness properties maps to exactly one property-based test, tagged `// Feature: apple-music-tui, Property {number}: {property_text}`, running at least 100 iterations with `proptest`.
- DRM playback, the embedded web-view sign-in, D-Bus/MPRIS, external token fetching, and resource budgets are covered by integration/smoke/profiling tests rather than property tests, per the design's Testing Strategy.
- Round-trip properties (5, 14, 20, 22) and the stateful Queue invariant (16) are prioritized.
- Checkpoints ensure incremental validation; each task references specific requirements and design components for traceability.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.3"] },
    { "id": 1, "tasks": ["1.2"] },
    { "id": 2, "tasks": ["2.1", "2.3", "3.1", "4.1", "5.1", "7.1", "7.4", "8.1", "11.2"] },
    { "id": 3, "tasks": ["2.2", "2.4", "3.2", "3.6", "4.2", "5.2", "5.3", "7.2", "7.3", "7.5", "7.6", "8.2", "11.1"] },
    { "id": 4, "tasks": ["3.3", "3.4", "3.5", "4.3", "4.4", "4.5", "5.4", "5.5", "5.6", "5.7", "5.8", "5.9", "5.10", "5.11", "5.12", "5.13", "5.14", "8.3", "9.1", "12.1", "12.2", "12.3"] },
    { "id": 5, "tasks": ["8.4", "9.2", "9.3", "9.4", "11.3", "12.4"] },
    { "id": 6, "tasks": ["9.5", "13.1"] },
    { "id": 7, "tasks": ["13.2", "13.3", "14.1"] },
    { "id": 8, "tasks": ["13.4", "14.2", "14.3", "15.1", "16.1"] },
    { "id": 9, "tasks": ["15.2", "16.2"] },
    { "id": 10, "tasks": ["16.3", "16.4"] }
  ]
}
```
