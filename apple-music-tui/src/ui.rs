//! TUI render/input loop and lifecycle (task 13.1).
//!
//! This module owns the terminal: it enters raw mode and the alternate screen,
//! runs the ratatui render loop and the crossterm input loop, redraws on resize,
//! and tears everything down cleanly on quit, on a termination signal
//! (SIGINT/SIGTERM/SIGHUP), or on an unrecoverable error.
//!
//! What this task provides (design "TUI (view + input loop)"):
//! - [`TerminalGuard`]: an RAII guard that restores the terminal to its
//!   pre-start mode (leaves the alternate screen, disables raw mode, shows the
//!   cursor) on drop, so even a panic or an early return restores the terminal
//!   (Requirements 7.6, 12.11).
//! - [`run`]: the async event loop that draws the main-view layout skeleton
//!   (navigation + playback-status areas, Requirement 10.1), handles resize
//!   redraws (Requirement 12.10), processes key presses with low latency
//!   (Requirements 6.6, 12.9), and drives the quit/signal lifecycle.
//! - The quit lifecycle (Requirement 7.6): save Session_State, stop playback and
//!   terminate the Playback_Engine over IPC, restore the terminal, exit 0. On a
//!   Session_State save failure the error is logged to the app log and the
//!   caller exits non-zero (Requirement 7.9).
//! - [`log_unrecoverable`]: writes an error to the app log with token values
//!   excluded, for the unrecoverable-error path (Requirement 12.11).
//!
//! The concrete search/library/queue views are later tasks (13.2/13.3); the
//! layout skeleton and the live Now_Playing_Bar line are drawn here so the main
//! view is visible within the startup deadline. Action dispatch is resolved
//! through the [`ActionResolver`] seam so task 14.1 can plug in the real
//! Keybinding resolver without this module changing.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::api::{
    LibraryList, LibrarySection, SearchPagination, SearchResults, SearchSection,
};
use crate::domain::{Domain_Object, PlaybackStatus};
use crate::ipc::{IpcSender, RequestMethod};
use crate::keybindings::{self, Resolution, TextInputField, TextInputOutcome};
use crate::now_playing::{self, format_duration, Playback_State};
use crate::preview::{self, EngineErrorState};
use crate::queue::{self, Queue, QueueFullError, QueueTrack, RemoveCurrentOutcome};
use crate::session::{self, SerializedQueue, SerializedTrack, Session_State};

/// The application log file name under the state directory (Requirements 7.9,
/// 12.11, 12.3).
const LOG_FILE: &str = "app.log";

/// Polling cadence for the Now_Playing_Bar's elapsed-time tick. The render loop
/// wakes at least this often even without input so the bar advances at least
/// once per second (Requirement 6.2) and resize/key handling stays responsive.
const TICK: Duration = Duration::from_millis(250);

/// The main views the TUI can display. Only the skeleton is rendered in this
/// task; the detailed per-view content is tasks 13.2/13.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Catalog search (task 13.2).
    Search,
    /// Library browsing (task 13.2).
    Library,
    /// The current Queue (task 13.3).
    Queue,
}

impl View {
    /// The short title shown in the navigation bar for this view.
    fn title(self) -> &'static str {
        match self {
            View::Search => "Search",
            View::Library => "Library",
            View::Queue => "Queue",
        }
    }

    /// The ordered list of views for navigation-bar rendering and tab cycling.
    const ALL: [View; 3] = [View::Search, View::Library, View::Queue];
}

/// A resolved action produced from a key event.
///
/// This is the full set of actions the render/input loop dispatches. The
/// Keybinding resolver ([`KeybindingActionResolver`]) maps every
/// [`keybindings::Action`] onto one of these so the loop can act on transport,
/// navigation, Queue editing, help, and sign-out — not just the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quit the application (default `q`): save session, stop playback,
    /// terminate the engine, restore the terminal, exit 0 (Requirement 7.6).
    Quit,
    /// Switch to a specific view.
    Switch(View),
    /// Toggle the help overlay (Requirement 7.3).
    ToggleHelp,
    /// Sign out of the current Apple ID session (Requirement 1.12).
    SignOut,
    /// Move the selection down one item in the active view (Requirement 7.2).
    NavDown,
    /// Move the selection up one item in the active view (Requirement 7.2).
    NavUp,
    /// Open/activate the selected item in the active view (Requirement 7.2).
    NavOpen,
    /// Return from a drilled-in list to the previous level (Requirement 7.2).
    NavBack,
    /// Toggle play/pause (Requirement 4.2).
    PlayPause,
    /// Skip to the next track (Requirement 4.3).
    NextTrack,
    /// Skip to the previous track (Requirements 4.4, 4.5).
    PreviousTrack,
    /// Seek forward by the configured step (Requirement 4.6).
    SeekForward,
    /// Seek backward by the configured step (Requirement 4.6).
    SeekBackward,
    /// Raise the volume (Requirement 4.7).
    VolumeUp,
    /// Lower the volume (Requirement 4.7).
    VolumeDown,
    /// Toggle shuffle mode (Requirements 5.6, 5.7).
    ToggleShuffle,
    /// Cycle the repeat mode off → all → one (Requirement 5.8).
    CycleRepeat,
    /// Refresh the current Library section (Requirement 3.6).
    RefreshLibrary,
    /// Insert the selected track directly after the current track (Req 5.2).
    PlayNext,
    /// Append the selected track to the end of the Queue (Requirement 5.3).
    AddToQueue,
    /// Remove the selected track from the Queue (Requirements 5.4, 5.11).
    RemoveFromQueue,
    /// Move the selected Queue track up one position (Requirement 5.5).
    MoveUp,
    /// Move the selected Queue track down one position (Requirement 5.5).
    MoveDown,
    /// A key that resolves to no action in the current view; ignored.
    Ignored,
}

/// Resolves a key event to an [`Action`].
///
/// The loop depends only on this trait, so the minimal built-in resolver
/// ([`DefaultActionResolver`]) and the production Keybinding resolver
/// ([`KeybindingActionResolver`]) are interchangeable. [`resolve`] handles a
/// single key press; [`resolve_chord`] additionally threads a pending first key
/// so two-key sequences (e.g. the default `g l` / `g q` / `g o`) resolve.
pub trait ActionResolver {
    /// Map a single key event in the given view to an [`Action`].
    fn resolve(&self, view: View, key: KeyEvent) -> Action;

    /// Map a key press that may be part of a two-key chord to a [`ChordAction`].
    ///
    /// `pending` is the previously held first key of a potential chord, or
    /// `None` for a fresh press. The default implementation ignores chords and
    /// defers to [`resolve`], which is sufficient for the built-in resolver.
    fn resolve_chord(&self, view: View, pending: Option<KeyEvent>, key: KeyEvent) -> ChordAction {
        let _ = pending;
        ChordAction::Resolved(self.resolve(view, key))
    }
}

/// The outcome of resolving one key press through [`ActionResolver::resolve_chord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordAction {
    /// A binding fired (single key or a completed two-key chord).
    Resolved(Action),
    /// The key is the first key of a two-key binding; hold it and wait for the
    /// next press.
    Pending,
}

/// Minimal built-in resolver. Provides the default quit key (`q` and Ctrl-C),
/// number/Tab view switching, and the vi/arrow navigation keys so the loop and
/// its lifecycle can be exercised without the full Config_File resolver.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultActionResolver;

impl ActionResolver for DefaultActionResolver {
    fn resolve(&self, _view: View, key: KeyEvent) -> Action {
        // Ctrl-C maps to Quit so the quit lifecycle runs (save + teardown)
        // rather than the process being killed; the SIGINT path below is a
        // belt-and-suspenders backstop for a raw terminal signal.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('1') => Action::Switch(View::Search),
            KeyCode::Char('2') => Action::Switch(View::Library),
            KeyCode::Char('3') => Action::Switch(View::Queue),
            KeyCode::Tab => Action::Switch(View::Queue),
            KeyCode::Char('j') | KeyCode::Down => Action::NavDown,
            KeyCode::Char('k') | KeyCode::Up => Action::NavUp,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => Action::NavOpen,
            KeyCode::Char('h') | KeyCode::Left => Action::NavBack,
            _ => Action::Ignored,
        }
    }
}

/// The real [`ActionResolver`], backed by the resolved Keybinding map from the
/// Config_File ([`keybindings::resolve_keybindings`]).
///
/// It maps a key press through the resolved bindings to a
/// [`keybindings::Action`] and translates the full action set into this
/// module's [`Action`] enum, which the running loop dispatches: view switches,
/// transport, navigation, Queue editing, help, and sign-out. Two-key sequences
/// are resolved through [`resolve_chord`].
///
/// Ctrl-C is honored as a quit regardless of the configured bindings so a quit
/// always runs the clean shutdown sequence even if the user rebinds `quit`.
pub struct KeybindingActionResolver {
    resolution: Resolution,
}

impl KeybindingActionResolver {
    /// Build a resolver from a resolved Keybinding map.
    pub fn new(resolution: Resolution) -> Self {
        KeybindingActionResolver { resolution }
    }

    /// The resolved Keybinding map, so callers (e.g. the help overlay) can read
    /// the active bindings and any resolution warnings.
    pub fn resolution(&self) -> &Resolution {
        &self.resolution
    }
}

/// Translate a resolved [`keybindings::Action`] into the loop's [`Action`].
fn translate_action(action: keybindings::Action) -> Action {
    match action {
        keybindings::Action::Quit => Action::Quit,
        keybindings::Action::Help => Action::ToggleHelp,
        keybindings::Action::Search => Action::Switch(View::Search),
        keybindings::Action::OpenLibrary => Action::Switch(View::Library),
        keybindings::Action::OpenQueue => Action::Switch(View::Queue),
        keybindings::Action::SignOut => Action::SignOut,
        keybindings::Action::NavDown => Action::NavDown,
        keybindings::Action::NavUp => Action::NavUp,
        keybindings::Action::NavOpen => Action::NavOpen,
        keybindings::Action::NavBack => Action::NavBack,
        keybindings::Action::PlayPause => Action::PlayPause,
        keybindings::Action::NextTrack => Action::NextTrack,
        keybindings::Action::PreviousTrack => Action::PreviousTrack,
        keybindings::Action::SeekForward => Action::SeekForward,
        keybindings::Action::SeekBackward => Action::SeekBackward,
        keybindings::Action::VolumeUp => Action::VolumeUp,
        keybindings::Action::VolumeDown => Action::VolumeDown,
        keybindings::Action::ToggleShuffle => Action::ToggleShuffle,
        keybindings::Action::CycleRepeat => Action::CycleRepeat,
        keybindings::Action::RefreshLibrary => Action::RefreshLibrary,
        keybindings::Action::PlayNext => Action::PlayNext,
        keybindings::Action::AddToQueue => Action::AddToQueue,
        keybindings::Action::RemoveFromQueue => Action::RemoveFromQueue,
        keybindings::Action::MoveUp => Action::MoveUp,
        keybindings::Action::MoveDown => Action::MoveDown,
    }
}

impl ActionResolver for KeybindingActionResolver {
    fn resolve(&self, view: View, key: KeyEvent) -> Action {
        // Ctrl-C always quits cleanly, independent of config, so the shutdown
        // sequence (save + teardown) runs rather than the process being killed.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match self.resolution.action_for_key(keybindings_view(view), key) {
            Some(action) => translate_action(action),
            None => Action::Ignored,
        }
    }

    fn resolve_chord(&self, view: View, pending: Option<KeyEvent>, key: KeyEvent) -> ChordAction {
        // Ctrl-C always quits cleanly, independent of config or any pending
        // chord prefix.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ChordAction::Resolved(Action::Quit);
        }
        match self
            .resolution
            .resolve_chord(keybindings_view(view), pending, key)
        {
            keybindings::ChordResolution::Action(action) => {
                ChordAction::Resolved(translate_action(action))
            }
            keybindings::ChordResolution::Pending => ChordAction::Pending,
            keybindings::ChordResolution::None => ChordAction::Resolved(Action::Ignored),
        }
    }
}

/// Map this module's render [`View`] to the Keybinding resolver's view scope so
/// view-scoped bindings (Library/Queue) resolve in the right context and the
/// Search view uses the global scope.
fn keybindings_view(view: View) -> keybindings::View {
    match view {
        // The Search view has no view-scoped bindings of its own; only global
        // and navigation bindings apply, so it resolves under the global scope.
        View::Search => keybindings::View::Global,
        View::Library => keybindings::View::Library,
        View::Queue => keybindings::View::Queue,
    }
}

/// RAII guard that owns the terminal setup and guarantees teardown.
///
/// On construction it enables raw mode and switches to the alternate screen,
/// snapshotting that it did so. On drop it reverses exactly what it set up —
/// leaving the alternate screen, disabling raw mode, and showing the cursor —
/// so the terminal returns to the mode it was in before the TUI started
/// (Requirements 7.6, 12.11). Because this runs in `Drop`, a normal return, an
/// error, a caught signal, or a panic all restore the terminal.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    /// Whether raw mode / the alternate screen were successfully entered and
    /// therefore need to be reversed on drop.
    active: bool,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen and build the ratatui terminal.
    ///
    /// On any failure the partial setup is reversed before returning the error,
    /// so a half-initialized terminal is never left behind.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            // Undo raw mode so a failed setup does not leave the terminal raw.
            let _ = disable_raw_mode();
            return Err(err);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let mut out = io::stdout();
                let _ = execute!(out, LeaveAlternateScreen, cursor::Show);
                let _ = disable_raw_mode();
                return Err(err);
            }
        };
        Ok(TerminalGuard {
            terminal,
            active: true,
        })
    }

    /// Mutable access to the underlying ratatui terminal for drawing.
    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    /// Restore the terminal to its pre-start mode now, before drop.
    ///
    /// Idempotent: after the first call the guard is inert and `Drop` does
    /// nothing. Returning the result lets the caller surface a restore failure;
    /// `Drop` calls this too so restoration still happens on panic or an early
    /// return.
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut stdout = io::stdout();
        // Best-effort: attempt every step even if an earlier one fails, so a
        // single failing call does not leave the terminal in raw mode.
        let leave = execute!(stdout, LeaveAlternateScreen, cursor::Show);
        let raw = disable_raw_mode();
        let _ = stdout.flush();
        leave.and(raw)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Restore on drop so a panic or early return still leaves the terminal
        // usable (Requirements 7.6, 12.11). Errors here cannot be propagated.
        let _ = self.restore();
    }
}

/// The outcome of the render/input loop, telling the caller how to exit.
///
/// The loop restores the terminal (via the [`TerminalGuard`]) before returning
/// in every case; the caller maps this to a process exit status.
#[derive(Debug)]
pub enum LoopOutcome {
    /// A clean quit (quit key or termination signal): Session_State was saved
    /// and the engine was asked to shut down. Exit 0 (Requirement 7.6).
    Quit,
    /// Quit was requested but saving Session_State failed. The error was logged
    /// to the app log (Requirement 7.9); the caller exits non-zero.
    QuitSaveFailed,
}

/// The application state the loop renders and mutates.
///
/// This is intentionally small for task 13.1: it carries the current view, the
/// Now_Playing_Bar's [`Playback_State`], and the session snapshot to persist on
/// quit. Later tasks attach the Queue, search/library results, and the
/// API/Auth wiring.
pub struct App {
    /// The currently focused main view.
    pub view: View,
    /// The authoritative mirror of the Playback_Engine's state, rendered by the
    /// Now_Playing_Bar (Requirement 6).
    pub playback: Playback_State,
    /// The session snapshot persisted on quit (Requirement 7.6). Later tasks
    /// keep this in sync with the live Queue; here it is saved as-is.
    pub session: Session_State,
    /// The Search view's state: results, pagination, and selection (task 13.2).
    pub search: SearchViewState,
    /// The Library view's state: section picker or a drilled-in list (task 13.2).
    pub library: LibraryViewState,
    /// The authoritative Queue the Queue view renders and edits (task 13.3,
    /// Requirement 5). The Now_Playing_Bar's current track is this Queue's
    /// current entry.
    pub queue: Queue,
    /// The Queue view's selection state (task 13.3).
    pub queue_view: QueueViewState,
    /// The Playback_Engine error surfaced with a restart action, when the engine
    /// failed to start or the connection was lost (Requirements 4.10, 4.14,
    /// task 16.2). `None` while the engine is healthy. While present the TUI
    /// shows the named failure and offers a restart; the Queue and current track
    /// are kept unchanged.
    pub engine_error: Option<EngineErrorState>,
    /// Whether the TUI is running in preview-only mode: the Playback_Engine is
    /// unavailable, so tracks play as 30-second previews and are labelled as
    /// such in the Now_Playing_Bar (Open Question 3, task 16.2).
    pub preview_mode: bool,
    /// Whether the help overlay is currently shown (Requirement 7.3). Toggled
    /// by the Help action.
    pub show_help: bool,
    /// The configured seek step in seconds, applied by Seek actions
    /// (Requirement 4.6).
    pub seek_step_seconds: u32,
    /// The stored Music_User_Token used to authorize engine `load` requests
    /// (Requirements 1.2, 1.6). `None` until a token is available; set at
    /// startup from the Auth_Manager and refreshed on a successful sign-in.
    pub music_user_token: Option<String>,
    /// The Developer_Token used to authorize MusicKit in the engine's player
    /// window for DRM playback. `None` until available; set at startup from the
    /// Auth_Manager. Without it the player's `MusicKit.configure` cannot
    /// authorize full-length streams.
    pub developer_token: Option<String>,
    /// Engine requests produced by key dispatch, drained and sent by the async
    /// run loop after each event. Keeping dispatch synchronous (it only
    /// enqueues here) makes [`handle_event`] pure and unit-testable while the
    /// loop owns the actual `await`ed IPC send.
    pub pending_requests: Vec<RequestMethod>,
    /// The first key of an in-flight two-key chord (e.g. the `g` of `g l`), held
    /// until the next key completes or cancels the chord.
    pub pending_chord: Option<KeyEvent>,
    /// A data fetch (catalog search or library page) produced by key dispatch,
    /// run by the async loop which owns the API client and tokens. Kept separate
    /// from [`App::pending_requests`] (engine transport) because a fetch needs
    /// the authenticated API path, not the engine socket. At most one is
    /// outstanding; a newer request replaces an unstarted older one.
    pub pending_fetch: Option<FetchRequest>,
}

/// A data fetch the async loop performs on the app's behalf (Requirements 2, 3).
///
/// Dispatch is synchronous, so it records the intent here; the loop runs the
/// actual authenticated HTTP request and feeds the result back into the view
/// state. This mirrors the [`App::pending_requests`] pattern used for engine
/// transport, keeping key handling pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchRequest {
    /// Run a catalog search for the given validated-at-submit query
    /// (Requirement 2.1). The result replaces the Search view's results.
    Search { query: String },
    /// Open a Library section's first page (Requirement 3.2).
    LibrarySection { section: LibrarySection },
    /// Open an album's or playlist's track list (Requirement 3.5).
    LibraryTracks { kind: LibrarySection, id: String },
}

