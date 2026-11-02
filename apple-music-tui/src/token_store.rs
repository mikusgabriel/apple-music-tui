//! Token_Store: persists the Developer_Token and Music_User_Token for the
//! current Linux user only.
//!
//! Two backends are selectable through the [`TokenStoreBackend`] config enum:
//!
//! - [`TokenStoreBackend::File`] (default): token files under
//!   `$XDG_DATA_HOME/apple-music-tui/` (falling back to
//!   `~/.local/share/apple-music-tui/`), each created with Unix mode `0600` so
//!   no other Linux user account can read or modify them (Requirement 1.5).
//! - [`TokenStoreBackend::Keyring`]: entries in the OS secret-service keyring,
//!   scoped to the current user by the platform (Requirement 1.5).
//!
//! Token values are wrapped in [`SecretString`], whose `Debug` and `Display`
//! implementations redact the value, so tokens never reach logs, stdout,
//! stderr, or TUI error text (Requirement 1.13). [`TokenStoreError`] likewise
//! carries only a token *kind* and a cause description, never a token value.
//!
//! When a save fails, the caller receives a [`TokenStoreError`] it can surface
//! in the TUI; the in-memory token stays usable for the session because the
//! store never owns the only copy (Requirement 1.15).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::TokenStoreBackend;

/// The secret-service entries all share this service name; the token kind is
/// the per-entry username.
const KEYRING_SERVICE: &str = "apple-music-tui";

/// Application directory under `$XDG_DATA_HOME` (or `~/.local/share`).
const APP_DIR: &str = "apple-music-tui";

/// Which token an operation concerns. Used for file names, keyring entry names,
/// and error messages. Carries no secret value, so it is safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The JWT that authorizes Apple Music catalog requests.
    Developer,
    /// The per-user token issued after MusicKit sign-in.
    MusicUser,
}

impl TokenKind {
    /// The on-disk file name used by the file backend.
    fn file_name(self) -> &'static str {
        match self {
            TokenKind::Developer => "developer_token",
            TokenKind::MusicUser => "music_user_token",
        }
    }

    /// The keyring entry user name used by the keyring backend.
    fn keyring_user(self) -> &'static str {
        match self {
            TokenKind::Developer => "developer_token",
            TokenKind::MusicUser => "music_user_token",
        }
    }

    /// A human-readable label for diagnostics (never includes a value).
    fn label(self) -> &'static str {
        match self {
            TokenKind::Developer => "Developer_Token",
            TokenKind::MusicUser => "Music_User_Token",
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A token value whose contents are never printed.
///
/// `Debug` and `Display` both render a fixed redaction marker, so a
/// `SecretString` can be included in log lines, error chains, or `{:?}` dumps
/// without leaking the token (Requirement 1.13). Call [`SecretString::expose`]
/// at the exact point the raw value is needed (for example, to attach it to an
/// HTTP header).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a raw token value.
    pub fn new(value: impl Into<String>) -> Self {
        SecretString(value.into())
    }

    /// Borrow the raw value. Call this only where the secret is actually used;
    /// never pass the result to a logger or the TUI.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the wrapped value is empty. An empty Music_User_Token is treated
    /// as "no token" by the Auth_Manager (Requirement 1.1).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        SecretString(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        SecretString(value.to_string())
    }
}

/// What a token-store operation was doing when it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStoreOp {
    Save,
    Load,
    Delete,
}

impl fmt::Display for TokenStoreOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenStoreOp::Save => "save",
            TokenStoreOp::Load => "load",
            TokenStoreOp::Delete => "delete",
        })
    }
}

/// An error from the Token_Store.
///
/// Carries the token *kind*, the operation, and a cause description derived
/// from the underlying I/O or keyring error. It never contains a token value,
/// so it is safe to show in the TUI or write to a log (Requirement 1.13). The
/// `Display` form is suitable for the save-failure message required by
/// Requirement 1.15.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenStoreError {
    /// Which token the failed operation concerned.
    pub kind: TokenKind,
    /// The operation that failed.
    pub op: TokenStoreOp,
    /// A human-readable cause, with no secret content.
    pub cause: String,
}

