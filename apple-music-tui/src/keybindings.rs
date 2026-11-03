//! Keybinding resolver (defaults, overrides, conflict/unknown warnings).
//!
//! Builds the action map for the TUI from the default Keybindings overlaid with
//! the custom bindings in the [`Configuration`](crate::config::Configuration)
//! (Requirement 7.4). Every rendered user action from Requirements 1–6 has a
//! default single- or two-key binding so it can be triggered without pointer
//! input (Requirement 7.1), including the default list-navigation bindings
//! (`j`/Down, `k`/Up, `l`/Right/Enter, `h`/Left) whose selection clamps at the
//! first and last item (Requirement 7.2).
//!
//! The resolver is pure and testable: [`resolve_keybindings`] takes the
//! configured overlay and returns a [`Resolution`] carrying the effective
//! bindings plus any [`KeybindingWarning`]s. It warns at startup and falls back
//! to the default binding when:
//! - a custom entry names an unknown action or an unrecognized key
//!   (Requirement 7.8), and
//! - two actions in the same view would share a key or key sequence, counting
//!   conflicts between a custom binding and the default binding of an unlisted
//!   action (Requirement 7.5).
//!
//! Text input fields (such as the search query field) consume printable keys as
//! text instead of triggering their bindings and return focus to the enclosing
//! view on Esc (Requirement 7.7); see [`TextInputField`].

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::Configuration;

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Every user action the TUI renders and binds to the keyboard (Requirement
/// 7.1). Each action belongs to a [`View`] (or is global) and carries a default
/// Keybinding. Interactions inside the Apple-hosted sign-in page are excluded
/// (Requirement 7.1), so there is no action for them here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    // --- Global list navigation (Requirement 7.2) ---
    /// Move the selection down one item (`j` / Down).
    NavDown,
    /// Move the selection up one item (`k` / Up).
    NavUp,
    /// Open the selected item (`l` / Right / Enter).
    NavOpen,
    /// Return to the previous view (`h` / Left).
    NavBack,

    // --- Global ---
    /// Quit the application, saving session state (Requirement 7.6, default `q`).
    Quit,
    /// Show the help overlay (Requirement 7.3, default `?`).
    Help,
    /// Focus the search query field (Requirement 2).
    Search,
    /// Open the Library view (Requirement 3).
    OpenLibrary,
    /// Open the Queue view (Requirement 5.1).
    OpenQueue,
    /// Sign out of the current Apple ID session (Requirement 1.12).
    SignOut,

    // --- Playback transport (Requirement 4) ---
    /// Toggle play/pause (Requirement 4.2, default Space).
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
    /// Toggle shuffle mode (Requirement 5.6, 5.7).
    ToggleShuffle,
    /// Cycle the repeat mode off → all → one (Requirement 5.8).
    CycleRepeat,

    // --- Library (Requirement 3) ---
    /// Refresh the current Library section (Requirement 3.6).
    RefreshLibrary,

    // --- Queue editing (Requirement 5) ---
    /// Insert the selected track directly after the current track (Requirement 5.2).
    PlayNext,
    /// Append the selected track to the end of the Queue (Requirement 5.3).
    AddToQueue,
    /// Remove the selected track from the Queue (Requirements 5.4, 5.11).
    RemoveFromQueue,
    /// Move the selected Queue track up one position (Requirement 5.5).
    MoveUp,
    /// Move the selected Queue track down one position (Requirement 5.5).
    MoveDown,
}

impl Action {
    /// The stable name used in the Config_File to bind this action
    /// (Requirement 7.4) and shown beside it in the help overlay (Requirement
    /// 7.3). An unknown name in the config is reported as a warning (Requirement
    /// 7.8).
    pub const fn name(self) -> &'static str {
        match self {
            Action::NavDown => "nav_down",
            Action::NavUp => "nav_up",
            Action::NavOpen => "nav_open",
            Action::NavBack => "nav_back",
            Action::Quit => "quit",
            Action::Help => "help",
            Action::Search => "search",
            Action::OpenLibrary => "open_library",
            Action::OpenQueue => "open_queue",
            Action::SignOut => "sign_out",
            Action::PlayPause => "play_pause",
            Action::NextTrack => "next_track",
            Action::PreviousTrack => "previous_track",
            Action::SeekForward => "seek_forward",
            Action::SeekBackward => "seek_backward",
            Action::VolumeUp => "volume_up",
            Action::VolumeDown => "volume_down",
            Action::ToggleShuffle => "toggle_shuffle",
            Action::CycleRepeat => "cycle_repeat",
            Action::RefreshLibrary => "refresh_library",
            Action::PlayNext => "play_next",
            Action::AddToQueue => "add_to_queue",
            Action::RemoveFromQueue => "remove_from_queue",
            Action::MoveUp => "move_up",
            Action::MoveDown => "move_down",
        }
    }

    /// A human-readable label for this action, shown beside its key sequences
    /// in the help overlay (Requirement 7.3). Unlike [`name`](Action::name),
    /// which is the stable config identifier, this is meant for display.
    pub const fn display_name(self) -> &'static str {
        match self {
            Action::NavDown => "Move down",
            Action::NavUp => "Move up",
            Action::NavOpen => "Open selected",
            Action::NavBack => "Back",
            Action::Quit => "Quit",
            Action::Help => "Help",
            Action::Search => "Search",
            Action::OpenLibrary => "Open Library",
            Action::OpenQueue => "Open Queue",
            Action::SignOut => "Sign out",
            Action::PlayPause => "Play / pause",
            Action::NextTrack => "Next track",
            Action::PreviousTrack => "Previous track",
            Action::SeekForward => "Seek forward",
            Action::SeekBackward => "Seek backward",
            Action::VolumeUp => "Volume up",
            Action::VolumeDown => "Volume down",
            Action::ToggleShuffle => "Toggle shuffle",
            Action::CycleRepeat => "Cycle repeat",
            Action::RefreshLibrary => "Refresh Library",
            Action::PlayNext => "Play next",
            Action::AddToQueue => "Add to Queue",
            Action::RemoveFromQueue => "Remove from Queue",
            Action::MoveUp => "Move up in Queue",
            Action::MoveDown => "Move down in Queue",
        }
    }

    /// Look up an action by its config name, or `None` if the name is unknown
    /// (Requirement 7.8).
    pub fn from_name(name: &str) -> Option<Action> {
        ALL_ACTIONS.iter().copied().find(|a| a.name() == name)
    }

    /// The view this action is active in (Requirement 7.5 conflicts are scoped
    /// per view). Global and navigation actions are active in every view.
    pub const fn view(self) -> View {
        match self {
            Action::RefreshLibrary => View::Library,
            Action::PlayNext
            | Action::AddToQueue
            | Action::RemoveFromQueue
            | Action::MoveUp
            | Action::MoveDown => View::Queue,
            _ => View::Global,
        }
    }
}

/// The set of actions in a stable order, used to seed the defaults and to
/// resolve config names.
const ALL_ACTIONS: &[Action] = &[
    Action::NavDown,
    Action::NavUp,
    Action::NavOpen,
    Action::NavBack,
    Action::Quit,
    Action::Help,
    Action::Search,
    Action::OpenLibrary,
    Action::OpenQueue,
    Action::SignOut,
    Action::PlayPause,
    Action::NextTrack,
    Action::PreviousTrack,
    Action::SeekForward,
    Action::SeekBackward,
    Action::VolumeUp,
    Action::VolumeDown,
    Action::ToggleShuffle,
    Action::CycleRepeat,
    Action::RefreshLibrary,
    Action::PlayNext,
    Action::AddToQueue,
    Action::RemoveFromQueue,
    Action::MoveUp,
    Action::MoveDown,
];

/// The views whose bindings are resolved together. A key may be reused across
/// different views without conflict, but not within the same view or against a
/// global binding (Requirement 7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum View {
    /// Active in every view (navigation and global commands).
    Global,
    /// The Library view (Requirement 3).
    Library,
    /// The Queue view (Requirement 5).
    Queue,
}

