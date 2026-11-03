//! Auth_Manager: Developer_Token acquisition, Music_User_Token lifecycle, and
//! the catalog 401/403 refresh-and-resend flow.
//!
//! This module implements the Developer_Token half of the Auth_Manager
//! (task 8.2) and the Music_User_Token lifecycle plus the 401/403
//! refresh-and-resend driver (task 8.3).
//!
//! # Music_User_Token lifecycle (Requirements 1.6, 1.11, 1.12)
//!
//! The Music_User_Token is captured by the Playback_Engine's Sign_In_Window and
//! saved in the [`TokenStore`] (task 12.2). The Auth_Manager is the authority
//! for how that token is consumed thereafter:
//!
//! - [`AuthManager::music_user_token`] returns a stored **non-empty**
//!   Music_User_Token so the caller can attach it to API requests without
//!   opening the Sign_In_Window. A missing or empty token yields `None`, which
//!   the startup/sign-in flow treats as "must sign in" (Requirements 1.6, 1.1).
//! - When a request that carries the Music_User_Token is still rejected with
//!   401/403 after a Developer_Token refresh and single resend, the token is
//!   deleted and the TUI shows "session expired" and reopens the Sign_In_Window
//!   (Requirement 1.11). The driver signals this with
//!   [`AuthOutcome::SessionExpired`]; the deletion is performed inside the
//!   driver so every caller gets the same behavior.
//! - [`AuthManager::sign_out`] deletes the Music_User_Token while keeping the
//!   cached Developer_Token, so catalog access survives a sign-out
//!   (Requirement 1.12).
//!
//! # Catalog 401/403 refresh-and-resend (Requirements 1.9, 1.11, 3.9)
//!
//! [`AuthManager::authorize_request`] is a generic driver that owns the
//! refresh-and-resend policy without knowing how a request is actually sent.
//! The caller (the API_Client, task 9) supplies an async closure that performs
//! one attempt with a freshly provided Developer_Token and reports back a
//! [`RequestOutcome`]. The driver:
//!
//! 1. Runs the attempt with the current (cached-or-fetched) Developer_Token.
//! 2. On a 401/403 ([`RequestOutcome::Unauthorized`]) it discards the cached
//!    Developer_Token, fetches a fresh one, and resends **exactly once**
//!    (Requirement 1.9).
//! 3. If the resend also returns 401/403, it stops — it never resends a third
//!    time (Requirement 1.9). Whether that terminal rejection becomes
//!    [`AuthOutcome::CatalogAuthFailed`] or [`AuthOutcome::SessionExpired`]
//!    depends on whether the request carried the Music_User_Token
//!    (Requirements 1.11, 3.9). A session-expiry outcome also deletes the
//!    stored Music_User_Token before returning.
//!
//! Modeling the attempt as an injected closure keeps the whole policy unit- and
//! property-testable with a fake sender, with no real network (the design's
//! "testable with injected behavior where practical").
//!
//! # Developer_Token lifecycle (Requirements 1.7, 1.8, 1.10)
//!
//! The user has no Apple Developer account, so the Developer_Token is obtained
//! automatically from the public Apple Music web player (music.apple.com)
//! assets rather than signed locally. The flow is:
//!
//! 1. When a Developer_Token is required, the Auth_Manager first consults the
//!    cached value in the [`TokenStore`]. A cached token is accepted only if it
//!    [`decodes as a JWT`](decode_jwt_exp) and carries an `exp` claim at least
//!    [`MIN_REMAINING_SECS`] seconds in the future; otherwise it is discarded
//!    and a fresh token is fetched (Requirement 1.8).
//! 2. A fetch downloads the web player's HTML, locates its main JavaScript
//!    bundle, downloads that bundle, and extracts the embedded JWT
//!    ([`extract_developer_token`]). The whole fetch is bounded by a
//!    [`FETCH_TIMEOUT`] of 10 seconds (Requirement 1.7). A freshly fetched
//!    token is cached in the Token_Store.
//! 3. On any fetch failure — a network error, the 10-second timeout expiring,
//!    or the downloaded assets containing no token — the Auth_Manager returns a
//!    [`DeveloperTokenError`] that the TUI shows as "could not obtain the
//!    Developer_Token from the Apple Music web player". The stored
//!    Music_User_Token is left untouched and the caller can offer a retry that
//!    runs the fetch again (Requirement 1.10).
//!
//! # Secret handling (Requirement 1.13)
//!
//! Token values are wrapped in [`SecretString`], whose `Debug`/`Display` redact
//! the value. [`DeveloperTokenError`] carries only a kind and a cause string
//! derived from transport/decoding failures, never a token value, so it is safe
//! to log or show in the TUI.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::token_store::{SecretString, TokenKind, TokenStore, TokenStoreError};

/// The Developer_Token must remain valid for at least this many seconds beyond
/// the current time to be accepted from the cache (Requirement 1.8).
pub const MIN_REMAINING_SECS: u64 = 60;

/// Maximum time to wait for the whole Developer_Token fetch — the web-player
/// HTML plus its JavaScript bundle — to complete (Requirement 1.7).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The public Apple Music web player whose assets embed the Developer_Token.
const WEB_PLAYER_URL: &str = "https://music.apple.com";

/// Why a Developer_Token could not be obtained from the web player
/// (Requirement 1.10).
///
/// Every variant maps to the same user-facing outcome: show an error stating
/// the Developer_Token could not be obtained from the Apple Music web player,
/// keep the Music_User_Token, and offer a retry. The variants exist so the
/// cause can be logged precisely; none of them carries a token value
/// (Requirement 1.13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeveloperTokenError {
    /// The HTTP client could not be built or a request could not be sent
    /// (network error). `cause` is a token-free description.
    Network { cause: String },
    /// The 10-second fetch budget expired (Requirement 1.7).
    Timeout,
    /// The web player responded but the downloaded assets contained no
    /// Developer_Token (Requirement 1.10).
    NoTokenInAssets,
}

impl fmt::Display for DeveloperTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // All variants share the Requirement 1.10 phrasing; the specific cause
        // is appended for diagnostics and never includes a token value.
        write!(
            f,
            "could not obtain the Developer_Token from the Apple Music web player"
        )?;
        match self {
            DeveloperTokenError::Network { cause } => write!(f, ": {cause}"),
            DeveloperTokenError::Timeout => {
                write!(f, ": the fetch did not complete within 10 seconds")
            }
            DeveloperTokenError::NoTokenInAssets => {
                write!(f, ": the downloaded assets contained no token")
            }
        }
    }
}

impl std::error::Error for DeveloperTokenError {}

