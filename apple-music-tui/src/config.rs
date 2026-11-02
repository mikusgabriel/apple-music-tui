//! Configuration types, parsing, validation, and defaults.
//!
//! Defines `Configuration` and the `TokenStoreBackend` enum, serialized as TOML
//! (Requirement 8.5 round-trip), and the `Config_Parser` that reads TOML into a
//! `Configuration` with typed, line-positioned error reporting.
//!
//! Parsing behavior (Requirement 8):
//! - Validates the type and value range of every present setting
//!   (seek step `1..=300`, Storefront as an ISO 3166-1 alpha-2 code).
//! - Returns a [`ConfigError`] naming the offending setting and its 1-based
//!   line number; syntax errors that cannot be tied to a setting carry a line
//!   number only (Requirement 8.3).
//! - Applies defaults for every absent setting without rewriting the file
//!   (Requirement 8.6).
//! - Ignores unknown settings, emitting a [`ConfigWarning`] naming the setting
//!   and its 1-based line number (Requirement 8.8).
//!
//! The `Config_Printer` ([`print_config`]) serializes a `Configuration` back to
//! TOML that [`parse_config`] accepts without error (Requirement 8.4), and the
//! print→parse round-trip preserves every setting (Requirement 8.5). The
//! `Configuration` holds no tokens or credentials, so the printed output never
//! contains any (Requirement 8.4).
//!
//! The Config_File lives at `$XDG_CONFIG_HOME/apple-music-tui/config.toml`,
//! defaulting to `~/.config/apple-music-tui/config.toml` when `$XDG_CONFIG_HOME`
//! is unset or empty ([`config_file_path`]). When the file is absent,
//! [`ensure_config_file`] creates it and any missing parent directories with the
//! printed default `Configuration` (Requirement 8.2); on a filesystem failure it
//! returns a [`ConfigFileWarning`] naming the path and reason so startup can warn
//! and continue with the defaults (Requirement 8.7).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the Token_Store persists tokens (Requirement 1.5, default `File`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TokenStoreBackend {
    #[default]
    File,
    Keyring,
}

/// Default seek step in seconds (Requirement 4.6).
pub const DEFAULT_SEEK_STEP_SECONDS: u32 = 10;

/// Minimum valid seek step in seconds (Requirement 4.6).
pub const MIN_SEEK_STEP_SECONDS: u32 = 1;

/// Maximum valid seek step in seconds (Requirement 4.6).
pub const MAX_SEEK_STEP_SECONDS: u32 = 300;

/// Default Storefront, an ISO 3166-1 alpha-2 country code (Requirement 8.3).
///
/// The parser rejects an empty or otherwise invalid Storefront, so the default
/// Configuration must carry a concrete valid code for the printed default file
/// to round-trip through [`parse_config`] (Requirement 8.5). `us` is the Apple
/// Music default storefront.
pub const DEFAULT_STOREFRONT: &str = "us";

/// User configuration, serialized as TOML.
///
/// `keybindings` overlays the defaults (Requirement 7.4); keys are action names
/// and values are key sequences. Validation of ranges and storefront codes is
/// performed by [`parse_config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    /// ISO 3166-1 alpha-2 storefront code (Requirement 8.3).
    pub storefront: String,
    /// Seek step in seconds, valid range 1..=300, default 10 (Requirement 4.6).
    pub seek_step_seconds: u32,
    /// Whether MPRIS integration is enabled (Requirement 11, default false).
    pub mpris_enabled: bool,
    /// Token storage backend (Requirement 1.5, default File).
    pub token_store_backend: TokenStoreBackend,
    /// Keybinding overlays mapping action name -> key sequence (Requirement 7.4).
    pub keybindings: BTreeMap<String, String>,
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration {
            storefront: DEFAULT_STOREFRONT.to_string(),
            seek_step_seconds: DEFAULT_SEEK_STEP_SECONDS,
            mpris_enabled: false,
            token_store_backend: TokenStoreBackend::default(),
            keybindings: BTreeMap::new(),
        }
    }
}

/// An error produced while parsing or validating the Config_File (Requirement 8.3).
///
/// Carries the 1-based `line` number of the offending content and, when the
/// error can be attributed to a particular setting, the `setting` name. Syntax
/// errors that cannot be tied to a setting carry a line number only (`setting`
/// is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The name of the offending setting, when the error can be attributed to
    /// one. `None` for syntax errors that cannot be tied to a setting.
    pub setting: Option<String>,
    /// The 1-based line number where the error occurred.
    pub line: usize,
    /// A human-readable description of the problem.
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.setting {
            Some(setting) => write!(
                f,
                "config error at line {}, setting `{}`: {}",
                self.line, setting, self.message
            ),
            None => write!(f, "config error at line {}: {}", self.line, self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// A warning produced while parsing the Config_File for an unknown setting
/// (Requirement 8.8). The setting is ignored and parsing continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// The name of the unknown setting that was ignored.
    pub setting: String,
    /// The 1-based line number where the unknown setting appears.
    pub line: usize,
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown config setting `{}` at line {} (ignored)",
            self.setting, self.line
        )
    }
}

/// Top-level setting names recognized by the Configuration object. Any other
/// top-level key is an unknown setting (Requirement 8.8).
const KNOWN_SETTINGS: &[&str] = &[
    "storefront",
    "seek_step_seconds",
    "mpris_enabled",
    "token_store_backend",
    "keybindings",
];