/// The result of running a [`FetchRequest`], applied back into the view state by
/// the loop (Requirements 2.3, 2.5, 3.5, 3.7).
///
/// Each variant carries either the parsed state to install or a user-facing
/// (token-free) error message to show. The async loop builds these from the API
/// responses; applying them is pure view-state mutation in [`App::apply_fetch`].
pub enum FetchOutcome {
    /// A completed catalog search: either the pagination to display or an error
    /// message (invalid query / request failure, Requirements 2.2, 2.5).
    Search(Result<SearchPagination, String>),
    /// A completed library open: either the list to display (with whether it is
    /// a track list, so Enter plays vs. drills) or an error message.
    Library {
        result: Result<LibraryList, String>,
        is_track_list: bool,
    },
}

/// The default seek step (seconds) used when an app is built without an explicit
/// configuration (e.g. in tests). Matches the Config_File default (Requirement
/// 4.6).
const DEFAULT_SEEK_STEP_SECONDS: u32 = 10;

impl App {
    /// Create an app with the given restored session, starting on the Search
    /// view with the restored Queue loaded and nothing playing yet.
    pub fn new(session: Session_State) -> Self {
        // Rebuild the authoritative Queue from the restored session so the Queue
        // view and the Now_Playing_Bar reflect it immediately (Requirement 9.4).
        let queue = queue_from_session(&session);

        // Reflect the restored volume/shuffle/repeat and the current track in
        // the bar so the restored session is visible immediately, paused with no
        // audio (Requirements 9.4, 6.1).
        let mut playback = Playback_State {
            volume: session.volume,
            shuffle: session.shuffle,
            repeat: session.repeat,
            ..Playback_State::default()
        };
        reflect_current_track(&mut playback, &queue);

        // Select the current track in the Queue view, or the first track when
        // there is no current pointer but the Queue is non-empty.
        let queue_view = QueueViewState::for_queue(&queue);

        App {
            view: View::Search,
            playback,
            session,
            search: SearchViewState::new(),
            library: LibraryViewState::new(),
            queue,
            queue_view,
            engine_error: None,
            preview_mode: false,
            show_help: false,
            seek_step_seconds: DEFAULT_SEEK_STEP_SECONDS,
            music_user_token: None,
            developer_token: None,
            pending_requests: Vec::new(),
            pending_chord: None,
            pending_fetch: None,
        }
    }

    /// Apply startup configuration that the loop needs for dispatch: the seek
    /// step (Requirement 4.6), the stored Music_User_Token used to authorize
    /// engine `load` requests (Requirements 1.2, 1.6), and the Developer_Token
    /// the engine's player needs to authorize DRM playback.
    pub fn configure(
        &mut self,
        seek_step_seconds: u32,
        music_user_token: Option<String>,
        developer_token: Option<String>,
    ) {
        self.seek_step_seconds = seek_step_seconds;
        self.music_user_token = music_user_token;
        self.developer_token = developer_token;
    }

    /// Record a Music_User_Token captured from a successful sign-in so later
    /// `load` requests are authorized (Requirements 1.3, 1.6).
    pub fn set_music_user_token(&mut self, token: String) {
        self.music_user_token = Some(token);
    }

    /// Set the Developer_Token used to authorize MusicKit for DRM playback in
    /// the engine's player window.
    pub fn set_developer_token(&mut self, token: String) {
        self.developer_token = Some(token);
    }

    /// Enter preview-only mode after the Playback_Engine proved unavailable
    /// (Requirement 4.10, Open Question 3, task 16.2).
    ///
    /// Records the named failure with a restart action (keeping the Queue and
    /// current track untouched) and flags preview mode so the Now_Playing_Bar
    /// labels tracks as previews. The current track's last reported position is
    /// captured so a successful restart reloads it at that point, paused
    /// (Requirement 4.14).
    pub fn enter_preview_mode(&mut self, failure: preview::EngineFailure) {
        self.engine_error = Some(EngineErrorState::new(
            failure,
            self.playback.track_id.clone(),
            self.playback.position_ms,
        ));
        self.preview_mode = true;
    }

    /// Clear the engine-error / preview state after a successful restart and
    /// reconnect (Requirement 4.14). The caller is responsible for loading the
    /// current track at its last position (see
    /// [`EngineErrorState::reconnect_load`]).
    pub fn clear_engine_error(&mut self) {
        self.engine_error = None;
        self.preview_mode = false;
    }

    /// Switch the focused view without disturbing playback or the Queue
    /// (Requirement 4.9).
    ///
    /// View switching only changes what is drawn in the content area; the
    /// authoritative Queue and the mirrored [`Playback_State`] are untouched, so
    /// audio continues uninterrupted across Search/Library/Queue. The loop keeps
    /// the engine connection and the tick running regardless of the active view.
    pub fn switch_view(&mut self, view: View) {
        self.view = view;
    }

    /// Apply a resolved Queue edit to the authoritative Queue and re-sync the
    /// derived state (task 13.3, Requirements 5.2–5.5, 5.11, 5.12).
    ///
    /// This is the single seam through which the Queue view mutates the Queue:
    /// it performs the structural edit, keeps the Queue-view selection valid,
    /// reflects the (possibly new) current track into the Now_Playing_Bar so a
    /// field change shows promptly (Requirement 6.6), and mirrors the Queue into
    /// the session snapshot persisted on quit. It returns a [`QueueEditOutcome`]
    /// describing any playback consequence so task 16.1 can drive the
    /// Playback_Engine (e.g. start the new current track or stop playback after
    /// removing the current track, Requirement 5.11); until then the loop simply
    /// updates the mirrored state.
    pub fn apply_queue_edit(&mut self, edit: QueueEdit) -> QueueEditOutcome {
        let outcome = edit.apply(&mut self.queue);
        self.queue_view.clamp_to(&self.queue);
        reflect_current_track(&mut self.playback, &self.queue);
        sync_session_queue(&mut self.session, &self.queue);
        outcome
    }

    /// Enqueue an engine request to be sent by the run loop after this event.
    fn enqueue(&mut self, request: RequestMethod) {
        self.pending_requests.push(request);
    }

    /// Build a `load` request for the Queue's current track at `position_ms`,
    /// authorized with the stored Music_User_Token (Requirements 1.2, 1.6, 4.3).
    /// Returns `None` when there is no current track or no token yet, in which
    /// case no engine request is issued.
    fn load_current(&self, position_ms: u64) -> Option<RequestMethod> {
        let current = self.queue.current?;
        let track = self.queue.tracks.get(current)?;
        let token = self.music_user_token.clone()?;
        Some(RequestMethod::Load {
            track_id: track.track_id.clone(),
            position_ms,
            music_user_token: token,
            // The Developer_Token may not be loaded yet; an empty string is
            // sent in that case and the engine player reports a DRM/auth error
            // rather than silently failing.
            developer_token: self.developer_token.clone().unwrap_or_default(),
        })
    }

    /// Toggle play/pause against the mirrored state, enqueueing the matching
    /// engine request (Requirement 4.2). Does nothing when there is no current
    /// track.
    fn toggle_play_pause(&mut self) {
        if self.queue.current.is_none() {
            return;
        }
        match self.playback.status {
            PlaybackStatus::Playing => {
                self.playback.status = PlaybackStatus::Paused;
                self.enqueue(RequestMethod::Pause);
            }
            PlaybackStatus::Paused | PlaybackStatus::Stopped => {
                self.playback.status = PlaybackStatus::Playing;
                self.enqueue(RequestMethod::Play);
            }
        }
    }

    /// Apply a computed [`queue::Transition`] (next/previous): move the current
    /// pointer, reflect the new track, reset the position, and enqueue a `load`
    /// (plus `pause` when the transition parks paused) (Requirements 4.3–4.5).
    fn apply_transition(&mut self, transition: queue::Transition) {
        self.queue.current = Some(transition.index);
        self.queue_view.selected = Some(transition.index);
        self.playback.position_ms = 0;
        self.playback.status = transition.status;
        reflect_current_track(&mut self.playback, &self.queue);
        sync_session_queue(&mut self.session, &self.queue);

        if let Some(load) = self.load_current(0) {
            self.enqueue(load);
            // The engine loads every track paused, so the follow-up transport
            // command must match the intended status: Pause to stay paused, or
            // Play to actually start audio on a Playing transition.
            match transition.status {
                PlaybackStatus::Paused => self.enqueue(RequestMethod::Pause),
                PlaybackStatus::Playing => self.enqueue(RequestMethod::Play),
                PlaybackStatus::Stopped => {}
            }
        }
    }

    /// Skip to the next track under the current repeat mode (Requirement 4.3).
    fn next_track(&mut self) {
        if let Some(transition) = self.queue.next_transition(self.playback.repeat) {
            self.apply_transition(transition);
        }
    }

    /// Skip to the previous track, honoring the restart threshold (Reqs 4.4/4.5).
    fn previous_track(&mut self) {
        if let Some(transition) = self.queue.previous_transition(self.playback.position_ms) {
            self.apply_transition(transition);
        }
    }

    /// Seek by the configured step in `direction`, updating the mirrored
    /// position and enqueueing a `seek` — or advancing like a track finish when
    /// a forward seek runs off the end (Requirement 4.6 / 4.8).
    fn seek(&mut self, direction: now_playing::SeekDirection) {
        if self.queue.current.is_none() {
            return;
        }
        match now_playing::seek(
            self.playback.position_ms,
            self.playback.duration_ms,
            self.seek_step_seconds,
            direction,
        ) {
            now_playing::SeekOutcome::Position(position_ms) => {
                self.playback.position_ms = position_ms;
                self.enqueue(RequestMethod::Seek { position_ms });
            }
            now_playing::SeekOutcome::TrackFinished => {
                if let Some(transition) =
                    self.queue.track_finished_transition(self.playback.repeat)
                {
                    self.apply_transition(transition);
                }
            }
        }
    }

    /// Adjust the volume by `delta`, clamping to 0..=100 and enqueueing a
    /// `setVolume` (Requirement 4.7).
    fn adjust_volume(&mut self, delta: i64) {
        let level = now_playing::clamp_volume(self.playback.volume as i64 + delta);
        self.playback.volume = level;
        self.session.volume = level;
        self.enqueue(RequestMethod::SetVolume { level });
    }

    /// Toggle shuffle mode on/off, restoring order when turning it off
    /// (Requirements 5.6, 5.7). Reflects the new flag into the session.
    fn toggle_shuffle(&mut self) {
        if self.playback.shuffle {
            self.queue.unshuffle();
            self.playback.shuffle = false;
        } else {
            self.queue.shuffle();
            self.playback.shuffle = true;
        }
        self.session.shuffle = self.playback.shuffle;
        self.queue_view.clamp_to(&self.queue);
        reflect_current_track(&mut self.playback, &self.queue);
        sync_session_queue(&mut self.session, &self.queue);
    }

    /// Cycle the repeat mode off → all → one (Requirement 5.8).
    fn cycle_repeat(&mut self) {
        self.playback.repeat = queue::cycle_repeat(self.playback.repeat);
        self.session.repeat = self.playback.repeat;
    }

    /// Apply a Queue-view edit (remove/move) and, when it changes the current
    /// track, drive the engine: load+play the new current track or stop on an
    /// emptied Queue (Requirements 5.4, 5.5, 5.11).
    fn dispatch_queue_edit(&mut self, edit: QueueEdit) {
        match self.apply_queue_edit(edit) {
            QueueEditOutcome::CurrentAdvanced { .. } => {
                // The pointer advanced to a new current track. Load it at the
                // start; keep playing only if playback was active.
                let was_playing = self.playback.status == PlaybackStatus::Playing;
                self.playback.position_ms = 0;
                if let Some(load) = self.load_current(0) {
                    self.enqueue(load);
                    if !was_playing {
                        self.enqueue(RequestMethod::Pause);
                    }
                }
            }
            QueueEditOutcome::Emptied => {
                self.playback.status = PlaybackStatus::Stopped;
                self.playback.position_ms = 0;
                self.enqueue(RequestMethod::Pause);
            }
            QueueEditOutcome::Edited
            | QueueEditOutcome::Rejected
            | QueueEditOutcome::NoOp => {}
        }
    }

    /// Replace the Queue with `queue` and start playback at its current track
    /// (Requirements 2.7, 3.5). Reflects the new current track into the bar and
    /// enqueues a `load` + `play` when a token is available.
    fn play_queue(&mut self, queue: Queue) {
        self.queue = queue;
        self.queue_view = QueueViewState::for_queue(&self.queue);
        self.playback.position_ms = 0;
        self.playback.status = PlaybackStatus::Playing;
        reflect_current_track(&mut self.playback, &self.queue);
        sync_session_queue(&mut self.session, &self.queue);
        if let Some(load) = self.load_current(0) {
            self.enqueue(load);
            // The engine loads a track paused; an explicit Play is required to
            // start audio (and to drive the first DRM license request). Without
            // this the UI shows Playing but the engine never starts the stream.
            self.enqueue(RequestMethod::Play);
        }
    }

    /// The [`QueueTrack`] for the selection in the Search view, for the
    /// play-next / add-to-queue edits (Requirements 5.2, 5.3). Only a selected
    /// Song yields a track; other resource types (which would need a drill-down
    /// fetch) and the other views yield `None`.
    fn selected_track_for_queue(&self) -> Option<QueueTrack> {
        let object = match self.view {
            View::Search => self.search.selected_object()?,
            // Library drill-down selections and the Queue view's own rows are
            // out of scope for play-next / add-to-queue here.
            View::Library | View::Queue => return None,
        };
        match object {
            Domain_Object::Song {
                id,
                name,
                artist_name,
                duration_ms,
                ..
            } => Some(QueueTrack {
                track_id: id.clone(),
                title: name.clone(),
                artist: artist_name.clone(),
                duration_ms: *duration_ms,
            }),
            _ => None,
        }
    }

    /// Apply a completed [`FetchOutcome`] to the view state (Requirements 2.3,
    /// 2.5, 3.5, 3.7). Pure view-state mutation; the async loop owns the fetch.
    pub fn apply_fetch(&mut self, outcome: FetchOutcome) {
        match outcome {
            FetchOutcome::Search(Ok(pagination)) => self.search.set_results(pagination),
            FetchOutcome::Search(Err(message)) => self.search.set_message(message),
            FetchOutcome::Library {
                result: Ok(list),
                is_track_list,
            } => {
                self.library.message = None;
                self.library.open_list(list, is_track_list);
            }
            FetchOutcome::Library {
                result: Err(message),
                ..
            } => {
                // Surface the failure in the Library view and log it; the view's
                // current contents are left in place (Requirement 3.7).
                log_app_error(&message);
                self.library.message = Some(message);
            }
        }
    }
}
/// arrives (Requirements 10.1, 12.9, 12.10, 7.6, 7.9).
///
/// The loop:
/// - draws the main-view layout skeleton immediately so it is visible within
///   the startup deadline (Requirement 10.1);
/// - redraws on a terminal resize within the input cycle (Requirement 12.10);
/// - resolves key events to actions through `resolver` and redraws, keeping
///   per-key latency low (Requirements 6.6, 12.9);
/// - on a quit action or a SIGINT/SIGTERM/SIGHUP signal, runs the shutdown
///   sequence: save Session_State, stop playback and terminate the engine over
///   `engine`, then return. The [`TerminalGuard`] restores the terminal on
///   return (Requirement 7.6). A Session_State save failure is logged and
///   surfaced as [`LoopOutcome::QuitSaveFailed`] (Requirement 7.9).
///
/// `engine` is optional so the loop runs (and the lifecycle is exercised) even
/// when the Playback_Engine is not connected (e.g. preview-only startup, task
/// 16.2); when present, the shutdown sequence sends `pause` then `shutdown`.
///
/// `restart_engine` is invoked when the user presses the restart key while the
/// engine-error banner is showing (Requirement 4.10, task 16.2). It returns
/// whether the restart (respawn + reconnect) succeeded; on success the loop
/// clears the error/preview state and the caller has reloaded the current track
/// at its last position, paused (Requirement 4.14). The banner only reacts to
/// the restart key while [`App::engine_error`] is set, so it never interferes
/// with normal input.
/// `engine_watch` is polled on each tick and reports a [`preview::EngineFailure`]
/// the moment the Playback_Engine is detected unavailable while the loop runs
/// (e.g. the notification pump saw the connection close, Requirement 4.10
/// "lost connection"). On the first such report the loop enters preview-only
/// mode (banner + preview labelling), keeping the Queue and current track
/// (task 16.2). It is only consulted while no engine error is already showing.
pub async fn run<F, Fut, W, T, Q, QFut>(
    guard: &mut TerminalGuard,
    app: &mut App,
    resolver: &dyn ActionResolver,
    engine: Option<&IpcSender>,
    mut restart_engine: F,
    mut engine_watch: W,
    mut token_watch: T,
    mut fetch: Q,
) -> io::Result<LoopOutcome>
where
    F: FnMut(EngineErrorState) -> Fut,
    Fut: std::future::Future<Output = bool>,
    W: FnMut() -> Option<preview::EngineFailure>,
    T: FnMut() -> Option<String>,
    Q: FnMut(FetchRequest) -> QFut,
    QFut: std::future::Future<Output = FetchOutcome>,
{
    let mut events = EventStream::new();
    let mut signals = SignalListener::new()?;
    let mut ticker = tokio::time::interval(TICK);

    // Initial draw so the main view is visible before the first event
    // (Requirement 10.1).
    draw(guard, app)?;

    loop {
        tokio::select! {
            // Terminal input (keys, resize). Low-latency path for Requirements
            // 12.9 and 12.10.
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        match handle_event(event, app, resolver) {
                            EventResult::Quit => {
                                return shutdown(guard, app, engine).await;
                            }
                            // The user asked to restart the Playback_Engine from
                            // the error banner (Requirement 4.10). Keep the
                            // Queue and current track; on a successful reconnect
                            // clear the error/preview state (Requirement 4.14).
                            EventResult::RestartEngine => {
                                if let Some(error) = app.engine_error.clone() {
                                    if restart_engine(error).await {
                                        app.clear_engine_error();
                                    }
                                }
                            }
                            EventResult::Continue => {}
                        }
                        // Send any engine requests the dispatch enqueued
                        // (transport/seek/volume/load). Draining here keeps
                        // `handle_event` synchronous while the loop owns the
                        // awaited IPC send; a send failure is best-effort and
                        // must not break the input loop.
                        send_pending_requests(app, engine).await;
                        // Run a data fetch (search / library open) the dispatch
                        // requested, then apply the result to the view state.
                        // Drawing once before awaiting the fetch shows the
                        // in-progress message (e.g. "searching…") immediately.
                        if let Some(request) = app.pending_fetch.take() {
                            draw(guard, app)?;
                            let outcome = fetch(request).await;
                            app.apply_fetch(outcome);
                        }
                        draw(guard, app)?;
                    }
                    // Input stream error or EOF (e.g. stdin closed): treat as a
                    // request to quit cleanly so we still save and tear down.
                    Some(Err(_)) | None => {
                        return shutdown(guard, app, engine).await;
                    }
                }
            }
            // A termination signal: exit cleanly like the quit key
            // (Requirements 7.6, 10.5).
            _ = signals.recv() => {
                return shutdown(guard, app, engine).await;
            }
            // Periodic tick: advance the elapsed-time display without input so
            // the Now_Playing_Bar updates at least once per second
            // (Requirement 6.2). Also poll the engine watch so a lost
            // connection detected mid-session degrades to preview-only mode
            // (Requirement 4.10, task 16.2).
            _ = ticker.tick() => {
                if app.engine_error.is_none() {
                    if let Some(failure) = engine_watch() {
                        app.enter_preview_mode(failure);
                    }
                }
                // Pick up a Music_User_Token captured by a mid-session sign-in
                // so post-login transport actions can authorize `load`
                // (Requirements 1.3, 1.6).
                if let Some(token) = token_watch() {
                    app.set_music_user_token(token);
                }
                draw(guard, app)?;
            }
        }
    }
}