impl View {
    /// A human-readable label for this view, used as the active-view section
    /// heading in the help overlay (Requirement 7.3).
    pub const fn display_name(self) -> &'static str {
        match self {
            View::Global => "Global",
            View::Library => "Library",
            View::Queue => "Queue",
        }
    }
}

// ---------------------------------------------------------------------------
// Keys and key sequences
// ---------------------------------------------------------------------------

/// A single key press: a [`KeyCode`] plus its modifiers.
///
/// Modifiers are normalized so that comparison matches how events arrive from
/// crossterm: the implicit shift of an uppercase character is folded into the
/// [`KeyCode::Char`] itself rather than carried as [`KeyModifiers::SHIFT`], so
/// `?` and `A` compare equal regardless of whether a terminal also reports
/// shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    /// Construct a key, normalizing the shift modifier for character keys.
    fn new(code: KeyCode, modifiers: KeyModifiers) -> Key {
        let modifiers = normalize_modifiers(code, modifiers);
        Key { code, modifiers }
    }

    /// Whether this key corresponds to a printable character with no control
    /// or alt modifier, i.e. one that a text input field consumes as text
    /// (Requirement 7.7).
    fn printable_char(self) -> Option<char> {
        match self.code {
            KeyCode::Char(c)
                if !self.modifiers.contains(KeyModifiers::CONTROL)
                    && !self.modifiers.contains(KeyModifiers::ALT)
                    && !self.modifiers.contains(KeyModifiers::SUPER) =>
            {
                Some(c)
            }
            _ => None,
        }
    }
}

/// Fold the implicit shift of an uppercase character into the code so that a
/// terminal reporting `SHIFT` and one that does not compare equal, and drop the
/// redundant `SHIFT` from non-character keys only when it adds no information.
fn normalize_modifiers(code: KeyCode, mut modifiers: KeyModifiers) -> KeyModifiers {
    if let KeyCode::Char(c) = code {
        // An uppercase or already-shifted character carries its shift in the
        // char value; drop the SHIFT flag so bindings written as a bare symbol
        // match regardless of how the terminal reports the modifier.
        if c.is_ascii_uppercase() || !c.is_ascii_alphanumeric() {
            modifiers.remove(KeyModifiers::SHIFT);
        }
    }
    modifiers
}

/// A Keybinding: a single key or a sequence of up to two keys (Requirement
/// 7.1). The TUI matches the sequence in order; a two-key binding fires only
/// after both keys arrive in sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySequence {
    keys: Vec<Key>,
}

impl KeySequence {
    /// A single-key sequence built from a normalized [`Key`].
    ///
    /// This is the one-key case of a Keybinding (Requirement 7.1). The running
    /// loop resolves a lone key press through [`Resolution::action_for_key`],
    /// which builds the sequence with this constructor; the two-key case is
    /// still parsed from config text via [`parse_key_sequence`].
    pub fn single(key: Key) -> KeySequence {
        KeySequence { keys: vec![key] }
    }

    /// The keys in this sequence, in press order (length 1 or 2).
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// Render the sequence back to the config/help text form (e.g. `g g`,
    /// `space`, `?`). Used by the help overlay (Requirement 7.3).
    pub fn display(&self) -> String {
        self.keys
            .iter()
            .map(|k| key_display(*k))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

// ---------------------------------------------------------------------------
// Key-name parsing (Requirement 7.8)
// ---------------------------------------------------------------------------

/// Error returned when a key sequence string in the config cannot be parsed
/// into recognized keys (Requirement 7.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyParseError {
    /// The sequence was empty or contained only whitespace.
    Empty,
    /// The sequence held more than the two keys a Keybinding allows.
    TooLong,
    /// A named key token was not recognized. Carries the offending token.
    UnknownKey(String),
}

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyParseError::Empty => write!(f, "empty key sequence"),
            KeyParseError::TooLong => {
                write!(f, "a keybinding may use at most two keys")
            }
            KeyParseError::UnknownKey(token) => {
                write!(f, "unrecognized key `{token}`")
            }
        }
    }
}

/// Parse a key sequence written in the Config_File into a [`KeySequence`].
///
/// Keys are separated by whitespace; up to two are allowed (Requirement 7.1). A
/// key may be a single printable character (`q`, `?`, `+`), a named special key
/// (`space`, `enter`, `up`, `esc`, `f5`, …), case-insensitively, or a
/// modifier-prefixed key (`ctrl+c`, `alt+enter`, `shift+tab`). An unrecognized
/// token yields [`KeyParseError::UnknownKey`] so the caller can warn and fall
/// back to the default (Requirement 7.8).
pub fn parse_key_sequence(text: &str) -> Result<KeySequence, KeyParseError> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(KeyParseError::Empty);
    }
    if tokens.len() > 2 {
        return Err(KeyParseError::TooLong);
    }
    let mut keys = Vec::with_capacity(tokens.len());
    for token in tokens {
        keys.push(parse_key(token)?);
    }
    Ok(KeySequence { keys })
}

/// Parse one key token, including an optional `ctrl+`/`alt+`/`shift+` prefix.
fn parse_key(token: &str) -> Result<Key, KeyParseError> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = token;

    // Peel off modifier prefixes in any order (e.g. `ctrl+alt+x`).
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(stripped) = lower
            .strip_prefix("ctrl+")
            .or_else(|| lower.strip_prefix("control+"))
        {
            modifiers.insert(KeyModifiers::CONTROL);
            rest = &rest[rest.len() - stripped.len()..];
        } else if let Some(stripped) = lower.strip_prefix("alt+") {
            modifiers.insert(KeyModifiers::ALT);
            rest = &rest[rest.len() - stripped.len()..];
        } else if let Some(stripped) = lower.strip_prefix("shift+") {
            modifiers.insert(KeyModifiers::SHIFT);
            rest = &rest[rest.len() - stripped.len()..];
        } else {
            break;
        }
    }

    let code = parse_key_code(rest)?;
    Ok(Key::new(code, modifiers))
}

/// Parse the non-modifier portion of a key token into a [`KeyCode`].
fn parse_key_code(token: &str) -> Result<KeyCode, KeyParseError> {
    // A single character is taken verbatim (case-sensitive, so `G` differs from
    // `g`), which also covers symbols like `?`, `+`, and `/`.
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Ok(KeyCode::Char(c));
    }

    let lower = token.to_ascii_lowercase();
    let code = match lower.as_str() {
        "space" | "spc" => KeyCode::Char(' '),
        "enter" | "return" | "ret" | "cr" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        other => {
            // Function keys: f1..=f12 (and beyond, as the terminal supports).
            if let Some(num) = other.strip_prefix('f') {
                if let Ok(n) = num.parse::<u8>() {
                    if n >= 1 {
                        return Ok(KeyCode::F(n));
                    }
                }
            }
            return Err(KeyParseError::UnknownKey(token.to_string()));
        }
    };
    Ok(code)
}

/// Render one key back to its config/help text form (Requirement 7.3).
fn key_display(key: Key) -> String {
    let mut prefix = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("alt+");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        prefix.push_str("shift+");
    }
    let body = match key.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    };
    format!("{prefix}{body}")
}

// ---------------------------------------------------------------------------
// Default bindings (Requirements 7.1, 7.2)
// ---------------------------------------------------------------------------