/// Parse the TOML text of the Config_File into a [`Configuration`].
///
/// On success returns the validated `Configuration` together with any
/// [`ConfigWarning`]s for unknown settings that were ignored (Requirement 8.8).
/// Absent settings take their default value without the file being rewritten
/// (Requirement 8.6).
///
/// On failure returns a [`ConfigError`] naming the 1-based line number and, when
/// the error can be tied to a setting, the setting name (Requirement 8.3).
pub fn parse_config(text: &str) -> Result<(Configuration, Vec<ConfigWarning>), ConfigError> {
    // Parse into an editable document so each key/value retains its byte span,
    // which we convert to 1-based line numbers for diagnostics. Syntax errors
    // surface here and may not be tied to a particular setting.
    let doc = text
        .parse::<toml_edit::ImDocument<String>>()
        .map_err(|err| syntax_error(text, &err))?;

    let mut config = Configuration::default();
    let mut warnings = Vec::new();

    for (key, item) in doc.iter() {
        let line = line_of(text, key_span_start(&doc, key));
        match key {
            "storefront" => {
                config.storefront = parse_storefront(item, line)?;
            }
            "seek_step_seconds" => {
                config.seek_step_seconds = parse_seek_step(item, line)?;
            }
            "mpris_enabled" => {
                config.mpris_enabled = parse_bool(item, "mpris_enabled", line)?;
            }
            "token_store_backend" => {
                config.token_store_backend = parse_backend(item, line)?;
            }
            "keybindings" => {
                config.keybindings = parse_keybindings(text, item, line)?;
            }
            unknown => {
                // Unknown setting: ignore it, record a warning, keep going.
                warnings.push(ConfigWarning {
                    setting: unknown.to_string(),
                    line,
                });
            }
        }
    }

    debug_assert!(KNOWN_SETTINGS.len() == 5);
    Ok((config, warnings))
}

/// Convert a byte offset into a 1-based line number within `text`.
fn line_of(text: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(text.len());
    // Lines are 1-based; count the newlines preceding the offset.
    text[..clamped].bytes().filter(|&b| b == b'\n').count() + 1
}

/// The starting byte offset of a top-level key's definition, if known.
fn key_span_start(doc: &toml_edit::ImDocument<String>, key: &str) -> usize {
    doc.key(key)
        .and_then(|k| k.span())
        .map(|span| span.start)
        .unwrap_or(0)
}

/// Build a [`ConfigError`] from a TOML syntax error, deriving the 1-based line
/// from the error span when available (Requirement 8.3: line only for syntax
/// errors that cannot be tied to a setting).
fn syntax_error(text: &str, err: &toml_edit::TomlError) -> ConfigError {
    let line = err
        .span()
        .map(|span| line_of(text, span.start))
        .unwrap_or(1);
    ConfigError {
        setting: None,
        line,
        message: format!("invalid TOML syntax: {}", err.message()),
    }
}

/// A type-mismatch error for a setting at a known line.
fn type_error(setting: &str, line: usize, expected: &str) -> ConfigError {
    ConfigError {
        setting: Some(setting.to_string()),
        line,
        message: format!("expected a {} value", expected),
    }
}

fn parse_storefront(item: &toml_edit::Item, line: usize) -> Result<String, ConfigError> {
    let value = item
        .as_str()
        .ok_or_else(|| type_error("storefront", line, "string"))?;
    if !is_iso_3166_1_alpha2(value) {
        return Err(ConfigError {
            setting: Some("storefront".to_string()),
            line,
            message: format!(
                "`{}` is not a valid ISO 3166-1 alpha-2 country code",
                value
            ),
        });
    }
    Ok(value.to_string())
}

fn parse_seek_step(item: &toml_edit::Item, line: usize) -> Result<u32, ConfigError> {
    let value = item
        .as_integer()
        .ok_or_else(|| type_error("seek_step_seconds", line, "integer"))?;
    if value < MIN_SEEK_STEP_SECONDS as i64 || value > MAX_SEEK_STEP_SECONDS as i64 {
        return Err(ConfigError {
            setting: Some("seek_step_seconds".to_string()),
            line,
            message: format!(
                "{} is out of range; must be between {} and {} seconds",
                value, MIN_SEEK_STEP_SECONDS, MAX_SEEK_STEP_SECONDS
            ),
        });
    }
    Ok(value as u32)
}

fn parse_bool(
    item: &toml_edit::Item,
    setting: &str,
    line: usize,
) -> Result<bool, ConfigError> {
    item.as_bool()
        .ok_or_else(|| type_error(setting, line, "boolean"))
}

fn parse_backend(item: &toml_edit::Item, line: usize) -> Result<TokenStoreBackend, ConfigError> {
    let value = item
        .as_str()
        .ok_or_else(|| type_error("token_store_backend", line, "string"))?;
    // Accept the serde-serialized forms (`File`/`Keyring`) case-insensitively so
    // the Config_Printer output round-trips (Requirement 8.5).
    match value.to_ascii_lowercase().as_str() {
        "file" => Ok(TokenStoreBackend::File),
        "keyring" => Ok(TokenStoreBackend::Keyring),
        _ => Err(ConfigError {
            setting: Some("token_store_backend".to_string()),
            line,
            message: format!("`{}` is not a valid backend; expected `File` or `Keyring`", value),
        }),
    }
}

fn parse_keybindings(
    text: &str,
    item: &toml_edit::Item,
    line: usize,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let table = item
        .as_table_like()
        .ok_or_else(|| type_error("keybindings", line, "table"))?;
    let mut map = BTreeMap::new();
    for (action, binding) in table.iter() {
        let binding_line = binding
            .span()
            .map(|span| line_of(text, span.start))
            .unwrap_or(line);
        let sequence = binding.as_str().ok_or_else(|| ConfigError {
            setting: Some(format!("keybindings.{}", action)),
            line: binding_line,
            message: "expected a string key sequence".to_string(),
        })?;
        map.insert(action.to_string(), sequence.to_string());
    }
    Ok(map)
}