/// The result of handling a single terminal event.
#[derive(Debug, PartialEq, Eq)]
enum EventResult {
    /// Keep looping.
    Continue,
    /// A quit action was triggered; run the shutdown sequence.
    Quit,
    /// The user pressed the restart key while the engine-error banner was
    /// showing (Requirement 4.10, task 16.2); attempt a Playback_Engine
    /// restart.
    RestartEngine,
}

/// The key that triggers a Playback_Engine restart from the error banner
/// (Requirement 4.10, task 16.2). It is only meaningful while
/// [`App::engine_error`] is set, so it never shadows a normal binding.
const RESTART_KEY: char = 'r';

/// Apply a terminal event to the app, returning whether the loop should quit.
///
/// Resize events need no explicit handling beyond a redraw: ratatui queries the
/// backend size on each `draw`, so the next draw fits the new dimensions
/// (Requirement 12.10). Key presses are resolved to actions; only key *press*
/// events are acted on so a key is not handled twice on terminals that also
/// report releases.
///
/// Dispatch is synchronous and pure with respect to I/O: it mutates the app's
/// view state / Queue / mirrored playback and pushes any engine requests onto
/// [`App::pending_requests`], which the async loop drains and sends. Two-key
/// sequences (e.g. `g l`) are tracked via [`App::pending_chord`].
fn handle_event(event: Event, app: &mut App, resolver: &dyn ActionResolver) -> EventResult {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // While the engine-error banner is showing, the restart key triggers
            // a Playback_Engine restart (Requirement 4.10, task 16.2). It is
            // handled before the normal resolver so the offered action is always
            // reachable; it does nothing when no error is showing.
            if app.engine_error.is_some()
                && key.code == KeyCode::Char(RESTART_KEY)
                && key.modifiers.is_empty()
            {
                return EventResult::RestartEngine;
            }

            // When the Search query field has focus, typed keys are text, not
            // bindings (Requirement 2.1, 7.7). Enter submits the query and Esc
            // leaves the field; everything else edits the text. This is checked
            // before binding resolution so a letter like `n` types a character
            // instead of triggering NextTrack.
            if app.view == View::Search && app.search.query_focused {
                return handle_search_input(key, app);
            }

            // Resolve the press, completing or opening a two-key chord as needed.
            let pending = app.pending_chord.take();
            match resolver.resolve_chord(app.view, pending, key) {
                ChordAction::Pending => {
                    // First key of a two-key binding: hold it for the next press.
                    app.pending_chord = Some(key);
                    EventResult::Continue
                }
                ChordAction::Resolved(action) => dispatch_action(action, app),
            }
        }
        // Resize and other events fall through to a redraw by the caller.
        _ => EventResult::Continue,
    }
}

/// Handle a key press while the Search query field has focus (Requirements 2.1,
/// 2.2, 7.7).
///
/// Enter submits: a non-blank query is enqueued as a [`FetchRequest::Search`]
/// for the async loop to run and focus leaves the field; a blank query shows a
/// validation message and keeps focus. Esc leaves the field without searching.
/// Every other key edits the text via [`TextInputField`], so characters are
/// inserted rather than triggering their bindings.
fn handle_search_input(key: KeyEvent, app: &mut App) -> EventResult {
    if key.code == KeyCode::Enter {
        let query = app.search.query.value().trim().to_string();
        if query.is_empty() {
            app.search.set_message("enter a search term");
            return EventResult::Continue;
        }
        app.search.query_focused = false;
        app.search.message = Some(format!("searching for \u{201c}{query}\u{201d}\u{2026}"));
        app.pending_fetch = Some(FetchRequest::Search { query });
        return EventResult::Continue;
    }
    match app.search.query.handle_key(key) {
        TextInputOutcome::Exited { .. } => {
            // Esc leaves the field; keep any typed text so the user can resume.
            app.search.query_focused = false;
        }
        TextInputOutcome::Inserted { .. }
        | TextInputOutcome::Deleted { .. }
        | TextInputOutcome::Passthrough => {}
    }
    EventResult::Continue
}

/// Dispatch a resolved [`Action`] against the app, mutating state and enqueueing
/// engine requests (Requirements 4, 5, 7). Returns the loop control flow.
fn dispatch_action(action: Action, app: &mut App) -> EventResult {
    match action {
        Action::Quit => return EventResult::Quit,
        Action::Switch(view) => {
            // Switching views must not disturb playback or the Queue
            // (Requirement 4.9): only the drawn content changes.
            app.switch_view(view);
            // Entering Search focuses the query field so the user can type a
            // term immediately (Requirement 2.1); leaving Search drops focus.
            app.search.query_focused = view == View::Search;
        }
        Action::ToggleHelp => app.show_help = !app.show_help,
        // Sign-out requires the Auth/engine wiring the loop does not own; record
        // it so the surrounding layer (main.rs) can act. For now it is a no-op
        // on view state so the key is recognized rather than silently dropped.
        Action::SignOut => {}
        Action::NavDown => nav_down(app),
        Action::NavUp => nav_up(app),
        Action::NavOpen => nav_open(app),
        Action::NavBack => nav_back(app),
        Action::PlayPause => app.toggle_play_pause(),
        Action::NextTrack => app.next_track(),
        Action::PreviousTrack => app.previous_track(),
        Action::SeekForward => app.seek(now_playing::SeekDirection::Forward),
        Action::SeekBackward => app.seek(now_playing::SeekDirection::Backward),
        Action::VolumeUp => app.adjust_volume(VOLUME_STEP),
        Action::VolumeDown => app.adjust_volume(-VOLUME_STEP),
        Action::ToggleShuffle => app.toggle_shuffle(),
        Action::CycleRepeat => app.cycle_repeat(),
        // Refreshing the Library requires an API fetch the loop does not own;
        // recognized but a no-op here (no network wiring in the loop).
        Action::RefreshLibrary => {}
        Action::PlayNext | Action::AddToQueue => {
            // Insert the selected search/library item into the Queue
            // (Requirements 5.2, 5.3). Only a selected Song maps to a Queue
            // track; other selections are no-ops here.
            if let Some(track) = app.selected_track_for_queue() {
                let edit = if matches!(action, Action::PlayNext) {
                    QueueEdit::PlayNext(track)
                } else {
                    QueueEdit::AddToQueue(track)
                };
                app.dispatch_queue_edit(edit);
            }
        }
        Action::RemoveFromQueue => {
            if let Some(edit) = app.queue_view.remove_selected() {
                app.dispatch_queue_edit(edit);
            }
        }
        Action::MoveUp => {
            if let Some(edit) = app.queue_view.move_selected_up() {
                app.dispatch_queue_edit(edit);
            }
        }
        Action::MoveDown => {
            if let Some(edit) = app.queue_view.move_selected_down() {
                app.dispatch_queue_edit(edit);
            }
        }
        Action::Ignored => {}
    }
    EventResult::Continue
}

/// The amount a single volume-up/down press changes the level (Requirement 4.7).
const VOLUME_STEP: i64 = 5;

/// Move the selection down in the active view (Requirement 7.2).
fn nav_down(app: &mut App) {
    match app.view {
        View::Search => app.search.select_next(),
        View::Library => app.library.select_next(),
        View::Queue => app.queue_view.select_next(&app.queue),
    }
}

/// Move the selection up in the active view (Requirement 7.2).
fn nav_up(app: &mut App) {
    match app.view {
        View::Search => app.search.select_previous(),
        View::Library => app.library.select_previous(),
        View::Queue => app.queue_view.select_previous(&app.queue),
    }
}

/// Open/activate the current selection in the active view (Requirements 2.7,
/// 2.8, 3.1, 3.5, 5, 7.2).
///
/// Playable activations that carry a ready-to-use Queue (a selected song, or a
/// library track list) replace the Queue and start playback immediately. Drill-
/// down activations (opening a library section, or an album/playlist's tracks)
/// enqueue a [`FetchRequest`] the async loop runs against the API client. In
/// the Queue view, opening the selection plays that track.
fn nav_open(app: &mut App) {
    match app.view {
        View::Search => match app.search.activate_selection() {
            Some(SearchActivation::PlaySong(queue)) => app.play_queue(queue),
            // Drilling into an album/playlist from search opens its tracks via
            // the library tracks endpoint (Requirement 2.8).
            Some(SearchActivation::OpenTracks { kind, id }) => {
                app.pending_fetch = Some(FetchRequest::LibraryTracks { kind, id });
            }
            // Artist drill-down needs a catalog detail fetch the loop does not
            // perform yet; recognized but not actioned here.
            Some(SearchActivation::OpenArtist { .. }) | None => {}
        },
        View::Library => match app.library.activate_selection() {
            Some(LibraryActivation::PlaySong(queue))
            | Some(LibraryActivation::PlayTracks(queue)) => app.play_queue(queue),
            // Opening a section from the picker fetches its first page
            // (Requirement 3.1/3.2).
            Some(LibraryActivation::OpenSection(section)) => {
                app.pending_fetch = Some(FetchRequest::LibrarySection { section });
            }
            // Opening an album/playlist fetches its tracks (Requirement 3.5).
            Some(LibraryActivation::OpenTracks { kind, id }) => {
                app.pending_fetch = Some(FetchRequest::LibraryTracks { kind, id });
            }
            // Artist drill-down is a catalog detail fetch not wired here.
            Some(LibraryActivation::OpenArtist { .. }) | None => {}
        },
        View::Queue => {
            if let Some(index) = app.queue_view.selected {
                if index < app.queue.len() {
                    let transition = queue::Transition {
                        index,
                        status: PlaybackStatus::Playing,
                    };
                    app.apply_transition(transition);
                }
            }
        }
    }
}

/// Return from a drilled-in Library list to the section picker (Requirement
/// 7.2). The other views have no "back" level, so this is a no-op there.
fn nav_back(app: &mut App) {
    if app.view == View::Library {
        app.library.back_to_sections(0);
    }
}

/// Drain [`App::pending_requests`] and send each over the engine connection.
///
/// Dispatch enqueues transport/seek/volume/load requests synchronously; this is
/// the single awaited send site. Sends are best-effort: a failed send is logged
/// (token values are never part of the logged message) and the loop continues,
/// so a momentarily dead socket does not freeze input. When no engine is
/// connected (preview-only), the queued requests are simply dropped.
async fn send_pending_requests(app: &mut App, engine: Option<&IpcSender>) {
    let requests = std::mem::take(&mut app.pending_requests);
    let Some(sender) = engine else {
        return;
    };
    for request in requests {
        if let Err(err) = sender.send(&request).await {
            log_app_error(&format!("failed to send engine request: {err}"));
        }
    }
}

/// Run the clean-shutdown sequence for a quit or a termination signal
/// (Requirements 7.6, 7.9, 10.5).
///
/// Order: save Session_State first (so a save failure is known before we tear
/// down the terminal), then ask the engine to pause and shut down, then restore
/// the terminal. The [`TerminalGuard`] also restores on drop, so restoration is
/// guaranteed even if an error occurs here.
async fn shutdown(
    guard: &mut TerminalGuard,
    app: &App,
    engine: Option<&IpcSender>,
) -> io::Result<LoopOutcome> {
    // 1. Save Session_State (Requirement 7.6). On failure, log to the app log
    //    and report QuitSaveFailed so the caller exits non-zero (Requirement
    //    7.9).
    let save_result = session::save_session(&app.session);

    // 2. Stop playback and terminate the Playback_Engine (Requirements 7.6,
    //    10.5). Best-effort: a dead socket must not block a clean quit.
    if let Some(sender) = engine {
        let _ = sender.send(&RequestMethod::Pause).await;
        let _ = sender.send(&RequestMethod::Shutdown).await;
    }

    // 3. Restore the terminal now so any messages print to a normal screen.
    guard.restore()?;

    match save_result {
        Ok(()) => Ok(LoopOutcome::Quit),
        Err(err) => {
            // Requirement 7.9: write the error to the app log (no token values
            // are involved in a session save error) and exit non-zero.
            log_app_error(&format!("failed to save session on quit: {err}"));
            Ok(LoopOutcome::QuitSaveFailed)
        }
    }
}

/// Draw one frame: the main-view layout skeleton plus the live Now_Playing_Bar.
fn draw(guard: &mut TerminalGuard, app: &App) -> io::Result<()> {
    guard.terminal().draw(|frame| render_main_view(frame, app))?;
    Ok(())
}

/// Render the whole application into an arbitrary [`Frame`] (Requirement 10.1,
/// 12.10).
///
/// This is the same render path the live loop uses via [`draw`], exposed so a
/// backend-agnostic harness (for example ratatui's `TestBackend`) can draw the
/// app at any terminal size. It is the single public entry point for rendering
/// a frame, which the resize-redraw integration test (task 16.3) uses to assert
/// the views and the Now_Playing_Bar fit each column/row dimension. It performs
/// no terminal I/O itself; it only paints `frame`.
pub fn render_app(frame: &mut Frame, app: &App) {
    render_main_view(frame, app);
}

/// Render the main-view layout skeleton (Requirement 10.1).
///
/// Three vertical regions: a one-line navigation bar at the top, the view's
/// content area in the middle (a labelled placeholder until tasks 13.2/13.3
/// fill it in), and the Now_Playing_Bar at the bottom. This is the "navigation
/// and playback status areas visible, with loading placeholders allowed"
/// layout the startup deadline requires.
fn render_main_view(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // A one-line engine-error banner is inserted above the content when the
    // Playback_Engine failed to start or the connection was lost (Requirement
    // 4.10, task 16.2): it names the failure and the restart action.
    let banner = app.engine_error.is_some();
    let mut constraints = vec![Constraint::Length(1)]; // navigation bar
    if banner {
        constraints.push(Constraint::Length(1)); // engine-error banner
    }
    constraints.push(Constraint::Min(1)); // view content
    constraints.push(Constraint::Length(3)); // Now_Playing_Bar (bordered)

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_nav_bar(frame, chunks[0], app.view);
    let mut idx = 1;
    if let Some(error) = &app.engine_error {
        render_engine_error_banner(frame, chunks[idx], error);
        idx += 1;
    }
    render_content(frame, chunks[idx], app);
    render_now_playing(frame, chunks[idx + 1], &app.playback, app.preview_mode);

    // The help overlay is drawn last, over the content area, when toggled on
    // (Requirement 7.3). It lists the default action keys as a quick reference.
    if app.show_help {
        render_help_overlay(frame, chunks[idx]);
    }
}

/// Render a simple help overlay listing the default keybindings (Requirement
/// 7.3). Drawn over the content area; dismissed by toggling Help again.
fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let lines: Vec<&str> = vec![
        "Help — press ? to close",
        "",
        "j / k / ↓ / ↑   move selection",
        "l / → / Enter   open selected",
        "h / ←           back",
        "/               search    g l  library    g q  queue",
        "Space           play / pause",
        "n / p           next / previous track",
        "] / [           seek forward / backward",
        "+ / -           volume up / down",
        "s               shuffle    r   cycle repeat",
        "N / a / x       play-next / add / remove (queue)",
        "K / J           move up / down (queue)",
        "g o             sign out   q   quit",
    ];
    let text: Vec<Line> = lines.into_iter().map(|l| Line::from(l.to_string())).collect();
    let block = Block::default().borders(Borders::ALL).title("Help");
    frame.render_widget(Paragraph::new(text).block(block), area);
}

/// Render the Playback_Engine error banner naming the failure and the restart
/// action (Requirement 4.10, task 16.2). Drawn in an attention style so the
/// user notices the engine is unavailable and that a restart is offered while
/// the Queue and current track are preserved.
fn render_engine_error_banner(frame: &mut Frame, area: Rect, error: &EngineErrorState) {
    let text = format!("⚠ {}", error.message());
    let style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        area,
    );
}

/// Render the current view's content area.
///
/// Each view has a dedicated renderer: Search and Library (task 13.2), and the
/// Queue (task 13.3), which also keeps the Now_Playing_Bar in sync with the
/// current track.
fn render_content(frame: &mut Frame, area: Rect, app: &App) {
    match app.view {
        View::Search => render_search_view(frame, area, &app.search),
        View::Library => render_library_view(frame, area, &app.library),
        View::Queue => render_queue_view(frame, area, &app.queue, &app.queue_view),
    }
}