/// The default Keybindings for every action, as config-style strings. Several
/// actions bind more than one key (Requirement 7.2: navigation accepts both the
/// vi keys and the arrow keys); each string is one alternative binding.
///
/// These strings parse through [`parse_key_sequence`], so the default set is
/// validated by the same path as user config and the round-trip is covered by
/// tests.
fn default_binding_specs() -> Vec<(Action, &'static [&'static str])> {
    vec![
        // List navigation with both vi and arrow keys (Requirement 7.2).
        (Action::NavDown, &["j", "down"]),
        (Action::NavUp, &["k", "up"]),
        (Action::NavOpen, &["l", "right", "enter"]),
        (Action::NavBack, &["h", "left"]),
        // Global.
        (Action::Quit, &["q"]),
        (Action::Help, &["?"]),
        // View switching accepts both the mnemonic key/chord and the number
        // row (1/2/3) as a familiar alternative (Requirement 7.2).
        (Action::Search, &["/", "1"]),
        (Action::OpenLibrary, &["g l", "2"]),
        (Action::OpenQueue, &["g q", "3"]),
        (Action::SignOut, &["g o"]),
        // Transport.
        (Action::PlayPause, &["space"]),
        (Action::NextTrack, &["n"]),
        (Action::PreviousTrack, &["p"]),
        (Action::SeekForward, &["]"]),
        (Action::SeekBackward, &["["]),
        (Action::VolumeUp, &["+"]),
        (Action::VolumeDown, &["-"]),
        (Action::ToggleShuffle, &["s"]),
        (Action::CycleRepeat, &["r"]),
        // Library.
        (Action::RefreshLibrary, &["R"]),
        // Queue editing.
        (Action::PlayNext, &["N"]),
        (Action::AddToQueue, &["a"]),
        (Action::RemoveFromQueue, &["x"]),
        (Action::MoveUp, &["K"]),
        (Action::MoveDown, &["J"]),
    ]
}

/// The default Keybindings: a map from action to its one or more default key
/// sequences (Requirements 7.1, 7.2).
///
/// Parsing the built-in specs is infallible by construction; a bad spec would
/// surface as a panic in the dedicated `default_specs_all_parse` test rather
/// than at runtime.
pub fn default_bindings() -> BTreeMap<Action, Vec<KeySequence>> {
    let mut map = BTreeMap::new();
    for (action, specs) in default_binding_specs() {
        let sequences = specs
            .iter()
            .map(|spec| parse_key_sequence(spec).expect("built-in default binding must parse"))
            .collect();
        map.insert(action, sequences);
    }
    map
}

// ---------------------------------------------------------------------------
// Warnings (Requirements 7.5, 7.8)
// ---------------------------------------------------------------------------

/// A startup warning about a Keybinding in the Config_File. The affected action
/// always falls back to its default binding (Requirements 7.5, 7.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeybindingWarning {
    /// The config bound an action name the TUI does not define (Requirement
    /// 7.8). The entry is ignored.
    UnknownAction { name: String },
    /// The config used a key sequence the TUI cannot parse (Requirement 7.8).
    /// The named action keeps its default binding.
    UnknownKey {
        action: String,
        sequence: String,
        reason: KeyParseError,
    },
    /// Two actions in the same view were bound to the same key sequence,
    /// counting conflicts between a custom binding and the default binding of an
    /// unlisted action (Requirement 7.5). Both actions fall back to their
    /// defaults.
    Conflict {
        view: View,
        sequence: String,
        first: Action,
        second: Action,
    },
}

impl fmt::Display for KeybindingWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeybindingWarning::UnknownAction { name } => write!(
                f,
                "config binds unknown action `{name}`; the entry is ignored"
            ),
            KeybindingWarning::UnknownKey {
                action,
                sequence,
                reason,
            } => write!(
                f,
                "config binds action `{action}` to invalid key `{sequence}` ({reason}); \
                 using the default binding"
            ),
            KeybindingWarning::Conflict {
                view,
                sequence,
                first,
                second,
            } => write!(
                f,
                "keybinding `{sequence}` is assigned to both `{}` and `{}` in {view:?} view; \
                 using the default bindings for both",
                first.name(),
                second.name()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution (Requirement 7.4)
// ---------------------------------------------------------------------------

/// The resolved Keybindings together with any startup warnings.
///
/// `bindings` maps each action to the key sequences that trigger it after
/// overlaying the config on the defaults (Requirement 7.4) and resolving
/// conflicts and unknown entries by falling back to defaults (Requirements 7.5,
/// 7.8). `warnings` is non-empty exactly when something was reported at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    bindings: BTreeMap<Action, Vec<KeySequence>>,
    warnings: Vec<KeybindingWarning>,
}