/// Whether `code` is a valid ISO 3166-1 alpha-2 country code (Requirement 8.3).
///
/// Comparison is case-insensitive against the officially assigned set.
fn is_iso_3166_1_alpha2(code: &str) -> bool {
    if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let upper = code.to_ascii_uppercase();
    ISO_3166_1_ALPHA2.binary_search(&upper.as_str()).is_ok()
}

/// Officially assigned ISO 3166-1 alpha-2 country codes, sorted for binary
/// search.
const ISO_3166_1_ALPHA2: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "YE", "YT", "ZA", "ZM", "ZW",
];

// ---------------------------------------------------------------------------
// Config_Printer (Requirements 8.4, 8.5)
// ---------------------------------------------------------------------------

/// Serialize a [`Configuration`] to TOML that [`parse_config`] accepts without
/// error (Requirement 8.4).
///
/// The output contains only the five Configuration settings; `Configuration`
/// holds no tokens or other credentials, so the printed TOML never contains any
/// (Requirement 8.4). Parsing the result reproduces an equal `Configuration`,
/// including every Keybinding (Requirement 8.5 round-trip).
///
/// The Storefront is emitted verbatim and the backend is emitted in the
/// serde-serialized `File`/`Keyring` form that [`parse_backend`] accepts.
/// Keybindings are written as a `[keybindings]` table in sorted key order (the
/// backing map is a [`BTreeMap`], so iteration is already ordered), and the
/// table is omitted entirely when there are no overrides.
pub fn print_config(config: &Configuration) -> String {
    let mut doc = toml_edit::DocumentMut::new();

    doc["storefront"] = toml_edit::value(config.storefront.as_str());
    doc["seek_step_seconds"] = toml_edit::value(config.seek_step_seconds as i64);
    doc["mpris_enabled"] = toml_edit::value(config.mpris_enabled);
    doc["token_store_backend"] = toml_edit::value(backend_to_str(config.token_store_backend));

    // Only emit the keybindings table when there are overrides; an empty table
    // round-trips to an empty map just the same, but omitting it keeps the
    // default file minimal (Requirement 8.2) and matches an absent-settings
    // Configuration exactly.
    if !config.keybindings.is_empty() {
        let mut table = toml_edit::Table::new();
        for (action, binding) in &config.keybindings {
            table[action.as_str()] = toml_edit::value(binding.as_str());
        }
        doc["keybindings"] = toml_edit::Item::Table(table);
    }

    doc.to_string()
}

/// The TOML string form of a [`TokenStoreBackend`], matching the values
/// [`parse_backend`] accepts so the printed output round-trips (Requirement 8.5).
fn backend_to_str(backend: TokenStoreBackend) -> &'static str {
    match backend {
        TokenStoreBackend::File => "File",
        TokenStoreBackend::Keyring => "Keyring",
    }
}

// ---------------------------------------------------------------------------
// Config_File path resolution and default-file creation (Requirements 8.2, 8.7)
// ---------------------------------------------------------------------------

/// Application directory under the XDG config root.
const APP_DIR: &str = "apple-music-tui";

/// File name of the Config_File within the config directory.
const CONFIG_FILE: &str = "config.toml";

/// A warning produced when the default Config_File cannot be created or written
/// because of a filesystem error (Requirement 8.7).
///
/// The TUI is expected to display this (naming the path and reason) and continue
/// startup using the default [`Configuration`]; it is not a fatal error.
#[derive(Debug)]
pub enum ConfigFileWarning {
    /// The XDG config directory could not be resolved (neither `$XDG_CONFIG_HOME`
    /// nor `$HOME` is set), so no destination path is known.
    NoConfigDir,
    /// A filesystem operation failed (creating the parent directories or writing
    /// the file). `path` is the file or directory the operation acted on.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for ConfigFileWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigFileWarning::NoConfigDir => write!(
                f,
                "could not create the default config file: neither $XDG_CONFIG_HOME \
                 nor $HOME is set, so the config path cannot be determined; \
                 continuing with the default configuration"
            ),
            ConfigFileWarning::Io { path, source } => write!(
                f,
                "could not create the config file at {}: {source}; \
                 continuing with the default configuration",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigFileWarning {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigFileWarning::NoConfigDir => None,
            ConfigFileWarning::Io { source, .. } => Some(source),
        }
    }
}

/// Resolve the directory that holds the Config_File:
/// `$XDG_CONFIG_HOME/apple-music-tui`, defaulting to
/// `~/.config/apple-music-tui` when `$XDG_CONFIG_HOME` is unset or empty.
///
/// Returns `None` only when neither `$XDG_CONFIG_HOME` nor `$HOME` is available.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Resolve the full Config_File path:
/// `$XDG_CONFIG_HOME/apple-music-tui/config.toml`, defaulting to
/// `~/.config/apple-music-tui/config.toml`.
pub fn config_file_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(CONFIG_FILE))
}

/// Pure resolver for the config directory from the given environment values, so
/// the XDG precedence can be tested without touching real process environment
/// variables (mirrors the session module's `state_dir_from`).
///
/// `$XDG_CONFIG_HOME` is used only when it is set and non-empty; otherwise the
/// default `~/.config` is derived from `$HOME`.
fn config_dir_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join(APP_DIR));
        }
    }
    let home = home.filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".config").join(APP_DIR))
}