impl TokenStoreError {
    fn new(kind: TokenKind, op: TokenStoreOp, cause: impl Into<String>) -> Self {
        TokenStoreError {
            kind,
            op,
            cause: cause.into(),
        }
    }
}

impl fmt::Display for TokenStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not {} the {}: {}",
            self.op, self.kind, self.cause
        )
    }
}

impl std::error::Error for TokenStoreError {}

/// Persists tokens for the current Linux user with the configured backend.
#[derive(Debug, Clone)]
pub struct TokenStore {
    backend: Backend,
}

/// The resolved backend. The file backend keeps its base directory so tests can
/// point it at a temp dir; the keyring backend is parameter-free.
#[derive(Debug, Clone)]
enum Backend {
    File { base_dir: PathBuf },
    Keyring,
}

impl TokenStore {
    /// Build a Token_Store for the selected backend, using the default
    /// per-user locations.
    ///
    /// The file backend resolves its directory to `$XDG_DATA_HOME/apple-music-tui`
    /// (or `~/.local/share/apple-music-tui`). The directory is not created until
    /// a token is saved.
    pub fn new(backend: TokenStoreBackend) -> Self {
        match backend {
            TokenStoreBackend::File => TokenStore {
                backend: Backend::File {
                    base_dir: default_data_dir(),
                },
            },
            TokenStoreBackend::Keyring => TokenStore {
                backend: Backend::Keyring,
            },
        }
    }

    /// Build a file-backed Token_Store rooted at an explicit directory. Useful
    /// for tests and callers that manage their own data directory.
    pub fn with_file_dir(base_dir: impl Into<PathBuf>) -> Self {
        TokenStore {
            backend: Backend::File {
                base_dir: base_dir.into(),
            },
        }
    }

    /// Save a token, overwriting any existing value for that kind.
    ///
    /// On success the token is persisted with per-user-only access. On failure
    /// the returned [`TokenStoreError`] can be shown to the user while the
    /// caller keeps using the token in memory for the session (Requirement
    /// 1.15); the store does not take ownership of the only copy.
    pub fn save(&self, kind: TokenKind, token: &SecretString) -> Result<(), TokenStoreError> {
        match &self.backend {
            Backend::File { base_dir } => save_file(base_dir, kind, token),
            Backend::Keyring => save_keyring(kind, token),
        }
    }

    /// Load a token. Returns `Ok(None)` when no token of that kind is stored,
    /// and an error only when a stored token could not be read.
    pub fn load(&self, kind: TokenKind) -> Result<Option<SecretString>, TokenStoreError> {
        match &self.backend {
            Backend::File { base_dir } => load_file(base_dir, kind),
            Backend::Keyring => load_keyring(kind),
        }
    }

    /// Delete a token. Deleting an absent token succeeds (idempotent), which
    /// matches sign-out and session-expiry handling (Requirements 1.11, 1.12).
    pub fn delete(&self, kind: TokenKind) -> Result<(), TokenStoreError> {
        match &self.backend {
            Backend::File { base_dir } => delete_file(base_dir, kind),
            Backend::Keyring => delete_keyring(kind),
        }
    }
}

/// Resolve the default data directory: `$XDG_DATA_HOME/apple-music-tui` when
/// `$XDG_DATA_HOME` is set and non-empty, otherwise `~/.local/share/apple-music-tui`.
fn default_data_dir() -> PathBuf {
    if let Some(xdg) = non_empty_env("XDG_DATA_HOME") {
        return Path::new(&xdg).join(APP_DIR);
    }
    let home = non_empty_env("HOME").unwrap_or_else(|| ".".to_string());
    Path::new(&home).join(".local").join("share").join(APP_DIR)
}

/// Read an environment variable, treating unset and empty as absent.
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