impl Resolution {
    /// The key sequences bound to `action` in the resolved map.
    pub fn sequences_for(&self, action: Action) -> &[KeySequence] {
        self.bindings
            .get(&action)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The full resolved action → sequences map (used by the help overlay,
    /// Requirement 7.3).
    pub fn bindings(&self) -> &BTreeMap<Action, Vec<KeySequence>> {
        &self.bindings
    }

    /// Warnings raised while resolving (Requirements 7.5, 7.8).
    pub fn warnings(&self) -> &[KeybindingWarning] {
        &self.warnings
    }

    /// Resolve a single key sequence to the action it triggers in `view`, if
    /// any. Global and navigation actions resolve in every view; a view-scoped
    /// action resolves only within its view.
    pub fn action_for(&self, view: View, sequence: &KeySequence) -> Option<Action> {
        self.bindings.iter().find_map(|(action, sequences)| {
            let in_scope = action.view() == View::Global || action.view() == view;
            if in_scope && sequences.iter().any(|s| s == sequence) {
                Some(*action)
            } else {
                None
            }
        })
    }

    /// Resolve a single crossterm [`KeyEvent`] to the action it triggers in
    /// `view`, if any.
    ///
    /// This is the convenience the running render loop uses: it normalizes the
    /// event into a one-key [`KeySequence`] and defers to [`action_for`]. It
    /// keeps the `Key`/`KeySequence` internals private to this module while
    /// still letting the loop drive dispatch from real resolved bindings.
    /// Two-key sequences, which require tracking a pending first key, are not
    /// resolved here; the loop treats each press independently.
    pub fn action_for_key(&self, view: View, event: KeyEvent) -> Option<Action> {
        let sequence = KeySequence::single(key_from_event(event));
        self.action_for(view, &sequence)
    }

    /// Resolve a key press against the bindings in `view`, accounting for the
    /// two-key sequences (e.g. the default `g l` / `g q` / `g o`).
    ///
    /// `pending` is the previously pressed key when the loop is mid-chord (the
    /// first key of a potential two-key sequence), or `None` for a fresh press.
    /// The running loop keeps a one-key history and feeds it here so a chord
    /// like `g` then `l` fires the Library action while a lone `g` is held as a
    /// pending prefix.
    ///
    /// Returns:
    /// - [`ChordResolution::Action`] when a single-key binding matches `event`
    ///   directly, or when `pending` + `event` completes a two-key binding;
    /// - [`ChordResolution::Pending`] when `event` is the first key of some
    ///   two-key binding in scope (so the loop should hold it and wait for the
    ///   second key);
    /// - [`ChordResolution::None`] when nothing matches, so the loop clears any
    ///   pending prefix and ignores the key.
    ///
    /// A completed two-key chord is attempted first so an explicit chord wins
    /// over a same-first-key single binding; a direct single-key match is tried
    /// next; only then is the key considered as a new chord prefix.
    pub fn resolve_chord(
        &self,
        view: View,
        pending: Option<KeyEvent>,
        event: KeyEvent,
    ) -> ChordResolution {
        let key = key_from_event(event);

        // 1. Complete a two-key chord when we already hold a prefix.
        if let Some(prefix) = pending {
            let sequence = KeySequence {
                keys: vec![key_from_event(prefix), key],
            };
            if let Some(action) = self.action_for(view, &sequence) {
                return ChordResolution::Action(action);
            }
        }

        // 2. A direct single-key binding.
        if let Some(action) = self.action_for(view, &KeySequence::single(key)) {
            return ChordResolution::Action(action);
        }

        // 3. The key begins some two-key binding in scope: hold it as a prefix.
        if self.is_chord_prefix(view, key) {
            return ChordResolution::Pending;
        }

        ChordResolution::None
    }

    /// Whether `key` is the first key of any two-key binding active in `view`.
    fn is_chord_prefix(&self, view: View, key: Key) -> bool {
        self.bindings.iter().any(|(action, sequences)| {
            let in_scope = action.view() == View::Global || action.view() == view;
            in_scope
                && sequences
                    .iter()
                    .any(|s| s.keys.len() == 2 && s.keys[0] == key)
        })
    }
}

/// The outcome of resolving one key press that may be part of a two-key chord
/// (see [`Resolution::resolve_chord`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordResolution {
    /// A binding fired (single key, or a completed two-key chord).
    Action(Action),
    /// The key is the first key of a two-key binding; hold it and wait for the
    /// next press.
    Pending,
    /// Nothing matched; clear any pending prefix and ignore the key.
    None,
}

/// Build the effective action map from the defaults overlaid with the custom
/// bindings in `config` (Requirement 7.4).
///
/// For each action listed in `config.keybindings`, the custom key sequence
/// replaces that action's default (Requirement 7.4); unlisted actions keep
/// their defaults. The resolver then:
/// - ignores entries naming an unknown action, warning once each (Requirement
///   7.8);
/// - keeps the default binding for an action whose custom key sequence does not
///   parse, warning once (Requirement 7.8);
/// - detects, per view, any key sequence assigned to two actions — including a
///   custom binding colliding with the default of an unlisted action — warns
///   naming the key and both actions, and reverts both to their defaults
///   (Requirement 7.5).
///
/// The function is pure: it reads only `config` and the built-in defaults, so it
/// is fully unit-testable.
pub fn resolve_keybindings(config: &Configuration) -> Resolution {
    let defaults = default_bindings();
    let mut warnings = Vec::new();

    // Start from the defaults, then overlay each valid custom entry.
    let mut effective = defaults.clone();

    // Track which actions the user successfully customized so conflict recovery
    // can revert exactly those to their defaults (Requirement 7.5).
    let mut customized: BTreeMap<Action, KeySequence> = BTreeMap::new();

    for (name, sequence_text) in &config.keybindings {
        let Some(action) = Action::from_name(name) else {
            warnings.push(KeybindingWarning::UnknownAction {
                name: name.clone(),
            });
            continue;
        };
        match parse_key_sequence(sequence_text) {
            Ok(sequence) => {
                effective.insert(action, vec![sequence.clone()]);
                customized.insert(action, sequence);
            }
            Err(reason) => {
                // Keep the default binding for this action (Requirement 7.8).
                warnings.push(KeybindingWarning::UnknownKey {
                    action: name.clone(),
                    sequence: sequence_text.clone(),
                    reason,
                });
            }
        }
    }

    // Detect per-view conflicts, then revert every action involved in a
    // conflict to its default binding (Requirement 7.5).
    let conflicts = find_conflicts(&effective);
    let mut reverted: std::collections::BTreeSet<Action> = std::collections::BTreeSet::new();
    for conflict in &conflicts {
        if let KeybindingWarning::Conflict { first, second, .. } = conflict {
            reverted.insert(*first);
            reverted.insert(*second);
        }
    }
    for action in &reverted {
        if let Some(default) = defaults.get(action) {
            effective.insert(*action, default.clone());
        }
    }
    warnings.extend(conflicts);

    Resolution {
        bindings: effective,
        warnings,
    }
}

/// Find every per-view conflict in `bindings`: a key sequence bound to two
/// actions whose scopes overlap (both global, or one global and one view-scoped,
/// or two in the same view). Reported deterministically in action order so
/// warnings are stable (Requirement 7.5).
fn find_conflicts(bindings: &BTreeMap<Action, Vec<KeySequence>>) -> Vec<KeybindingWarning> {
    // For each view, map a sequence to the first action that claimed it; a
    // second claimant is a conflict. A Global action participates in every
    // view's map so a view-scoped binding colliding with a global one is caught.
    let mut warnings = Vec::new();
    let mut seen_pairs: std::collections::BTreeSet<(Action, Action, String)> =
        std::collections::BTreeSet::new();

    for view in [View::Library, View::Queue] {
        let mut claimed: HashMap<KeySequence, Action> = HashMap::new();
        // Iterate in action order (BTreeMap) for deterministic first-claimant.
        for (action, sequences) in bindings {
            let scope = action.view();
            if scope != View::Global && scope != view {
                continue;
            }
            for sequence in sequences {
                if let Some(&first) = claimed.get(sequence) {
                    // Order the pair by action for a stable warning identity, so
                    // the same conflict is not reported once per view.
                    let (a, b) = if first <= *action {
                        (first, *action)
                    } else {
                        (*action, first)
                    };
                    let key = (a, b, sequence.display());
                    if seen_pairs.insert(key) {
                        warnings.push(KeybindingWarning::Conflict {
                            view: conflict_view(a, b, view),
                            sequence: sequence.display(),
                            first: a,
                            second: b,
                        });
                    }
                } else {
                    claimed.insert(sequence.clone(), *action);
                }
            }
        }
    }

    warnings
}

/// Pick the most specific view to name in a conflict warning: the view-scoped
/// side when one action is global and the other is not, otherwise the view the
/// conflict was detected in.
fn conflict_view(a: Action, b: Action, detected_in: View) -> View {
    match (a.view(), b.view()) {
        (View::Global, other) if other != View::Global => other,
        (other, View::Global) if other != View::Global => other,
        (View::Global, View::Global) => detected_in,
        (va, _) => va,
    }
}

// ---------------------------------------------------------------------------
// Help overlay (Requirement 7.3)
// ---------------------------------------------------------------------------

/// One row of the help overlay: an action's display label and the key
/// sequences that trigger it after config overrides (Requirement 7.3). The
/// sequences are the effective (custom or default) bindings resolved for the
/// action, so customized keys appear here (Requirement 7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    action: Action,
    sequences: Vec<KeySequence>,
}

impl HelpEntry {
    /// The action this row documents.
    pub fn action(&self) -> Action {
        self.action
    }

    /// The human-readable action label (Requirement 7.3).
    pub fn label(&self) -> &'static str {
        self.action.display_name()
    }

    /// The effective key sequences bound to the action, in resolution order
    /// (Requirement 7.4).
    pub fn sequences(&self) -> &[KeySequence] {
        &self.sequences
    }

    /// The key sequences rendered as a single display string, each sequence
    /// separated by `", "` (e.g. `j, down`). Convenience for the renderer; the
    /// raw sequences remain available via [`sequences`](HelpEntry::sequences).
    pub fn keys_display(&self) -> String {
        self.sequences
            .iter()
            .map(KeySequence::display)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Build the ordered help entries for `view`: the active view's own bindings
/// first, then the global bindings (Requirement 7.3).
///
/// When `view` is [`View::Global`] there are no view-scoped bindings, so the
/// result is exactly the global entries. Within each group, entries follow the
/// stable action order. Only actions that have at least one effective binding
/// are included.
fn help_entries_for(resolution: &Resolution, view: View) -> Vec<HelpEntry> {
    let mut view_entries = Vec::new();
    let mut global_entries = Vec::new();

    for (action, sequences) in resolution.bindings() {
        if sequences.is_empty() {
            continue;
        }
        let entry = HelpEntry {
            action: *action,
            sequences: sequences.clone(),
        };
        match action.view() {
            View::Global => global_entries.push(entry),
            scope if scope == view => view_entries.push(entry),
            _ => {}
        }
    }

    // Active-view bindings first, then global bindings (Requirement 7.3).
    view_entries.extend(global_entries);
    view_entries
}

/// The help overlay state and behavior (Requirement 7.3).
///
/// This is pure, testable state the renderer (`ui.rs`) consumes: it holds the
/// ordered [`HelpEntry`] list for the view that was active when it opened, a
/// scroll offset moved by the list-navigation actions, and the view to restore
/// when it closes. [`HelpOverlay::open`] records the prior view; [`handle_action`]
/// scrolls on [`Action::NavUp`]/[`Action::NavDown`] and closes on
/// [`Action::Help`] or [`Action::NavBack`] (the Esc/`?` behavior), returning the
/// view to restore (Requirement 7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpOverlay {
    /// The view that was active when the overlay opened, restored on close.
    prior_view: View,
    /// The help rows for `prior_view`, in display order.
    entries: Vec<HelpEntry>,
    /// Index of the first visible entry when the list exceeds the viewport.
    scroll: usize,
}