/// Ensure the Config_File exists at the default XDG path, creating it with the
/// printed default [`Configuration`] when absent (Requirement 8.2).
///
/// Returns:
/// - `Ok(None)` when the file already exists (left untouched),
/// - `Ok(Some(path))` when a default file was created,
/// - `Err(ConfigFileWarning)` when the path cannot be resolved or a filesystem
///   error prevents creation; the caller should warn and continue with the
///   default `Configuration` (Requirement 8.7).
pub fn ensure_config_file() -> Result<Option<PathBuf>, ConfigFileWarning> {
    let path = config_file_path().ok_or(ConfigFileWarning::NoConfigDir)?;
    ensure_config_file_at(&path).map(|created| created.then_some(path))
}

/// Ensure a Config_File exists at `path`, creating it and any missing parent
/// directories with the printed default [`Configuration`] when absent
/// (Requirement 8.2). Returns `Ok(true)` when a file was created and `Ok(false)`
/// when it already existed.
///
/// An existing file is never overwritten, so defaults are not rewritten over a
/// user's configuration (Requirement 8.6). On any filesystem failure this
/// returns a [`ConfigFileWarning::Io`] naming the offending path (Requirement
/// 8.7); the destination is created with `create_new` so a file appearing
/// between the existence check and the write is reported rather than clobbered.
pub fn ensure_config_file_at(path: &Path) -> Result<bool, ConfigFileWarning> {
    if path.exists() {
        return Ok(false);
    }

    // Create any missing parent directories (Requirement 8.2).
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| ConfigFileWarning::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let contents = print_config(&Configuration::default());

    // `create_new` fails if the file was created concurrently, so we never
    // overwrite existing content (Requirement 8.6).
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        // Someone else created the file first; treat it as already present.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(source) => {
            return Err(ConfigFileWarning::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    file.write_all(contents.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|source| ConfigFileWarning::Io {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_list_is_sorted_for_binary_search() {
        let mut sorted = ISO_3166_1_ALPHA2.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ISO_3166_1_ALPHA2.to_vec());
    }

    #[test]
    fn empty_file_yields_all_defaults_and_no_warnings() {
        let (config, warnings) = parse_config("").expect("empty config parses");
        assert_eq!(config, Configuration::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn absent_settings_take_defaults() {
        let (config, warnings) = parse_config("storefront = \"us\"\n").unwrap();
        assert_eq!(config.storefront, "us");
        // Everything else is the default.
        assert_eq!(config.seek_step_seconds, DEFAULT_SEEK_STEP_SECONDS);
        assert!(!config.mpris_enabled);
        assert_eq!(config.token_store_backend, TokenStoreBackend::File);
        assert!(config.keybindings.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn parses_all_known_settings() {
        let text = concat!(
            "storefront = \"GB\"\n",
            "seek_step_seconds = 30\n",
            "mpris_enabled = true\n",
            "token_store_backend = \"Keyring\"\n",
            "\n",
            "[keybindings]\n",
            "play_pause = \"space\"\n",
            "quit = \"q\"\n",
        );
        let (config, warnings) = parse_config(text).unwrap();
        assert_eq!(config.storefront, "GB");
        assert_eq!(config.seek_step_seconds, 30);
        assert!(config.mpris_enabled);
        assert_eq!(config.token_store_backend, TokenStoreBackend::Keyring);
        assert_eq!(config.keybindings.get("play_pause").map(String::as_str), Some("space"));
        assert_eq!(config.keybindings.get("quit").map(String::as_str), Some("q"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn backend_is_case_insensitive() {
        let (config, _) = parse_config("token_store_backend = \"keyring\"\n").unwrap();
        assert_eq!(config.token_store_backend, TokenStoreBackend::Keyring);
        let (config, _) = parse_config("token_store_backend = \"FILE\"\n").unwrap();
        assert_eq!(config.token_store_backend, TokenStoreBackend::File);
    }

    #[test]
    fn invalid_storefront_names_setting_and_line() {
        let text = "seek_step_seconds = 10\nstorefront = \"zz\"\n";
        let err = parse_config(text).unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("storefront"));
        assert_eq!(err.line, 2);
    }

    #[test]
    fn storefront_wrong_type_is_type_error() {
        let err = parse_config("storefront = 42\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("storefront"));
        assert_eq!(err.line, 1);
        assert!(err.message.contains("string"));
    }

    #[test]
    fn seek_step_out_of_range_low() {
        let err = parse_config("seek_step_seconds = 0\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("seek_step_seconds"));
        assert_eq!(err.line, 1);
    }

    #[test]
    fn seek_step_out_of_range_high() {
        let err = parse_config("seek_step_seconds = 301\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("seek_step_seconds"));
        assert_eq!(err.line, 1);
    }

    #[test]
    fn seek_step_boundaries_are_valid() {
        let (low, _) = parse_config("seek_step_seconds = 1\n").unwrap();
        assert_eq!(low.seek_step_seconds, 1);
        let (high, _) = parse_config("seek_step_seconds = 300\n").unwrap();
        assert_eq!(high.seek_step_seconds, 300);
    }

    #[test]
    fn seek_step_wrong_type_is_type_error() {
        let err = parse_config("seek_step_seconds = \"ten\"\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("seek_step_seconds"));
        assert!(err.message.contains("integer"));
    }

    #[test]
    fn mpris_wrong_type_is_type_error() {
        let err = parse_config("mpris_enabled = \"yes\"\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("mpris_enabled"));
        assert!(err.message.contains("boolean"));
    }

    #[test]
    fn invalid_backend_value() {
        let err = parse_config("token_store_backend = \"vault\"\n").unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("token_store_backend"));
        assert_eq!(err.line, 1);
    }

    #[test]
    fn unknown_setting_is_ignored_with_warning() {
        let text = "storefront = \"us\"\nunknown_option = true\n";
        let (config, warnings) = parse_config(text).unwrap();
        assert_eq!(config.storefront, "us");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].setting, "unknown_option");
        assert_eq!(warnings[0].line, 2);
    }

    #[test]
    fn syntax_error_has_line_and_no_setting() {
        // Missing value after `=` is a syntax error that cannot be tied to a
        // setting value.
        let text = "storefront = \"us\"\nseek_step_seconds =\n";
        let err = parse_config(text).unwrap_err();
        assert!(err.setting.is_none());
        assert!(err.line >= 1);
    }

    #[test]
    fn keybinding_wrong_type_names_scoped_setting() {
        let text = "[keybindings]\nquit = 5\n";
        let err = parse_config(text).unwrap_err();
        assert_eq!(err.setting.as_deref(), Some("keybindings.quit"));
        assert_eq!(err.line, 2);
    }

    #[test]
    fn valid_storefront_is_case_insensitive() {
        assert!(is_iso_3166_1_alpha2("us"));
        assert!(is_iso_3166_1_alpha2("US"));
        assert!(is_iso_3166_1_alpha2("Gb"));
        assert!(!is_iso_3166_1_alpha2("zz"));
        assert!(!is_iso_3166_1_alpha2("u"));
        assert!(!is_iso_3166_1_alpha2("usa"));
        assert!(!is_iso_3166_1_alpha2("1a"));
    }

    // --- Config_Printer and defaults (Requirements 8.2, 8.4, 8.5, 8.7) ---

    #[test]
    fn default_storefront_is_a_valid_code() {
        // The default Configuration must carry a parseable Storefront so the
        // printed default file round-trips (Requirement 8.5).
        assert!(is_iso_3166_1_alpha2(&Configuration::default().storefront));
        assert_eq!(Configuration::default().storefront, DEFAULT_STOREFRONT);
    }

    #[test]
    fn printed_default_parses_without_error() {
        let printed = print_config(&Configuration::default());
        let (config, warnings) =
            parse_config(&printed).expect("printed default config must parse");
        assert_eq!(config, Configuration::default());
        assert!(warnings.is_empty(), "default config has no unknown settings");
    }

    #[test]
    fn printer_output_contains_no_credentials() {
        // Configuration holds no tokens or credentials, so the printed form must
        // emit only the five known settings (Requirement 8.4). `token` appears
        // only as part of the benign `token_store_backend` setting name, never
        // as a value, so we assert the output is exactly the known settings
        // rather than scanning for substrings.
        let mut config = Configuration::default();
        config
            .keybindings
            .insert("play_pause".to_string(), "space".to_string());
        let printed = print_config(&config);

        // Every non-blank, non-header line must be one of the known settings or
        // a keybinding entry; nothing resembling a credential can appear.
        for line in printed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let key = line.split('=').next().unwrap_or("").trim();
            let is_known_setting = KNOWN_SETTINGS.contains(&key);
            let is_keybinding = config.keybindings.contains_key(key);
            assert!(
                is_known_setting || is_keybinding,
                "unexpected line in printed config (possible credential leak): {line}"
            );
        }

        // Guard against a secret-bearing value ever being introduced. The
        // `token_store_backend` name is expected; its value is not a credential.
        let printed_lower = printed.to_lowercase();
        for forbidden in ["secret", "credential", "password", "bearer"] {
            assert!(
                !printed_lower.contains(forbidden),
                "printed config must not contain `{forbidden}`: {printed}"
            );
        }
    }

    #[test]
    fn printed_default_is_exact_toml_content() {
        // The concrete default file written when the Config_File is absent
        // (Requirement 8.2): the four default settings in print order, each on
        // its own line, with no keybindings table. This pins the exact bytes so
        // a change to the default content is caught deliberately.
        let printed = print_config(&Configuration::default());
        let expected = "\
storefront = \"us\"
seek_step_seconds = 10
mpris_enabled = false
token_store_backend = \"File\"
";
        assert_eq!(printed, expected);
    }

    #[test]
    fn print_parse_round_trips_all_settings_including_keybindings() {
        let mut config = Configuration {
            storefront: "GB".to_string(),
            seek_step_seconds: 45,
            mpris_enabled: true,
            token_store_backend: TokenStoreBackend::Keyring,
            keybindings: BTreeMap::new(),
        };
        config
            .keybindings
            .insert("play_pause".to_string(), "space".to_string());
        config
            .keybindings
            .insert("quit".to_string(), "ctrl+c".to_string());

        let printed = print_config(&config);
        let (restored, warnings) = parse_config(&printed).expect("round-trip parses");
        assert_eq!(restored, config);
        assert!(warnings.is_empty());
    }

    #[test]
    fn printer_omits_empty_keybindings_table() {
        let printed = print_config(&Configuration::default());
        assert!(
            !printed.contains("[keybindings]"),
            "no keybindings table when there are no overrides: {printed}"
        );
    }

    #[test]
    fn config_dir_prefers_xdg_config_home_when_set() {
        let dir = config_dir_from(
            Some(OsString::from("/custom/config")),
            Some(OsString::from("/home/user")),
        )
        .expect("resolves");
        assert_eq!(dir, PathBuf::from("/custom/config").join(APP_DIR));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_xdg_unset_or_empty() {
        let expected = PathBuf::from("/home/user").join(".config").join(APP_DIR);

        let from_unset = config_dir_from(None, Some(OsString::from("/home/user")))
            .expect("resolves from home");
        assert_eq!(from_unset, expected);

        let from_empty = config_dir_from(
            Some(OsString::from("")),
            Some(OsString::from("/home/user")),
        )
        .expect("empty xdg falls back to home");
        assert_eq!(from_empty, expected);
    }

    #[test]
    fn config_dir_is_none_without_xdg_or_home() {
        assert!(config_dir_from(None, None).is_none());
        assert!(config_dir_from(Some(OsString::from("")), Some(OsString::from(""))).is_none());
    }

    #[test]
    fn config_file_path_ends_with_app_dir_and_file() {
        let dir = config_dir_from(Some(OsString::from("/c")), None).unwrap();
        let path = dir.join(CONFIG_FILE);
        assert_eq!(path, PathBuf::from("/c").join(APP_DIR).join(CONFIG_FILE));
    }

    /// A unique temp directory for a test, created under the OS temp dir.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "amtui-config-test-{}-{}-{}",
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
    fn ensure_creates_default_file_and_parents_when_absent() {
        let dir = unique_temp_dir("create");
        let path = dir.join("a").join("b").join(CONFIG_FILE);

        let created = ensure_config_file_at(&path).expect("creates default file");
        assert!(created, "file should have been created");
        assert!(path.exists(), "config file should exist after creation");

        // The written content is the printed default and round-trips cleanly.
        let text = fs::read_to_string(&path).expect("read back");
        assert_eq!(text, print_config(&Configuration::default()));
        let (config, warnings) = parse_config(&text).expect("default file parses");
        assert_eq!(config, Configuration::default());
        assert!(warnings.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_leaves_existing_file_untouched() {
        let dir = unique_temp_dir("existing");
        let path = dir.join(CONFIG_FILE);

        // Seed a user-authored config that differs from the defaults.
        let user_text = "storefront = \"gb\"\nseek_step_seconds = 5\n";
        fs::write(&path, user_text).expect("seed existing config");

        let created = ensure_config_file_at(&path).expect("no error when present");
        assert!(!created, "existing file must not be recreated");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            user_text,
            "existing config must not be overwritten (Requirement 8.6)"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn ensure_warns_on_filesystem_error_naming_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("perm");
        // Make the directory read-only so creating the file fails.
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o500); // r-x------ : no write
        fs::set_permissions(&dir, perms).unwrap();

        let path = dir.join(CONFIG_FILE);
        let result = ensure_config_file_at(&path);

        // Restore permissions before asserting so cleanup always works.
        let mut restore = fs::metadata(&dir).unwrap().permissions();
        restore.set_mode(0o700);
        fs::set_permissions(&dir, restore).unwrap();

        match result {
            Err(ConfigFileWarning::Io { path: p, .. }) => {
                assert_eq!(p, path, "warning should name the offending path");
            }
            other => panic!("expected an Io warning, got {other:?}"),
        }
        assert!(!path.exists(), "no config file should have been left behind");

        fs::remove_dir_all(&dir).ok();
    }

    // --- Property-based test for Config validation (Property 19) ---

    use proptest::prelude::*;

    /// One top-level setting rendered as a single TOML line, carrying whether
    /// the value we chose is valid for that setting. The generators below pick
    /// from both the valid and invalid value spaces for each setting so the
    /// property exercises the full accept/reject boundary.
    #[derive(Debug, Clone)]
    struct SettingLine {
        /// The exact `key = value` text written on its own line.
        text: String,
        /// The top-level setting name the parser should attribute an error to.
        setting: &'static str,
        /// Whether the value is valid for this setting (correct type and range).
        valid: bool,
    }

    /// Generate a `storefront = <value>` line, valid or invalid.
    fn storefront_line() -> impl Strategy<Value = SettingLine> {
        prop_oneof![
            // Valid: a real ISO 3166-1 alpha-2 code (any case).
            (proptest::sample::select(ISO_3166_1_ALPHA2), any::<bool>()).prop_map(
                |(code, lower)| {
                    let code = if lower { code.to_ascii_lowercase() } else { code.to_string() };
                    SettingLine {
                        text: format!("storefront = \"{code}\""),
                        setting: "storefront",
                        valid: true,
                    }
                }
            ),
            // Invalid value: a string that is not a valid country code.
            prop_oneof![
                Just("zz".to_string()),
                Just("usa".to_string()),
                Just("u".to_string()),
                Just("1a".to_string()),
                Just("".to_string()),
            ]
            .prop_map(|v| SettingLine {
                text: format!("storefront = \"{v}\""),
                setting: "storefront",
                valid: false,
            }),
            // Invalid type: a non-string value.
            Just(SettingLine {
                text: "storefront = 42".to_string(),
                setting: "storefront",
                valid: false,
            }),
        ]
    }

    /// Generate a `seek_step_seconds = <value>` line, valid or invalid.
    fn seek_step_line() -> impl Strategy<Value = SettingLine> {
        prop_oneof![
            // Valid: an integer within the inclusive 1..=300 range.
            (MIN_SEEK_STEP_SECONDS..=MAX_SEEK_STEP_SECONDS).prop_map(|n| SettingLine {
                text: format!("seek_step_seconds = {n}"),
                setting: "seek_step_seconds",
                valid: true,
            }),
            // Invalid: an out-of-range integer (below 1 or above 300).
            prop_oneof![
                (i64::from(i32::MIN)..=0i64),
                ((MAX_SEEK_STEP_SECONDS as i64 + 1)..=i64::from(i32::MAX)),
            ]
            .prop_map(|n| SettingLine {
                text: format!("seek_step_seconds = {n}"),
                setting: "seek_step_seconds",
                valid: false,
            }),
            // Invalid type: a non-integer value.
            Just(SettingLine {
                text: "seek_step_seconds = \"ten\"".to_string(),
                setting: "seek_step_seconds",
                valid: false,
            }),
        ]
    }

    /// Generate an `mpris_enabled = <value>` line, valid or invalid.
    fn mpris_line() -> impl Strategy<Value = SettingLine> {
        prop_oneof![
            any::<bool>().prop_map(|b| SettingLine {
                text: format!("mpris_enabled = {b}"),
                setting: "mpris_enabled",
                valid: true,
            }),
            // Invalid type: not a boolean.
            Just(SettingLine {
                text: "mpris_enabled = \"yes\"".to_string(),
                setting: "mpris_enabled",
                valid: false,
            }),
        ]
    }

    /// Generate a `token_store_backend = <value>` line, valid or invalid.
    fn backend_line() -> impl Strategy<Value = SettingLine> {
        prop_oneof![
            // Valid: an accepted backend name, any case.
            proptest::sample::select(vec!["File", "Keyring", "file", "keyring", "FILE"])
                .prop_map(|v| SettingLine {
                    text: format!("token_store_backend = \"{v}\""),
                    setting: "token_store_backend",
                    valid: true,
                }),
            // Invalid value: an unrecognized backend name.
            prop_oneof![Just("vault"), Just("none"), Just("db")].prop_map(|v| SettingLine {
                text: format!("token_store_backend = \"{v}\""),
                setting: "token_store_backend",
                valid: false,
            }),
            // Invalid type: not a string.
            Just(SettingLine {
                text: "token_store_backend = true".to_string(),
                setting: "token_store_backend",
                valid: false,
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 19: Config validation
        //
        // Validates: Requirements 8.3
        //
        // For any Config_File content, the Config_Parser succeeds if and only
        // if every present setting has a valid type and value (including a
        // Storefront that is a valid ISO 3166-1 alpha-2 code); otherwise it
        // returns an error naming the offending setting and 1-based line
        // number.
        #[test]
        fn property_config_validation(
            // Each of the four settings is independently either present (with a
            // valid-or-invalid value) or absent. Building from per-setting
            // optionals guarantees distinct top-level keys, so the assembled
            // TOML is always syntactically well-formed and any error is a
            // type/value error tied to a setting (the subject of Property 19),
            // never a duplicate-key syntax error.
            storefront in proptest::option::of(storefront_line()),
            seek_step in proptest::option::of(seek_step_line()),
            mpris in proptest::option::of(mpris_line()),
            backend in proptest::option::of(backend_line()),
        ) {
            let lines: Vec<SettingLine> =
                [storefront, seek_step, mpris, backend].into_iter().flatten().collect();

            // Assemble the generated settings into a Config_File, one per line,
            // remembering each setting's 1-based line number and validity.
            let mut text = String::new();
            let mut per_line: Vec<SettingLine> = Vec::new();
            for line in &lines {
                text.push_str(&line.text);
                text.push('\n');
                per_line.push(line.clone());
            }

            let result = parse_config(&text);
            let all_valid = per_line.iter().all(|l| l.valid);

            if all_valid {
                // iff direction 1: every present setting is valid -> success.
                prop_assert!(
                    result.is_ok(),
                    "expected success for all-valid config:\n{text}\n got {result:?}"
                );
            } else {
                // iff direction 2: some present setting is invalid -> error.
                let err = match result {
                    Err(e) => e,
                    Ok(ok) => {
                        return Err(TestCaseError::fail(format!(
                            "expected an error for config with an invalid setting:\n{text}\n got {ok:?}"
                        )));
                    }
                };

                // The error must name an offending setting (type/value errors
                // are always tied to a setting here; the generated TOML is
                // always syntactically valid).
                let named = err.setting.as_deref().expect("value error names a setting");

                // The named setting must be one of the invalid ones we wrote.
                let invalid_settings: Vec<&str> =
                    per_line.iter().filter(|l| !l.valid).map(|l| l.setting).collect();
                prop_assert!(
                    invalid_settings.contains(&named),
                    "error named `{named}` but invalid settings were {invalid_settings:?}\n{text}"
                );

                // The 1-based line number must be within the file and point at
                // a line whose setting matches the one named in the error.
                prop_assert!(err.line >= 1 && err.line <= per_line.len(),
                    "line {} out of range 1..={}\n{text}", err.line, per_line.len());
                let line_setting = per_line[err.line - 1].setting;
                prop_assert_eq!(
                    line_setting, named,
                    "error line {} holds `{}` but error named `{}`\n{}",
                    err.line, line_setting, named, text
                );
            }
        }
    }
}

#[cfg(test)]
mod property_tests {
    //! Property-based tests for the Configuration subsystem, each exercising a
    //! single design correctness property at >=100 iterations.
    use super::*;
    use proptest::collection::btree_map;
    use proptest::prelude::*;

    /// Generate a valid ISO 3166-1 alpha-2 storefront code by indexing into the
    /// officially assigned set the parser validates against, mixing in random
    /// case so the printer's verbatim output and the case-insensitive parser
    /// both round-trip (Requirement 8.5).
    fn valid_storefront() -> impl Strategy<Value = String> {
        (0..ISO_3166_1_ALPHA2.len(), any::<bool>()).prop_map(|(i, lower)| {
            let code = ISO_3166_1_ALPHA2[i];
            if lower {
                code.to_ascii_lowercase()
            } else {
                code.to_string()
            }
        })
    }

    /// Generate an arbitrary valid [`Configuration`]: a valid storefront, a seek
    /// step in `1..=300`, arbitrary MPRIS/backend flags, and arbitrary
    /// keybindings (arbitrary action names and key sequences).
    fn arbitrary_configuration() -> impl Strategy<Value = Configuration> {
        (
            valid_storefront(),
            MIN_SEEK_STEP_SECONDS..=MAX_SEEK_STEP_SECONDS,
            any::<bool>(),
            prop_oneof![
                Just(TokenStoreBackend::File),
                Just(TokenStoreBackend::Keyring),
            ],
            btree_map(".*", ".*", 0..8),
        )
            .prop_map(
                |(storefront, seek_step_seconds, mpris_enabled, token_store_backend, keybindings)| {
                    Configuration {
                        storefront,
                        seek_step_seconds,
                        mpris_enabled,
                        token_store_backend,
                        keybindings,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 20: Config print then parse round-trip
        // Validates: Requirements 8.5
        //
        // For any valid Configuration, parsing the Config_Printer output yields a
        // Configuration equal to the original in every setting, including every
        // keybinding. Storefront comparison is case-insensitive because the
        // printer emits the code verbatim and the parser accepts it as given.
        #[test]
        fn property_config_print_parse_round_trip(config in arbitrary_configuration()) {
            let printed = print_config(&config);
            let (restored, warnings) =
                parse_config(&printed).expect("printed config must parse");

            // No setting of a valid Configuration is unknown, so there are no
            // warnings on a round-trip.
            prop_assert!(warnings.is_empty());

            // The storefront is preserved exactly as printed (the parser keeps
            // the given casing), so equality is byte-for-byte here.
            prop_assert_eq!(&restored.storefront, &config.storefront);
            prop_assert_eq!(restored.seek_step_seconds, config.seek_step_seconds);
            prop_assert_eq!(restored.mpris_enabled, config.mpris_enabled);
            prop_assert_eq!(restored.token_store_backend, config.token_store_backend);
            prop_assert_eq!(&restored.keybindings, &config.keybindings);

            // And the whole Configuration is equal in every setting.
            prop_assert_eq!(restored, config);
        }
    }

    /// Render a single top-level scalar setting to the TOML line
    /// [`parse_config`] accepts, matching the forms [`print_config`] emits.
    fn render_setting(config: &Configuration, setting: &str) -> String {
        match setting {
            "storefront" => format!("storefront = \"{}\"\n", config.storefront),
            "seek_step_seconds" => format!("seek_step_seconds = {}\n", config.seek_step_seconds),
            "mpris_enabled" => format!("mpris_enabled = {}\n", config.mpris_enabled),
            "token_store_backend" => format!(
                "token_store_backend = \"{}\"\n",
                match config.token_store_backend {
                    TokenStoreBackend::File => "File",
                    TokenStoreBackend::Keyring => "Keyring",
                }
            ),
            other => unreachable!("unexpected scalar setting {other}"),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 21: Config applies defaults for absent settings
        //
        // Validates: Requirements 8.6
        //
        // For any valid Config_File with an arbitrary subset of settings omitted,
        // the parsed Configuration equals that file's present settings combined
        // with the default value for each omitted setting, and no defaults are
        // written back to the file. parse_config is pure over its `&str` input,
        // so the file text is unchanged after parsing.
        #[test]
        fn property_config_applies_defaults_for_absent_settings(
            full in arbitrary_configuration(),
            // Independent present/absent choice for each of the four scalar
            // settings plus the keybindings table, in declaration order.
            present in prop::collection::vec(any::<bool>(), 5),
        ) {
            let scalars = [
                "storefront",
                "seek_step_seconds",
                "mpris_enabled",
                "token_store_backend",
            ];

            // Build the Config_File from only the present settings. A valid
            // storefront code never needs quoting beyond the usual TOML string,
            // and the keybinding map is generated with arbitrary strings, so emit
            // the keybindings table via the printer to keep escaping correct.
            let mut text = String::new();
            for (setting, &keep) in scalars.iter().zip(present.iter()) {
                if keep {
                    text.push_str(&render_setting(&full, setting));
                }
            }

            // The keybindings table is the fifth present/absent choice. An empty
            // table and an absent table both mean "no overrides", so only treat
            // keybindings as present when kept and non-empty. Reuse the printer's
            // table rendering (via a keybindings-only Configuration) so arbitrary
            // action names and key sequences are escaped exactly as the parser
            // expects.
            let keybindings_present = present[4] && !full.keybindings.is_empty();
            if keybindings_present {
                let only_keys = Configuration {
                    keybindings: full.keybindings.clone(),
                    ..Configuration::default()
                };
                let printed = print_config(&only_keys);
                // Keep just the `[keybindings]` table and its entries from the
                // printed output; the leading scalar defaults are dropped so this
                // case exercises an absent-scalar + present-table file.
                if let Some(idx) = printed.find("[keybindings]") {
                    text.push('\n');
                    text.push_str(&printed[idx..]);
                }
            }

            // Expected: present settings carry `full`'s value; absent settings
            // take the default.
            let defaults = Configuration::default();
            let expected = Configuration {
                storefront: if present[0] { full.storefront.clone() } else { defaults.storefront.clone() },
                seek_step_seconds: if present[1] { full.seek_step_seconds } else { defaults.seek_step_seconds },
                mpris_enabled: if present[2] { full.mpris_enabled } else { defaults.mpris_enabled },
                token_store_backend: if present[3] { full.token_store_backend } else { defaults.token_store_backend },
                keybindings: if keybindings_present { full.keybindings.clone() } else { defaults.keybindings.clone() },
            };

            let original = text.clone();
            let (parsed, warnings) = parse_config(&text).expect("valid config parses");

            // Present settings keep their value; absent settings take the default.
            prop_assert_eq!(parsed, expected);
            // A valid file of known settings yields no unknown-setting warnings.
            prop_assert!(warnings.is_empty());
            // Defaults are applied in memory only: the file text is untouched.
            prop_assert_eq!(text, original);
        }
    }
}