/// Render the top navigation bar, highlighting the active view.
fn render_nav_bar(frame: &mut Frame, area: ratatui::layout::Rect, active: View) {
    let mut spans = Vec::new();
    for (idx, view) in View::ALL.iter().enumerate() {
        if idx > 0 {
            spans.push(ratatui::text::Span::raw("  "));
        }
        let label = format!("[{}] {}", idx + 1, view.title());
        let style = if *view == active {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(ratatui::text::Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render the Now_Playing_Bar by painting the pure, width-aware line that
/// [`now_playing::render_line`] computes (Requirement 6).
fn render_now_playing(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    playback: &Playback_State,
    preview_mode: bool,
) {
    // In preview-only mode the block title and the track title both carry the
    // preview label so it is unmistakable that only a 30-second preview plays
    // (design: "label tracks as previews", task 16.2).
    let title = if preview_mode {
        "Now Playing (preview)"
    } else {
        "Now Playing"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner_width = area.width.saturating_sub(2) as usize;

    // When a preview is loaded, label the track title so the Now_Playing_Bar
    // itself shows the preview marker without changing the authoritative
    // Playback_State.
    let line = if preview_mode && playback.track_id.is_some() {
        let mut labelled = playback.clone();
        labelled.title = preview::label_as_preview(&playback.title);
        now_playing::render_line(&labelled, inner_width)
    } else {
        now_playing::render_line(playback, inner_width)
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// Listener for process termination signals (SIGINT/SIGTERM/SIGHUP).
///
/// Each of these is treated as a request to quit cleanly (Requirements 7.6,
/// 10.5): the loop runs the same shutdown sequence as the quit key.
struct SignalListener {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl SignalListener {
    /// Register handlers for SIGINT, SIGTERM, and SIGHUP.
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(SignalListener {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolve when any of the registered signals is delivered.
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// Resolve the application log path `$XDG_STATE_HOME/apple-music-tui/app.log`
/// (Requirements 7.9, 12.11, 12.3), sharing the state directory with the
/// session file so both live under the same XDG root.
pub fn log_file_path() -> Option<PathBuf> {
    session::state_dir().map(|dir| dir.join(LOG_FILE))
}

/// Append `message` to the application log, creating the state directory and
/// the log file if needed. Best-effort: logging failures are swallowed because
/// the caller is already on an error/exit path and has nowhere better to report
/// to.
fn log_app_error(message: &str) {
    if let Some(path) = log_file_path() {
        append_log_line(&path, message);
    }
}

/// Append a single timestamp-free log line to `path`, creating parents as
/// needed. Kept separate so [`log_unrecoverable`] and [`log_app_error`] share
/// one writer and it can be tested against an explicit path.
fn append_log_line(path: &Path, message: &str) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

/// Log an unrecoverable TUI error to the app log with token values excluded
/// (Requirement 12.11).
///
/// The caller is responsible for restoring the terminal first (dropping the
/// [`TerminalGuard`] does this) and then exiting non-zero. `description` is the
/// error text to record; [`redact_tokens`] strips anything that looks like a
/// token so no credential ever reaches the log (Requirements 1.13, 12.11).
pub fn log_unrecoverable(description: &str) {
    let redacted = redact_tokens(description);
    if let Some(path) = log_file_path() {
        append_log_line(&path, &format!("unrecoverable error: {redacted}"));
    }
}

/// Remove anything resembling a token from a log message so credentials are
/// never written to disk (Requirements 1.13, 12.11).
///
/// This is a defensive scrub for the error path: the TUI never deliberately
/// puts a token into an error string, but an unhandled error could carry one in
/// a URL or header. Long opaque tokens (JWTs and Music_User_Tokens are long,
/// dot/base64url strings) are replaced with a redaction marker, and any
/// `token=...`-style query parameter value is masked.
fn redact_tokens(message: &str) -> String {
    message
        .split_whitespace()
        .map(redact_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Redact a single whitespace-delimited word if it looks like a bearer token or
/// a `key=secret` pair whose key names a credential.
fn redact_word(word: &str) -> String {
    // Mask `developerToken=...`, `musicUserToken=...`, `token=...`,
    // `authorization=...`, etc., keeping the key for context.
    if let Some((key, value)) = word.split_once('=') {
        let lower = key.to_ascii_lowercase();
        if (lower.contains("token") || lower.contains("auth") || lower.contains("secret"))
            && !value.is_empty()
        {
            return format!("{key}=[REDACTED]");
        }
    }
    // Mask long opaque strings that look like a JWT (three dot-separated
    // base64url segments) regardless of surrounding key.
    if looks_like_jwt(word) {
        return "[REDACTED]".to_string();
    }
    word.to_string()
}

/// Whether a word looks like a JWT: three non-empty segments separated by dots,
/// each made up of base64url characters, and long enough to be a real token
/// rather than an abbreviation.
fn looks_like_jwt(word: &str) -> bool {
    let segments: Vec<&str> = word.split('.').collect();
    if segments.len() != 3 {
        return false;
    }
    if word.len() < 40 {
        return false;
    }
    segments.iter().all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

// ===========================================================================
// Search and Library views (task 13.2).
//
// The Search and Library views are modelled as a flat list of selectable
// *rows* built purely from the data layer's [`SearchResults`]/[`SearchPagination`]
// (api.rs, task 9.1/9.2) and [`LibraryList`] (api.rs, task 9.3). Keeping the row
// model and the selection → action mapping pure (no terminal, no network) lets
// this logic be unit-tested in isolation; the ratatui renderers below only
// paint the rows these builders produce, mirroring how now_playing.rs splits
// pure line-layout from widget drawing.
//
// What this covers:
// - Grouped search sections (songs/albums/artists/playlists), each shown with a
//   header and omitted entirely when it returned zero results (Requirement 2.4);
//   the "no results" message when every section is empty (Requirement 2.4); and
//   the empty/prompt state before any search (Requirement 2.3).
// - Search selection actions (Requirement 2.7, 2.8, 2.9): selecting a song
//   yields a queue replacement + play; selecting an album/playlist/artist yields
//   a drill-down request.
// - Library section list, drill-down, loading indicators and empty states
//   (Requirements 3.1, 3.2, 3.4, 3.5, 3.8).
// - Selecting a track inside an album/playlist replaces the Queue in stored
//   order and starts at the selected position (Requirement 3.5), reusing the
//   shared [`tracks_to_queue`] helper.
//
// Durations are rendered with [`now_playing::format_duration`] so rows match the
// Now_Playing_Bar's `m:ss` / `h:mm:ss` formatting (Requirements 2.3, 3.4).
// ===========================================================================

/// Placeholder shown for a row whose text value is missing, matching the
/// Now_Playing_Bar's unknown-field convention (Requirement 2.3, 3.4).
const ROW_UNKNOWN: &str = "Unknown";

/// Message shown before the user has run any search (Requirement 2.3). The view
/// is otherwise empty until a query is submitted.
pub const SEARCH_PROMPT: &str = "Type a query and press Enter to search the catalog";

/// Message shown when a search returns zero results across all four sections
/// (Requirement 2.4).
pub const SEARCH_NO_RESULTS: &str = "No results matched your query";

/// Message shown while a library list is loading its first page (Requirement
/// 3.2).
pub const LIBRARY_LOADING: &str = "Loading\u{2026}";

/// Message shown when a library section, album, or playlist contains no items
/// (Requirement 3.8).
pub const LIBRARY_EMPTY: &str = "This section is empty";

/// Message shown when the library could not be loaded (Requirement 3.7), shown
/// by the view when a list records a load error.
pub const LIBRARY_LOAD_ERROR: &str = "The library could not be loaded";

/// Message shown when a next-page search request failed and will retry on the
/// next end-of-list navigation (Requirement 2.11).
pub const SEARCH_LOAD_MORE_ERROR: &str = "More results could not be loaded";

/// Format the duration of a track in milliseconds for a list row, using the
/// shared `m:ss` / `h:mm:ss` formatter (Requirements 2.3, 3.4). A missing
/// duration renders the unknown placeholder (Requirement 2.3).
fn row_duration(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) => format_duration(ms / 1000),
        None => ROW_UNKNOWN.to_string(),
    }
}

/// Substitute the unknown placeholder for an empty text value in a row
/// (Requirement 2.3 / 3.4).
fn row_text(value: &str) -> &str {
    if value.is_empty() {
        ROW_UNKNOWN
    } else {
        value
    }
}

/// Render one [`Domain_Object`] as the text of a single result/library row.
///
/// Per Requirement 2.3 / 3.4: a song shows title, artist, and duration; an
/// album shows name and artist; an artist shows name; a playlist shows name.
/// Missing text/duration fall back to the unknown placeholder so a row is never
/// blank.
pub fn object_row_text(object: &Domain_Object) -> String {
    match object {
        Domain_Object::Song {
            name,
            artist_name,
            duration_ms,
            ..
        } => format!(
            "{}  —  {}  ({})",
            row_text(name),
            row_text(artist_name),
            row_duration(*duration_ms)
        ),
        Domain_Object::Album {
            name, artist_name, ..
        } => format!("{}  —  {}", row_text(name), row_text(artist_name)),
        Domain_Object::Artist { name, .. } => row_text(name).to_string(),
        Domain_Object::Playlist { name, .. } => row_text(name).to_string(),
    }
}

/// Convert a catalog/library [`Domain_Object::Song`] into a [`QueueTrack`].
///
/// Only songs are playable queue entries; albums/artists/playlists drill down
/// rather than enqueue, so this returns `None` for them. A missing artist maps
/// to the empty string (the Now_Playing_Bar substitutes its own placeholder),
/// matching the Queue's `QueueTrack` shape.
pub fn song_to_queue_track(object: &Domain_Object) -> Option<QueueTrack> {
    match object {
        Domain_Object::Song {
            id,
            name,
            artist_name,
            duration_ms,
            ..
        } => Some(QueueTrack {
            track_id: id.clone(),
            title: name.clone(),
            artist: artist_name.clone(),
            duration_ms: *duration_ms,
        }),
        _ => None,
    }
}

/// Build a Queue from a selected song, replacing any previous contents with the
/// song as the sole, current entry (Requirement 2.7).
///
/// Returns `None` when `object` is not a playable song (an album/artist/playlist
/// drills down instead of enqueueing). The caller then issues `load`/`play` to
/// the Playback_Engine for the current track.
pub fn song_to_queue(object: &Domain_Object) -> Option<Queue> {
    let track = song_to_queue_track(object)?;
    Some(Queue {
        tracks: vec![track],
        current: Some(0),
        pre_shuffle_order: None,
    })
}

/// Replace the Queue with `tracks` in their stored order and set the current
/// track to `selected`, clamped into range (Requirement 3.5).
///
/// Used when the user selects a track inside an album or playlist: every track
/// of that album/playlist becomes the Queue in stored order and playback starts
/// at the selected track's position. Non-song objects in `objects` are skipped
/// (an album/playlist's tracks are songs, but this stays robust to the mapper
/// returning a stray non-song). The returned Queue's `current` indexes the
/// selected track within the *kept* songs; when `objects` yields no playable
/// song the Queue is empty with no current track.
pub fn tracks_to_queue(objects: &[Domain_Object], selected: usize) -> Queue {
    // Keep each song and remember its original index so the selection can be
    // mapped onto the kept tracks even if a non-song was skipped.
    let mut tracks: Vec<QueueTrack> = Vec::with_capacity(objects.len());
    let mut selected_in_kept: Option<usize> = None;
    for (idx, object) in objects.iter().enumerate() {
        if let Some(track) = song_to_queue_track(object) {
            if idx <= selected {
                // The kept index of the selected track is the count of songs at
                // or before it; update on every song up to and including it.
                selected_in_kept = Some(tracks.len());
            }
            tracks.push(track);
        }
    }

    let current = if tracks.is_empty() {
        None
    } else {
        // Clamp the selection into range; default to the first track when the
        // selection fell before any kept song.
        Some(selected_in_kept.unwrap_or(0).min(tracks.len() - 1))
    };

    Queue {
        tracks,
        current,
        pre_shuffle_order: None,
    }
}

/// A single rendered row of the Search view: either a non-selectable section
/// header, a selectable result referring back to its section and in-section
/// index, or an informational line (empty/error message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRow {
    /// A section header (e.g. "Songs"); not selectable.
    Header(String),
    /// A selectable result; carries its section and the index within that
    /// section so a selection maps straight back to the [`Domain_Object`].
    Result {
        section: SearchSection,
        index: usize,
        text: String,
    },
    /// An informational, non-selectable line (prompt, no-results, or a
    /// per-section load-more error).
    Info(String),
}

impl SearchRow {
    /// Whether this row can be selected (results only). Headers and info lines
    /// are skipped by selection navigation.
    pub fn is_selectable(&self) -> bool {
        matches!(self, SearchRow::Result { .. })
    }
}

/// The display title for a search section header (Requirement 2.3).
fn search_section_title(section: SearchSection) -> &'static str {
    match section {
        SearchSection::Songs => "Songs",
        SearchSection::Albums => "Albums",
        SearchSection::Artists => "Artists",
        SearchSection::Playlists => "Playlists",
    }
}

/// Borrow the items of one section from a [`SearchResults`].
fn search_section_items(results: &SearchResults, section: SearchSection) -> &[Domain_Object] {
    match section {
        SearchSection::Songs => &results.songs,
        SearchSection::Albums => &results.albums,
        SearchSection::Artists => &results.artists,
        SearchSection::Playlists => &results.playlists,
    }
}

/// The Search view's state: the submitted results (with pagination), an
/// optional one-line message (validation or search failure, Requirements 2.2 /
/// 2.5), and the current selection among the selectable rows.
///
/// The view is "empty" (showing [`SEARCH_PROMPT`]) until `pagination` is set by
/// a completed search. The selection is an index into the selectable rows
/// produced by [`rows`](SearchViewState::rows); navigation moves it among
/// results only, skipping headers and info lines.
#[derive(Debug, Default)]
pub struct SearchViewState {
    /// The query text field. When [`query_focused`](SearchViewState::query_focused)
    /// is set, key presses are routed here as text instead of triggering their
    /// bindings (Requirement 2.1, 7.7).
    pub query: TextInputField,
    /// Whether the query field has focus and is capturing typed text. Entered
    /// with the Search action and left on Esc or when a search is submitted.
    pub query_focused: bool,
    /// The accumulated results and per-section pagination of the latest search,
    /// or `None` before any search has completed.
    pub pagination: Option<SearchPagination>,
    /// A transient message shown above the results: a query-validation message
    /// (Requirement 2.2) or a search-failure message (Requirement 2.5).
    pub message: Option<String>,
    /// The currently selected selectable-row index (0-based), or `None` when
    /// there is nothing selectable.
    pub selected: Option<usize>,
}

impl SearchViewState {
    /// A fresh Search view with no results and nothing selected.
    pub fn new() -> Self {
        SearchViewState::default()
    }

    /// Install the results of a completed search and select the first result
    /// (Requirement 2.3). Clears any prior validation/failure message.
    pub fn set_results(&mut self, pagination: SearchPagination) {
        let has_selectable = !pagination.results().is_empty();
        self.pagination = Some(pagination);
        self.message = None;
        self.selected = has_selectable.then_some(0);
    }

    /// Show a one-line message (validation or failure) without changing the
    /// displayed results (Requirements 2.2, 2.5).
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    /// Build the flat, ordered list of rows to render (Requirement 2.3, 2.4).
    ///
    /// Before any search, a single prompt line is shown. After a search that
    /// matched nothing, the no-results message is shown (Requirement 2.4).
    /// Otherwise each non-empty section is rendered as a header followed by its
    /// result rows; a section with zero results is omitted entirely
    /// (Requirement 2.4), and a section with an outstanding load-more error
    /// appends an info line (Requirement 2.11).
    pub fn rows(&self) -> Vec<SearchRow> {
        let mut rows = Vec::new();

        let Some(pagination) = &self.pagination else {
            rows.push(SearchRow::Info(SEARCH_PROMPT.to_string()));
            return rows;
        };

        let results = pagination.results();
        if results.is_empty() {
            rows.push(SearchRow::Info(SEARCH_NO_RESULTS.to_string()));
            return rows;
        }

        for section in SearchSection::ALL {
            let items = search_section_items(results, section);
            if items.is_empty() {
                // Omit the section for a resource type with zero results
                // (Requirement 2.4).
                continue;
            }
            rows.push(SearchRow::Header(search_section_title(section).to_string()));
            for (index, object) in items.iter().enumerate() {
                rows.push(SearchRow::Result {
                    section,
                    index,
                    text: object_row_text(object),
                });
            }
            if pagination.has_load_error(section) {
                rows.push(SearchRow::Info(SEARCH_LOAD_MORE_ERROR.to_string()));
            }
        }
        rows
    }

    /// The indices (into [`rows`](SearchViewState::rows)) of the selectable
    /// rows, in order. Used to map the selection onto concrete rows.
    fn selectable_indices(rows: &[SearchRow]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.is_selectable())
            .map(|(i, _)| i)
            .collect()
    }

    /// Move the selection down one result, clamping at the last one
    /// (Requirement 7.2 edge-clamp; wired through the resolver in task 14.1).
    pub fn select_next(&mut self) {
        let count = Self::selectable_indices(&self.rows()).len();
        if count == 0 {
            self.selected = None;
            return;
        }
        let next = match self.selected {
            Some(i) => (i + 1).min(count - 1),
            None => 0,
        };
        self.selected = Some(next);
    }

    /// Move the selection up one result, clamping at the first one.
    pub fn select_previous(&mut self) {
        let count = Self::selectable_indices(&self.rows()).len();
        if count == 0 {
            self.selected = None;
            return;
        }
        let prev = match self.selected {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.selected = Some(prev);
    }

    /// The currently selected [`Domain_Object`], if any.
    pub fn selected_object(&self) -> Option<&Domain_Object> {
        let pagination = self.pagination.as_ref()?;
        let selected = self.selected?;
        let rows = self.rows();
        let indices = Self::selectable_indices(&rows);
        let row_index = *indices.get(selected)?;
        match &rows[row_index] {
            SearchRow::Result { section, index, .. } => {
                search_section_items(pagination.results(), *section).get(*index)
            }
            _ => None,
        }
    }

    /// Resolve the action to take when the user opens the current selection
    /// (Requirements 2.7, 2.8, 2.9). Returns `None` when nothing is selected.
    pub fn activate_selection(&self) -> Option<SearchActivation> {
        Some(search_activation(self.selected_object()?))
    }
}

/// The action produced by opening a selected search result (Requirements 2.7,
/// 2.8, 2.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchActivation {
    /// A song was selected: replace the Queue with it and start playback
    /// (Requirement 2.7). Carries the ready-to-use single-entry Queue.
    PlaySong(Queue),
    /// An album or playlist was selected: drill down into its tracks
    /// (Requirement 2.8). Carries the resource id and which kind it is.
    OpenTracks { kind: LibrarySection, id: String },
    /// An artist was selected: drill down into the artist's albums and top
    /// songs (Requirement 2.9). Carries the artist id.
    OpenArtist { id: String },
}

/// Map a selected [`Domain_Object`] to its activation action (Requirements 2.7,
/// 2.8, 2.9).
fn search_activation(object: &Domain_Object) -> SearchActivation {
    match object {
        Domain_Object::Song { .. } => {
            // A song always produces a single-entry Queue (Requirement 2.7).
            SearchActivation::PlaySong(
                song_to_queue(object).expect("a song maps to a single-entry queue"),
            )
        }
        Domain_Object::Album { id, .. } => SearchActivation::OpenTracks {
            kind: LibrarySection::Albums,
            id: id.clone(),
        },
        Domain_Object::Playlist { id, .. } => SearchActivation::OpenTracks {
            kind: LibrarySection::Playlists,
            id: id.clone(),
        },
        Domain_Object::Artist { id, .. } => SearchActivation::OpenArtist { id: id.clone() },
    }
}

/// Where the Library view currently is: the section picker, or drilled into one
/// list (a section's items or an album's/playlist's tracks).
#[derive(Debug)]
pub enum LibraryFocus {
    /// The top-level picker over the four Library sections (Requirement 3.1).
    Sections {
        /// The selected section (0..4), always `Some` for a non-empty picker.
        selected: usize,
    },
    /// A drilled-in list: a section's items or an album/playlist's tracks.
    List {
        /// The list state machine owning items/pagination (api.rs task 9.3).
        list: LibraryList,
        /// Whether this list holds playable tracks (an album/playlist drill-in)
        /// so selecting a row replaces the Queue (Requirement 3.5), versus a
        /// section of albums/playlists/artists/songs that drills down further.
        is_track_list: bool,
        /// The selected row (0-based) among the loaded items, or `None` when the
        /// list is empty.
        selected: Option<usize>,
    },
}

/// The display title for a Library section header (Requirement 3.1).
fn library_section_title(section: LibrarySection) -> &'static str {
    match section {
        LibrarySection::Playlists => "Playlists",
        LibrarySection::Albums => "Albums",
        LibrarySection::Artists => "Artists",
        LibrarySection::Songs => "Songs",
    }
}

/// The Library view's state: where the user is (section picker or a list) plus
/// the rendered section labels. Pagination/loading/empty state live on the
/// contained [`LibraryList`] (api.rs task 9.3).
#[derive(Debug)]
pub struct LibraryViewState {
    /// Current focus: the section picker or a drilled-in list.
    pub focus: LibraryFocus,
    /// A transient one-line message shown above the content: a load failure or
    /// session-expiry notice (Requirements 3.7, 3.9). `None` when there is
    /// nothing to report.
    pub message: Option<String>,
}

impl Default for LibraryViewState {
    fn default() -> Self {
        LibraryViewState {
            focus: LibraryFocus::Sections { selected: 0 },
            message: None,
        }
    }
}

impl LibraryViewState {
    /// A fresh Library view showing the section picker (Requirement 3.1).
    pub fn new() -> Self {
        LibraryViewState::default()
    }

    /// Drill into a list (a section's items or an album/playlist's tracks),
    /// selecting the first loaded item when there is one.
    pub fn open_list(&mut self, list: LibraryList, is_track_list: bool) {
        let selected = (!list.is_empty()).then_some(0);
        self.focus = LibraryFocus::List {
            list,
            is_track_list,
            selected,
        };
    }

    /// Return from a drilled-in list to the section picker (Requirement 7.2
    /// "return to the previous view"; wired through the resolver in task 14.1).
    pub fn back_to_sections(&mut self, selected: usize) {
        self.focus = LibraryFocus::Sections {
            selected: selected.min(LibrarySection::ALL.len() - 1),
        };
    }

    /// Move the selection down one item, clamping at the last (Requirement 7.2
    /// edge-clamp). In the section picker this moves among the four sections; in
    /// a list it moves among the loaded items.
    pub fn select_next(&mut self) {
        match &mut self.focus {
            LibraryFocus::Sections { selected } => {
                *selected = (*selected + 1).min(LibrarySection::ALL.len() - 1);
            }
            LibraryFocus::List { list, selected, .. } => {
                let count = list.items().len();
                if count == 0 {
                    *selected = None;
                } else {
                    let next = match *selected {
                        Some(i) => (i + 1).min(count - 1),
                        None => 0,
                    };
                    *selected = Some(next);
                }
            }
        }
    }

    /// Move the selection up one item, clamping at the first.
    pub fn select_previous(&mut self) {
        match &mut self.focus {
            LibraryFocus::Sections { selected } => {
                *selected = selected.saturating_sub(1);
            }
            LibraryFocus::List { list, selected, .. } => {
                let count = list.items().len();
                if count == 0 {
                    *selected = None;
                } else {
                    let prev = match *selected {
                        Some(i) => i.saturating_sub(1),
                        None => 0,
                    };
                    *selected = Some(prev);
                }
            }
        }
    }

    /// Resolve the action to take when the user opens the current selection.
    ///
    /// From the section picker this is a request to open that section's list
    /// (Requirement 3.1/3.2). Inside a track list, opening a track replaces the
    /// Queue in stored order starting at the selected position (Requirement
    /// 3.5). Inside a non-track section list, opening an item drills down into
    /// it (Requirement 3.5 for albums/playlists; artists/songs handled by the
    /// caller). Returns `None` when there is nothing to open.
    pub fn activate_selection(&self) -> Option<LibraryActivation> {
        match &self.focus {
            LibraryFocus::Sections { selected } => {
                let section = *LibrarySection::ALL.get(*selected)?;
                Some(LibraryActivation::OpenSection(section))
            }
            LibraryFocus::List {
                list,
                is_track_list,
                selected,
            } => {
                let selected = (*selected)?;
                if *is_track_list {
                    // Replace the Queue with all tracks in stored order and
                    // start at the selected position (Requirement 3.5).
                    let queue = tracks_to_queue(list.items(), selected);
                    Some(LibraryActivation::PlayTracks(queue))
                } else {
                    let object = list.items().get(selected)?;
                    Some(library_item_activation(object))
                }
            }
        }
    }
}

/// The action produced by opening a selection in the Library view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryActivation {
    /// Open a top-level section's list (Requirement 3.1/3.2).
    OpenSection(LibrarySection),
    /// Drill into an album or playlist's tracks (Requirement 3.5).
    OpenTracks { kind: LibrarySection, id: String },
    /// Drill into an artist (Requirement 3.5 drill-down; catalog detail handled
    /// by the caller).
    OpenArtist { id: String },
    /// Open a library song as a single-track Queue and play it (a song row in a
    /// section list; Requirement 3.5 playback).
    PlaySong(Queue),
    /// Replace the Queue with an album/playlist's tracks in stored order,
    /// starting at the selected position (Requirement 3.5).
    PlayTracks(Queue),
}

/// Map a selected library item (inside a non-track section list) to its
/// activation action (Requirement 3.5).
fn library_item_activation(object: &Domain_Object) -> LibraryActivation {
    match object {
        Domain_Object::Album { id, .. } => LibraryActivation::OpenTracks {
            kind: LibrarySection::Albums,
            id: id.clone(),
        },
        Domain_Object::Playlist { id, .. } => LibraryActivation::OpenTracks {
            kind: LibrarySection::Playlists,
            id: id.clone(),
        },
        Domain_Object::Artist { id, .. } => LibraryActivation::OpenArtist { id: id.clone() },
        Domain_Object::Song { .. } => LibraryActivation::PlaySong(
            song_to_queue(object).expect("a song maps to a single-entry queue"),
        ),
    }
}

// ---------------------------------------------------------------------------
// ratatui rendering for the Search and Library views.
//
// These paint the pure row models above into the content area. They are thin:
// all the "what to show / what is selectable" decisions live in the state
// types, so these functions only translate rows to styled lines.
// ---------------------------------------------------------------------------

/// Render the Search view into `area` (Requirements 2.3, 2.4).
///
/// Draws a bordered block titled "Search" containing the grouped section
/// headers and result rows from [`SearchViewState::rows`]; the selected result
/// is highlighted. An optional validation/failure message is shown on the first
/// line (Requirements 2.2, 2.5).
pub fn render_search_view(frame: &mut Frame, area: Rect, state: &SearchViewState) {
    let block = Block::default().borders(Borders::ALL).title("Search");
    let rows = state.rows();
    let selectable = SearchViewState::selectable_indices(&rows);
    let selected_row = state
        .selected
        .and_then(|sel| selectable.get(sel).copied());

    let mut items: Vec<ListItem> = Vec::with_capacity(rows.len() + 2);

    // The query input line. A trailing cursor marks the field when focused so
    // the user can see their typed term (Requirement 2.1).
    let prompt = if state.query_focused {
        format!("Search: {}\u{2588}", state.query.value())
    } else {
        format!("Search: {}", state.query.value())
    };
    let prompt_style = if state.query_focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    items.push(ListItem::new(Line::from(Span::styled(prompt, prompt_style))));

    if let Some(message) = &state.message {
        items.push(ListItem::new(Line::from(Span::styled(
            message.clone(),
            Style::default().add_modifier(Modifier::ITALIC),
        ))));
    }
    for (idx, row) in rows.iter().enumerate() {
        items.push(search_row_item(row, Some(idx) == selected_row));
    }

    frame.render_widget(List::new(items).block(block), area);
}

/// Build a styled [`ListItem`] for one [`SearchRow`].
fn search_row_item(row: &SearchRow, selected: bool) -> ListItem<'static> {
    match row {
        SearchRow::Header(title) => ListItem::new(Line::from(Span::styled(
            title.clone(),
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ))),
        SearchRow::Info(text) => ListItem::new(Line::from(Span::styled(
            text.clone(),
            Style::default().add_modifier(Modifier::ITALIC),
        ))),
        SearchRow::Result { text, .. } => {
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!("  {text}"), style)))
        }
    }
}

/// Render the Library view into `area` (Requirements 3.1, 3.2, 3.4, 3.8).
///
/// In the section picker it lists the four Library sections with the selected
/// one highlighted (Requirement 3.1). In a drilled-in list it shows a loading
/// indicator while the first page loads (Requirement 3.2), the empty-state
/// message when the list has no items (Requirement 3.8), a load-error message
/// when a request failed (Requirement 3.7), or the item rows with the selected
/// one highlighted.
pub fn render_library_view(frame: &mut Frame, area: Rect, state: &LibraryViewState) {
    // A load-failure / session-expiry notice is shown on the first line when
    // present (Requirements 3.7, 3.9), above whatever the view is focused on.
    let message_item = state.message.as_ref().map(|message| {
        ListItem::new(Line::from(Span::styled(
            message.clone(),
            Style::default().add_modifier(Modifier::ITALIC | Modifier::BOLD),
        )))
    });

    match &state.focus {
        LibraryFocus::Sections { selected } => {
            let block = Block::default().borders(Borders::ALL).title("Library");
            let mut items: Vec<ListItem> = Vec::new();
            if let Some(message_item) = message_item {
                items.push(message_item);
            }
            items.extend(LibrarySection::ALL.iter().enumerate().map(|(idx, section)| {
                let style = if idx == *selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    library_section_title(*section).to_string(),
                    style,
                )))
            }));
            frame.render_widget(List::new(items).block(block), area);
        }
        LibraryFocus::List {
            list, selected, ..
        } => render_library_list(frame, area, list, *selected, message_item),
    }
}