/// The result of feeding an action to an open [`HelpOverlay`] (Requirement
/// 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpOverlayOutcome {
    /// The overlay stayed open; the scroll offset may have changed.
    StayOpen,
    /// The overlay closed; the caller should restore the carried view
    /// unchanged (Requirement 7.3).
    Close { restore: View },
}

impl HelpOverlay {
    /// Open the overlay over `view`, recording it as the view to restore and
    /// snapshotting its help entries from `resolution` (Requirement 7.3). The
    /// scroll starts at the top.
    pub fn open(resolution: &Resolution, view: View) -> HelpOverlay {
        HelpOverlay {
            prior_view: view,
            entries: help_entries_for(resolution, view),
            scroll: 0,
        }
    }

    /// The view that will be restored unchanged when the overlay closes
    /// (Requirement 7.3).
    pub fn prior_view(&self) -> View {
        self.prior_view
    }

    /// The ordered help entries: active-view bindings followed by the global
    /// bindings (Requirement 7.3).
    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    /// The total number of help entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the overlay has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the first visible entry given the last-applied viewport
    /// (Requirement 7.3). Zero until the list is scrolled.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The slice of entries visible in a viewport `viewport_rows` tall, starting
    /// at the current scroll offset (Requirement 7.3). A zero-height viewport
    /// yields an empty slice.
    pub fn visible(&self, viewport_rows: usize) -> &[HelpEntry] {
        if viewport_rows == 0 || self.entries.is_empty() {
            return &[];
        }
        let start = self.scroll.min(self.entries.len());
        let end = (start + viewport_rows).min(self.entries.len());
        &self.entries[start..end]
    }

    /// Apply a resolved action to the open overlay (Requirement 7.3).
    ///
    /// [`Action::NavDown`]/[`Action::NavUp`] scroll the list by one, clamped so
    /// the offset never leaves at least one entry visible and never goes below
    /// zero; [`Action::Help`] (the `?` key) and [`Action::NavBack`] (Esc/`h`)
    /// close the overlay and report the view to restore. Any other action
    /// leaves the overlay open and unchanged.
    ///
    /// `viewport_rows` is the number of entry rows the overlay can show, used to
    /// clamp downward scrolling so the viewport never scrolls past the end of
    /// the list.
    pub fn handle_action(&mut self, action: Action, viewport_rows: usize) -> HelpOverlayOutcome {
        match action {
            Action::Help | Action::NavBack => HelpOverlayOutcome::Close {
                restore: self.prior_view,
            },
            Action::NavDown => {
                self.scroll = self.clamp_scroll(self.scroll.saturating_add(1), viewport_rows);
                HelpOverlayOutcome::StayOpen
            }
            Action::NavUp => {
                self.scroll = self.clamp_scroll(self.scroll.saturating_sub(1), viewport_rows);
                HelpOverlayOutcome::StayOpen
            }
            _ => HelpOverlayOutcome::StayOpen,
        }
    }

    /// The largest scroll offset that still fills the viewport from the end of
    /// the list: `len - viewport_rows`, or zero when everything fits. Clamps
    /// `desired` into `0..=max` (Requirement 7.3 edge clamping, mirroring
    /// list-navigation behavior).
    fn clamp_scroll(&self, desired: usize, viewport_rows: usize) -> usize {
        let max = self
            .entries
            .len()
            .saturating_sub(viewport_rows.max(1));
        desired.min(max)
    }
}

// ---------------------------------------------------------------------------
// Text input field behavior (Requirement 7.7)
// ---------------------------------------------------------------------------

/// What a focused text input field did with a key (Requirement 7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInputOutcome {
    /// A printable key was inserted as text; the field now reads `value`.
    Inserted { value: String },
    /// Backspace removed the last character; the field now reads `value`.
    Deleted { value: String },
    /// Esc was pressed: focus returns to the enclosing view and the field value
    /// is reported unchanged. The caller stops treating keys as text.
    Exited { value: String },
    /// The key is neither printable, Backspace, nor Esc (e.g. Enter to submit);
    /// the field leaves it for the enclosing view to handle. The value is
    /// unchanged.
    Passthrough,
}

/// A text input field (such as the search query field) that consumes printable
/// keys as text and returns focus on Esc (Requirement 7.7).
///
/// While focused, a field does *not* trigger Keybindings for printable keys;
/// it inserts them as text instead. Esc returns focus to the enclosing view.
/// Enter and other non-printable keys are passed through for the enclosing view
/// to interpret (for example, submitting the query — Requirement 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInputField {
    value: String,
}

impl TextInputField {
    /// Create an empty field.
    pub fn new() -> TextInputField {
        TextInputField::default()
    }

    /// The current text of the field.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Set the field's text (e.g. when retaining a query after a failed search,
    /// Requirements 2.2, 2.5).
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
    }

    /// Handle a key event while the field has focus (Requirement 7.7).
    ///
    /// Printable keys are inserted as text (so they do not trigger their
    /// Keybindings), Backspace deletes the last character, Esc returns focus to
    /// the enclosing view, and every other key is passed through unchanged for
    /// the enclosing view to handle.
    pub fn handle_key(&mut self, event: KeyEvent) -> TextInputOutcome {
        // Ignore key-release / repeat echoes so a character is inserted once.
        if event.kind == KeyEventKind::Release {
            return TextInputOutcome::Passthrough;
        }
        let key = Key::new(event.code, event.modifiers);
        match key.code {
            KeyCode::Esc => TextInputOutcome::Exited {
                value: self.value.clone(),
            },
            KeyCode::Backspace => {
                self.value.pop();
                TextInputOutcome::Deleted {
                    value: self.value.clone(),
                }
            }
            _ => match key.printable_char() {
                Some(c) => {
                    self.value.push(c);
                    TextInputOutcome::Inserted {
                        value: self.value.clone(),
                    }
                }
                None => TextInputOutcome::Passthrough,
            },
        }
    }
}