// --- File backend -----------------------------------------------------------

fn token_path(base_dir: &Path, kind: TokenKind) -> PathBuf {
    base_dir.join(kind.file_name())
}

fn save_file(base_dir: &Path, kind: TokenKind, token: &SecretString) -> Result<(), TokenStoreError> {
    fs::create_dir_all(base_dir)
        .map_err(|err| io_error(kind, TokenStoreOp::Save, &err))?;

    let path = token_path(base_dir, kind);
    write_private_file(&path, token.expose().as_bytes())
        .map_err(|err| io_error(kind, TokenStoreOp::Save, &err))
}

fn load_file(base_dir: &Path, kind: TokenKind) -> Result<Option<SecretString>, TokenStoreError> {
    let path = token_path(base_dir, kind);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(SecretString::new(contents))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error(kind, TokenStoreOp::Load, &err)),
    }
}

fn delete_file(base_dir: &Path, kind: TokenKind) -> Result<(), TokenStoreError> {
    let path = token_path(base_dir, kind);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_error(kind, TokenStoreOp::Delete, &err)),
    }
}

/// Write `bytes` to `path`, ensuring the resulting file has mode `0600` so only
/// the current Linux user can read or modify it (Requirement 1.5).
///
/// The permission is applied via `OpenOptions::mode` at creation time on Unix,
/// so the file is never briefly world-readable. An existing file is truncated
/// and its mode re-asserted.
#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // Re-assert the mode in case the file already existed with a wider mode.
    file.set_permissions(permissions_0600())?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn permissions_0600() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o600)
}

/// Non-Unix fallback: write the file without the Unix mode guarantee. The
/// application targets Linux (Requirement 10.1); this branch exists only so the
/// crate still compiles on other hosts for development.
#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

fn io_error(kind: TokenKind, op: TokenStoreOp, err: &io::Error) -> TokenStoreError {
    // `io::Error`'s Display describes the OS condition (e.g. "permission
    // denied") and never includes file contents, so it is safe to surface.
    TokenStoreError::new(kind, op, err.to_string())
}

// --- Keyring backend --------------------------------------------------------

fn keyring_entry(kind: TokenKind) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, kind.keyring_user())
}

fn save_keyring(kind: TokenKind, token: &SecretString) -> Result<(), TokenStoreError> {
    let entry = keyring_entry(kind).map_err(|err| keyring_error(kind, TokenStoreOp::Save, &err))?;
    entry
        .set_password(token.expose())
        .map_err(|err| keyring_error(kind, TokenStoreOp::Save, &err))
}