/// The message shown in place of the item list when a Library section,
/// playlist, or album has loaded zero items (Requirement 3.8).
///
/// When the list is empty because the last request failed (and no prior items
/// were kept) the load-error message is shown instead (Requirement 3.7);
/// otherwise the empty-state message is shown. Factored out of
/// [`render_library_list`] so the exact message is unit-testable without a
/// terminal frame.
fn library_empty_message(has_load_error: bool) -> &'static str {
    if has_load_error {
        LIBRARY_LOAD_ERROR
    } else {
        LIBRARY_EMPTY
    }
}

/// Render a drilled-in Library list: loading / empty / error states and the
/// item rows (Requirements 3.2, 3.4, 3.7, 3.8). An optional `message_item`
/// (a load-failure / session-expiry notice) is shown on the first line.
fn render_library_list(
    frame: &mut Frame,
    area: Rect,
    list: &LibraryList,
    selected: Option<usize>,
    message_item: Option<ListItem<'static>>,
) {
    let block = Block::default().borders(Borders::ALL).title("Library");
    let st = list.state();
    let has_message = message_item.is_some();

    // Loading indicator for the first page: loading with nothing shown yet
    // (Requirement 3.2).
    if st.loading && list.is_empty() {
        let p = Paragraph::new(LIBRARY_LOADING).block(block);
        frame.render_widget(p, area);
        return;
    }

    // Empty state once a page has loaded and there is nothing to show
    // (Requirement 3.8). A load error with no prior items shows the load error.
    if list.is_empty() {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(message_item) = message_item {
            items.push(message_item);
        }
        items.push(ListItem::new(Line::from(library_empty_message(
            list.has_load_error(),
        ))));
        frame.render_widget(List::new(items).block(block), area);
        return;
    }

    let mut items: Vec<ListItem> = Vec::with_capacity(list.items().len() + 2);
    if let Some(message_item) = message_item {
        items.push(message_item);
    }
    if list.has_load_error() {
        // Keep prior items (Requirement 3.7) and surface the load error above.
        items.push(ListItem::new(Line::from(Span::styled(
            LIBRARY_LOAD_ERROR,
            Style::default().add_modifier(Modifier::ITALIC),
        ))));
    }
    // Count the non-selectable leading rows so the selection highlight lands on
    // the right item.
    let lead = (has_message as usize) + (list.has_load_error() as usize);
    for (idx, object) in list.items().iter().enumerate() {
        let style = if Some(idx) == selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        items.push(ListItem::new(Line::from(Span::styled(
            object_row_text(object),
            style,
        ))));
    }

    let mut list_state = ListState::default();
    list_state.select(selected.map(|s| s + lead));
    frame.render_stateful_widget(List::new(items).block(block), area, &mut list_state);
}

// ===========================================================================
// Queue view and live Now_Playing_Bar wiring (task 13.3).
//
// The Queue view renders the authoritative [`Queue`] (queue.rs) as a flat list:
// every track on one row showing its 1-based position, title, artist, and
// duration (via [`now_playing::format_duration`]), with the current track
// highlighted (Requirement 5.1). An empty Queue shows a single message
// (Requirement 5.10).
//
// Queue edits performed from this view (play-next, add-to-queue, remove, move)
// are expressed as a pure [`QueueEdit`] value and applied through
// [`App::apply_queue_edit`], which calls the corresponding [`Queue`] operation
// and reports a [`QueueEditOutcome`] so the startup wiring (task 16.1) can drive
// the Playback_Engine. Keeping the edit → operation mapping pure (no terminal,
// no IPC) mirrors how the Search/Library views separate their row/selection
// logic from rendering, and lets this logic be unit-tested in isolation.
//
// The Now_Playing_Bar is driven by the mirrored [`Playback_State`]: the loop's
// >=1/sec tick advances the elapsed time (Requirement 6.2) and any change to the
// current track, status, volume, shuffle, or repeat is reflected into
// `app.playback` so the next draw shows it promptly (Requirement 6.6). Switching
// views never touches the Queue or playback, so audio continues across view
// switches (Requirement 4.9).
// ===========================================================================

/// Message shown when the Queue view is opened with no tracks (Requirement
/// 5.10).
pub const QUEUE_EMPTY: &str = "The Queue is empty";

/// Message shown when an add action is rejected because the Queue is at its
/// 10,000-track limit (Requirement 5.12). Surfaced by the view when an edit
/// reports [`QueueEditOutcome::Rejected`].
pub const QUEUE_FULL: &str = "The Queue limit of 10,000 tracks was reached";

/// Build the authoritative [`Queue`] from a restored session's serialized queue
/// (Requirement 9.4).
///
/// The session stores the track list and a separate current-index pointer; this
/// reassembles them into a [`Queue`], clamping the pointer into range and
/// leaving `pre_shuffle_order` unset (shuffle state is not part of the restored
/// order). An out-of-range or missing pointer over a non-empty queue defaults to
/// the first track so the Queue invariant holds (Requirement 5.9).
pub fn queue_from_session(session: &Session_State) -> Queue {
    let tracks: Vec<QueueTrack> = session
        .queue
        .tracks
        .iter()
        .map(|t| QueueTrack {
            track_id: t.track_id.clone(),
            title: t.title.clone(),
            artist: t.artist.clone(),
            duration_ms: t.duration_ms,
        })
        .collect();

    let current = if tracks.is_empty() {
        None
    } else {
        Some(session.current_index.unwrap_or(0).min(tracks.len() - 1))
    };

    Queue {
        tracks,
        current,
        pre_shuffle_order: None,
    }
}

/// Mirror the authoritative [`Queue`] back into the session snapshot persisted
/// on quit (Requirements 9.1, 9.3), keeping the two in sync after an edit.
///
/// Only the queue tracks and the current-index pointer are updated here;
/// position/volume/shuffle/repeat are owned by the playback/transport wiring and
/// are left untouched.
pub fn sync_session_queue(session: &mut Session_State, queue: &Queue) {
    session.queue = SerializedQueue {
        tracks: queue
            .tracks
            .iter()
            .map(|t| SerializedTrack {
                track_id: t.track_id.clone(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                duration_ms: t.duration_ms,
            })
            .collect(),
    };
    session.current_index = queue.current;
}

/// Reflect the Queue's current track into the Now_Playing_Bar's
/// [`Playback_State`] (Requirements 6.1, 6.6).
///
/// Copies the current track's id/title/artist/duration into the mirror so the
/// bar shows the right track, and resets the elapsed position to 0 for the new
/// track. When the Queue has no current track the track fields are cleared so
/// the bar shows the "nothing playing" state (Requirement 6.3). The actual
/// play/pause status and live position come from Playback_Engine events (task
/// 16.1); this keeps the *track identity* fields in sync when the Queue changes
/// so a current-track change shows within the next draw (Requirement 6.6).
///
/// Album is left as-is: the Queue does not carry album metadata, so the field
/// keeps whatever the Playback_Engine last reported (or its placeholder).
pub fn reflect_current_track(playback: &mut Playback_State, queue: &Queue) {
    match queue.current.and_then(|i| queue.tracks.get(i)) {
        Some(track) => {
            // Only reset the position when the current track identity changed,
            // so a no-op re-sync does not rewind a playing track.
            let changed = playback.track_id.as_deref() != Some(track.track_id.as_str());
            playback.track_id = Some(track.track_id.clone());
            playback.title = track.title.clone();
            playback.artist = track.artist.clone();
            playback.duration_ms = track.duration_ms;
            if changed {
                playback.position_ms = 0;
            }
        }
        None => {
            // No current track: clear the track fields so the bar shows the
            // "nothing playing" message (Requirement 6.3).
            playback.track_id = None;
            playback.title = String::new();
            playback.artist = String::new();
            playback.album = String::new();
            playback.position_ms = 0;
            playback.duration_ms = None;
        }
    }
}

/// A structural edit the Queue view can request against the authoritative Queue
/// (Requirements 5.2–5.5, 5.11, 5.12).
///
/// These are pure descriptions of an edit; [`QueueEdit::apply`] performs it via
/// the matching [`Queue`] operation and returns a [`QueueEditOutcome`]. The
/// Queue view produces one of these from the current selection (see
/// [`QueueViewState`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEdit {
    /// Insert `track` after the current track (Requirement 5.2).
    PlayNext(QueueTrack),
    /// Append `track` to the end of the Queue (Requirement 5.3).
    AddToQueue(QueueTrack),
    /// Remove the track at 0-based `index` (Requirement 5.4 / 5.11). Removing
    /// the current track advances the pointer (Requirement 5.11).
    Remove { index: usize },
    /// Move the track at 0-based `from` to 1-based `target`, clamped to the
    /// Queue length (Requirement 5.5).
    Move { from: usize, target: usize },
}

impl QueueEdit {
    /// Apply this edit to `queue`, returning the playback consequence
    /// (Requirements 5.2–5.5, 5.11, 5.12).
    pub fn apply(self, queue: &mut Queue) -> QueueEditOutcome {
        match self {
            QueueEdit::PlayNext(track) => match queue.play_next(track) {
                Ok(()) => QueueEditOutcome::Edited,
                Err(QueueFullError) => QueueEditOutcome::Rejected,
            },
            QueueEdit::AddToQueue(track) => match queue.add_to_queue(track) {
                Ok(()) => QueueEditOutcome::Edited,
                Err(QueueFullError) => QueueEditOutcome::Rejected,
            },
            QueueEdit::Remove { index } => {
                // Removing the current track has a playback consequence
                // (Requirement 5.11); a non-current removal does not.
                if queue.current == Some(index) {
                    match queue.remove_current() {
                        Some(RemoveCurrentOutcome::Advanced { new_current }) => {
                            QueueEditOutcome::CurrentAdvanced { new_current }
                        }
                        Some(RemoveCurrentOutcome::Emptied) => QueueEditOutcome::Emptied,
                        None => QueueEditOutcome::NoOp,
                    }
                } else if queue.remove(index) {
                    QueueEditOutcome::Edited
                } else {
                    QueueEditOutcome::NoOp
                }
            }
            QueueEdit::Move { from, target } => {
                if queue.move_track(from, target) {
                    QueueEditOutcome::Edited
                } else {
                    QueueEditOutcome::NoOp
                }
            }
        }
    }
}