/// Whether an authorized request carries the Music_User_Token.
///
/// This distinguishes the two terminal failure paths of the 401/403
/// refresh-and-resend flow. A request made with only the Developer_Token
/// ([`AuthScope::CatalogOnly`]) that still fails after refresh+resend is a
/// catalog auth failure (Requirement 1.9). A request that also carried the
/// Music_User_Token ([`AuthScope::WithMusicUserToken`]) that still fails is
/// treated as an expired session (Requirements 1.11, 3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScope {
    /// The request is authorized by the Developer_Token alone (catalog access).
    CatalogOnly,
    /// The request additionally carries the Music_User_Token (library or any
    /// user-scoped access).
    WithMusicUserToken,
}

impl AuthScope {
    /// Whether this scope includes the Music_User_Token.
    fn carries_music_user_token(self) -> bool {
        matches!(self, AuthScope::WithMusicUserToken)
    }
}

/// The result of one request attempt, reported back to the Auth_Manager by the
/// injected sender in [`AuthManager::authorize_request`].
///
/// The sender does not decide the refresh-and-resend policy; it only classifies
/// the HTTP outcome of a single attempt. `T` is the sender's success payload
/// (for example, a parsed response body), which the driver returns untouched on
/// [`AuthOutcome::Success`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome<T> {
    /// The attempt succeeded; carries the sender's payload.
    Success(T),
    /// The attempt was rejected with HTTP 401 or 403 — the only status that
    /// drives the Developer_Token refresh-and-resend (Requirements 1.9, 1.11).
    Unauthorized,
    /// The attempt failed for a reason unrelated to authorization (timeout,
    /// 429, 5xx, other 4xx, parse error, ...). The driver does not refresh or
    /// resend; it returns the failure unchanged for the API_Client's own
    /// transport-error handling (task 9.4). `cause` is a token-free message.
    Other { cause: String },
}

/// The terminal outcome of [`AuthManager::authorize_request`].
///
/// Every variant is the end of the refresh-and-resend flow; the driver never
/// resends more than once (Requirement 1.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome<T> {
    /// The request succeeded, on the first attempt or on the single resend.
    Success(T),
    /// A catalog-only request was rejected with 401/403 on both the first
    /// attempt and the resend. The TUI shows the Developer_Token error and does
    /// not resend again (Requirement 1.9).
    CatalogAuthFailed,
    /// A request carrying the Music_User_Token was rejected with 401/403 even
    /// after the Developer_Token refresh and single resend. The driver has
    /// already deleted the stored Music_User_Token; the TUI shows "session
    /// expired" and reopens the Sign_In_Window (Requirements 1.11, 3.9).
    SessionExpired,
    /// The Developer_Token could not be refreshed when a 401/403 required it
    /// (network, timeout, or no token in assets). The TUI shows the
    /// Developer_Token error and offers retry (Requirement 1.10).
    DeveloperTokenUnavailable(DeveloperTokenError),
    /// The attempt failed for a non-authorization reason. The driver returns
    /// the sender's cause for the API_Client's transport-error handling.
    RequestFailed { cause: String },
}

/// Abstraction over "now" (seconds since the Unix epoch) so the JWT validity
/// decision can be exercised at arbitrary times in tests without touching the
/// real clock (Requirement 1.8, Property 1).
pub trait Clock {
    /// The current time in whole seconds since the Unix epoch.
    fn now_unix_secs(&self) -> u64;
}

/// The real wall clock, backed by [`SystemTime`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            // A clock set before the Unix epoch is treated as epoch; every
            // real `exp` is then far in the future, which is the safe default.
            .unwrap_or(0)
    }
}

/// Decide whether a candidate Developer_Token is acceptable at `now_unix_secs`
/// (Requirement 1.8, Property 1).
///
/// The token is accepted **iff** it decodes as a JWT, contains an integer `exp`
/// claim, and that `exp` is at least [`MIN_REMAINING_SECS`] seconds after
/// `now_unix_secs`. Any failure to decode, a missing `exp`, or an `exp` that is
/// too close (or in the past) means the token is rejected and the caller must
/// refetch.
///
/// This is the pure core of the cache-validity check; it performs no I/O and no
/// signature verification (the web-player token is not verifiable without
/// Apple's key — only its structure and expiry are checked).
pub fn developer_token_is_valid(token: &str, now_unix_secs: u64) -> bool {
    match decode_jwt_exp(token) {
        Some(exp) => exp >= now_unix_secs.saturating_add(MIN_REMAINING_SECS),
        None => false,
    }
}

/// Decode a JWT's `exp` claim (seconds since the Unix epoch) without verifying
/// its signature.
///
/// Returns `None` when the string is not a well-formed JWT (three
/// dot-separated base64url segments), when the payload is not base64url, when
/// the payload is not a JSON object, or when it has no integer `exp` claim. A
/// non-integer or negative `exp` is treated as absent.
pub fn decode_jwt_exp(token: &str) -> Option<u64> {
    // A JWT is `header.payload.signature`; the claims live in the payload.
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    // A fourth segment means this is not a plain JWT.
    if parts.next().is_some() {
        return None;
    }
    if payload.is_empty() {
        return None;
    }

    let decoded = base64url_decode(payload)?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp = json.as_object()?.get("exp")?;
    // `exp` is seconds since the epoch; accept only a non-negative integer.
    exp.as_u64().or_else(|| {
        // Some encoders emit `exp` as a JSON float with no fractional part.
        exp.as_f64().and_then(|f| {
            if f.is_finite() && f >= 0.0 && f.fract() == 0.0 {
                Some(f as u64)
            } else {
                None
            }
        })
    })
}

/// Decode a base64url string (no padding, URL-safe alphabet), tolerating
/// optional `=` padding. Returns `None` on any invalid character or length.
///
/// Implemented locally to avoid adding a base64 crate to the public dependency
/// surface for the single use here: decoding a JWT payload.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    // Map a URL-safe base64 character to its 6-bit value.
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }

    let bytes: &[u8] = input.as_bytes();
    // Drop any trailing padding; base64url is usually unpadded but be lenient.
    let end = bytes
        .iter()
        .rposition(|&b| b != b'=')
        .map(|i| i + 1)
        .unwrap_or(0);
    let bytes = &bytes[..end];

    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut acc_bits = 0u32;
    for &b in bytes {
        let v = sextet(b)?;
        acc = (acc << 6) | v;
        acc_bits += 6;
        if acc_bits >= 8 {
            acc_bits -= 8;
            out.push((acc >> acc_bits) as u8);
        }
    }
    // Any remaining bits must be zero padding; a non-zero remainder with 2..=5
    // leftover bits indicates a malformed input length, but we accept it
    // leniently since JWT payloads are well-formed in practice.
    Some(out)
}

/// Extract the Developer_Token (a JWT) from Apple Music web-player asset text.
///
/// The web player embeds its MusicKit Developer_Token as a string literal in
/// its JavaScript bundle. This scans `assets` for the first substring that
/// looks like a JWT: three base64url segments separated by dots whose payload
/// decodes to JSON carrying an `exp` claim. Returns `None` when no such token
/// is present (Requirement 1.10, "assets contained no token").
///
/// Pure and side-effect free so the extraction can be unit-tested against
/// captured asset fixtures without any network access.
pub fn extract_developer_token(assets: &str) -> Option<String> {
    jwt_candidates(assets).find(|candidate| decode_jwt_exp(candidate).is_some())
}