/// Convert a raw crossterm [`KeyEvent`] into the normalized [`Key`] the resolver
/// compares against. Key-release and repeat events are left to the caller to
/// filter; this only normalizes modifiers.
pub fn key_from_event(event: KeyEvent) -> Key {
    Key::new(event.code, event.modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(bindings: &[(&str, &str)]) -> Configuration {
        let mut config = Configuration::default();
        for (action, sequence) in bindings {
            config
                .keybindings
                .insert((*action).to_string(), (*sequence).to_string());
        }
        config
    }

    fn seq(text: &str) -> KeySequence {
        parse_key_sequence(text).expect("test sequence parses")
    }

    // --- Defaults (Requirements 7.1, 7.2) ---

    #[test]
    fn default_specs_all_parse() {
        // Building the defaults must not panic: every built-in spec parses.
        let defaults = default_bindings();
        // Every action has at least one default binding (Requirement 7.1).
        for action in ALL_ACTIONS {
            assert!(
                defaults.get(action).map(|v| !v.is_empty()).unwrap_or(false),
                "action {:?} has no default binding",
                action
            );
        }
    }

    #[test]
    fn navigation_defaults_cover_vi_and_arrows() {
        // Requirement 7.2: j/Down, k/Up, l/Right/Enter, h/Left.
        let r = resolve_keybindings(&Configuration::default());
        assert!(r.sequences_for(Action::NavDown).contains(&seq("j")));
        assert!(r.sequences_for(Action::NavDown).contains(&seq("down")));
        assert!(r.sequences_for(Action::NavUp).contains(&seq("k")));
        assert!(r.sequences_for(Action::NavUp).contains(&seq("up")));
        assert!(r.sequences_for(Action::NavOpen).contains(&seq("l")));
        assert!(r.sequences_for(Action::NavOpen).contains(&seq("right")));
        assert!(r.sequences_for(Action::NavOpen).contains(&seq("enter")));
        assert!(r.sequences_for(Action::NavBack).contains(&seq("h")));
        assert!(r.sequences_for(Action::NavBack).contains(&seq("left")));
    }

    #[test]
    fn default_resolution_has_no_warnings() {
        let r = resolve_keybindings(&Configuration::default());
        assert!(
            r.warnings().is_empty(),
            "defaults should not conflict: {:?}",
            r.warnings()
        );
    }

    // --- Overlay (Requirement 7.4) ---

    #[test]
    fn custom_binding_replaces_default_and_keeps_others() {
        let config = config_with(&[("quit", "ctrl+c")]);
        let r = resolve_keybindings(&config);
        assert_eq!(r.warnings(), &[]);
        // The custom binding replaces the default `q`.
        assert_eq!(r.sequences_for(Action::Quit), &[seq("ctrl+c")]);
        // An unlisted action keeps its default.
        assert!(r.sequences_for(Action::Help).contains(&seq("?")));
    }

    #[test]
    fn two_key_sequence_binding() {
        let config = config_with(&[("quit", "g x")]);
        let r = resolve_keybindings(&config);
        assert_eq!(r.warnings(), &[]);
        assert_eq!(r.sequences_for(Action::Quit), &[seq("g x")]);
        assert_eq!(r.sequences_for(Action::Quit)[0].keys().len(), 2);
    }

    // --- Unknown action / key (Requirement 7.8) ---

    #[test]
    fn unknown_action_name_warns_and_is_ignored() {
        let config = config_with(&[("teleport", "t")]);
        let r = resolve_keybindings(&config);
        assert_eq!(
            r.warnings(),
            &[KeybindingWarning::UnknownAction {
                name: "teleport".to_string()
            }]
        );
    }

    #[test]
    fn unknown_key_name_warns_and_falls_back_to_default() {
        let config = config_with(&[("quit", "nope")]);
        let r = resolve_keybindings(&config);
        assert_eq!(r.warnings().len(), 1);
        assert!(matches!(
            &r.warnings()[0],
            KeybindingWarning::UnknownKey { action, .. } if action == "quit"
        ));
        // Falls back to the default `q` binding (Requirement 7.8).
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
    }

    #[test]
    fn too_long_sequence_is_an_unknown_key_warning() {
        let config = config_with(&[("quit", "a b c")]);
        let r = resolve_keybindings(&config);
        assert!(matches!(
            &r.warnings()[0],
            KeybindingWarning::UnknownKey { reason, .. } if *reason == KeyParseError::TooLong
        ));
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
    }

    // --- Conflicts (Requirement 7.5) ---

    #[test]
    fn two_custom_actions_sharing_a_key_conflict_and_revert() {
        // Bind both quit and help to `z` (free in the defaults): a conflict in
        // the Global scope with no collateral collision against other defaults.
        let config = config_with(&[("quit", "z"), ("help", "z")]);
        let r = resolve_keybindings(&config);
        let conflicts: Vec<_> = r
            .warnings()
            .iter()
            .filter(|w| matches!(w, KeybindingWarning::Conflict { .. }))
            .collect();
        assert_eq!(conflicts.len(), 1, "exactly one conflict: {:?}", r.warnings());
        // Both revert to their defaults (Requirement 7.5).
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
        assert_eq!(r.sequences_for(Action::Help), &[seq("?")]);
    }

    #[test]
    fn custom_binding_conflicting_with_unlisted_default_reverts_both() {
        // `?` is the default for help. Binding quit to `?` conflicts with the
        // unlisted default of help (Requirement 7.5).
        let config = config_with(&[("quit", "?")]);
        let r = resolve_keybindings(&config);
        let conflicts: Vec<_> = r
            .warnings()
            .iter()
            .filter(|w| matches!(w, KeybindingWarning::Conflict { .. }))
            .collect();
        assert_eq!(conflicts.len(), 1, "conflict with unlisted default: {:?}", r.warnings());
        // quit reverts to `q`, help keeps its default `?`.
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
        assert!(r.sequences_for(Action::Help).contains(&seq("?")));
    }

    #[test]
    fn same_key_in_different_views_does_not_conflict() {
        // refresh_library (Library view) and remove_from_queue (Queue view) can
        // share a key because their scopes do not overlap.
        let config = config_with(&[("refresh_library", "z"), ("remove_from_queue", "z")]);
        let r = resolve_keybindings(&config);
        let conflicts: Vec<_> = r
            .warnings()
            .iter()
            .filter(|w| matches!(w, KeybindingWarning::Conflict { .. }))
            .collect();
        assert!(conflicts.is_empty(), "cross-view reuse is allowed: {:?}", r.warnings());
        assert_eq!(r.sequences_for(Action::RefreshLibrary), &[seq("z")]);
        assert_eq!(r.sequences_for(Action::RemoveFromQueue), &[seq("z")]);
    }

    #[test]
    fn global_binding_conflicts_with_view_scoped_binding() {
        // A global action (quit) and a queue-scoped action (add_to_queue) share
        // a key: global applies in every view, so this conflicts.
        let config = config_with(&[("quit", "w"), ("add_to_queue", "w")]);
        let r = resolve_keybindings(&config);
        let conflicts: Vec<_> = r
            .warnings()
            .iter()
            .filter(|w| matches!(w, KeybindingWarning::Conflict { .. }))
            .collect();
        assert_eq!(conflicts.len(), 1, "global vs view-scoped conflict: {:?}", r.warnings());
    }

    // --- action_for resolution ---

    #[test]
    fn action_for_resolves_global_and_scoped() {
        let r = resolve_keybindings(&Configuration::default());
        // Global navigation resolves in any view.
        assert_eq!(r.action_for(View::Library, &seq("j")), Some(Action::NavDown));
        assert_eq!(r.action_for(View::Queue, &seq("q")), Some(Action::Quit));
        // Queue-scoped action resolves only in the Queue view.
        assert_eq!(
            r.action_for(View::Queue, &seq("a")),
            Some(Action::AddToQueue)
        );
        assert_eq!(r.action_for(View::Library, &seq("a")), None);
    }

    #[test]
    fn action_for_key_resolves_a_single_key_press() {
        let r = resolve_keybindings(&Configuration::default());
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(r.action_for_key(View::Queue, q), Some(Action::Quit));

        // A view-scoped key resolves only in its view.
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(r.action_for_key(View::Queue, a), Some(Action::AddToQueue));
        assert_eq!(r.action_for_key(View::Library, a), None);

        // An unbound key resolves to nothing.
        let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        assert_eq!(r.action_for_key(View::Global, z), None);
    }

    // --- Key parsing (Requirement 7.8) ---

    #[test]
    fn parses_named_and_modified_keys() {
        assert_eq!(parse_key("space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(parse_key("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(parse_key("esc").unwrap().code, KeyCode::Esc);
        assert_eq!(parse_key("f5").unwrap().code, KeyCode::F(5));
        let ctrl_c = parse_key("ctrl+c").unwrap();
        assert_eq!(ctrl_c.code, KeyCode::Char('c'));
        assert!(ctrl_c.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn unknown_key_token_errors() {
        assert_eq!(
            parse_key_sequence("wiggle"),
            Err(KeyParseError::UnknownKey("wiggle".to_string()))
        );
        assert_eq!(parse_key_sequence("   "), Err(KeyParseError::Empty));
    }

    #[test]
    fn key_display_round_trips_through_parse() {
        for spec in ["q", "?", "space", "enter", "ctrl+c", "f5", "g l", "["] {
            let parsed = parse_key_sequence(spec).unwrap();
            let reparsed = parse_key_sequence(&parsed.display()).unwrap();
            assert_eq!(parsed, reparsed, "round-trip failed for `{spec}`");
        }
    }

    #[test]
    fn uppercase_char_and_shift_normalize_equal() {
        // A terminal that reports `?` with SHIFT and one that reports it without
        // must compare equal so the help binding matches either way.
        let bare = Key::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let shifted = Key::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(bare, shifted);
    }

    // --- Text input field (Requirement 7.7) ---

    #[test]
    fn text_field_inserts_printable_keys() {
        let mut field = TextInputField::new();
        let outcome = field.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(outcome, TextInputOutcome::Inserted { value: "h".into() });
        field.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(field.value(), "hi");
    }

    #[test]
    fn text_field_printable_key_does_not_trigger_binding() {
        // `q` is the quit binding, but inside a focused field it is text, not a
        // quit trigger (Requirement 7.7).
        let mut field = TextInputField::new();
        let outcome = field.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(outcome, TextInputOutcome::Inserted { value: "q".into() });
    }

    #[test]
    fn text_field_backspace_deletes() {
        let mut field = TextInputField::new();
        field.set_value("ab");
        let outcome = field.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(outcome, TextInputOutcome::Deleted { value: "a".into() });
    }

    #[test]
    fn text_field_esc_exits_with_value_preserved() {
        let mut field = TextInputField::new();
        field.set_value("query");
        let outcome = field.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            TextInputOutcome::Exited {
                value: "query".into()
            }
        );
        // Esc does not mutate the value (Requirement 7.7).
        assert_eq!(field.value(), "query");
    }

    #[test]
    fn text_field_passes_through_non_text_keys() {
        let mut field = TextInputField::new();
        field.set_value("q");
        // Enter is left for the enclosing view to submit (Requirement 2).
        let outcome = field.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, TextInputOutcome::Passthrough);
        assert_eq!(field.value(), "q");
    }

    // --- Help overlay (Requirement 7.3) ---

    #[test]
    fn help_entries_list_view_bindings_then_global() {
        let r = resolve_keybindings(&Configuration::default());
        let overlay = HelpOverlay::open(&r, View::Queue);
        let entries = overlay.entries();

        // Queue-scoped actions are present.
        let actions: Vec<Action> = entries.iter().map(HelpEntry::action).collect();
        assert!(actions.contains(&Action::AddToQueue));
        assert!(actions.contains(&Action::RemoveFromQueue));
        // Global actions are present too.
        assert!(actions.contains(&Action::Quit));
        assert!(actions.contains(&Action::Help));
        // Library-only actions are not shown in the Queue view.
        assert!(!actions.contains(&Action::RefreshLibrary));

        // View-scoped bindings come before the global ones (Requirement 7.3).
        let first_global = actions
            .iter()
            .position(|a| a.view() == View::Global)
            .expect("has a global entry");
        let last_view_scoped = actions
            .iter()
            .rposition(|a| a.view() == View::Queue)
            .expect("has a queue entry");
        assert!(last_view_scoped < first_global);
    }

    #[test]
    fn help_global_view_has_only_global_entries() {
        let r = resolve_keybindings(&Configuration::default());
        let overlay = HelpOverlay::open(&r, View::Global);
        assert!(overlay
            .entries()
            .iter()
            .all(|e| e.action().view() == View::Global));
        assert!(!overlay.is_empty());
    }

    #[test]
    fn help_entry_shows_custom_binding() {
        // A custom binding should surface in the help overlay (Requirement 7.4).
        let config = config_with(&[("quit", "ctrl+c")]);
        let r = resolve_keybindings(&config);
        let overlay = HelpOverlay::open(&r, View::Library);
        let quit = overlay
            .entries()
            .iter()
            .find(|e| e.action() == Action::Quit)
            .expect("quit entry present");
        assert_eq!(quit.keys_display(), "ctrl+c");
        assert_eq!(quit.label(), "Quit");
    }

    #[test]
    fn help_entry_renders_multiple_sequences() {
        let r = resolve_keybindings(&Configuration::default());
        let overlay = HelpOverlay::open(&r, View::Global);
        let nav_down = overlay
            .entries()
            .iter()
            .find(|e| e.action() == Action::NavDown)
            .expect("nav_down entry present");
        // Both the vi key and the arrow key are listed (Requirement 7.2/7.3).
        assert_eq!(nav_down.keys_display(), "j, down");
    }

    #[test]
    fn help_scroll_clamps_at_both_ends() {
        let r = resolve_keybindings(&Configuration::default());
        let mut overlay = HelpOverlay::open(&r, View::Queue);
        let viewport = 3;
        assert_eq!(overlay.scroll(), 0);

        // Scrolling up at the top stays at zero (Requirement 7.3 clamping).
        assert_eq!(
            overlay.handle_action(Action::NavUp, viewport),
            HelpOverlayOutcome::StayOpen
        );
        assert_eq!(overlay.scroll(), 0);

        // Scroll far past the end; offset clamps to len - viewport.
        for _ in 0..100 {
            overlay.handle_action(Action::NavDown, viewport);
        }
        let max = overlay.len().saturating_sub(viewport);
        assert_eq!(overlay.scroll(), max);

        // The visible window stays full and within bounds.
        let visible = overlay.visible(viewport);
        assert_eq!(visible.len(), viewport.min(overlay.len()));
    }

    #[test]
    fn help_visible_window_follows_scroll() {
        let r = resolve_keybindings(&Configuration::default());
        let mut overlay = HelpOverlay::open(&r, View::Queue);
        let viewport = 4;
        let top: Vec<Action> = overlay
            .visible(viewport)
            .iter()
            .map(HelpEntry::action)
            .collect();
        overlay.handle_action(Action::NavDown, viewport);
        let scrolled: Vec<Action> = overlay
            .visible(viewport)
            .iter()
            .map(HelpEntry::action)
            .collect();
        // After one scroll down, the first visible entry advanced by one.
        assert_eq!(scrolled[0], top[1]);
    }

    #[test]
    fn help_closes_and_restores_prior_view_on_help_key() {
        let r = resolve_keybindings(&Configuration::default());
        let mut overlay = HelpOverlay::open(&r, View::Library);
        assert_eq!(overlay.prior_view(), View::Library);
        // `?` (Help) closes and restores the prior view unchanged (Req 7.3).
        assert_eq!(
            overlay.handle_action(Action::Help, 10),
            HelpOverlayOutcome::Close {
                restore: View::Library
            }
        );
    }

    #[test]
    fn help_closes_and_restores_prior_view_on_back() {
        let r = resolve_keybindings(&Configuration::default());
        let mut overlay = HelpOverlay::open(&r, View::Queue);
        // Esc/`h` maps to NavBack, which also closes the overlay (Req 7.3).
        assert_eq!(
            overlay.handle_action(Action::NavBack, 10),
            HelpOverlayOutcome::Close {
                restore: View::Queue
            }
        );
    }

    #[test]
    fn help_unrelated_action_keeps_overlay_open() {
        let r = resolve_keybindings(&Configuration::default());
        let mut overlay = HelpOverlay::open(&r, View::Library);
        assert_eq!(
            overlay.handle_action(Action::PlayPause, 10),
            HelpOverlayOutcome::StayOpen
        );
        assert_eq!(overlay.scroll(), 0);
    }

    #[test]
    fn help_visible_handles_zero_and_oversized_viewport() {
        let r = resolve_keybindings(&Configuration::default());
        let overlay = HelpOverlay::open(&r, View::Queue);
        // Zero rows show nothing.
        assert!(overlay.visible(0).is_empty());
        // A viewport taller than the list shows every entry.
        assert_eq!(overlay.visible(overlay.len() + 5).len(), overlay.len());
    }

    // --- Warning message content (Requirements 7.5, 7.8) ---
    //
    // Requirement 7.5 requires the conflict warning to name the conflicting key
    // and both actions; Requirement 7.8 requires the unknown-action and
    // unknown-key warnings to name the invalid entry. The tests above assert the
    // variant and the fall-back bindings; these assert the user-facing message
    // content carries the required identifying detail.

    /// Pull the single conflict warning out of a resolution, failing loudly if
    /// there is not exactly one.
    fn sole_conflict(r: &Resolution) -> &KeybindingWarning {
        let conflicts: Vec<_> = r
            .warnings()
            .iter()
            .filter(|w| matches!(w, KeybindingWarning::Conflict { .. }))
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "expected exactly one conflict: {:?}",
            r.warnings()
        );
        conflicts[0]
    }

    #[test]
    fn conflict_warning_names_key_and_both_actions() {
        // Two custom actions bound to the same free key `z`: the warning must
        // name the key and both action names (Requirement 7.5).
        let config = config_with(&[("quit", "z"), ("help", "z")]);
        let r = resolve_keybindings(&config);
        let warning = sole_conflict(&r);

        match warning {
            KeybindingWarning::Conflict {
                sequence,
                first,
                second,
                ..
            } => {
                assert_eq!(sequence, "z", "warning names the conflicting key");
                let names = [first.name(), second.name()];
                assert!(names.contains(&"quit"), "names quit: {names:?}");
                assert!(names.contains(&"help"), "names help: {names:?}");
            }
            other => panic!("expected a conflict warning, got {other:?}"),
        }

        // The rendered message also carries the key and both action names so the
        // startup output is self-describing (Requirement 7.5).
        let message = warning.to_string();
        assert!(message.contains('z'), "message names the key: {message}");
        assert!(message.contains("quit"), "message names quit: {message}");
        assert!(message.contains("help"), "message names help: {message}");
    }

    #[test]
    fn conflict_with_unlisted_default_names_key_and_both_actions() {
        // Binding quit to `?` collides with help's unlisted default `?`
        // (Requirement 7.5). The warning still names the key and both actions,
        // even though help was never listed in the config.
        let config = config_with(&[("quit", "?")]);
        let r = resolve_keybindings(&config);
        let warning = sole_conflict(&r);

        match warning {
            KeybindingWarning::Conflict {
                sequence,
                first,
                second,
                ..
            } => {
                assert_eq!(sequence, "?");
                let names = [first.name(), second.name()];
                assert!(names.contains(&"quit"), "names quit: {names:?}");
                assert!(
                    names.contains(&"help"),
                    "names the unlisted default owner help: {names:?}"
                );
            }
            other => panic!("expected a conflict warning, got {other:?}"),
        }
        // Both named actions fall back to their defaults (Requirement 7.5).
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
        assert!(r.sequences_for(Action::Help).contains(&seq("?")));
    }

    #[test]
    fn unknown_action_warning_names_the_invalid_entry() {
        // Requirement 7.8: the warning names the invalid action entry.
        let config = config_with(&[("teleport", "t")]);
        let r = resolve_keybindings(&config);
        assert_eq!(r.warnings().len(), 1);
        match &r.warnings()[0] {
            KeybindingWarning::UnknownAction { name } => assert_eq!(name, "teleport"),
            other => panic!("expected an unknown-action warning, got {other:?}"),
        }
        assert!(
            r.warnings()[0].to_string().contains("teleport"),
            "message names the invalid entry: {}",
            r.warnings()[0]
        );
    }

    #[test]
    fn unknown_key_warning_names_entry_and_falls_back() {
        // Requirement 7.8: an unrecognized key name warns naming the action and
        // the offending key, and the action keeps its default binding.
        let config = config_with(&[("quit", "wiggle")]);
        let r = resolve_keybindings(&config);
        assert_eq!(r.warnings().len(), 1);
        match &r.warnings()[0] {
            KeybindingWarning::UnknownKey {
                action,
                sequence,
                reason,
            } => {
                assert_eq!(action, "quit");
                assert_eq!(sequence, "wiggle");
                assert_eq!(*reason, KeyParseError::UnknownKey("wiggle".to_string()));
            }
            other => panic!("expected an unknown-key warning, got {other:?}"),
        }
        let message = r.warnings()[0].to_string();
        assert!(message.contains("quit"), "message names the action: {message}");
        assert!(message.contains("wiggle"), "message names the key: {message}");
        // Falls back to the default `q` for the affected action (Requirement 7.8).
        assert_eq!(r.sequences_for(Action::Quit), &[seq("q")]);
    }

    #[test]
    fn unknown_action_and_unknown_key_coexist_independently() {
        // A config fixture mixing both failure modes: each affected action falls
        // back to its default and each produces its own warning (Requirement
        // 7.8). Valid entries alongside them are still applied.
        let config = config_with(&[
            ("teleport", "t"), // unknown action -> ignored
            ("help", "nope"),  // unknown key -> help keeps default `?`
            ("quit", "ctrl+c"), // valid override applied
        ]);
        let r = resolve_keybindings(&config);

        let has_unknown_action = r.warnings().iter().any(|w| {
            matches!(w, KeybindingWarning::UnknownAction { name } if name == "teleport")
        });
        let has_unknown_key = r.warnings().iter().any(|w| {
            matches!(w, KeybindingWarning::UnknownKey { action, .. } if action == "help")
        });
        assert!(has_unknown_action, "unknown action warned: {:?}", r.warnings());
        assert!(has_unknown_key, "unknown key warned: {:?}", r.warnings());

        // help keeps its default, quit takes the valid override.
        assert!(r.sequences_for(Action::Help).contains(&seq("?")));
        assert_eq!(r.sequences_for(Action::Quit), &[seq("ctrl+c")]);
    }

    // --- Help overlay contents for a representative view (Requirement 7.3) ---

    #[test]
    fn help_overlay_lists_active_view_and_global_bindings_with_action_names() {
        // The Queue view is a representative non-global view: its overlay must
        // list the active view's bindings and every global binding, each with
        // its action name and key sequences, including a custom binding
        // (Requirement 7.3, 7.4).
        let config = config_with(&[("add_to_queue", "ctrl+a")]);
        let r = resolve_keybindings(&config);
        let overlay = HelpOverlay::open(&r, View::Queue);
        let entries = overlay.entries();

        // Every entry exposes a non-empty action label and at least one key
        // sequence (Requirement 7.3: "each with its action name").
        for entry in entries {
            assert!(
                !entry.label().is_empty(),
                "entry for {:?} has an action label",
                entry.action()
            );
            assert!(
                !entry.sequences().is_empty(),
                "entry for {:?} lists at least one key sequence",
                entry.action()
            );
            assert!(
                !entry.keys_display().is_empty(),
                "entry for {:?} renders its keys",
                entry.action()
            );
        }

        // A representative active-view binding and a representative global
        // binding both appear with their display labels.
        let labels: Vec<&str> = entries.iter().map(HelpEntry::label).collect();
        assert!(
            labels.contains(&Action::AddToQueue.display_name()),
            "active-view action labelled: {labels:?}"
        );
        assert!(
            labels.contains(&Action::Quit.display_name()),
            "global action labelled: {labels:?}"
        );

        // The custom binding is shown in the overlay rather than the default
        // `a` (Requirement 7.4: "show the custom Keybindings in the help
        // overlay").
        let add = entries
            .iter()
            .find(|e| e.action() == Action::AddToQueue)
            .expect("add_to_queue entry present");
        assert_eq!(add.keys_display(), "ctrl+a");

        // A global multi-key action still renders all of its sequences.
        let nav_down = entries
            .iter()
            .find(|e| e.action() == Action::NavDown)
            .expect("nav_down entry present");
        assert_eq!(nav_down.keys_display(), "j, down");
    }
}