/// The playback consequence of applying a [`QueueEdit`] (Requirements 5.11,
/// 5.12).
///
/// The loop uses this to update the mirrored state and, once the startup wiring
/// (task 16.1) connects the Playback_Engine, to issue the right transport
/// command: start the new current track on [`QueueEditOutcome::CurrentAdvanced`]
/// (when playback was active), stop playback on [`QueueEditOutcome::Emptied`],
/// and surface the limit message on [`QueueEditOutcome::Rejected`] (Requirement
/// 5.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEditOutcome {
    /// A structural edit that did not change the current track.
    Edited,
    /// The current track was removed and the pointer advanced to `new_current`
    /// (Requirement 5.11). The engine should start that track if playback was
    /// active.
    CurrentAdvanced { new_current: usize },
    /// The current (and last) track was removed; the Queue is now empty and
    /// playback should stop (Requirement 5.11).
    Emptied,
    /// The add was rejected because the Queue is at its 10,000-track limit; the
    /// Queue is unchanged (Requirement 5.12).
    Rejected,
    /// The edit referred to a position that does not exist; nothing changed.
    NoOp,
}

/// The Queue view's selection state (task 13.3).
///
/// Holds the selected row (0-based) among the Queue's tracks, or `None` when the
/// Queue is empty. Navigation clamps at the edges (Requirement 7.2 edge-clamp,
/// wired through the resolver in task 14.1); the move action maps the selection
/// onto a [`QueueEdit::Move`].
#[derive(Debug, Default)]
pub struct QueueViewState {
    /// The currently selected track row, or `None` when the Queue is empty.
    pub selected: Option<usize>,
}

impl QueueViewState {
    /// A fresh Queue view with nothing selected.
    pub fn new() -> Self {
        QueueViewState::default()
    }

    /// Build a Queue view selecting the Queue's current track, or the first
    /// track when there is no current pointer but the Queue is non-empty, or
    /// nothing when the Queue is empty.
    pub fn for_queue(queue: &Queue) -> Self {
        let selected = if queue.is_empty() {
            None
        } else {
            Some(queue.current.unwrap_or(0).min(queue.len() - 1))
        };
        QueueViewState { selected }
    }

    /// Keep the selection valid after the Queue changed: clamp into range, or
    /// clear it when the Queue is now empty.
    pub fn clamp_to(&mut self, queue: &Queue) {
        if queue.is_empty() {
            self.selected = None;
        } else {
            let last = queue.len() - 1;
            self.selected = Some(self.selected.unwrap_or(0).min(last));
        }
    }

    /// Move the selection down one track, clamping at the last (Requirement 7.2
    /// edge-clamp).
    pub fn select_next(&mut self, queue: &Queue) {
        if queue.is_empty() {
            self.selected = None;
            return;
        }
        let last = queue.len() - 1;
        let next = match self.selected {
            Some(i) => (i + 1).min(last),
            None => 0,
        };
        self.selected = Some(next);
    }

    /// Move the selection up one track, clamping at the first.
    pub fn select_previous(&mut self, queue: &Queue) {
        if queue.is_empty() {
            self.selected = None;
            return;
        }
        let prev = match self.selected {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.selected = Some(prev);
    }

    /// The [`QueueEdit`] that removes the selected track, or `None` when nothing
    /// is selected (Requirement 5.4 / 5.11).
    pub fn remove_selected(&self) -> Option<QueueEdit> {
        self.selected.map(|index| QueueEdit::Remove { index })
    }

    /// The [`QueueEdit`] that moves the selected track to 1-based `target`,
    /// clamped to the Queue length by the operation (Requirement 5.5). Returns
    /// `None` when nothing is selected.
    pub fn move_selected_to(&self, target: usize) -> Option<QueueEdit> {
        self.selected
            .map(|from| QueueEdit::Move { from, target })
    }

    /// The [`QueueEdit`] that moves the selected track up one position
    /// (Requirement 5.5), or `None` when nothing is selected or it is already
    /// first.
    pub fn move_selected_up(&self) -> Option<QueueEdit> {
        match self.selected {
            Some(from) if from > 0 => Some(QueueEdit::Move {
                from,
                // `from` is 0-based; moving up one is 1-based target `from`.
                target: from,
            }),
            _ => None,
        }
    }

    /// The [`QueueEdit`] that moves the selected track down one position
    /// (Requirement 5.5), or `None` when nothing is selected. The operation
    /// clamps the target to the Queue length, so a request past the end is a
    /// no-op on the last track.
    pub fn move_selected_down(&self) -> Option<QueueEdit> {
        self.selected.map(|from| QueueEdit::Move {
            from,
            // `from` is 0-based; moving down one is 1-based target `from + 2`.
            target: from + 2,
        })
    }
}

/// One rendered row of the Queue view: a track with its 1-based position, or an
/// informational line (the empty-Queue message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueRow {
    /// A track row: `position` is 1-based, `current` marks the playing track
    /// (highlighted), and `text` is the position/title/artist/duration line.
    Track {
        position: usize,
        current: bool,
        text: String,
    },
    /// An informational, non-selectable line (the empty-Queue message).
    Info(String),
}

/// Build the ordered rows for the Queue view (Requirements 5.1, 5.10).
///
/// An empty Queue yields the single empty-Queue message (Requirement 5.10).
/// Otherwise every track is one row showing its 1-based position, title,
/// artist, and duration formatted with the shared `m:ss` / `h:mm:ss` formatter
/// (Requirement 5.1); the current track is flagged so the renderer highlights
/// it.
pub fn queue_rows(queue: &Queue) -> Vec<QueueRow> {
    if queue.is_empty() {
        return vec![QueueRow::Info(QUEUE_EMPTY.to_string())];
    }

    queue
        .tracks
        .iter()
        .enumerate()
        .map(|(idx, track)| QueueRow::Track {
            position: idx + 1,
            current: queue.current == Some(idx),
            text: queue_track_row_text(idx + 1, track),
        })
        .collect()
}

/// Format one Queue track row: `"{position}. {title}  —  {artist}  ({dur})"`,
/// substituting the unknown placeholder for blank text/missing duration
/// (Requirement 5.1), matching the Search/Library row conventions.
fn queue_track_row_text(position: usize, track: &QueueTrack) -> String {
    format!(
        "{}. {}  —  {}  ({})",
        position,
        row_text(&track.title),
        row_text(&track.artist),
        row_duration(track.duration_ms)
    )
}

/// Render the Queue view into `area` (Requirements 5.1, 5.10).
///
/// Draws a bordered block titled "Queue" listing every track with its 1-based
/// position; the current track is highlighted (bold + reversed) and the
/// selected row is marked with a selection highlight. An empty Queue shows the
/// empty-Queue message (Requirement 5.10).
pub fn render_queue_view(frame: &mut Frame, area: Rect, queue: &Queue, state: &QueueViewState) {
    let block = Block::default().borders(Borders::ALL).title("Queue");
    let rows = queue_rows(queue);

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| queue_row_item(row, Some(idx) == state.selected))
        .collect();

    let mut list_state = ListState::default();
    list_state.select(state.selected);
    frame.render_stateful_widget(List::new(items).block(block), area, &mut list_state);
}

/// Build a styled [`ListItem`] for one [`QueueRow`].
///
/// The current track is shown bold; the selected row is reversed. When a row is
/// both current and selected it carries both modifiers so the playing track is
/// still distinguishable while selected.
fn queue_row_item(row: &QueueRow, selected: bool) -> ListItem<'static> {
    match row {
        QueueRow::Info(text) => ListItem::new(Line::from(Span::styled(
            text.clone(),
            Style::default().add_modifier(Modifier::ITALIC),
        ))),
        QueueRow::Track {
            current, text, ..
        } => {
            let mut style = Style::default();
            if *current {
                style = style.add_modifier(Modifier::BOLD);
            }
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            // Mark the current track with a leading indicator so it is
            // identifiable even without color (Requirement 5.1 highlight).
            let marker = if *current { "\u{25B6} " } else { "  " };
            ListItem::new(Line::from(Span::styled(format!("{marker}{text}"), style)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlaybackStatus, RepeatMode};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};
    use std::path::PathBuf;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_app() -> App {
        App::new(Session_State::default())
    }

    #[test]
    fn default_resolver_maps_q_to_quit() {
        let resolver = DefaultActionResolver;
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('q'))),
            Action::Quit
        );
    }

    #[test]
    fn default_resolver_maps_ctrl_c_to_quit() {
        let resolver = DefaultActionResolver;
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(resolver.resolve(View::Search, ctrl_c), Action::Quit);
    }

    #[test]
    fn default_resolver_switches_views_by_number() {
        let resolver = DefaultActionResolver;
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('1'))),
            Action::Switch(View::Search)
        );
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('2'))),
            Action::Switch(View::Library)
        );
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('3'))),
            Action::Switch(View::Queue)
        );
    }

    #[test]
    fn default_resolver_ignores_unknown_keys() {
        let resolver = DefaultActionResolver;
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('z'))),
            Action::Ignored
        );
    }

    #[test]
    fn keybinding_resolver_maps_default_quit_and_search() {
        // Built from the default Configuration: `q` quits, `/` focuses Search.
        let resolution = keybindings::resolve_keybindings(&crate::config::Configuration::default());
        let resolver = KeybindingActionResolver::new(resolution);
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('q'))),
            Action::Quit
        );
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('/'))),
            Action::Switch(View::Search)
        );
    }

    #[test]
    fn keybinding_resolver_honors_ctrl_c_even_when_quit_is_rebound() {
        // Rebind quit away from `q`; Ctrl-C must still quit so the clean
        // shutdown sequence always runs.
        let mut config = crate::config::Configuration::default();
        config
            .keybindings
            .insert("quit".to_string(), "Q".to_string());
        let resolution = keybindings::resolve_keybindings(&config);
        let resolver = KeybindingActionResolver::new(resolution);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(resolver.resolve(View::Search, ctrl_c), Action::Quit);
    }

    #[test]
    fn keybinding_resolver_maps_custom_single_key_view_switch() {
        // A single-key override for open_queue resolves to a Queue switch in
        // any view (open_queue is a global action).
        let mut config = crate::config::Configuration::default();
        config
            .keybindings
            .insert("open_queue".to_string(), "u".to_string());
        let resolution = keybindings::resolve_keybindings(&config);
        let resolver = KeybindingActionResolver::new(resolution);
        assert_eq!(
            resolver.resolve(View::Library, press(KeyCode::Char('u'))),
            Action::Switch(View::Queue)
        );
    }

    #[test]
    fn keybinding_resolver_maps_transport_and_ignores_unbound() {
        let resolution = keybindings::resolve_keybindings(&crate::config::Configuration::default());
        let resolver = KeybindingActionResolver::new(resolution);
        // `z` is unbound and resolves to Ignored.
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char('z'))),
            Action::Ignored
        );
        // `space` is the default PlayPause binding and now resolves to the
        // transport action the loop dispatches.
        assert_eq!(
            resolver.resolve(View::Search, press(KeyCode::Char(' '))),
            Action::PlayPause
        );
    }

    #[test]
    fn handle_quit_key_requests_quit() {
        let mut app = sample_app();
        let resolver = DefaultActionResolver;
        let result = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &resolver,
        );
        assert_eq!(result, EventResult::Quit);
    }

    #[test]
    fn handle_switch_key_changes_view_and_continues() {
        let mut app = sample_app();
        let resolver = DefaultActionResolver;
        let result = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            &mut app,
            &resolver,
        );
        assert_eq!(result, EventResult::Continue);
        assert_eq!(app.view, View::Library);
    }

    #[test]
    fn enter_preview_mode_captures_failure_and_current_track() {
        // A session with a current track so preview mode captures it for the
        // restart reconnect (Requirements 4.10, 4.14).
        let session = Session_State {
            queue: SerializedQueue {
                tracks: vec![SerializedTrack {
                    track_id: "t1".to_string(),
                    title: "Song".to_string(),
                    artist: "Artist".to_string(),
                    duration_ms: Some(200_000),
                }],
            },
            current_index: Some(0),
            ..Session_State::default()
        };
        let mut app = App::new(session);
        // Simulate the engine having reported a mid-track position.
        app.playback.position_ms = 12_000;
        app.enter_preview_mode(preview::EngineFailure::StartFailure);

        assert!(app.preview_mode);
        let error = app.engine_error.as_ref().expect("an engine error");
        assert_eq!(error.failure, preview::EngineFailure::StartFailure);
        let load = error.reconnect_load().expect("current track to reload");
        assert_eq!(load.track_id, "t1");
        assert_eq!(load.position_ms, 12_000);
    }

    #[test]
    fn clear_engine_error_exits_preview_mode() {
        let mut app = sample_app();
        app.enter_preview_mode(preview::EngineFailure::LostConnection);
        assert!(app.engine_error.is_some());
        app.clear_engine_error();
        assert!(app.engine_error.is_none());
        assert!(!app.preview_mode);
    }

    #[test]
    fn restart_key_requests_restart_only_while_error_showing() {
        let mut app = sample_app();
        let resolver = DefaultActionResolver;
        let r = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

        // No error showing: `r` is an ordinary (ignored) key, not a restart.
        assert_eq!(
            handle_event(r.clone(), &mut app, &resolver),
            EventResult::Continue
        );

        // Error showing: `r` requests a restart.
        app.enter_preview_mode(preview::EngineFailure::StartFailure);
        assert_eq!(
            handle_event(r, &mut app, &resolver),
            EventResult::RestartEngine
        );
    }

    #[test]
    fn quit_still_works_while_engine_error_showing() {
        // The restart key must not shadow the quit lifecycle.
        let mut app = sample_app();
        app.enter_preview_mode(preview::EngineFailure::LostConnection);
        let resolver = DefaultActionResolver;
        let result = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &resolver,
        );
        assert_eq!(result, EventResult::Quit);
    }

    #[test]
    fn handle_key_release_is_ignored() {
        // A key *release* of the quit key must not quit; only presses act.
        let mut app = sample_app();
        let resolver = DefaultActionResolver;
        let mut key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        let result = handle_event(Event::Key(key), &mut app, &resolver);
        assert_eq!(result, EventResult::Continue);
    }

    #[test]
    fn handle_resize_event_continues_without_quitting() {
        let mut app = sample_app();
        let resolver = DefaultActionResolver;
        let result = handle_event(Event::Resize(80, 24), &mut app, &resolver);
        assert_eq!(result, EventResult::Continue);
    }

    #[test]
    fn app_reflects_restored_session_settings() {
        let session = Session_State {
            volume: 42,
            shuffle: true,
            repeat: RepeatMode::RepeatAll,
            ..Session_State::default()
        };
        let app = App::new(session);
        assert_eq!(app.playback.volume, 42);
        assert!(app.playback.shuffle);
        assert_eq!(app.playback.repeat, RepeatMode::RepeatAll);
        assert_eq!(app.playback.status, PlaybackStatus::Stopped);
        assert_eq!(app.view, View::Search);
    }

    #[test]
    fn log_path_lives_under_state_dir_with_app_log_name() {
        // Only assert the shape when a state dir is resolvable in this env.
        if let Some(path) = log_file_path() {
            assert!(path.ends_with(PathBuf::from("apple-music-tui").join(LOG_FILE)));
        }
    }

    #[test]
    fn append_log_line_creates_file_and_appends() {
        let dir = std::env::temp_dir().join(format!(
            "amtui-log-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("app.log");

        append_log_line(&path, "first");
        append_log_line(&path, "second");

        let contents = std::fs::read_to_string(&path).expect("log written");
        assert_eq!(contents, "first\nsecond\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redact_masks_jwt_like_tokens() {
        let jwt = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJBQkMiLCJleHAiOjE2MDB9.c2lnbmF0dXJlX3ZhbHVl";
        let msg = format!("request failed with token {jwt} in header");
        let redacted = redact_tokens(&msg);
        assert!(!redacted.contains(jwt), "jwt must be redacted: {redacted}");
        assert!(redacted.contains("[REDACTED]"));
        // Non-token words are preserved.
        assert!(redacted.contains("request"));
        assert!(redacted.contains("header"));
    }

    #[test]
    fn redact_masks_token_key_value_pairs() {
        let redacted = redact_tokens("GET /v1?developerToken=abc123secret&foo=bar");
        // The whole word carries the query string; the token value is masked.
        assert!(redacted.contains("developerToken=[REDACTED]"));
        assert!(!redacted.contains("abc123secret"));
    }

    #[test]
    fn redact_leaves_ordinary_text_unchanged() {
        let msg = "panic at src/ui.rs line 42: index out of bounds";
        assert_eq!(redact_tokens(msg), msg);
    }

    #[test]
    fn looks_like_jwt_rejects_short_or_non_triple_segments() {
        assert!(!looks_like_jwt("a.b.c"));
        assert!(!looks_like_jwt("not-a-token"));
        assert!(!looks_like_jwt("one.two"));
    }

    // -------------------------------------------------------------------------
    // Keyboard dispatch (transport / navigation / queue editing / chords).
    // -------------------------------------------------------------------------

    /// An app with a three-track Queue (current at index 0), a stored token so
    /// `load` requests are produced, and playing status.
    fn playing_app() -> App {
        let tracks = (0..3)
            .map(|i| SerializedTrack {
                track_id: format!("t{i}"),
                title: format!("Song {i}"),
                artist: "Artist".to_string(),
                duration_ms: Some(200_000),
            })
            .collect();
        let session = Session_State {
            queue: SerializedQueue { tracks },
            current_index: Some(0),
            ..Session_State::default()
        };
        let mut app = App::new(session);
        app.configure(10, Some("mut-token".to_string()), Some("dev-token".to_string()));
        app.playback.status = PlaybackStatus::Playing;
        app
    }

    /// Dispatch a press through the real Keybinding resolver (default config).
    fn dispatch_key(app: &mut App, code: KeyCode) -> EventResult {
        let resolution = keybindings::resolve_keybindings(&crate::config::Configuration::default());
        let resolver = KeybindingActionResolver::new(resolution);
        handle_event(Event::Key(press(code)), app, &resolver)
    }

    #[test]
    fn space_toggles_play_pause_and_enqueues_request() {
        let mut app = playing_app();
        // Playing -> Paused, enqueues Pause.
        dispatch_key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.playback.status, PlaybackStatus::Paused);
        assert_eq!(app.pending_requests, vec![RequestMethod::Pause]);

        app.pending_requests.clear();
        // Paused -> Playing, enqueues Play.
        dispatch_key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.playback.status, PlaybackStatus::Playing);
        assert_eq!(app.pending_requests, vec![RequestMethod::Play]);
    }

    #[test]
    fn next_track_advances_pointer_and_loads() {
        let mut app = playing_app();
        dispatch_key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.queue.current, Some(1));
        assert_eq!(
            app.pending_requests,
            vec![
                RequestMethod::Load {
                    track_id: "t1".to_string(),
                    position_ms: 0,
                    music_user_token: "mut-token".to_string(),
                    developer_token: "dev-token".to_string(),
                },
                RequestMethod::Play,
            ]
        );
    }

    #[test]
    fn previous_track_steps_back_within_threshold() {
        let mut app = playing_app();
        app.queue.current = Some(2);
        app.queue_view.selected = Some(2);
        app.playback.position_ms = 1_000; // within the restart threshold
        dispatch_key(&mut app, KeyCode::Char('p'));
        assert_eq!(app.queue.current, Some(1));
    }

    #[test]
    fn volume_keys_clamp_and_enqueue_set_volume() {
        let mut app = playing_app();
        app.playback.volume = 100;
        dispatch_key(&mut app, KeyCode::Char('+')); // clamps at 100
        assert_eq!(app.playback.volume, 100);
        assert_eq!(app.pending_requests, vec![RequestMethod::SetVolume { level: 100 }]);

        app.pending_requests.clear();
        dispatch_key(&mut app, KeyCode::Char('-')); // 100 - 5
        assert_eq!(app.playback.volume, 95);
        assert_eq!(app.pending_requests, vec![RequestMethod::SetVolume { level: 95 }]);
    }

    #[test]
    fn seek_forward_enqueues_seek_at_new_position() {
        let mut app = playing_app();
        app.playback.position_ms = 0;
        app.playback.duration_ms = Some(200_000);
        dispatch_key(&mut app, KeyCode::Char(']')); // +10s with default step
        assert_eq!(app.playback.position_ms, 10_000);
        assert_eq!(app.pending_requests, vec![RequestMethod::Seek { position_ms: 10_000 }]);
    }

    #[test]
    fn cycle_repeat_and_toggle_shuffle_update_state() {
        let mut app = playing_app();
        assert_eq!(app.playback.repeat, RepeatMode::Off);
        dispatch_key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.playback.repeat, RepeatMode::RepeatAll);

        assert!(!app.playback.shuffle);
        dispatch_key(&mut app, KeyCode::Char('s'));
        assert!(app.playback.shuffle);
        assert!(app.session.shuffle);
    }

    #[test]
    fn nav_down_up_moves_queue_selection() {
        let mut app = playing_app();
        app.switch_view(View::Queue);
        app.queue_view.selected = Some(0);
        dispatch_key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.queue_view.selected, Some(1));
        dispatch_key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.queue_view.selected, Some(0));
    }

    #[test]
    fn remove_from_queue_edits_the_queue() {
        let mut app = playing_app();
        app.switch_view(View::Queue);
        app.queue_view.selected = Some(1); // a non-current track
        let before = app.queue.len();
        dispatch_key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.queue.len(), before - 1);
    }

    #[test]
    fn g_chord_opens_queue_and_lone_g_is_pending() {
        let mut app = playing_app();
        // First `g`: held as a pending chord prefix, view unchanged.
        let result = dispatch_key(&mut app, KeyCode::Char('g'));
        assert_eq!(result, EventResult::Continue);
        assert!(app.pending_chord.is_some());
        assert_eq!(app.view, View::Search);
        // Then `q`: completes `g q` -> switch to the Queue view (not Quit).
        let result = dispatch_key(&mut app, KeyCode::Char('q'));
        assert_eq!(result, EventResult::Continue);
        assert!(app.pending_chord.is_none());
        assert_eq!(app.view, View::Queue);
    }

    #[test]
    fn lone_q_still_quits_when_no_chord_pending() {
        let mut app = playing_app();
        let result = dispatch_key(&mut app, KeyCode::Char('q'));
        assert_eq!(result, EventResult::Quit);
    }

    #[test]
    fn help_key_toggles_overlay() {
        let mut app = playing_app();
        assert!(!app.show_help);
        dispatch_key(&mut app, KeyCode::Char('?'));
        assert!(app.show_help);
        dispatch_key(&mut app, KeyCode::Char('?'));
        assert!(!app.show_help);
    }

    #[tokio::test]
    async fn send_pending_requests_drains_queue() {
        let mut app = playing_app();
        app.pending_requests
            .push(RequestMethod::SetVolume { level: 50 });
        // With no engine connected, requests are dropped but the queue is drained
        // so stale requests are not re-sent.
        send_pending_requests(&mut app, None).await;
        assert!(app.pending_requests.is_empty());
    }

    #[test]
    fn switching_to_search_focuses_the_query_field() {
        let mut app = playing_app();
        // Default config: `/` focuses Search.
        dispatch_key(&mut app, KeyCode::Char('/'));
        assert_eq!(app.view, View::Search);
        assert!(app.search.query_focused);
    }

    #[test]
    fn typing_while_focused_inserts_text_not_bindings() {
        let mut app = playing_app();
        app.switch_view(View::Search);
        app.search.query_focused = true;
        // `n` would be NextTrack if not focused; here it must type a character.
        for c in ['n', 'i', 'n'] {
            dispatch_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.search.query.value(), "nin");
        // No transport happened.
        assert!(app.pending_requests.is_empty());
    }

    #[test]
    fn enter_submits_search_and_enqueues_fetch() {
        let mut app = playing_app();
        app.switch_view(View::Search);
        app.search.query_focused = true;
        for c in ['j', 'a', 'z'] {
            dispatch_key(&mut app, KeyCode::Char(c));
        }
        dispatch_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.pending_fetch,
            Some(FetchRequest::Search { query: "jaz".to_string() })
        );
        // Focus leaves the field after submitting.
        assert!(!app.search.query_focused);
    }

    #[test]
    fn empty_search_shows_message_and_keeps_focus() {
        let mut app = playing_app();
        app.switch_view(View::Search);
        app.search.query_focused = true;
        dispatch_key(&mut app, KeyCode::Enter);
        assert!(app.pending_fetch.is_none());
        assert!(app.search.query_focused);
        assert!(app.search.message.is_some());
    }

    #[test]
    fn esc_leaves_the_query_field() {
        let mut app = playing_app();
        app.switch_view(View::Search);
        app.search.query_focused = true;
        dispatch_key(&mut app, KeyCode::Esc);
        assert!(!app.search.query_focused);
    }

    #[test]
    fn opening_a_library_section_enqueues_a_fetch() {
        let mut app = playing_app();
        app.switch_view(View::Library);
        // The Library view starts on the section picker; the first section in
        // display order is Playlists.
        dispatch_key(&mut app, KeyCode::Enter); // NavOpen via default binding
        assert_eq!(
            app.pending_fetch,
            Some(FetchRequest::LibrarySection {
                section: crate::api::LibrarySection::Playlists
            })
        );
    }

    #[test]
    fn apply_fetch_search_error_sets_message() {
        let mut app = playing_app();
        app.apply_fetch(FetchOutcome::Search(Err("boom".to_string())));
        assert_eq!(app.search.message.as_deref(), Some("boom"));
    }
}