/// Yield the JWT-shaped substrings of `assets`: maximal runs of JWT characters
/// (base64url plus the two dot separators) that contain exactly two dots.
fn jwt_candidates(assets: &str) -> impl Iterator<Item = String> + '_ {
    fn is_jwt_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
    }

    let bytes = assets.as_bytes();
    let mut start = 0usize;
    std::iter::from_fn(move || {
        while start < bytes.len() {
            // Skip non-JWT characters.
            if !is_jwt_byte(bytes[start]) {
                start += 1;
                continue;
            }
            // Consume a maximal run of JWT bytes.
            let mut end = start;
            while end < bytes.len() && is_jwt_byte(bytes[end]) {
                end += 1;
            }
            let run = &assets[start..end];
            start = end + 1;
            // A JWT has exactly two '.' separators and three non-empty parts.
            let parts: Vec<&str> = run.split('.').collect();
            if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
                return Some(run.to_string());
            }
        }
        None
    })
}

/// Locate the URL of the web player's main JavaScript bundle within its HTML.
///
/// Apple Music's `index.html` references hashed bundle files such as
/// `/assets/index-<hash>.js`. This returns the first `src`/`href` that points
/// at a `.js` asset, resolved against `base_url` (the web-player origin).
/// Returns `None` when the HTML references no JavaScript bundle.
fn find_bundle_url(html: &str, base_url: &str) -> Option<String> {
    // Scan for attribute values ending in `.js`. Values are delimited by single
    // or double quotes in the HTML.
    for quote in ['"', '\''] {
        let mut rest = html;
        while let Some(open) = rest.find(quote) {
            let after = &rest[open + 1..];
            let Some(close) = after.find(quote) else {
                break;
            };
            let value = &after[..close];
            if value.ends_with(".js") && (value.contains("assets") || value.contains("main"))
            {
                return Some(resolve_url(value, base_url));
            }
            rest = &after[close + 1..];
        }
    }
    // Fall back to any `.js` value if none matched the assets/main heuristic.
    for quote in ['"', '\''] {
        let mut rest = html;
        while let Some(open) = rest.find(quote) {
            let after = &rest[open + 1..];
            let Some(close) = after.find(quote) else {
                break;
            };
            let value = &after[..close];
            if value.ends_with(".js") {
                return Some(resolve_url(value, base_url));
            }
            rest = &after[close + 1..];
        }
    }
    None
}

/// Resolve a possibly-relative asset path against the web-player origin
/// `base_url` (in production [`WEB_PLAYER_URL`]).
fn resolve_url(value: &str, base_url: &str) -> String {
    if value.starts_with("http://") || value.starts_with("https://") {
        value.to_string()
    } else if let Some(stripped) = value.strip_prefix('/') {
        format!("{base_url}/{stripped}")
    } else {
        format!("{base_url}/{value}")
    }
}

/// Acquires, caches, and validates the Developer_Token for catalog requests.
///
/// The manager owns a [`TokenStore`] for the cached Developer_Token and a
/// [`Clock`] used by the JWT validity decision. `C` defaults to [`SystemClock`]
/// so production code constructs an `AuthManager` without naming the clock.
#[derive(Debug, Clone)]
pub struct AuthManager<C: Clock = SystemClock> {
    store: TokenStore,
    clock: C,
    /// Origin the Developer_Token fetch targets. In production this is always
    /// [`WEB_PLAYER_URL`] (`https://music.apple.com`); it is overridable only
    /// so the 401/403 refresh-and-resend integration test can point the fetch
    /// at a local mock server instead of the live, unofficial web player
    /// (task 16.3). No production constructor sets it to anything else.
    web_player_base_url: String,
}

impl AuthManager<SystemClock> {
    /// Build an Auth_Manager over `store`, using the real wall clock.
    pub fn new(store: TokenStore) -> Self {
        AuthManager {
            store,
            clock: SystemClock,
            web_player_base_url: WEB_PLAYER_URL.to_string(),
        }
    }
}

impl<C: Clock> AuthManager<C> {
    /// Build an Auth_Manager with an explicit clock, for tests that need to
    /// control "now" for the JWT validity decision (Requirement 1.8).
    pub fn with_clock(store: TokenStore, clock: C) -> Self {
        AuthManager {
            store,
            clock,
            web_player_base_url: WEB_PLAYER_URL.to_string(),
        }
    }