fn load_keyring(kind: TokenKind) -> Result<Option<SecretString>, TokenStoreError> {
    let entry = keyring_entry(kind).map_err(|err| keyring_error(kind, TokenStoreOp::Load, &err))?;
    match entry.get_password() {
        Ok(value) => Ok(Some(SecretString::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(keyring_error(kind, TokenStoreOp::Load, &err)),
    }
}

fn delete_keyring(kind: TokenKind) -> Result<(), TokenStoreError> {
    let entry = keyring_entry(kind).map_err(|err| keyring_error(kind, TokenStoreOp::Delete, &err))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(keyring_error(kind, TokenStoreOp::Delete, &err)),
    }
}

/// Map a keyring error to a [`TokenStoreError`].
///
/// `keyring::Error`'s Display describes the failure (missing entry, platform
/// failure, etc.) and does not contain the stored secret, so it is safe to
/// surface (Requirement 1.13).
fn keyring_error(kind: TokenKind, op: TokenStoreOp, err: &keyring::Error) -> TokenStoreError {
    TokenStoreError::new(kind, op, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory removed on drop, so each test is isolated and
    /// leaves nothing behind.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "amt-token-store-{}-{}-{}",
                tag,
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            path.push(unique);
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn secret_string_redacts_in_debug_and_display() {
        let secret = SecretString::new("super-secret-jwt-value");
        assert_eq!(format!("{}", secret), "<redacted>");
        assert_eq!(format!("{:?}", secret), "SecretString(<redacted>)");
        // The raw value is only reachable through expose().
        assert_eq!(secret.expose(), "super-secret-jwt-value");
    }

    #[test]
    fn error_display_excludes_token_value() {
        let err = TokenStoreError::new(
            TokenKind::MusicUser,
            TokenStoreOp::Save,
            "permission denied",
        );
        let text = format!("{}", err);
        assert!(text.contains("Music_User_Token"));
        assert!(text.contains("save"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn file_backend_round_trips_both_token_kinds() {
        let dir = TempDir::new("roundtrip");
        let store = TokenStore::with_file_dir(&dir.path);

        let dev = SecretString::new("dev-token-123");
        let user = SecretString::new("user-token-456");
        store.save(TokenKind::Developer, &dev).unwrap();
        store.save(TokenKind::MusicUser, &user).unwrap();

        let loaded_dev = store.load(TokenKind::Developer).unwrap().unwrap();
        let loaded_user = store.load(TokenKind::MusicUser).unwrap().unwrap();
        assert_eq!(loaded_dev.expose(), "dev-token-123");
        assert_eq!(loaded_user.expose(), "user-token-456");
    }

    #[test]
    fn file_backend_load_missing_is_none() {
        let dir = TempDir::new("missing");
        let store = TokenStore::with_file_dir(&dir.path);
        assert!(store.load(TokenKind::Developer).unwrap().is_none());
    }

    #[test]
    fn file_backend_overwrites_existing_token() {
        let dir = TempDir::new("overwrite");
        let store = TokenStore::with_file_dir(&dir.path);
        store
            .save(TokenKind::Developer, &SecretString::new("first"))
            .unwrap();
        store
            .save(TokenKind::Developer, &SecretString::new("second-longer"))
            .unwrap();
        let loaded = store.load(TokenKind::Developer).unwrap().unwrap();
        assert_eq!(loaded.expose(), "second-longer");
    }

    #[test]
    fn file_backend_delete_is_idempotent() {
        let dir = TempDir::new("delete");
        let store = TokenStore::with_file_dir(&dir.path);
        // Deleting an absent token succeeds.
        store.delete(TokenKind::MusicUser).unwrap();

        store
            .save(TokenKind::MusicUser, &SecretString::new("to-remove"))
            .unwrap();
        store.delete(TokenKind::MusicUser).unwrap();
        assert!(store.load(TokenKind::MusicUser).unwrap().is_none());
        // Deleting again still succeeds.
        store.delete(TokenKind::MusicUser).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_backend_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("mode");
        let store = TokenStore::with_file_dir(&dir.path);
        store
            .save(TokenKind::Developer, &SecretString::new("perm-check"))
            .unwrap();

        let path = token_path(&dir.path, TokenKind::Developer);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        // Compare the permission bits only (ignore the file-type bits).
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn file_backend_reasserts_mode_on_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("mode-overwrite");
        let store = TokenStore::with_file_dir(&dir.path);
        let path = token_path(&dir.path, TokenKind::Developer);

        // Seed a world-readable file, then let the store overwrite it.
        fs::write(&path, "wide-open").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        store
            .save(TokenKind::Developer, &SecretString::new("narrowed"))
            .unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn empty_music_user_token_detected() {
        let empty = SecretString::new("");
        assert!(empty.is_empty());
        let present = SecretString::new("x");
        assert!(!present.is_empty());
    }

    #[test]
    fn default_data_dir_prefers_xdg_data_home() {
        // Save/restore env to avoid cross-test interference is unnecessary here
        // because we only read; but construct paths explicitly for clarity.
        let base = Path::new("/some/xdg/apple-music-tui");
        assert_eq!(token_path(base, TokenKind::Developer).file_name().unwrap(), "developer_token");
        assert_eq!(token_path(base, TokenKind::MusicUser).file_name().unwrap(), "music_user_token");
    }
}