#[cfg(test)]
mod view_tests {
    //! Unit tests for the Search and Library view row models and the
    //! selection/drill-down → action mapping (task 13.2). These exercise the
    //! pure logic: grouped sections with zero-result omission (Requirement 2.4),
    //! empty/prompt states (Requirements 2.3, 3.8), song→queue replacement
    //! (Requirement 2.7), album/playlist/artist drill-down (Requirements 2.8,
    //! 2.9, 3.5), and track-list → queue-in-stored-order-at-selected-position
    //! (Requirement 3.5).
    use super::*;
    use crate::api::{LibraryList, LibraryPage, LibraryRequest, SearchPagination, SearchRequest};
    use crate::api::{validate_query, SearchResults};

    fn song(id: &str, dur_ms: Option<u64>) -> Domain_Object {
        Domain_Object::Song {
            id: id.to_string(),
            name: format!("Song {id}"),
            artist_name: format!("Artist {id}"),
            duration_ms: dur_ms,
            artwork_url: None,
            is_playable: true,
        }
    }

    fn album(id: &str) -> Domain_Object {
        Domain_Object::Album {
            id: id.to_string(),
            name: format!("Album {id}"),
            artist_name: format!("Artist {id}"),
            artwork_url: None,
        }
    }

    fn artist(id: &str) -> Domain_Object {
        Domain_Object::Artist {
            id: id.to_string(),
            name: format!("Artist {id}"),
            artwork_url: None,
        }
    }

    fn playlist(id: &str) -> Domain_Object {
        Domain_Object::Playlist {
            id: id.to_string(),
            name: format!("Playlist {id}"),
            artwork_url: None,
        }
    }

    fn pagination(results: SearchResults) -> SearchPagination {
        let query = validate_query("q").unwrap();
        let request = SearchRequest::new(&query, "us");
        SearchPagination::new(&request, results, Default::default())
    }

    // --- object_row_text (R2.3 / R3.4) ---

    #[test]
    fn song_row_shows_title_artist_and_formatted_duration() {
        let text = object_row_text(&song("1", Some(185_000)));
        assert!(text.contains("Song 1"));
        assert!(text.contains("Artist 1"));
        // 185s -> 3:05 via the shared formatter (Requirement 3.4).
        assert!(text.contains("3:05"), "row was {text}");
    }

    #[test]
    fn song_row_missing_duration_shows_unknown() {
        let text = object_row_text(&song("1", None));
        assert!(text.contains(ROW_UNKNOWN));
    }

    #[test]
    fn album_row_shows_name_and_artist_only() {
        let text = object_row_text(&album("a"));
        assert!(text.contains("Album a"));
        assert!(text.contains("Artist a"));
    }

    #[test]
    fn artist_and_playlist_rows_show_name() {
        assert_eq!(object_row_text(&artist("x")), "Artist x");
        assert_eq!(object_row_text(&playlist("y")), "Playlist y");
    }

    #[test]
    fn blank_name_falls_back_to_unknown() {
        let s = Domain_Object::Song {
            id: "1".to_string(),
            name: String::new(),
            artist_name: String::new(),
            duration_ms: None,
            artwork_url: None,
            is_playable: true,
        };
        let text = object_row_text(&s);
        assert!(text.contains(ROW_UNKNOWN));
    }

    // --- Search rows: prompt / no-results / grouped sections / omission ---

    #[test]
    fn search_view_before_any_search_shows_prompt() {
        let state = SearchViewState::new();
        let rows = state.rows();
        assert_eq!(rows, vec![SearchRow::Info(SEARCH_PROMPT.to_string())]);
    }

    #[test]
    fn search_view_zero_results_shows_no_results_message() {
        let mut state = SearchViewState::new();
        state.set_results(pagination(SearchResults::default()));
        let rows = state.rows();
        assert_eq!(rows, vec![SearchRow::Info(SEARCH_NO_RESULTS.to_string())]);
        // The exact user-facing zero-results message (Requirement 2.4).
        assert_eq!(SEARCH_NO_RESULTS, "No results matched your query");
        // Nothing selectable when there are no results.
        assert_eq!(state.selected, None);
    }

    #[test]
    fn search_view_omits_every_zero_result_section_when_all_empty() {
        // When all four resource types return zero results, no section header
        // is rendered and only the single no-results message is shown
        // (Requirement 2.4: omit zero-result sections).
        let mut state = SearchViewState::new();
        state.set_results(pagination(SearchResults::default()));
        let rows = state.rows();
        assert!(!rows.iter().any(|r| matches!(r, SearchRow::Header(_))));
        assert_eq!(rows, vec![SearchRow::Info(SEARCH_NO_RESULTS.to_string())]);
    }

    #[test]
    fn search_view_groups_sections_and_omits_empty_ones() {
        // Only songs and artists have results; albums and playlists are empty
        // and must be omitted entirely (Requirement 2.4).
        let results = SearchResults {
            songs: vec![song("1", Some(60_000)), song("2", Some(120_000))],
            albums: vec![],
            artists: vec![artist("a")],
            playlists: vec![],
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        let rows = state.rows();

        // Headers present only for non-empty sections.
        let headers: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                SearchRow::Header(h) => Some(h.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["Songs", "Artists"]);

        // No Albums/Playlists header anywhere.
        assert!(!rows.iter().any(|r| matches!(r, SearchRow::Header(h) if h == "Albums")));
        assert!(!rows.iter().any(|r| matches!(r, SearchRow::Header(h) if h == "Playlists")));

        // Three selectable results: 2 songs + 1 artist.
        let selectable = rows.iter().filter(|r| r.is_selectable()).count();
        assert_eq!(selectable, 3);

        // First result selected by default (Requirement 2.3).
        assert_eq!(state.selected, Some(0));
    }

    // --- Search selection navigation (edge clamp, R7.2) ---

    #[test]
    fn search_selection_clamps_at_edges() {
        let results = SearchResults {
            songs: vec![song("1", None), song("2", None), song("3", None)],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        assert_eq!(state.selected, Some(0));
        state.select_previous(); // already at top, clamps
        assert_eq!(state.selected, Some(0));
        state.select_next();
        state.select_next();
        assert_eq!(state.selected, Some(2));
        state.select_next(); // at bottom, clamps
        assert_eq!(state.selected, Some(2));
    }

    #[test]
    fn search_selected_object_maps_back_to_the_right_resource() {
        let results = SearchResults {
            songs: vec![song("s1", None)],
            albums: vec![album("al1")],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        // Row order: [Header Songs, song s1, Header Albums, album al1].
        // Selectable order: [s1, al1].
        assert!(matches!(
            state.selected_object(),
            Some(Domain_Object::Song { id, .. }) if id == "s1"
        ));
        state.select_next();
        assert!(matches!(
            state.selected_object(),
            Some(Domain_Object::Album { id, .. }) if id == "al1"
        ));
    }

    // --- Search activation (R2.7, R2.8, R2.9) ---

    #[test]
    fn selecting_a_song_replaces_queue_and_plays() {
        let results = SearchResults {
            songs: vec![song("s1", Some(200_000))],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        match state.activate_selection().unwrap() {
            SearchActivation::PlaySong(queue) => {
                assert_eq!(queue.tracks.len(), 1);
                assert_eq!(queue.current, Some(0));
                assert_eq!(queue.tracks[0].track_id, "s1");
            }
            other => panic!("expected PlaySong, got {other:?}"),
        }
    }

    #[test]
    fn selecting_an_album_drills_into_tracks() {
        let results = SearchResults {
            albums: vec![album("al9")],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        assert_eq!(
            state.activate_selection().unwrap(),
            SearchActivation::OpenTracks {
                kind: LibrarySection::Albums,
                id: "al9".to_string()
            }
        );
    }

    #[test]
    fn selecting_a_playlist_drills_into_tracks() {
        let results = SearchResults {
            playlists: vec![playlist("p3")],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        assert_eq!(
            state.activate_selection().unwrap(),
            SearchActivation::OpenTracks {
                kind: LibrarySection::Playlists,
                id: "p3".to_string()
            }
        );
    }

    #[test]
    fn selecting_an_artist_drills_into_artist() {
        let results = SearchResults {
            artists: vec![artist("ar7")],
            ..SearchResults::default()
        };
        let mut state = SearchViewState::new();
        state.set_results(pagination(results));
        assert_eq!(
            state.activate_selection().unwrap(),
            SearchActivation::OpenArtist {
                id: "ar7".to_string()
            }
        );
    }

    #[test]
    fn search_load_more_error_appends_info_row() {
        let results = SearchResults {
            songs: vec![song("s1", None)],
            ..SearchResults::default()
        };
        let query = validate_query("q").unwrap();
        let request = SearchRequest::new(&query, "us");
        let more = crate::api::SectionMore {
            songs: true,
            ..Default::default()
        };
        let mut page = SearchPagination::new(&request, results, more);
        // Trigger a next-page request and then fail it to arm the error line.
        let _ = page.reach_end_of_section(SearchSection::Songs);
        page.record_failure(SearchSection::Songs);
        let mut state = SearchViewState::new();
        state.set_results(page);
        let rows = state.rows();
        assert!(rows
            .iter()
            .any(|r| matches!(r, SearchRow::Info(t) if t == SEARCH_LOAD_MORE_ERROR)));
    }

    // --- tracks_to_queue (R3.5) ---

    #[test]
    fn tracks_to_queue_replaces_in_stored_order_at_selected_position() {
        let tracks = vec![
            song("t0", Some(60_000)),
            song("t1", Some(60_000)),
            song("t2", Some(60_000)),
        ];
        let queue = tracks_to_queue(&tracks, 1);
        // Stored order preserved.
        let ids: Vec<&str> = queue.tracks.iter().map(|t| t.track_id.as_str()).collect();
        assert_eq!(ids, vec!["t0", "t1", "t2"]);
        // Current starts at the selected position.
        assert_eq!(queue.current, Some(1));
    }

    #[test]
    fn tracks_to_queue_clamps_out_of_range_selection() {
        let tracks = vec![song("a", None), song("b", None)];
        let queue = tracks_to_queue(&tracks, 99);
        assert_eq!(queue.current, Some(1));
    }

    #[test]
    fn tracks_to_queue_empty_input_has_no_current() {
        let queue = tracks_to_queue(&[], 0);
        assert!(queue.tracks.is_empty());
        assert_eq!(queue.current, None);
    }

    #[test]
    fn tracks_to_queue_skips_non_song_objects_and_remaps_selection() {
        // A stray non-song (artist) before the selected song must be skipped and
        // the selection remapped onto the kept songs.
        let objects = vec![artist("x"), song("s1", None), song("s2", None)];
        // Select index 2 (song s2).
        let queue = tracks_to_queue(&objects, 2);
        let ids: Vec<&str> = queue.tracks.iter().map(|t| t.track_id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2"]);
        // s2 is the second kept song (index 1).
        assert_eq!(queue.current, Some(1));
    }

    // --- Library view: sections, drill-down, loading, empty ---

    fn library_page(count: usize, more: bool) -> LibraryPage {
        LibraryPage {
            items: (0..count).map(|i| song(&format!("l{i}"), Some(60_000))).collect(),
            dropped: vec![],
            more_available: more,
        }
    }

    #[test]
    fn library_view_starts_on_section_picker() {
        let state = LibraryViewState::new();
        match state.focus {
            LibraryFocus::Sections { selected } => assert_eq!(selected, 0),
            _ => panic!("expected the section picker"),
        }
    }

    #[test]
    fn library_section_picker_activation_opens_that_section() {
        let state = LibraryViewState::new();
        // Section 0 is Playlists (Requirement 3.1 order).
        assert_eq!(
            state.activate_selection().unwrap(),
            LibraryActivation::OpenSection(LibrarySection::Playlists)
        );
    }

    #[test]
    fn library_section_navigation_clamps() {
        let mut state = LibraryViewState::new();
        state.select_previous(); // clamps at 0
        if let LibraryFocus::Sections { selected } = state.focus {
            assert_eq!(selected, 0);
        }
        for _ in 0..10 {
            state.select_next();
        }
        // Clamps at the last section (Songs, index 3).
        assert_eq!(
            state.activate_selection().unwrap(),
            LibraryActivation::OpenSection(LibrarySection::Songs)
        );
    }

    #[test]
    fn library_open_track_list_selects_first_item() {
        let mut list = LibraryList::new(LibraryRequest::tracks(
            LibrarySection::Albums,
            "l.abc",
            0,
        ));
        let _ = list.first_page_request();
        list.apply_page(library_page(3, false));
        let mut state = LibraryViewState::new();
        state.open_list(list, true);
        match &state.focus {
            LibraryFocus::List { selected, .. } => assert_eq!(*selected, Some(0)),
            _ => panic!("expected a list"),
        }
    }

    #[test]
    fn library_selecting_track_replaces_queue_in_stored_order_at_position() {
        let mut list = LibraryList::new(LibraryRequest::tracks(
            LibrarySection::Playlists,
            "p.1",
            0,
        ));
        let _ = list.first_page_request();
        list.apply_page(library_page(4, false));
        let mut state = LibraryViewState::new();
        state.open_list(list, true);
        state.select_next();
        state.select_next(); // selected index 2
        match state.activate_selection().unwrap() {
            LibraryActivation::PlayTracks(queue) => {
                let ids: Vec<&str> =
                    queue.tracks.iter().map(|t| t.track_id.as_str()).collect();
                assert_eq!(ids, vec!["l0", "l1", "l2", "l3"]);
                assert_eq!(queue.current, Some(2));
            }
            other => panic!("expected PlayTracks, got {other:?}"),
        }
    }

    #[test]
    fn library_section_list_of_albums_drills_down_on_open() {
        // A section list (not a track list) of albums: opening an item drills
        // into its tracks (Requirement 3.5).
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Albums, 0));
        let _ = list.first_page_request();
        list.apply_page(LibraryPage {
            items: vec![album("al5")],
            dropped: vec![],
            more_available: false,
        });
        let mut state = LibraryViewState::new();
        state.open_list(list, false);
        assert_eq!(
            state.activate_selection().unwrap(),
            LibraryActivation::OpenTracks {
                kind: LibrarySection::Albums,
                id: "al5".to_string()
            }
        );
    }

    #[test]
    fn library_section_list_of_songs_plays_on_open() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let _ = list.first_page_request();
        list.apply_page(LibraryPage {
            items: vec![song("ls1", Some(90_000))],
            dropped: vec![],
            more_available: false,
        });
        let mut state = LibraryViewState::new();
        state.open_list(list, false);
        match state.activate_selection().unwrap() {
            LibraryActivation::PlaySong(queue) => {
                assert_eq!(queue.tracks.len(), 1);
                assert_eq!(queue.tracks[0].track_id, "ls1");
                assert_eq!(queue.current, Some(0));
            }
            other => panic!("expected PlaySong, got {other:?}"),
        }
    }

    #[test]
    fn library_empty_list_has_no_selection_and_no_activation() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let _ = list.first_page_request();
        list.apply_page(library_page(0, false)); // empty page
        let mut state = LibraryViewState::new();
        state.open_list(list, false);
        match &state.focus {
            LibraryFocus::List { selected, .. } => assert_eq!(*selected, None),
            _ => panic!("expected a list"),
        }
        // Nothing to open in an empty list (Requirement 3.8).
        assert!(state.activate_selection().is_none());
    }

    // --- Empty-state messages (R3.8) ---

    #[test]
    fn library_empty_section_shows_exact_empty_message() {
        // A Library section that returned zero items shows the exact
        // empty-state message in place of the item list (Requirement 3.8).
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let _ = list.first_page_request();
        list.apply_page(library_page(0, false)); // loaded, no items, no error
        assert!(list.is_empty());
        assert!(!list.has_load_error());
        assert_eq!(
            library_empty_message(list.has_load_error()),
            LIBRARY_EMPTY
        );
        assert_eq!(LIBRARY_EMPTY, "This section is empty");
    }

    #[test]
    fn library_empty_playlist_shows_exact_empty_message() {
        // An opened Library playlist with no tracks shows the empty-state
        // message (Requirement 3.8).
        let mut list = LibraryList::new(LibraryRequest::tracks(
            LibrarySection::Playlists,
            "p.empty",
            0,
        ));
        let _ = list.first_page_request();
        list.apply_page(library_page(0, false));
        assert!(list.is_empty());
        assert_eq!(library_empty_message(list.has_load_error()), LIBRARY_EMPTY);
    }

    #[test]
    fn library_empty_album_shows_exact_empty_message() {
        // An opened Library album with no tracks shows the empty-state
        // message (Requirement 3.8).
        let mut list = LibraryList::new(LibraryRequest::tracks(
            LibrarySection::Albums,
            "al.empty",
            0,
        ));
        let _ = list.first_page_request();
        list.apply_page(library_page(0, false));
        assert!(list.is_empty());
        assert_eq!(library_empty_message(list.has_load_error()), LIBRARY_EMPTY);
    }

    #[test]
    fn library_empty_because_of_load_error_shows_load_error_not_empty_message() {
        // When the list is empty because the request failed (no prior items),
        // the load-error message is shown instead of the empty-state message
        // (Requirement 3.7 vs 3.8), so the two are not confused.
        assert_eq!(library_empty_message(true), LIBRARY_LOAD_ERROR);
        assert_ne!(library_empty_message(true), LIBRARY_EMPTY);
    }

    #[test]
    fn library_loading_first_page_is_reflected_in_state() {
        // Before the first page arrives the list is loading and empty, which the
        // renderer shows as a loading indicator (Requirement 3.2).
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Albums, 0));
        let _ = list.first_page_request();
        assert!(list.state().loading);
        assert!(list.is_empty());
    }

    #[test]
    fn library_back_to_sections_restores_picker() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let _ = list.first_page_request();
        list.apply_page(library_page(2, false));
        let mut state = LibraryViewState::new();
        state.open_list(list, false);
        state.back_to_sections(1);
        match state.focus {
            LibraryFocus::Sections { selected } => assert_eq!(selected, 1),
            _ => panic!("expected the section picker"),
        }
    }
}