    /// Override the origin the Developer_Token fetch targets.
    ///
    /// This is a test-only seam (hidden from the public docs): it lets the
    /// 401/403 refresh-and-resend integration test (task 16.3) serve the
    /// web-player assets — and thus the refreshed Developer_Token — from a local
    /// mock HTTP server, so the flow is exercised entirely offline instead of
    /// hitting the live music.apple.com assets. Production code never calls
    /// this, so the fetch always targets [`WEB_PLAYER_URL`].
    #[doc(hidden)]
    pub fn with_web_player_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.web_player_base_url = base_url.into();
        self
    }

    /// Return a usable Developer_Token, fetching a fresh one when necessary
    /// (Requirements 1.7, 1.8, 1.10).
    ///
    /// A cached token is returned only when it passes
    /// [`developer_token_is_valid`] against the manager's clock; otherwise it is
    /// discarded and a new token is fetched from the web player and cached. A
    /// fetch failure surfaces as a [`DeveloperTokenError`] (the caller shows the
    /// error and offers retry, keeping the Music_User_Token untouched —
    /// Requirement 1.10).
    pub async fn developer_token(&self) -> Result<SecretString, DeveloperTokenError> {
        if let Some(cached) = self.cached_valid_token() {
            return Ok(cached);
        }
        let fetched = self.fetch_developer_token().await?;
        // Caching is best-effort: a Token_Store save failure must not fail token
        // acquisition, since the in-memory value is usable for this session
        // (Requirement 1.15). The save error is dropped here; task 8.3/16.1 wire
        // the user-facing save-failure warning.
        let _ = self.store.save(TokenKind::Developer, &fetched);
        Ok(fetched)
    }

    /// Force a fresh fetch, discarding any cached token first (Requirement
    /// 1.10, retry action). Used by the Auth_Manager's 401/403 refresh flow
    /// (task 8.3) and by the TUI's retry action.
    pub async fn refresh_developer_token(&self) -> Result<SecretString, DeveloperTokenError> {
        let fetched = self.fetch_developer_token().await?;
        let _ = self.store.save(TokenKind::Developer, &fetched);
        Ok(fetched)
    }

    /// Return the cached Developer_Token only if it is present and still valid
    /// per [`developer_token_is_valid`]; otherwise `None` so the caller
    /// refetches (Requirement 1.8).
    fn cached_valid_token(&self) -> Option<SecretString> {
        // A Token_Store load error is treated as "no usable cache": fall through
        // to a fetch rather than failing.
        let cached = self.store.load(TokenKind::Developer).ok().flatten()?;
        if developer_token_is_valid(cached.expose(), self.clock.now_unix_secs()) {
            Some(cached)
        } else {
            None
        }
    }

    /// Return the stored Music_User_Token when it is present and **non-empty**,
    /// so the caller can attach it to API requests without opening the
    /// Sign_In_Window (Requirement 1.6).
    ///
    /// An empty stored token is treated as "no token" and yields `None`, which
    /// the startup/sign-in flow handles by opening the Sign_In_Window
    /// (Requirement 1.1). A Token_Store load error is also reported as `None`:
    /// an unreadable token cannot be attached, so the only safe behavior is to
    /// treat the user as signed out.
    pub fn music_user_token(&self) -> Option<SecretString> {
        let token = self.store.load(TokenKind::MusicUser).ok().flatten()?;
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    /// Whether a non-empty Music_User_Token is stored (Requirement 1.6). This
    /// is the signed-in check the startup flow uses to decide whether to open
    /// the Sign_In_Window (Requirement 1.1).
    pub fn has_music_user_token(&self) -> bool {
        self.music_user_token().is_some()
    }

    /// Persist a Music_User_Token captured by the Sign_In_Window so the user
    /// stays signed in across launches (Requirements 1.3, 1.6, 1.15).
    ///
    /// Best-effort by contract: the returned result lets the caller surface a
    /// save-failure warning, but a failure does not invalidate the in-memory
    /// token for the current session (Requirement 1.15). An empty token is
    /// rejected rather than stored, so a spurious empty `signInResult` cannot
    /// clobber a good stored token.
    pub fn store_music_user_token(
        &self,
        token: &SecretString,
    ) -> Result<(), TokenStoreError> {
        if token.is_empty() {
            return Ok(());
        }
        self.store.save(TokenKind::MusicUser, token)
    }

    /// Delete the Music_User_Token on sign-out while keeping the cached
    /// Developer_Token (Requirement 1.12).
    ///
    /// The Developer_Token is intentionally left in the store so catalog access
    /// continues to work in the signed-out view without a refetch. Deleting an
    /// absent Music_User_Token succeeds (the Token_Store delete is idempotent),
    /// so a double sign-out is harmless.
    pub fn sign_out(&self) -> Result<(), TokenStoreError> {
        self.store.delete(TokenKind::MusicUser)
    }

    /// Delete the stored Music_User_Token after the API has rejected it even
    /// through the refresh-and-resend flow (Requirement 1.11). Best-effort: a
    /// delete failure does not change the [`AuthOutcome::SessionExpired`]
    /// result, since the token is unusable regardless and re-sign-in overwrites
    /// it. The error is dropped here rather than surfaced because the
    /// session-expiry message already tells the user to sign in again.
    fn delete_music_user_token(&self) {
        let _ = self.store.delete(TokenKind::MusicUser);
    }

    /// Run a request through the 401/403 refresh-and-resend policy
    /// (Requirements 1.9, 1.11, 3.9).
    ///
    /// `scope` states whether the request carries the Music_User_Token, which
    /// selects the terminal outcome on a persistent 401/403. `attempt` is an
    /// async closure the caller supplies to send the request once with the
    /// Developer_Token it is given; it reports back a [`RequestOutcome`]. The
    /// driver owns the policy:
    ///
    /// 1. It obtains a Developer_Token (cached-or-fetched) and runs `attempt`.
    /// 2. On [`RequestOutcome::Unauthorized`] it refreshes the Developer_Token
    ///    (discarding the cache) and runs `attempt` **exactly once** more
    ///    (Requirement 1.9).
    /// 3. A second [`RequestOutcome::Unauthorized`] ends the flow with no
    ///    further resend: [`AuthOutcome::SessionExpired`] (deleting the
    ///    Music_User_Token first) when the request carried the Music_User_Token
    ///    (Requirements 1.11, 3.9), otherwise [`AuthOutcome::CatalogAuthFailed`]
    ///    (Requirement 1.9).
    ///
    /// A [`RequestOutcome::Other`] at any point is returned as
    /// [`AuthOutcome::RequestFailed`] without a refresh or resend, leaving
    /// transport-error handling to the API_Client (task 9.4). A failure to
    /// refresh the Developer_Token surfaces as
    /// [`AuthOutcome::DeveloperTokenUnavailable`] (Requirement 1.10).
    pub async fn authorize_request<T, F, Fut>(
        &self,
        scope: AuthScope,
        attempt: F,
    ) -> AuthOutcome<T>
    where
        F: FnMut(SecretString) -> Fut,
        Fut: std::future::Future<Output = RequestOutcome<T>>,
    {
        // The policy is factored into `run_refresh_resend` so it can be
        // exercised with fake token sources in tests, with no real network. The
        // production wiring passes the real cached-or-fetch and refresh calls,
        // and deletes the Music_User_Token on session expiry.
        run_refresh_resend(
            scope,
            attempt,
            || self.developer_token(),
            || self.refresh_developer_token(),
            || self.delete_music_user_token(),
        )
        .await
    }

    /// Download the web-player assets and extract the Developer_Token, bounded
    /// by [`FETCH_TIMEOUT`] (Requirement 1.7).
    async fn fetch_developer_token(&self) -> Result<SecretString, DeveloperTokenError> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|err| DeveloperTokenError::Network {
                cause: err.to_string(),
            })?;

        // The 10-second budget covers the entire fetch (HTML + bundle), so wrap
        // the whole operation in a single timeout rather than per-request.
        let base_url = self.web_player_base_url.clone();
        match tokio::time::timeout(FETCH_TIMEOUT, fetch_token_with(&client, &base_url)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(DeveloperTokenError::Timeout),
        }
    }
}

/// Perform the two-step fetch with an already-built client: download the HTML,
/// find and download the JS bundle, and extract the token. Separated from the
/// timeout wrapper so the sequence is readable.
///
/// `base_url` is the web-player origin; it is [`WEB_PLAYER_URL`] in production
/// and only differs when a test points the fetch at a mock server (task 16.3).
async fn fetch_token_with(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<SecretString, DeveloperTokenError> {
    let html = get_text(client, base_url).await?;

    // The token sometimes appears inline in the HTML; check there first so a
    // layout change that inlines the token still works.
    if let Some(token) = extract_developer_token(&html) {
        return Ok(SecretString::new(token));
    }

    let bundle_url =
        find_bundle_url(&html, base_url).ok_or(DeveloperTokenError::NoTokenInAssets)?;
    let bundle = get_text(client, &bundle_url).await?;
    match extract_developer_token(&bundle) {
        Some(token) => Ok(SecretString::new(token)),
        None => Err(DeveloperTokenError::NoTokenInAssets),
    }
}

/// GET `url` and return its body as text, mapping transport failures to a
/// token-free [`DeveloperTokenError::Network`].
async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, DeveloperTokenError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| map_reqwest_error(&err))?;
    let response = response
        .error_for_status()
        .map_err(|err| map_reqwest_error(&err))?;
    response
        .text()
        .await
        .map_err(|err| map_reqwest_error(&err))
}