#[cfg(test)]
mod queue_view_tests {
    //! Unit tests for the Queue view row model, selection/edit mapping, and the
    //! Now_Playing_Bar sync (task 13.3). These exercise the pure logic:
    //! 1-based positions with the current track flagged (Requirement 5.1), the
    //! empty-Queue message (Requirement 5.10), the edit → Queue-operation
    //! mapping (Requirements 5.2–5.5, 5.11, 5.12), keeping playback mirrored
    //! across edits and view switches (Requirements 6.6, 4.9), and the session
    //! round-trip of the restored Queue (Requirement 9.4).
    use super::*;
    use crate::domain::PlaybackStatus;

    fn qtrack(id: &str, dur_ms: Option<u64>) -> QueueTrack {
        QueueTrack {
            track_id: id.to_string(),
            title: format!("Title {id}"),
            artist: format!("Artist {id}"),
            duration_ms: dur_ms,
        }
    }

    fn queue_of(ids: &[&str]) -> Queue {
        let tracks = ids.iter().map(|id| qtrack(id, Some(180_000))).collect();
        Queue {
            tracks,
            current: Some(0),
            pre_shuffle_order: None,
        }
    }

    fn ids(q: &Queue) -> Vec<String> {
        q.tracks.iter().map(|t| t.track_id.clone()).collect()
    }

    // --- queue_rows (R5.1, R5.10) ---

    #[test]
    fn empty_queue_shows_empty_message() {
        let q = Queue::new();
        assert_eq!(queue_rows(&q), vec![QueueRow::Info(QUEUE_EMPTY.to_string())]);
        // The exact user-facing empty-Queue message (Requirement 5.10).
        assert_eq!(QUEUE_EMPTY, "The Queue is empty");
    }

    #[test]
    fn queue_rows_number_tracks_from_one_and_flag_current() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(1); // "b" is current
        let rows = queue_rows(&q);
        assert_eq!(rows.len(), 3);
        // Positions are 1-based.
        match &rows[0] {
            QueueRow::Track { position, current, .. } => {
                assert_eq!(*position, 1);
                assert!(!*current);
            }
            other => panic!("expected a track row, got {other:?}"),
        }
        // The current track (index 1) is flagged.
        match &rows[1] {
            QueueRow::Track { position, current, .. } => {
                assert_eq!(*position, 2);
                assert!(*current);
            }
            other => panic!("expected a track row, got {other:?}"),
        }
    }

    #[test]
    fn queue_row_text_shows_position_title_artist_and_formatted_duration() {
        let q = queue_of(&["x"]);
        let rows = queue_rows(&q);
        match &rows[0] {
            QueueRow::Track { text, .. } => {
                assert!(text.starts_with("1. "), "row was {text}");
                assert!(text.contains("Title x"));
                assert!(text.contains("Artist x"));
                // 180_000 ms -> 3:00 via the shared formatter (Requirement 5.1).
                assert!(text.contains("3:00"), "row was {text}");
            }
            other => panic!("expected a track row, got {other:?}"),
        }
    }

    #[test]
    fn queue_row_missing_duration_and_blank_text_fall_back_to_unknown() {
        let mut q = Queue::new();
        q.tracks.push(QueueTrack {
            track_id: "1".to_string(),
            title: String::new(),
            artist: String::new(),
            duration_ms: None,
        });
        q.current = Some(0);
        let rows = queue_rows(&q);
        match &rows[0] {
            QueueRow::Track { text, .. } => assert!(text.contains(ROW_UNKNOWN)),
            other => panic!("expected a track row, got {other:?}"),
        }
    }

    // --- selection navigation (R7.2 edge-clamp) ---

    #[test]
    fn queue_view_for_queue_selects_current_track() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(2);
        let state = QueueViewState::for_queue(&q);
        assert_eq!(state.selected, Some(2));
    }

    #[test]
    fn queue_view_for_empty_queue_selects_nothing() {
        let state = QueueViewState::for_queue(&Queue::new());
        assert_eq!(state.selected, None);
    }

    #[test]
    fn queue_selection_clamps_at_edges() {
        let q = queue_of(&["a", "b", "c"]);
        let mut state = QueueViewState { selected: Some(0) };
        state.select_previous(&q); // already at top, clamps
        assert_eq!(state.selected, Some(0));
        state.select_next(&q);
        state.select_next(&q);
        assert_eq!(state.selected, Some(2));
        state.select_next(&q); // at bottom, clamps
        assert_eq!(state.selected, Some(2));
    }

    #[test]
    fn queue_clamp_to_shrinks_selection_and_clears_on_empty() {
        let mut state = QueueViewState { selected: Some(5) };
        let q = queue_of(&["a", "b"]);
        state.clamp_to(&q);
        assert_eq!(state.selected, Some(1));
        state.clamp_to(&Queue::new());
        assert_eq!(state.selected, None);
    }

    // --- edit → operation mapping (R5.2–R5.5, R5.11, R5.12) ---

    #[test]
    fn play_next_edit_inserts_after_current() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(1); // "b"
        let outcome = QueueEdit::PlayNext(qtrack("x", None)).apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::Edited);
        assert_eq!(ids(&q), ["a", "b", "x", "c"]);
    }

    #[test]
    fn add_to_queue_edit_appends() {
        let mut q = queue_of(&["a", "b"]);
        let outcome = QueueEdit::AddToQueue(qtrack("x", None)).apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::Edited);
        assert_eq!(ids(&q), ["a", "b", "x"]);
    }

    #[test]
    fn remove_non_current_edit_preserves_pointer() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(2); // "c"
        let outcome = QueueEdit::Remove { index: 0 }.apply(&mut q); // remove "a"
        assert_eq!(outcome, QueueEditOutcome::Edited);
        assert_eq!(ids(&q), ["b", "c"]);
        // Pointer still on "c" (now index 1).
        assert_eq!(q.current, Some(1));
    }

    #[test]
    fn remove_current_edit_advances_pointer() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(1); // "b"
        let outcome = QueueEdit::Remove { index: 1 }.apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::CurrentAdvanced { new_current: 1 });
        assert_eq!(ids(&q), ["a", "c"]);
        assert_eq!(q.current, Some(1));
    }

    #[test]
    fn remove_last_current_edit_empties_queue() {
        let mut q = queue_of(&["a"]);
        q.current = Some(0);
        let outcome = QueueEdit::Remove { index: 0 }.apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::Emptied);
        assert!(q.is_empty());
        assert_eq!(q.current, None);
    }

    #[test]
    fn remove_out_of_range_edit_is_noop() {
        let mut q = queue_of(&["a"]);
        let outcome = QueueEdit::Remove { index: 9 }.apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::NoOp);
        assert_eq!(ids(&q), ["a"]);
    }

    #[test]
    fn move_edit_relocates_track() {
        let mut q = queue_of(&["a", "b", "c", "d"]);
        q.current = Some(0); // "a"
        // Move "b" (index 1) to position 4 (end).
        let outcome = QueueEdit::Move { from: 1, target: 4 }.apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::Edited);
        assert_eq!(ids(&q), ["a", "c", "d", "b"]);
    }

    #[test]
    fn add_edit_at_limit_is_rejected() {
        let mut q = Queue::new();
        q.tracks = (0..crate::queue::MAX_QUEUE_LEN)
            .map(|i| qtrack(&i.to_string(), None))
            .collect();
        q.current = Some(0);
        let before = q.len();
        let outcome = QueueEdit::AddToQueue(qtrack("overflow", None)).apply(&mut q);
        assert_eq!(outcome, QueueEditOutcome::Rejected);
        assert_eq!(q.len(), before);
    }

    // --- QueueViewState edit builders ---

    #[test]
    fn remove_selected_targets_the_selection() {
        let state = QueueViewState { selected: Some(2) };
        assert_eq!(state.remove_selected(), Some(QueueEdit::Remove { index: 2 }));
        assert_eq!(QueueViewState::new().remove_selected(), None);
    }

    #[test]
    fn move_selected_up_and_down_build_1_based_targets() {
        let state = QueueViewState { selected: Some(2) }; // 0-based index 2
        // Up one: 1-based target `from` (= 2).
        assert_eq!(
            state.move_selected_up(),
            Some(QueueEdit::Move { from: 2, target: 2 })
        );
        // Down one: 1-based target `from + 2` (= 4).
        assert_eq!(
            state.move_selected_down(),
            Some(QueueEdit::Move { from: 2, target: 4 })
        );
    }

    #[test]
    fn move_selected_up_on_first_track_is_none() {
        let state = QueueViewState { selected: Some(0) };
        assert_eq!(state.move_selected_up(), None);
    }

    #[test]
    fn move_selected_up_down_apply_as_expected() {
        // Verify the up/down builders actually relocate the track when applied.
        let mut q = queue_of(&["a", "b", "c"]);
        q.current = Some(0);
        let state = QueueViewState { selected: Some(2) }; // "c"
        let up = state.move_selected_up().unwrap();
        up.apply(&mut q);
        assert_eq!(ids(&q), ["a", "c", "b"]);
    }

    // --- Now_Playing_Bar sync (R6.1, R6.3, R6.6) ---

    #[test]
    fn reflect_current_track_copies_track_fields() {
        let mut playback = Playback_State::default();
        let q = queue_of(&["a"]);
        reflect_current_track(&mut playback, &q);
        assert_eq!(playback.track_id.as_deref(), Some("a"));
        assert_eq!(playback.title, "Title a");
        assert_eq!(playback.artist, "Artist a");
        assert_eq!(playback.duration_ms, Some(180_000));
        assert_eq!(playback.position_ms, 0);
    }

    #[test]
    fn reflect_current_track_none_shows_nothing_playing() {
        let mut playback = Playback_State {
            track_id: Some("a".to_string()),
            title: "Title a".to_string(),
            position_ms: 42_000,
            ..Playback_State::default()
        };
        reflect_current_track(&mut playback, &Queue::new());
        assert!(playback.track_id.is_none());
        assert!(playback.title.is_empty());
        assert_eq!(playback.position_ms, 0);
    }

    #[test]
    fn reflect_current_track_same_track_keeps_position() {
        // Re-syncing the same current track must not rewind a playing position
        // (so the >=1/sec tick keeps advancing; Requirement 6.2).
        let q = queue_of(&["a"]);
        let mut playback = Playback_State::default();
        reflect_current_track(&mut playback, &q);
        playback.position_ms = 30_000;
        reflect_current_track(&mut playback, &q);
        assert_eq!(playback.position_ms, 30_000);
    }

    #[test]
    fn apply_queue_edit_updates_playback_and_session() {
        // An edit that changes the current track reflects it into the bar and
        // mirrors the Queue into the session (Requirements 6.6, 9.1).
        let mut session = Session_State {
            queue: SerializedQueue {
                tracks: vec![
                    SerializedTrack {
                        track_id: "a".to_string(),
                        title: "Title a".to_string(),
                        artist: "Artist a".to_string(),
                        duration_ms: Some(180_000),
                    },
                    SerializedTrack {
                        track_id: "b".to_string(),
                        title: "Title b".to_string(),
                        artist: "Artist b".to_string(),
                        duration_ms: Some(180_000),
                    },
                ],
            },
            current_index: Some(0),
            ..Session_State::default()
        };
        let mut app = App::new(std::mem::take(&mut session));
        assert_eq!(app.playback.track_id.as_deref(), Some("a"));

        // Remove the current track "a": pointer advances to "b".
        let outcome = app.apply_queue_edit(QueueEdit::Remove { index: 0 });
        assert_eq!(outcome, QueueEditOutcome::CurrentAdvanced { new_current: 0 });
        // Bar now reflects "b".
        assert_eq!(app.playback.track_id.as_deref(), Some("b"));
        // Session mirrors the edited Queue.
        assert_eq!(app.session.queue.tracks.len(), 1);
        assert_eq!(app.session.queue.tracks[0].track_id, "b");
        assert_eq!(app.session.current_index, Some(0));
        // Queue-view selection stayed valid.
        assert_eq!(app.queue_view.selected, Some(0));
    }

    #[test]
    fn switching_view_does_not_touch_queue_or_playback() {
        // Requirement 4.9: switching views keeps playback and the Queue intact.
        let q = queue_of(&["a", "b"]);
        let mut session = Session_State::default();
        sync_session_queue(&mut session, &q);
        session.current_index = Some(0);
        let mut app = App::new(session);
        app.playback.status = PlaybackStatus::Playing;
        app.playback.position_ms = 12_345;

        let queue_before = app.queue.clone();
        app.switch_view(View::Library);
        app.switch_view(View::Queue);
        app.switch_view(View::Search);

        assert_eq!(app.queue, queue_before);
        assert_eq!(app.playback.status, PlaybackStatus::Playing);
        assert_eq!(app.playback.position_ms, 12_345);
    }

    // --- session round-trip (R9.4) ---

    #[test]
    fn queue_from_session_rebuilds_tracks_and_pointer() {
        let q = queue_of(&["a", "b", "c"]);
        let mut session = Session_State::default();
        sync_session_queue(&mut session, &q);
        session.current_index = Some(2);
        let rebuilt = queue_from_session(&session);
        assert_eq!(ids(&rebuilt), ["a", "b", "c"]);
        assert_eq!(rebuilt.current, Some(2));
    }

    #[test]
    fn queue_from_session_clamps_out_of_range_pointer() {
        let q = queue_of(&["a", "b"]);
        let mut session = Session_State::default();
        sync_session_queue(&mut session, &q);
        session.current_index = Some(99);
        let rebuilt = queue_from_session(&session);
        assert_eq!(rebuilt.current, Some(1));
    }

    #[test]
    fn queue_from_empty_session_has_no_current() {
        let rebuilt = queue_from_session(&Session_State::default());
        assert!(rebuilt.is_empty());
        assert_eq!(rebuilt.current, None);
    }
}