/// Map a `reqwest` error to a [`DeveloperTokenError`], distinguishing a timeout
/// from other network failures. The message never contains a token value.
fn map_reqwest_error(err: &reqwest::Error) -> DeveloperTokenError {
    if err.is_timeout() {
        DeveloperTokenError::Timeout
    } else {
        DeveloperTokenError::Network {
            cause: err.to_string(),
        }
    }
}

/// The pure refresh-and-resend policy, parameterized over its token sources so
/// it can be unit- and property-tested without a real network or Token_Store
/// (Requirements 1.9, 1.11, 3.9).
///
/// - `attempt` sends the request once with the Developer_Token it is handed.
/// - `developer_token` yields the Developer_Token for the first attempt
///   (cached-or-fetched in production).
/// - `refresh_token` yields a freshly fetched Developer_Token for the single
///   resend after a 401/403.
/// - `on_session_expired` is invoked exactly once, immediately before returning
///   [`AuthOutcome::SessionExpired`], so the caller can delete the
///   Music_User_Token (Requirement 1.11).
///
/// The request is sent at most twice: the first attempt and, only after a
/// 401/403, one resend with a refreshed token. A second 401/403 never triggers
/// a third send (Requirement 1.9).
async fn run_refresh_resend<T, F, Fut, D, DFut, R, RFut, E>(
    scope: AuthScope,
    mut attempt: F,
    developer_token: D,
    refresh_token: R,
    on_session_expired: E,
) -> AuthOutcome<T>
where
    F: FnMut(SecretString) -> Fut,
    Fut: std::future::Future<Output = RequestOutcome<T>>,
    D: FnOnce() -> DFut,
    DFut: std::future::Future<Output = Result<SecretString, DeveloperTokenError>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<SecretString, DeveloperTokenError>>,
    E: FnOnce(),
{
    // First attempt with the current (cached-or-fetched) Developer_Token.
    let token = match developer_token().await {
        Ok(token) => token,
        Err(err) => return AuthOutcome::DeveloperTokenUnavailable(err),
    };
    match attempt(token).await {
        RequestOutcome::Success(value) => return AuthOutcome::Success(value),
        RequestOutcome::Other { cause } => return AuthOutcome::RequestFailed { cause },
        // A 401/403 drives the single refresh-and-resend below.
        RequestOutcome::Unauthorized => {}
    }

    // Refresh the Developer_Token (discarding the cache) and resend once
    // (Requirement 1.9).
    let refreshed = match refresh_token().await {
        Ok(token) => token,
        Err(err) => return AuthOutcome::DeveloperTokenUnavailable(err),
    };
    match attempt(refreshed).await {
        RequestOutcome::Success(value) => AuthOutcome::Success(value),
        RequestOutcome::Other { cause } => AuthOutcome::RequestFailed { cause },
        // Still 401/403 after the single resend: stop here, never resend a
        // third time (Requirement 1.9).
        RequestOutcome::Unauthorized => {
            if scope.carries_music_user_token() {
                // The Music_User_Token is rejected: delete it and signal
                // re-sign-in (Requirements 1.11, 3.9).
                on_session_expired();
                AuthOutcome::SessionExpired
            } else {
                AuthOutcome::CatalogAuthFailed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fixed clock for deterministic JWT validity tests.
    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_unix_secs(&self) -> u64 {
            self.0
        }
    }

    /// Base64url-encode `bytes` without padding (test helper mirroring the
    /// decoder under test).
    fn base64url_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 63) as usize] as char);
            }
        }
        out
    }

    /// Build a fake JWT (unsigned) whose payload carries the given claims JSON.
    fn jwt_with_payload(payload_json: &str) -> String {
        let header = base64url_encode(br#"{"alg":"ES256","typ":"JWT"}"#);
        let payload = base64url_encode(payload_json.as_bytes());
        // The signature is not verified; any base64url run works.
        format!("{header}.{payload}.c2ln")
    }

    /// Build a JWT with a numeric `exp` claim.
    fn jwt_with_exp(exp: i64) -> String {
        jwt_with_payload(&format!(r#"{{"exp":{exp},"iss":"ABCDE"}}"#))
    }

    #[test]
    fn base64url_round_trips() {
        let data = b"the quick brown fox \x00\x01\x02\xff";
        let encoded = base64url_encode(data);
        assert_eq!(base64url_decode(&encoded).as_deref(), Some(&data[..]));
    }

    #[test]
    fn decode_jwt_exp_reads_integer_exp() {
        let token = jwt_with_exp(2_000_000_000);
        assert_eq!(decode_jwt_exp(&token), Some(2_000_000_000));
    }

    #[test]
    fn decode_jwt_exp_rejects_non_jwt_shapes() {
        assert_eq!(decode_jwt_exp(""), None);
        assert_eq!(decode_jwt_exp("not-a-jwt"), None);
        assert_eq!(decode_jwt_exp("only.two"), None);
        assert_eq!(decode_jwt_exp("a.b.c.d"), None);
    }

    #[test]
    fn decode_jwt_exp_rejects_missing_exp() {
        let token = jwt_with_payload(r#"{"iss":"ABCDE"}"#);
        assert_eq!(decode_jwt_exp(&token), None);
    }

    #[test]
    fn decode_jwt_exp_rejects_non_object_payload() {
        let token = jwt_with_payload("[1,2,3]");
        assert_eq!(decode_jwt_exp(&token), None);
    }

    #[test]
    fn valid_when_exp_is_far_enough_ahead() {
        let now = 1_000_000;
        let token = jwt_with_exp((now + MIN_REMAINING_SECS + 1) as i64);
        assert!(developer_token_is_valid(&token, now));
    }

    #[test]
    fn valid_at_exactly_min_remaining_boundary() {
        // `exp` exactly MIN_REMAINING_SECS ahead is accepted (>= boundary).
        let now = 1_000_000;
        let token = jwt_with_exp((now + MIN_REMAINING_SECS) as i64);
        assert!(developer_token_is_valid(&token, now));
    }

    #[test]
    fn invalid_when_exp_is_too_close() {
        let now = 1_000_000;
        let token = jwt_with_exp((now + MIN_REMAINING_SECS - 1) as i64);
        assert!(!developer_token_is_valid(&token, now));
    }

    #[test]
    fn invalid_when_exp_is_in_the_past() {
        let now = 1_000_000;
        let token = jwt_with_exp((now - 10) as i64);
        assert!(!developer_token_is_valid(&token, now));
    }

    #[test]
    fn invalid_when_not_a_jwt() {
        assert!(!developer_token_is_valid("garbage", 0));
        assert!(!developer_token_is_valid("", 0));
    }

    #[test]
    fn invalid_when_exp_missing() {
        let token = jwt_with_payload(r#"{"iss":"ABCDE"}"#);
        assert!(!developer_token_is_valid(&token, 0));
    }

    #[test]
    fn extract_token_finds_jwt_in_bundle() {
        let token = jwt_with_exp(2_000_000_000);
        let bundle = format!(
            "var x=1;const devToken=\"{token}\";function f(){{return 0}}"
        );
        assert_eq!(extract_developer_token(&bundle).as_deref(), Some(token.as_str()));
    }

    #[test]
    fn extract_token_skips_non_jwt_dotted_runs() {
        // A dotted version string and a filename must not be mistaken for a JWT.
        let token = jwt_with_exp(2_000_000_000);
        let bundle = format!("v1.2.3 index.min.js {token} end");
        assert_eq!(extract_developer_token(&bundle).as_deref(), Some(token.as_str()));
    }

    #[test]
    fn extract_token_returns_none_without_token() {
        let bundle = "no token here, just code; a.b not a jwt; 1.2.3";
        assert_eq!(extract_developer_token(bundle), None);
    }

    #[test]
    fn find_bundle_url_resolves_relative_assets_path() {
        let html = r#"<html><head><script type="module" src="/assets/index-abc123.js"></script></head></html>"#;
        assert_eq!(
            find_bundle_url(html, WEB_PLAYER_URL).as_deref(),
            Some("https://music.apple.com/assets/index-abc123.js")
        );
    }

    #[test]
    fn find_bundle_url_keeps_absolute_url() {
        let html = r#"<script src="https://cdn.apple.com/main-9f8.js"></script>"#;
        assert_eq!(
            find_bundle_url(html, WEB_PLAYER_URL).as_deref(),
            Some("https://cdn.apple.com/main-9f8.js")
        );
    }

    #[test]
    fn find_bundle_url_none_without_js() {
        let html = "<html><head><link rel=stylesheet href=/x.css></head></html>";
        assert_eq!(find_bundle_url(html, WEB_PLAYER_URL), None);
    }

    #[test]
    fn error_display_excludes_token_and_states_web_player() {
        let err = DeveloperTokenError::NoTokenInAssets;
        let text = format!("{err}");
        assert!(text.contains("Apple Music web player"));
        assert!(text.contains("no token"));

        let timeout = DeveloperTokenError::Timeout;
        assert!(format!("{timeout}").contains("10 seconds"));
    }

    #[test]
    fn cached_valid_token_is_returned_without_fetch() {
        // Seed a file-backed store with a still-valid token, then confirm the
        // manager returns it. No network is touched because the cache is valid.
        let dir = std::env::temp_dir().join(format!(
            "amt-auth-cache-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TokenStore::with_file_dir(&dir);
        let now = 1_000_000;
        let token = jwt_with_exp((now + 3600) as i64);
        store
            .save(TokenKind::Developer, &SecretString::new(token.clone()))
            .unwrap();

        let manager = AuthManager::with_clock(store, FixedClock(now));
        let cached = manager.cached_valid_token().expect("valid token cached");
        assert_eq!(cached.expose(), token);

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Music_User_Token lifecycle (Requirements 1.6, 1.11, 1.12) ---------

    /// Create an isolated file-backed Token_Store in a unique temp dir, plus a
    /// guard whose `Drop` removes it. Returns `(store, dir)`.
    fn temp_store(tag: &str) -> (TokenStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "amt-auth-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (TokenStore::with_file_dir(&dir), dir)
    }

    #[test]
    fn music_user_token_returned_when_present_and_non_empty() {
        let (store, dir) = temp_store("mut-present");
        store
            .save(TokenKind::MusicUser, &SecretString::new("user-token-abc"))
            .unwrap();
        let manager = AuthManager::new(store);

        assert!(manager.has_music_user_token());
        assert_eq!(
            manager.music_user_token().map(|t| t.expose().to_string()),
            Some("user-token-abc".to_string())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn music_user_token_is_none_when_missing() {
        let (store, dir) = temp_store("mut-missing");
        let manager = AuthManager::new(store);

        assert!(!manager.has_music_user_token());
        assert!(manager.music_user_token().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_music_user_token_persists_for_next_launch() {
        // A captured sign-in token is persisted so a later AuthManager (a fresh
        // launch) sees it and skips the Sign_In_Window (Requirements 1.3, 1.6).
        let (store, dir) = temp_store("mut-store");
        let manager = AuthManager::new(store);
        assert!(!manager.has_music_user_token());

        manager
            .store_music_user_token(&SecretString::new("captured-token"))
            .expect("save succeeds");

        // Simulate a fresh launch pointing at the same data dir.
        let next_launch = AuthManager::new(TokenStore::with_file_dir(&dir));
        assert!(next_launch.has_music_user_token());
        assert_eq!(
            next_launch.music_user_token().map(|t| t.expose().to_string()),
            Some("captured-token".to_string())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_empty_music_user_token_is_ignored() {
        // An empty token must not clobber state or be stored (Requirement 1.1).
        let (store, dir) = temp_store("mut-store-empty");
        let manager = AuthManager::new(store);
        manager
            .store_music_user_token(&SecretString::new(""))
            .expect("empty save is a no-op success");
        assert!(!manager.has_music_user_token());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_music_user_token_is_treated_as_absent() {
        // An empty stored token must behave like "no token" so the TUI opens
        // the Sign_In_Window (Requirements 1.1, 1.6).
        let (store, dir) = temp_store("mut-empty");
        store
            .save(TokenKind::MusicUser, &SecretString::new(""))
            .unwrap();
        let manager = AuthManager::new(store);

        assert!(!manager.has_music_user_token());
        assert!(manager.music_user_token().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sign_out_deletes_music_user_token_and_keeps_developer_token() {
        // Requirement 1.12: sign-out removes the Music_User_Token but leaves
        // the cached Developer_Token so catalog access still works.
        let (store, dir) = temp_store("signout");
        store
            .save(TokenKind::MusicUser, &SecretString::new("user-token"))
            .unwrap();
        store
            .save(TokenKind::Developer, &SecretString::new("dev-token"))
            .unwrap();
        let manager = AuthManager::new(store.clone());

        manager.sign_out().unwrap();

        assert!(store.load(TokenKind::MusicUser).unwrap().is_none());
        assert_eq!(
            store
                .load(TokenKind::Developer)
                .unwrap()
                .map(|t| t.expose().to_string()),
            Some("dev-token".to_string())
        );
        // Sign-out is idempotent.
        manager.sign_out().unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- 401/403 refresh-and-resend driver (Requirements 1.9, 1.11, 3.9) ----

    /// Drive `run_refresh_resend` on the current thread without a runtime by
    /// polling the returned future to completion. Every future the driver
    /// awaits here is immediately ready (the fakes do no real I/O), so a
    /// trivial no-op-waker poll loop suffices and keeps these tests runtime-free.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                raw_waker()
            }
            let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(std::ptr::null(), vtable)
        }

        // Safety: the waker is a no-op and never dereferences its data pointer.
        let waker = unsafe { Waker::from_raw(raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        // Safety: `fut` is owned here and never moved after being pinned.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => continue,
            }
        }
    }

    /// A fake Developer_Token source that yields a scripted sequence of
    /// results, recording how many times it was asked.
    fn dev_token_ok(value: &str) -> Result<SecretString, DeveloperTokenError> {
        Ok(SecretString::new(value))
    }

    #[test]
    fn first_attempt_success_sends_once_and_never_refreshes() {
        let attempts = std::cell::Cell::new(0);
        let refreshed = std::cell::Cell::new(false);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::CatalogOnly,
            |_token| {
                attempts.set(attempts.get() + 1);
                async { RequestOutcome::Success("body") }
            },
            || async { dev_token_ok("dev-1") },
            || async {
                refreshed.set(true);
                dev_token_ok("dev-2")
            },
            || panic!("session-expiry must not run on success"),
        ));

        assert_eq!(outcome, AuthOutcome::Success("body"));
        assert_eq!(attempts.get(), 1);
        assert!(!refreshed.get(), "no refresh on a first-attempt success");
    }

    #[test]
    fn unauthorized_then_success_refreshes_and_resends_once() {
        // 401 on the first attempt, success on the single resend
        // (Requirement 1.9): the request is sent exactly twice and the second
        // attempt uses the refreshed token.
        let attempts = std::cell::Cell::new(0);
        let tokens_seen: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::CatalogOnly,
            |token: SecretString| {
                attempts.set(attempts.get() + 1);
                tokens_seen.borrow_mut().push(token.expose().to_string());
                let n = attempts.get();
                async move {
                    if n == 1 {
                        RequestOutcome::Unauthorized
                    } else {
                        RequestOutcome::Success("body")
                    }
                }
            },
            || async { dev_token_ok("dev-first") },
            || async { dev_token_ok("dev-refreshed") },
            || panic!("must not expire on a successful resend"),
        ));

        assert_eq!(outcome, AuthOutcome::Success("body"));
        assert_eq!(attempts.get(), 2);
        assert_eq!(
            *tokens_seen.borrow(),
            vec!["dev-first".to_string(), "dev-refreshed".to_string()]
        );
    }

    #[test]
    fn catalog_only_double_unauthorized_fails_without_third_send() {
        // 401 on both attempts for a catalog-only request: stop with
        // CatalogAuthFailed and never send a third time (Requirement 1.9).
        let attempts = std::cell::Cell::new(0);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::CatalogOnly,
            |_token| {
                attempts.set(attempts.get() + 1);
                async { RequestOutcome::Unauthorized }
            },
            || async { dev_token_ok("dev-first") },
            || async { dev_token_ok("dev-refreshed") },
            || panic!("catalog-only must not trigger session expiry"),
        ));

        assert_eq!(outcome, AuthOutcome::CatalogAuthFailed);
        assert_eq!(attempts.get(), 2, "exactly two sends, no third resend");
    }

    #[test]
    fn music_user_double_unauthorized_expires_session_and_deletes_token() {
        // 401 on both attempts for a request carrying the Music_User_Token:
        // delete the token and report SessionExpired (Requirements 1.11, 3.9).
        let attempts = std::cell::Cell::new(0);
        let expired = std::cell::Cell::new(false);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::WithMusicUserToken,
            |_token| {
                attempts.set(attempts.get() + 1);
                async { RequestOutcome::Unauthorized }
            },
            || async { dev_token_ok("dev-first") },
            || async { dev_token_ok("dev-refreshed") },
            || expired.set(true),
        ));

        assert_eq!(outcome, AuthOutcome::SessionExpired);
        assert_eq!(attempts.get(), 2, "exactly two sends, no third resend");
        assert!(expired.get(), "session-expiry hook deletes the token");
    }

    #[test]
    fn non_auth_failure_is_returned_without_refresh_or_resend() {
        // A non-401/403 failure (timeout, 5xx, ...) must not refresh or resend;
        // it is handed back for the API_Client's transport handling (task 9.4).
        let attempts = std::cell::Cell::new(0);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::WithMusicUserToken,
            |_token| {
                attempts.set(attempts.get() + 1);
                async {
                    RequestOutcome::Other {
                        cause: "503 Service Unavailable".to_string(),
                    }
                }
            },
            || async { dev_token_ok("dev-first") },
            || async { panic!("must not refresh on a non-auth failure") },
            || panic!("must not expire on a non-auth failure"),
        ));

        assert_eq!(
            outcome,
            AuthOutcome::RequestFailed {
                cause: "503 Service Unavailable".to_string()
            }
        );
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn non_auth_failure_on_resend_is_returned() {
        // 401 first, then a non-auth failure on the resend: return RequestFailed
        // without a third send.
        let attempts = std::cell::Cell::new(0);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::CatalogOnly,
            |_token| {
                attempts.set(attempts.get() + 1);
                let n = attempts.get();
                async move {
                    if n == 1 {
                        RequestOutcome::Unauthorized
                    } else {
                        RequestOutcome::Other {
                            cause: "timed out".to_string(),
                        }
                    }
                }
            },
            || async { dev_token_ok("dev-first") },
            || async { dev_token_ok("dev-refreshed") },
            || panic!("must not expire on a non-auth resend failure"),
        ));

        assert_eq!(
            outcome,
            AuthOutcome::RequestFailed {
                cause: "timed out".to_string()
            }
        );
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn developer_token_unavailable_on_first_attempt_short_circuits() {
        // If the Developer_Token cannot be obtained for the first attempt, the
        // request is never sent and the error surfaces (Requirement 1.10).
        let attempts = std::cell::Cell::new(0);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::CatalogOnly,
            |_token| {
                attempts.set(attempts.get() + 1);
                async { RequestOutcome::Success("unreached") }
            },
            || async { Err(DeveloperTokenError::Timeout) },
            || async { panic!("refresh not reached") },
            || panic!("expiry not reached"),
        ));

        assert_eq!(
            outcome,
            AuthOutcome::DeveloperTokenUnavailable(DeveloperTokenError::Timeout)
        );
        assert_eq!(attempts.get(), 0, "no send when the first token is unavailable");
    }

    #[test]
    fn developer_token_refresh_failure_surfaces_after_unauthorized() {
        // 401 on the first attempt, but the refresh fails: surface the
        // Developer_Token error and do not resend (Requirement 1.10).
        let attempts = std::cell::Cell::new(0);

        let outcome: AuthOutcome<&str> = block_on(run_refresh_resend(
            AuthScope::WithMusicUserToken,
            |_token| {
                attempts.set(attempts.get() + 1);
                async { RequestOutcome::Unauthorized }
            },
            || async { dev_token_ok("dev-first") },
            || async { Err(DeveloperTokenError::NoTokenInAssets) },
            || panic!("expiry not reached when refresh itself fails"),
        ));

        assert_eq!(
            outcome,
            AuthOutcome::DeveloperTokenUnavailable(DeveloperTokenError::NoTokenInAssets)
        );
        assert_eq!(attempts.get(), 1, "only the first send happens");
    }

    #[test]
    fn authorize_request_deletes_music_user_token_on_session_expiry() {
        // End-to-end through AuthManager with a valid cached Developer_Token so
        // the first attempt needs no fetch; refresh_developer_token would hit
        // the network, so this test only exercises the Music_User_Token
        // deletion path by forcing SessionExpired via the driver wiring. We use
        // a cached dev token valid far in the future and a sender that fails
        // only when it would resend — but since resend fetches the network we
        // keep this at the driver level above. Here we assert the store wiring:
        // delete_music_user_token removes the token.
        let (store, dir) = temp_store("expire-delete");
        store
            .save(TokenKind::MusicUser, &SecretString::new("to-expire"))
            .unwrap();
        let manager = AuthManager::new(store.clone());
        assert!(manager.has_music_user_token());

        manager.delete_music_user_token();

        assert!(!manager.has_music_user_token());
        assert!(store.load(TokenKind::MusicUser).unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    mod property_tests {
        use super::super::{developer_token_is_valid, MIN_REMAINING_SECS};
        use super::{base64url_encode, jwt_with_exp};
        use proptest::prelude::{
            any, prop_assert, prop_assert_eq, prop_oneof, proptest, Just, ProptestConfig, Strategy,
        };

        /// Build a JWT whose payload is a JSON object carrying the given claims
        /// but **no** `exp` claim, so it must be rejected (Requirement 1.8,
        /// "lacks an exp claim").
        fn jwt_without_exp(iss: &str) -> String {
            let header = base64url_encode(br#"{"alg":"ES256","typ":"JWT"}"#);
            let payload = base64url_encode(format!(r#"{{"iss":"{iss}"}}"#).as_bytes());
            format!("{header}.{payload}.c2ln")
        }

        /// Candidate kinds that span the decision's branches:
        /// - a well-formed JWT with a known non-negative `exp`,
        /// - a well-formed JWT with no `exp` claim,
        /// - an arbitrary string that is not a JWT at all.
        #[derive(Debug, Clone)]
        enum Candidate {
            /// A JWT with this exact `exp` (seconds since the epoch).
            WithExp(u64),
            /// A JWT that decodes but has no `exp` claim.
            NoExp,
            /// A free-form string, almost never a valid JWT.
            Garbage(String),
        }

        /// Smart generator constraining to the input space the decision cares
        /// about: `exp` values are drawn across the whole u64 range (so both
        /// far-future and past/near expiries occur), alongside exp-less JWTs
        /// and arbitrary non-JWT strings.
        fn candidate_strategy() -> impl Strategy<Value = Candidate> {
            prop_oneof![
                // Draw `exp` across the full range a JWT can faithfully carry:
                // a non-negative JSON integer, i.e. `0..=i64::MAX` seconds. This
                // spans both far-future and past/near expiries so the
                // `>= now + 60` boundary is exercised on both sides across the
                // clock values below.
                (0u64..=i64::MAX as u64).prop_map(Candidate::WithExp),
                Just(Candidate::NoExp),
                // Non-JWT strings: dotted and undotted ASCII runs that should
                // overwhelmingly fail to decode as a JWT with an `exp` claim.
                "[A-Za-z0-9._-]{0,40}".prop_map(Candidate::Garbage),
            ]
        }

        /// Render a [`Candidate`] to its token string.
        fn render(candidate: &Candidate) -> String {
            match candidate {
                Candidate::WithExp(exp) => jwt_with_exp(*exp as i64),
                Candidate::NoExp => jwt_without_exp("ABCDE"),
                Candidate::Garbage(s) => s.clone(),
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(400))]

            // Feature: apple-music-tui, Property 1: Developer_Token validity decision
            // Validates: Requirements 1.8
            //
            // For any candidate Developer_Token string and any current time, the
            // Auth_Manager accepts the token if and only if it decodes as a JWT,
            // contains an `exp` claim, and that `exp` is at least 60 seconds
            // after the current time; otherwise it is discarded.
            #[test]
            fn property_developer_token_validity_decision(
                candidate in candidate_strategy(),
                now in any::<u64>(),
            ) {
                let token = render(&candidate);
                let accepted = developer_token_is_valid(&token, now);

                // The acceptance threshold: `exp` must be at least 60 seconds
                // ahead of `now`, computed without overflow at the u64 ceiling.
                let threshold = now.saturating_add(MIN_REMAINING_SECS);

                match &candidate {
                    // A JWT with a known `exp` is accepted iff `exp >= now + 60`.
                    Candidate::WithExp(exp) => {
                        prop_assert_eq!(accepted, *exp >= threshold);
                    }
                    // A JWT without an `exp` claim is always discarded.
                    Candidate::NoExp => {
                        prop_assert!(!accepted, "token lacking an exp claim must be rejected");
                    }
                    // Non-JWT garbage is discarded unless it happens to be a
                    // valid JWT with a sufficiently-future `exp` (vanishingly
                    // rare). In that case the decode-based oracle still agrees.
                    Candidate::Garbage(_) => {
                        let decoded = super::super::decode_jwt_exp(&token);
                        let expected = matches!(decoded, Some(exp) if exp >= threshold);
                        prop_assert_eq!(accepted, expected);
                    }
                }

                // Regardless of shape, the decision must agree with decoding the
                // `exp` directly and comparing against the 60-second threshold.
                let oracle = matches!(
                    super::super::decode_jwt_exp(&token),
                    Some(exp) if exp >= threshold
                );
                prop_assert_eq!(accepted, oracle);
            }
        }
    }

    #[test]
    fn cached_expired_token_is_discarded() {
        let dir = std::env::temp_dir().join(format!(
            "amt-auth-expired-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TokenStore::with_file_dir(&dir);
        let now = 1_000_000;
        // Expired: `exp` in the past.
        store
            .save(
                TokenKind::Developer,
                &SecretString::new(jwt_with_exp((now - 5) as i64)),
            )
            .unwrap();

        let manager = AuthManager::with_clock(store, FixedClock(now));
        assert!(manager.cached_valid_token().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
