//! API_Client (catalog search, library browsing, pagination, errors).
//!
//! This module implements the pure mapping layer that converts Apple Music API
//! JSON resources into [`Domain_Object`]s (Requirement 12.1–12.3) and the
//! catalog **search** path (Requirement 2.1, 2.2, 2.6). Library browsing,
//! pagination, and the full transport error-handling policy are implemented in
//! later subtasks (9.2–9.4); the pieces here are kept small and, wherever
//! possible, pure so they can be property-tested in isolation.
//!
//! # Catalog search (task 9.1)
//!
//! Search has three independently-testable concerns, each kept pure so the
//! design's correctness properties can be exercised without a network:
//!
//! - **Query validation** ([`validate_query`], Property 3 / Requirement 2.2):
//!   a request is sent iff the query trimmed of leading/trailing whitespace is
//!   1–200 characters; otherwise the query is retained and a validation message
//!   is shown and no request is sent.
//! - **Section caps** ([`SEARCH_SECTION_LIMIT`], Property 2 / Requirement 2.1):
//!   each page requests at most 25 songs, 25 albums, 25 artists, and 25
//!   playlists for the configured Storefront.
//! - **Latest-query-wins** ([`SearchCoordinator`], Property 4 / Requirement
//!   2.6): overlapping in-flight searches are tagged with a monotonically
//!   increasing generation so a response for a superseded query is discarded.
//!
//! [`CatalogApi`] wires these together over `reqwest`/`tokio` consistent with
//! [`crate::auth`]; the networked method stays a thin shell around the pure
//! request builder ([`SearchRequest`]) and response parser
//! ([`parse_search_response`]).
//!
//! The mapping layer below is kept as a pure, side-effect-free function so it
//! can be property-tested in isolation (subtasks 7.5, 7.6).
//!
//! # Mapping rules
//!
//! For every resource in a successful (2xx) response body:
//!
//! - A resource with both an `id` and a `name` is converted into one
//!   [`Domain_Object`] of the variant indicated by its `type`
//!   (Requirement 12.1).
//! - Present optional attributes (`artistName`, `durationInMillis`,
//!   `artwork.url`) are preserved. A missing text attribute becomes the empty
//!   string and a missing duration becomes `None` (Requirement 12.2).
//! - A resource missing an `id` or a `name` is dropped from the result and a
//!   [`DroppedResource`] entry is recorded so the caller can write it to the
//!   log file (Requirement 12.3).
//!
//! The Apple Music API shapes each resource as `{ "id", "type", "attributes":
//! { "name", "artistName", "durationInMillis", "artwork": { "url" } } }`. The
//! mapper reads `id`/`type` from the top level and the remaining fields from
//! `attributes`, which mirrors the documented response model.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::domain::Domain_Object;
use crate::token_store::SecretString;

/// A resource that was omitted during mapping because it lacked a required
/// field or could not be interpreted (Requirement 12.3).
///
/// Mapping is pure: rather than writing to the log file directly, the mapper
/// returns these entries so the caller (the TUI/API_Client) can append them to
/// `$XDG_STATE_HOME/apple-music-tui/app.log`. This keeps the mapping logic
/// side-effect free and property-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedResource {
    /// The 0-based index of the resource within the response's resource list.
    pub index: usize,
    /// The `type` of the resource if present (e.g. `songs`), for diagnostics.
    pub resource_type: Option<String>,
    /// The `id` of the resource if present, for diagnostics.
    pub id: Option<String>,
    /// Why the resource was dropped.
    pub reason: DropReason,
}

/// The reason a resource was omitted from the mapped result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The resource lacked a non-empty `id` (Requirement 12.3).
    MissingId,
    /// The resource lacked a non-empty `name` (Requirement 12.3).
    MissingName,
    /// The resource had an unknown or missing `type`, so no variant applied.
    UnknownType,
    /// The resource was not a JSON object and could not be interpreted.
    NotAnObject,
}

impl std::fmt::Display for DroppedResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.reason {
            DropReason::MissingId => "missing id",
            DropReason::MissingName => "missing name",
            DropReason::UnknownType => "unknown or missing type",
            DropReason::NotAnObject => "not a JSON object",
        };
        write!(
            f,
            "dropped API resource at index {} (type: {}, id: {}): {}",
            self.index,
            self.resource_type.as_deref().unwrap_or("?"),
            self.id.as_deref().unwrap_or("?"),
            reason
        )
    }
}

/// The result of mapping a set of API resources into [`Domain_Object`]s.
///
/// `objects` holds the successfully mapped resources in their original order;
/// `dropped` holds one entry per omitted resource (Requirement 12.3). The two
/// together account for every input resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapOutcome {
    /// The successfully mapped Domain_Objects, in input order.
    pub objects: Vec<Domain_Object>,
    /// Diagnostics for each resource that was omitted.
    pub dropped: Vec<DroppedResource>,
}

/// Map a list of Apple Music API resource JSON values into [`Domain_Object`]s.
///
/// `resources` is the `data` array of a successful (2xx) Apple Music API
/// response. Each element is expected to be a resource object with a top-level
/// `id` and `type` and a nested `attributes` object.
///
/// Returns a [`MapOutcome`] whose `objects` preserve every present optional
/// attribute (Requirement 12.1), substitute an empty string for a missing text
/// attribute and `None` for a missing duration (Requirement 12.2), and whose
/// `dropped` lists every resource omitted for missing `id`/`name`, an unknown
/// `type`, or not being an object (Requirement 12.3).
pub fn map_resources(resources: &[Value]) -> MapOutcome {
    let mut outcome = MapOutcome::default();
    for (index, resource) in resources.iter().enumerate() {
        match map_resource(index, resource) {
            Ok(object) => outcome.objects.push(object),
            Err(dropped) => outcome.dropped.push(dropped),
        }
    }
    outcome
}

/// Map a full successful-response body into [`Domain_Object`]s.
///
/// Accepts either an object with a `data` array (the standard Apple Music API
/// envelope, e.g. `{ "data": [ ... ] }`) or a bare JSON array of resources. If
/// the body is neither — or `data` is not an array — the whole body is reported
/// as a single unparseable resource (Requirement 12.3).
pub fn map_response(body: &Value) -> MapOutcome {
    match body {
        Value::Array(items) => map_resources(items),
        Value::Object(map) => match map.get("data") {
            Some(Value::Array(items)) => map_resources(items),
            _ => single_dropped(DropReason::NotAnObject, None, None),
        },
        _ => single_dropped(DropReason::NotAnObject, None, None),
    }
}

fn single_dropped(
    reason: DropReason,
    resource_type: Option<String>,
    id: Option<String>,
) -> MapOutcome {
    MapOutcome {
        objects: Vec::new(),
        dropped: vec![DroppedResource {
            index: 0,
            resource_type,
            id,
            reason,
        }],
    }
}

/// Map a single resource, returning either the mapped object or a drop reason.
fn map_resource(index: usize, resource: &Value) -> Result<Domain_Object, DroppedResource> {
    let obj = resource.as_object().ok_or(DroppedResource {
        index,
        resource_type: None,
        id: None,
        reason: DropReason::NotAnObject,
    })?;

    // `type` is read first so it can be reported in any drop diagnostic.
    let resource_type = non_empty_string(obj.get("type"));
    let id = non_empty_string(obj.get("id"));

    let drop = |reason: DropReason| DroppedResource {
        index,
        resource_type: resource_type.clone(),
        id: id.clone(),
        reason,
    };

    // `id` is required (Requirement 12.3).
    let id = id.clone().ok_or_else(|| drop(DropReason::MissingId))?;

    // Attributes hold name and the optional text/duration/artwork fields. A
    // missing `attributes` object is treated as all-attributes-absent, so a
    // resource with no attributes fails the required-`name` check below.
    let attributes = obj.get("attributes").and_then(Value::as_object);
    let attr = |key: &str| attributes.and_then(|a| a.get(key));

    // `name` is required (Requirement 12.3).
    let name = non_empty_string(attr("name")).ok_or_else(|| drop(DropReason::MissingName))?;

    // Optional attributes: missing text -> empty string (Requirement 12.2).
    let artist_name = text_or_empty(attr("artistName"));
    let artwork_url = non_empty_string(artwork_url(attr("artwork")));

    match resource_type.as_deref() {
        Some("songs") => Ok(Domain_Object::Song {
            id,
            name,
            artist_name,
            // Missing duration -> no value (Requirement 12.2).
            duration_ms: duration_millis(attr("durationInMillis")),
            artwork_url,
            // `isPlayable` is reported by the catalog; absent defaults to true
            // so a song without the flag is still offered for playback.
            is_playable: attr("isPlayable").and_then(Value::as_bool).unwrap_or(true),
        }),
        Some("albums") => Ok(Domain_Object::Album {
            id,
            name,
            artist_name,
            artwork_url,
        }),
        Some("artists") => Ok(Domain_Object::Artist {
            id,
            name,
            artwork_url,
        }),
        Some("playlists") => Ok(Domain_Object::Playlist {
            id,
            name,
            artwork_url,
        }),
        _ => Err(drop(DropReason::UnknownType)),
    }
}

/// Return the trimmed-nonempty string value of a JSON field, or `None`.
///
/// A JSON number `id` (the API sometimes returns numeric-looking values) is
/// accepted and rendered as its string form so identifiers are not lost.
fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Return a JSON string field's value, substituting the empty string when the
/// field is absent or not a string (Requirement 12.2, missing text attribute).
fn text_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Extract the `url` from an `artwork` object, if present.
fn artwork_url(artwork: Option<&Value>) -> Option<&Value> {
    artwork.and_then(Value::as_object).and_then(|a| a.get("url"))
}

/// Parse a duration-in-milliseconds field into `Some(ms)`, or `None` when the
/// field is absent, not a non-negative integer, or otherwise unusable
/// (Requirement 12.2, missing duration -> no value).
fn duration_millis(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

// ---------------------------------------------------------------------------
// Catalog search (task 9.1): query validation, section caps, latest-query-wins.
// ---------------------------------------------------------------------------

/// The configured host for Apple Music catalog requests.
const API_HOST: &str = "https://api.music.apple.com";

/// Maximum items requested per result section per page (Requirement 2.1,
/// Property 2). Each of songs/albums/artists/playlists is capped at this value.
pub const SEARCH_SECTION_LIMIT: usize = 25;

/// Inclusive minimum length of a trimmed search query (Requirement 2.2).
pub const QUERY_MIN_LEN: usize = 1;

/// Inclusive maximum length of a trimmed search query (Requirement 2.2).
pub const QUERY_MAX_LEN: usize = 200;

/// Maximum time to wait for a single search request (Requirement 2.5). The
/// retry/backoff policy around this timeout is implemented in task 9.4; here it
/// only bounds the request.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The four catalog resource types a search requests, in display order
/// (Requirement 2.1, 2.3). The API `types` parameter uses these plural names.
const SEARCH_TYPES: [&str; 4] = ["songs", "albums", "artists", "playlists"];

/// Why a submitted search query was not sent (Requirement 2.2, Property 3).
///
/// The query itself is retained by the caller in the input field; this value
/// only reports that validation failed and why, so the TUI can show the
/// "query must be 1 to 200 characters" message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    /// The query was empty or whitespace-only after trimming (trimmed length 0).
    Empty,
    /// The query exceeded [`QUERY_MAX_LEN`] characters after trimming.
    TooLong,
}

impl QueryError {
    /// The user-facing validation message (Requirement 2.2). Both rejection
    /// reasons share the same required phrasing; the variant is retained for
    /// precise diagnostics/logging.
    pub fn message(self) -> &'static str {
        "the query must be 1 to 200 characters"
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for QueryError {}

/// A search query that has passed validation (Requirement 2.2, Property 3).
///
/// Holds the trimmed term that is actually sent to the API. The type can only
/// be constructed through [`validate_query`], so possessing a `ValidQuery` is
/// proof the 1–200-character rule held and a request may be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidQuery(String);

impl ValidQuery {
    /// The trimmed query term that is sent to the API.
    pub fn term(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ValidQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate a raw search query (Requirement 2.2, Property 3).
///
/// Returns `Ok(ValidQuery)` **iff** the query, after trimming leading and
/// trailing whitespace, has a character count in `1..=200`; otherwise returns
/// the [`QueryError`] describing why no request should be sent. Length is
/// measured in Unicode scalar values (`char`s), not bytes, so multi-byte
/// characters each count once.
///
/// This is the pure core of Property 4's "send a request iff valid" decision;
/// it performs no I/O.
pub fn validate_query(raw: &str) -> Result<ValidQuery, QueryError> {
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    if len < QUERY_MIN_LEN {
        Err(QueryError::Empty)
    } else if len > QUERY_MAX_LEN {
        Err(QueryError::TooLong)
    } else {
        Ok(ValidQuery(trimmed.to_string()))
    }
}

/// The grouped results of a catalog search (Requirement 2.1, 2.3).
///
/// Each section holds the [`Domain_Object`]s of one resource type, in the order
/// the API returned them, capped at [`SEARCH_SECTION_LIMIT`] per page. An empty
/// `Vec` means that section returned zero results (Requirement 2.4: the TUI
/// omits a zero-result section).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// Matching songs (title, artist, duration rendered by the view).
    pub songs: Vec<Domain_Object>,
    /// Matching albums.
    pub albums: Vec<Domain_Object>,
    /// Matching artists.
    pub artists: Vec<Domain_Object>,
    /// Matching playlists.
    pub playlists: Vec<Domain_Object>,
}

impl SearchResults {
    /// `true` when every section is empty, i.e. the search matched nothing
    /// across all four resource types (Requirement 2.4).
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
            && self.albums.is_empty()
            && self.artists.is_empty()
            && self.playlists.is_empty()
    }

    /// Total number of mapped results across all sections.
    pub fn total(&self) -> usize {
        self.songs.len() + self.albums.len() + self.artists.len() + self.playlists.len()
    }
}

/// A fully-formed catalog search request (Requirement 2.1).
///
/// Built purely from a validated query and the configured Storefront so the
/// exact URL — path plus percent-encoded query string — can be asserted in
/// tests without sending anything. The networked [`CatalogApi::search`] turns
/// this into a `reqwest` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// The storefront segment of the path (ISO 3166-1 alpha-2, lowercased).
    pub storefront: String,
    /// The trimmed search term.
    pub term: String,
    /// Zero-based item offset for pagination (0 for the first page). Task 9.2
    /// advances this; task 9.1 only ever builds the first page (offset 0).
    pub offset: usize,
}

impl SearchRequest {
    /// Build the first-page request for `query` against `storefront`
    /// (Requirement 2.1). The storefront is lowercased to match the API's path
    /// convention; callers validate it as ISO 3166-1 alpha-2 at config time.
    pub fn new(query: &ValidQuery, storefront: &str) -> Self {
        SearchRequest {
            storefront: storefront.to_ascii_lowercase(),
            term: query.term().to_string(),
            offset: 0,
        }
    }

    /// The request path, e.g. `/v1/catalog/us/search`.
    pub fn path(&self) -> String {
        format!("/v1/catalog/{}/search", self.storefront)
    }

    /// The query parameters as `(key, value)` pairs, before URL encoding
    /// (Requirement 2.1). `types` names all four sections and `limit` caps each
    /// section at [`SEARCH_SECTION_LIMIT`] (Property 2).
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("term".to_string(), self.term.clone()),
            ("types".to_string(), SEARCH_TYPES.join(",")),
            ("limit".to_string(), SEARCH_SECTION_LIMIT.to_string()),
        ];
        if self.offset > 0 {
            pairs.push(("offset".to_string(), self.offset.to_string()));
        }
        pairs
    }

    /// The absolute request URL with a percent-encoded query string.
    pub fn url(&self) -> String {
        let query = self
            .query_pairs()
            .into_iter()
            .map(|(k, v)| format!("{}={}", percent_encode(&k), percent_encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{API_HOST}{}?{}", self.path(), query)
    }
}

/// Percent-encode a query-parameter component, encoding every byte that is not
/// an RFC 3986 unreserved character. Kept local to avoid pulling a URL crate in
/// for the single use here; mirrors the conservative encoding `auth.rs` relies
/// on `reqwest` for elsewhere.
fn percent_encode(input: &str) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
    }
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Parse a successful (2xx) catalog-search response body into [`SearchResults`]
/// (Requirement 2.1, reusing the Requirement 12 mapping).
///
/// The Apple Music search response nests results under
/// `results.<type>.data`, e.g. `{ "results": { "songs": { "data": [ ... ] },
/// ... } }`. Each section's `data` array is mapped with [`map_resources`], so
/// malformed resources are dropped and logged (Requirement 12.3) while the rest
/// are returned. The dropped entries from every section are concatenated into
/// the returned [`MapOutcome`]-style diagnostics list.
///
/// A missing `results` object or a missing section yields an empty section
/// rather than an error, matching the API returning no items for that type
/// (Requirement 2.4). Pure and side-effect free for property testing.
pub fn parse_search_response(body: &Value) -> (SearchResults, Vec<DroppedResource>) {
    let results = body.get("results");
    let mut dropped = Vec::new();

    let mut section = |name: &str| -> Vec<Domain_Object> {
        let data = results
            .and_then(|r| r.get(name))
            .and_then(|s| s.get("data"))
            .and_then(Value::as_array);
        match data {
            Some(items) => {
                let outcome = map_resources(items);
                dropped.extend(outcome.dropped);
                // Enforce the 25-per-section cap defensively (Property 2): the
                // request asks for 25, but a non-conforming response is still
                // truncated so the invariant holds regardless of the server.
                let mut objects = outcome.objects;
                objects.truncate(SEARCH_SECTION_LIMIT);
                objects
            }
            None => Vec::new(),
        }
    };

    let results = SearchResults {
        songs: section("songs"),
        albums: section("albums"),
        artists: section("artists"),
        playlists: section("playlists"),
    };
    (results, dropped)
}

/// A generation stamp that orders overlapping search submissions
/// (Requirement 2.6, Property 4).
///
/// Each submission draws a strictly larger value from a [`SearchCoordinator`];
/// a response is applied only if its generation is the newest one issued, so a
/// slow response for a superseded query is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

/// Coordinates overlapping in-flight searches so the latest query wins
/// (Requirement 2.6, Property 4).
///
/// The TUI calls [`begin`](SearchCoordinator::begin) when it submits a query to
/// obtain a [`Generation`], runs the request, then calls
/// [`accept`](SearchCoordinator::accept) with that generation when the response
/// arrives. `accept` returns `true` only when the generation is still the most
/// recent one issued; an out-of-order response for an earlier query returns
/// `false` and the caller discards it, so the displayed results always
/// correspond to the most recently submitted query regardless of the order
/// responses resolve in.
///
/// The counter is an [`AtomicU64`] so a `&SearchCoordinator` shared across async
/// tasks can issue and compare generations without a lock.
#[derive(Debug, Default)]
pub struct SearchCoordinator {
    latest: AtomicU64,
}

impl SearchCoordinator {
    /// Create a coordinator with no searches issued yet.
    pub fn new() -> Self {
        SearchCoordinator {
            latest: AtomicU64::new(0),
        }
    }

    /// Register a new search submission and return its generation
    /// (Requirement 2.6). Each call returns a strictly larger value than the
    /// previous call, marking every earlier in-flight search as superseded.
    pub fn begin(&self) -> Generation {
        // `fetch_add` returns the previous value; the new latest is that + 1.
        let previous = self.latest.fetch_add(1, Ordering::SeqCst);
        Generation(previous + 1)
    }

    /// Decide whether a response tagged `generation` should be applied
    /// (Requirement 2.6, Property 4).
    ///
    /// Returns `true` iff `generation` is the most recent generation issued by
    /// [`begin`](SearchCoordinator::begin); a response for any superseded query
    /// returns `false` and must be discarded.
    pub fn accept(&self, generation: Generation) -> bool {
        self.latest.load(Ordering::SeqCst) == generation.0
    }

    /// The most recent generation issued, or `None` when no search has begun.
    pub fn current(&self) -> Option<Generation> {
        match self.latest.load(Ordering::SeqCst) {
            0 => None,
            n => Some(Generation(n)),
        }
    }
}

/// An error from a networked catalog request (Requirement 2.5).
///
/// Task 9.1 distinguishes a rejected query (no request sent) from a transport
/// failure; the richer timeout/429/5xx policy is layered on in task 9.4. The
/// message never contains a token value (Requirement 1.13, 12.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// The query failed validation, so no request was sent (Requirement 2.2).
    /// The caller keeps the query in the input field and shows the message.
    InvalidQuery(QueryError),
    /// The request failed or exceeded the 10-second timeout (Requirement 2.5).
    /// `cause` is a token-free description for logging.
    Transport { cause: String },
    /// The API returned a non-success HTTP status. Carries the status code for
    /// the error message; the detailed 401/403/429/5xx policy lands in 9.4.
    Status { code: u16 },
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::InvalidQuery(err) => write!(f, "{err}"),
            SearchError::Transport { cause } => {
                write!(f, "the search failed: {cause}")
            }
            SearchError::Status { code } => {
                write!(f, "the search failed: the Apple Music API returned status {code}")
            }
        }
    }
}

impl std::error::Error for SearchError {}

/// The networked catalog API_Client for the configured Storefront.
///
/// Holds a `reqwest` client (built with the 10-second search timeout) and the
/// storefront code. [`search`](CatalogApi::search) validates the query, builds
/// a [`SearchRequest`], sends it with the Developer_Token (and Music_User_Token
/// when present) attached, and parses the response. Latest-query-wins is the
/// caller's concern via a shared [`SearchCoordinator`]; this type performs one
/// request and leaves generation bookkeeping to the TUI so a single client can
/// serve many overlapping submissions.
#[derive(Debug, Clone)]
pub struct CatalogApi {
    client: reqwest::Client,
    storefront: String,
}

impl CatalogApi {
    /// Build a catalog client for `storefront` (ISO 3166-1 alpha-2). Returns a
    /// [`SearchError::Transport`] if the HTTP client cannot be constructed.
    pub fn new(storefront: impl Into<String>) -> Result<Self, SearchError> {
        let client = reqwest::Client::builder()
            .timeout(SEARCH_TIMEOUT)
            .build()
            .map_err(|err| SearchError::Transport {
                cause: err.to_string(),
            })?;
        Ok(CatalogApi {
            client,
            storefront: storefront.into(),
        })
    }

    /// Build a catalog client over an existing `reqwest` client, so the TUI can
    /// share one connection pool across the API_Client's request paths.
    pub fn with_client(client: reqwest::Client, storefront: impl Into<String>) -> Self {
        CatalogApi {
            client,
            storefront: storefront.into(),
        }
    }

    /// The pure request builder for a validated query, exposed so callers and
    /// tests can inspect the exact request without sending it (Requirement 2.1).
    pub fn request_for(&self, query: &ValidQuery) -> SearchRequest {
        SearchRequest::new(query, &self.storefront)
    }

    /// Run a catalog search for `raw_query` (Requirement 2.1, 2.2).
    ///
    /// Validates the query first: an invalid query returns
    /// [`SearchError::InvalidQuery`] and sends no request (Requirement 2.2,
    /// Property 3). A valid query is sent with the Developer_Token in the
    /// `Authorization: Bearer` header and, when present and non-empty, the
    /// Music_User_Token in the `Music-User-Token` header. The 2xx body is parsed
    /// into [`SearchResults`] with each section capped at 25 (Property 2); the
    /// dropped-resource diagnostics are returned alongside for the caller to log
    /// (Requirement 12.3).
    ///
    /// Latest-query-wins is handled by the caller: it draws a [`Generation`]
    /// from a [`SearchCoordinator`] before calling this, and discards the result
    /// if the generation is no longer current when this resolves (Requirement
    /// 2.6, Property 4).
    pub async fn search(
        &self,
        raw_query: &str,
        developer_token: &SecretString,
        music_user_token: Option<&SecretString>,
    ) -> Result<(SearchResults, Vec<DroppedResource>), SearchError> {
        let query = validate_query(raw_query).map_err(SearchError::InvalidQuery)?;
        let request = self.request_for(&query);
        self.send_search(&request, developer_token, music_user_token)
            .await
    }

    /// Send an already-built [`SearchRequest`] and parse the response. Separated
    /// from [`search`](CatalogApi::search) so task 9.2's next-page requests
    /// (which build a `SearchRequest` with a non-zero offset) can reuse the exact
    /// same transport and parsing path.
    pub async fn send_search(
        &self,
        request: &SearchRequest,
        developer_token: &SecretString,
        music_user_token: Option<&SecretString>,
    ) -> Result<(SearchResults, Vec<DroppedResource>), SearchError> {
        let body = self
            .fetch_search_body(request, developer_token, music_user_token)
            .await?;
        Ok(parse_search_response(&body))
    }

    /// Fetch the next page for one section and return the section's new items,
    /// the dropped-resource diagnostics, and the refreshed more-available marker
    /// (Requirement 2.10). Reuses the same transport as
    /// [`send_search`](CatalogApi::send_search); the caller feeds the returned
    /// items and marker into [`SearchPagination::apply_page`] so they are
    /// appended to the end of the section without moving the selection.
    pub async fn fetch_next_page(
        &self,
        request: &SearchRequest,
        section: SearchSection,
        developer_token: &SecretString,
        music_user_token: Option<&SecretString>,
    ) -> Result<(Vec<Domain_Object>, Vec<DroppedResource>, SectionMore), SearchError> {
        let body = self
            .fetch_search_body(request, developer_token, music_user_token)
            .await?;
        let (results, dropped) = parse_search_response(&body);
        let more = parse_section_more(&body);
        let items = match section {
            SearchSection::Songs => results.songs,
            SearchSection::Albums => results.albums,
            SearchSection::Artists => results.artists,
            SearchSection::Playlists => results.playlists,
        };
        Ok((items, dropped, more))
    }

    /// Shared transport: attach the Developer_Token (and Music_User_Token when
    /// present and non-empty), send the request, enforce a 2xx status, and
    /// return the parsed JSON body. Keeps the single place where tokens are
    /// attached and HTTP status is checked (Requirement 1.13, 2.5).
    async fn fetch_search_body(
        &self,
        request: &SearchRequest,
        developer_token: &SecretString,
        music_user_token: Option<&SecretString>,
    ) -> Result<Value, SearchError> {
        let mut builder = self
            .client
            .get(request.url())
            .bearer_auth(developer_token.expose());
        if let Some(token) = music_user_token {
            if !token.is_empty() {
                builder = builder.header("Music-User-Token", token.expose());
            }
        }

        let response = builder.send().await.map_err(map_search_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SearchError::Status {
                code: status.as_u16(),
            });
        }
        response.json().await.map_err(map_search_error)
    }
}

/// Map a `reqwest` transport error to a token-free [`SearchError`]
/// (Requirement 2.5, 1.13).
fn map_search_error(err: reqwest::Error) -> SearchError {
    SearchError::Transport {
        cause: err.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Search pagination (task 9.2): per-section offset tracking, more-available
// detection, append-without-moving-selection, and the load-more error state
// that retries on the next end-of-list navigation (Requirement 2.10, 2.11).
// ---------------------------------------------------------------------------

/// The four catalog result sections, used to index per-section pagination state
/// (Requirement 2.10). The order matches [`SEARCH_TYPES`] and the display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchSection {
    /// The songs section.
    Songs,
    /// The albums section.
    Albums,
    /// The artists section.
    Artists,
    /// The playlists section.
    Playlists,
}

impl SearchSection {
    /// Every section in display order.
    pub const ALL: [SearchSection; 4] = [
        SearchSection::Songs,
        SearchSection::Albums,
        SearchSection::Artists,
        SearchSection::Playlists,
    ];

    /// The Apple Music `results.<type>` key for this section.
    pub fn api_key(self) -> &'static str {
        match self {
            SearchSection::Songs => "songs",
            SearchSection::Albums => "albums",
            SearchSection::Artists => "artists",
            SearchSection::Playlists => "playlists",
        }
    }
}

/// Which sections of a parsed search response advertise further pages
/// (Requirement 2.10).
///
/// The Apple Music search response includes a `results.<type>.next` URL for a
/// section when more results are available in the Catalog. This records, per
/// section, whether that `next` marker was present so the pagination state
/// machine knows a section has more to load without re-inspecting the raw JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SectionMore {
    /// `true` when the songs section reports a further page.
    pub songs: bool,
    /// `true` when the albums section reports a further page.
    pub albums: bool,
    /// `true` when the artists section reports a further page.
    pub artists: bool,
    /// `true` when the playlists section reports a further page.
    pub playlists: bool,
}

impl SectionMore {
    /// Whether the given section reports a further page.
    pub fn get(&self, section: SearchSection) -> bool {
        match section {
            SearchSection::Songs => self.songs,
            SearchSection::Albums => self.albums,
            SearchSection::Artists => self.artists,
            SearchSection::Playlists => self.playlists,
        }
    }
}

/// Inspect a parsed (2xx) search-response body for the per-section `next`
/// markers that indicate more Catalog results are available (Requirement 2.10).
///
/// A section reports "more available" when its `results.<type>.next` field is a
/// non-empty string, mirroring how the Apple Music API signals a further page.
/// Pure and side-effect free so the detection can be property-tested without a
/// network.
pub fn parse_section_more(body: &Value) -> SectionMore {
    let results = body.get("results");
    let has_next = |section: SearchSection| -> bool {
        results
            .and_then(|r| r.get(section.api_key()))
            .and_then(|s| s.get("next"))
            .and_then(Value::as_str)
            .map(|url| !url.is_empty())
            .unwrap_or(false)
    };
    SectionMore {
        songs: has_next(SearchSection::Songs),
        albums: has_next(SearchSection::Albums),
        artists: has_next(SearchSection::Artists),
        playlists: has_next(SearchSection::Playlists),
    }
}

/// The per-section bookkeeping the pagination state machine keeps for one result
/// section (Requirement 2.10, 2.11).
///
/// Tracks how many items have been loaded (`loaded`, which doubles as the offset
/// of the next page), whether the Catalog reports more pages (`more_available`),
/// whether a next-page request is currently in flight (`loading`, so a second
/// end-of-list navigation does not launch a duplicate request), and whether the
/// last next-page attempt failed (`load_error`, which both drives the load-more
/// error message and arms the retry on the next end-of-list navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SectionState {
    /// Number of items loaded so far; also the offset for the next page request.
    pub loaded: usize,
    /// `true` while the Catalog still advertises a further page for this section.
    pub more_available: bool,
    /// `true` while a next-page request for this section is in flight.
    pub loading: bool,
    /// `true` when the most recent next-page request failed and has not yet been
    /// retried (Requirement 2.11).
    pub load_error: bool,
}

/// The decision produced when the selection reaches a section's last item
/// (Requirement 2.10, 2.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndOfSection {
    /// A next page should be requested with the carried [`SearchRequest`]
    /// (offset set to the section's current item count). The caller sends it and
    /// reports the outcome back via
    /// [`SearchPagination::apply_page`] / [`SearchPagination::record_failure`].
    RequestNextPage(SearchRequest),
    /// Nothing to do: the section has no further pages, or a request for it is
    /// already in flight.
    Idle,
}

/// Drives search-result pagination across the four sections (Requirement 2.10,
/// 2.11).
///
/// The state machine is pure: it owns the displayed [`SearchResults`] and the
/// per-section [`SectionState`], and the TUI calls into it. When the selection
/// lands on a section's last item it calls
/// [`reach_end_of_section`](SearchPagination::reach_end_of_section); if that
/// returns [`EndOfSection::RequestNextPage`] the TUI performs the networked
/// request (reusing [`CatalogApi::send_search`]) and reports the result with
/// [`apply_page`](SearchPagination::apply_page) on success or
/// [`record_failure`](SearchPagination::record_failure) on failure.
///
/// Successful pages are appended to the end of the section and never change the
/// caller's current selection (Requirement 2.10 is a view concern, but the
/// state machine guarantees it only ever grows a section's tail). A failure
/// keeps the already-displayed results, records a load-more error for the
/// section, and re-arms the request so the next end-of-list navigation retries
/// (Requirement 2.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPagination {
    /// The storefront for building next-page requests (lowercased path segment).
    storefront: String,
    /// The validated search term shared by every page of this result set.
    term: String,
    /// The accumulated, displayed results across all loaded pages.
    results: SearchResults,
    songs: SectionState,
    albums: SectionState,
    artists: SectionState,
    playlists: SectionState,
}

impl SearchPagination {
    /// Build pagination state from the first page of a search (Requirement
    /// 2.10).
    ///
    /// `request` is the first-page request that produced `results` (its
    /// storefront and term seed later next-page requests); `more` reports which
    /// sections the Catalog says have further pages. Each section's loaded count
    /// is initialised from the first page's item count.
    pub fn new(request: &SearchRequest, results: SearchResults, more: SectionMore) -> Self {
        let section = |len: usize, more_available: bool| SectionState {
            loaded: len,
            more_available,
            loading: false,
            load_error: false,
        };
        SearchPagination {
            storefront: request.storefront.clone(),
            term: request.term.clone(),
            songs: section(results.songs.len(), more.songs),
            albums: section(results.albums.len(), more.albums),
            artists: section(results.artists.len(), more.artists),
            playlists: section(results.playlists.len(), more.playlists),
            results,
        }
    }

    /// The accumulated results displayed so far.
    pub fn results(&self) -> &SearchResults {
        &self.results
    }

    /// The per-section state (offset/more/loading/error) for `section`.
    pub fn section_state(&self, section: SearchSection) -> SectionState {
        *self.state(section)
    }

    /// `true` when `section` has an outstanding load-more error (Requirement
    /// 2.11), so the TUI can render "more results could not be loaded".
    pub fn has_load_error(&self, section: SearchSection) -> bool {
        self.state(section).load_error
    }

    /// React to the selection reaching the last loaded item of `section`
    /// (Requirement 2.10, 2.11).
    ///
    /// Returns [`EndOfSection::RequestNextPage`] with a ready-to-send
    /// [`SearchRequest`] (offset = the section's current item count) when a
    /// further page is available and no request is already in flight — this is
    /// the same decision whether or not a previous attempt failed, so a prior
    /// load error is retried here (Requirement 2.11). Otherwise returns
    /// [`EndOfSection::Idle`]. Calling this marks the section as loading and
    /// clears any stale load error so the message is replaced by the retry.
    pub fn reach_end_of_section(&mut self, section: SearchSection) -> EndOfSection {
        let state = self.state(section);
        if !state.more_available || state.loading {
            return EndOfSection::Idle;
        }
        let offset = state.loaded;
        let state = self.state_mut(section);
        state.loading = true;
        state.load_error = false;
        EndOfSection::RequestNextPage(SearchRequest {
            storefront: self.storefront.clone(),
            term: self.term.clone(),
            offset,
        })
    }

    /// Append a successfully fetched next page for `section` (Requirement 2.10).
    ///
    /// The page's items are appended to the end of the section — the current
    /// selection is the caller's and is never touched — the loaded count grows
    /// by the number of appended items, the more-available flag is refreshed
    /// from the new page's `next` marker, and the in-flight/error flags are
    /// cleared. `page_more` is the [`SectionMore`] parsed from the page body;
    /// only this section's marker is consulted.
    pub fn apply_page(
        &mut self,
        section: SearchSection,
        page: Vec<Domain_Object>,
        page_more: SectionMore,
    ) {
        let added = page.len();
        self.section_items_mut(section).extend(page);
        let more = page_more.get(section);
        let state = self.state_mut(section);
        state.loaded += added;
        state.more_available = more;
        state.loading = false;
        state.load_error = false;
    }

    /// Record that a next-page request for `section` failed (Requirement 2.11).
    ///
    /// The already-displayed results are kept untouched, the load-more error is
    /// set so the TUI can show "more results could not be loaded", and the
    /// in-flight flag is cleared while `more_available` stays set so the next
    /// end-of-list navigation re-issues the request.
    pub fn record_failure(&mut self, section: SearchSection) {
        let state = self.state_mut(section);
        state.loading = false;
        state.load_error = true;
    }

    fn state(&self, section: SearchSection) -> &SectionState {
        match section {
            SearchSection::Songs => &self.songs,
            SearchSection::Albums => &self.albums,
            SearchSection::Artists => &self.artists,
            SearchSection::Playlists => &self.playlists,
        }
    }

    fn state_mut(&mut self, section: SearchSection) -> &mut SectionState {
        match section {
            SearchSection::Songs => &mut self.songs,
            SearchSection::Albums => &mut self.albums,
            SearchSection::Artists => &mut self.artists,
            SearchSection::Playlists => &mut self.playlists,
        }
    }

    fn section_items_mut(&mut self, section: SearchSection) -> &mut Vec<Domain_Object> {
        match section {
            SearchSection::Songs => &mut self.results.songs,
            SearchSection::Albums => &mut self.results.albums,
            SearchSection::Artists => &mut self.results.artists,
            SearchSection::Playlists => &mut self.results.playlists,
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_query_accepts_trimmed_length_in_range() {
        let q = validate_query("  hello  ").expect("valid");
        // Leading/trailing whitespace is trimmed before the request term.
        assert_eq!(q.term(), "hello");
    }

    #[test]
    fn validate_query_accepts_single_char() {
        assert_eq!(validate_query("a").unwrap().term(), "a");
    }

    #[test]
    fn validate_query_accepts_exactly_200_chars() {
        let q: String = "x".repeat(QUERY_MAX_LEN);
        assert_eq!(validate_query(&q).unwrap().term().chars().count(), 200);
    }

    #[test]
    fn validate_query_rejects_empty_and_whitespace_only() {
        assert_eq!(validate_query(""), Err(QueryError::Empty));
        assert_eq!(validate_query("   \t\n "), Err(QueryError::Empty));
    }

    #[test]
    fn validate_query_rejects_over_200_chars() {
        let q: String = "x".repeat(QUERY_MAX_LEN + 1);
        assert_eq!(validate_query(&q), Err(QueryError::TooLong));
    }

    #[test]
    fn validate_query_counts_unicode_scalar_values_not_bytes() {
        // 200 multi-byte chars are valid (char count, not byte count).
        let q: String = "é".repeat(QUERY_MAX_LEN);
        assert!(validate_query(&q).is_ok());
        // 201 multi-byte chars are rejected.
        let too_long: String = "é".repeat(QUERY_MAX_LEN + 1);
        assert_eq!(validate_query(&too_long), Err(QueryError::TooLong));
    }

    #[test]
    fn query_error_message_states_one_to_two_hundred() {
        assert_eq!(
            QueryError::Empty.message(),
            "the query must be 1 to 200 characters"
        );
        assert_eq!(QueryError::TooLong.message(), QueryError::Empty.message());
    }

    #[test]
    fn search_request_builds_expected_path_and_params() {
        let query = validate_query("daft punk").unwrap();
        let request = SearchRequest::new(&query, "US");
        // Storefront lowercased in the path.
        assert_eq!(request.path(), "/v1/catalog/us/search");
        let pairs = request.query_pairs();
        assert!(pairs.contains(&("term".to_string(), "daft punk".to_string())));
        assert!(pairs.contains(&("types".to_string(), "songs,albums,artists,playlists".to_string())));
        // Section cap of 25 is expressed as the `limit` param (Property 2).
        assert!(pairs.contains(&("limit".to_string(), "25".to_string())));
        // No offset on the first page.
        assert!(!pairs.iter().any(|(k, _)| k == "offset"));
    }

    #[test]
    fn search_request_url_percent_encodes_term() {
        let query = validate_query("a&b c").unwrap();
        let request = SearchRequest::new(&query, "gb");
        let url = request.url();
        assert!(url.starts_with("https://api.music.apple.com/v1/catalog/gb/search?"));
        // Space -> %20, ampersand -> %26 inside the term value.
        assert!(url.contains("term=a%26b%20c"), "url was {url}");
    }

    #[test]
    fn parse_search_response_groups_all_four_sections() {
        let body = json!({
            "results": {
                "songs": { "data": [
                    { "id": "s1", "type": "songs", "attributes": { "name": "Song" } }
                ]},
                "albums": { "data": [
                    { "id": "al1", "type": "albums", "attributes": { "name": "Album", "artistName": "A" } }
                ]},
                "artists": { "data": [
                    { "id": "ar1", "type": "artists", "attributes": { "name": "Artist" } }
                ]},
                "playlists": { "data": [
                    { "id": "p1", "type": "playlists", "attributes": { "name": "Playlist" } }
                ]}
            }
        });

        let (results, dropped) = parse_search_response(&body);
        assert!(dropped.is_empty());
        assert_eq!(results.songs.len(), 1);
        assert_eq!(results.albums.len(), 1);
        assert_eq!(results.artists.len(), 1);
        assert_eq!(results.playlists.len(), 1);
        assert_eq!(results.total(), 4);
        assert!(!results.is_empty());
    }

    #[test]
    fn parse_search_response_missing_section_is_empty_not_error() {
        let body = json!({ "results": { "songs": { "data": [] } } });
        let (results, dropped) = parse_search_response(&body);
        assert!(dropped.is_empty());
        assert!(results.is_empty());
    }

    #[test]
    fn parse_search_response_drops_malformed_resources_and_keeps_rest() {
        let body = json!({
            "results": {
                "songs": { "data": [
                    { "id": "s1", "type": "songs", "attributes": { "name": "Good" } },
                    { "type": "songs", "attributes": { "name": "No Id" } }
                ]}
            }
        });
        let (results, dropped) = parse_search_response(&body);
        assert_eq!(results.songs.len(), 1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, DropReason::MissingId);
    }

    #[test]
    fn parse_search_response_caps_section_at_25() {
        // A non-conforming response with 30 songs is truncated to 25 (Property 2).
        let songs: Vec<Value> = (0..30)
            .map(|i| json!({ "id": format!("s{i}"), "type": "songs", "attributes": { "name": format!("S{i}") } }))
            .collect();
        let body = json!({ "results": { "songs": { "data": songs } } });
        let (results, _) = parse_search_response(&body);
        assert_eq!(results.songs.len(), SEARCH_SECTION_LIMIT);
    }

    #[test]
    fn coordinator_begin_is_strictly_increasing() {
        let coord = SearchCoordinator::new();
        let g1 = coord.begin();
        let g2 = coord.begin();
        let g3 = coord.begin();
        assert!(g1 < g2 && g2 < g3);
    }

    #[test]
    fn coordinator_accepts_only_the_latest_generation() {
        let coord = SearchCoordinator::new();
        let first = coord.begin();
        let second = coord.begin();
        // A late response for the first (superseded) query is rejected.
        assert!(!coord.accept(first));
        // The most recent query's response is accepted.
        assert!(coord.accept(second));
    }

    #[test]
    fn coordinator_out_of_order_resolution_latest_wins() {
        // Simulate three overlapping submissions resolving out of order; only
        // the newest generation's response is applied (Requirement 2.6).
        let coord = SearchCoordinator::new();
        let g1 = coord.begin();
        let g2 = coord.begin();
        let g3 = coord.begin();
        // Responses resolve in order g2, g1, g3.
        assert!(!coord.accept(g2));
        assert!(!coord.accept(g1));
        assert!(coord.accept(g3));
    }

    #[test]
    fn coordinator_current_is_none_before_any_search() {
        let coord = SearchCoordinator::new();
        assert_eq!(coord.current(), None);
        let g = coord.begin();
        assert_eq!(coord.current(), Some(g));
    }

    #[test]
    fn search_error_display_excludes_tokens() {
        let transport = SearchError::Transport {
            cause: "connection reset".to_string(),
        };
        assert!(format!("{transport}").contains("the search failed"));
        let status = SearchError::Status { code: 500 };
        assert!(format!("{status}").contains("500"));
        let invalid = SearchError::InvalidQuery(QueryError::TooLong);
        assert!(format!("{invalid}").contains("1 to 200"));
    }

    #[test]
    fn search_rejects_invalid_query_without_building_request() {
        // An invalid query must map to InvalidQuery; this is a pure check that
        // does not require the network because validation happens first.
        let err = validate_query("").map_err(SearchError::InvalidQuery).unwrap_err();
        assert_eq!(err, SearchError::InvalidQuery(QueryError::Empty));
    }
}

#[cfg(test)]
mod search_property_tests {
    //! Property-based tests for the pure search logic (task 9.1). Each test
    //! exercises one design correctness property at ≥100 iterations.
    use super::*;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    proptest! {
        // Feature: apple-music-tui, Property 3: Query length validation
        // Validates: Requirements 2.2
        //
        // A request is sent (validation succeeds) iff the trimmed query's
        // character count is in 1..=200; otherwise it is rejected and retained.
        #[test]
        fn property_query_length_validation(raw in ".*") {
            let trimmed_len = raw.trim().chars().count();
            let in_range = (QUERY_MIN_LEN..=QUERY_MAX_LEN).contains(&trimmed_len);
            match validate_query(&raw) {
                Ok(q) => {
                    prop_assert!(in_range);
                    // The sent term is exactly the trimmed input.
                    prop_assert_eq!(q.term(), raw.trim());
                }
                Err(_) => prop_assert!(!in_range),
            }
        }
    }

    proptest! {
        // Feature: apple-music-tui, Property 2: Search sections are capped at 25 per page
        // Validates: Requirements 2.1
        //
        // For any catalog contents, each of the four parsed sections holds at
        // most SEARCH_SECTION_LIMIT (25) items.
        #[test]
        fn property_sections_capped_at_25(
            songs in 0usize..60,
            albums in 0usize..60,
            artists in 0usize..60,
            playlists in 0usize..60,
        ) {
            fn make(kind: &str, n: usize) -> Value {
                let data: Vec<Value> = (0..n)
                    .map(|i| json!({
                        "id": format!("{kind}-{i}"),
                        "type": kind,
                        "attributes": { "name": format!("{kind} {i}") }
                    }))
                    .collect();
                json!({ "data": data })
            }
            let body = json!({
                "results": {
                    "songs": make("songs", songs),
                    "albums": make("albums", albums),
                    "artists": make("artists", artists),
                    "playlists": make("playlists", playlists),
                }
            });
            let (results, _dropped) = parse_search_response(&body);
            prop_assert!(results.songs.len() <= SEARCH_SECTION_LIMIT);
            prop_assert!(results.albums.len() <= SEARCH_SECTION_LIMIT);
            prop_assert!(results.artists.len() <= SEARCH_SECTION_LIMIT);
            prop_assert!(results.playlists.len() <= SEARCH_SECTION_LIMIT);
            // Each section holds min(requested, 25) well-formed items.
            prop_assert_eq!(results.songs.len(), songs.min(SEARCH_SECTION_LIMIT));
            prop_assert_eq!(results.albums.len(), albums.min(SEARCH_SECTION_LIMIT));
            prop_assert_eq!(results.artists.len(), artists.min(SEARCH_SECTION_LIMIT));
            prop_assert_eq!(results.playlists.len(), playlists.min(SEARCH_SECTION_LIMIT));
        }
    }

    proptest! {
        // Feature: apple-music-tui, Property 4: Latest query wins
        // Validates: Requirements 2.6
        //
        // For any interleaving of submissions and out-of-order resolutions, a
        // response is accepted iff its generation is the latest one issued, so
        // the displayed results correspond to the most recently submitted query.
        #[test]
        fn property_latest_query_wins(
            submissions in 1usize..20,
            // A permutation seed deciding the resolution order of the issued
            // generations; any order must yield "only the last wins".
            resolve_order in prop::collection::vec(0usize..20, 1..20),
        ) {
            let coord = SearchCoordinator::new();
            let generations: Vec<Generation> =
                (0..submissions).map(|_| coord.begin()).collect();
            let latest = *generations.last().unwrap();

            // Resolve generations in an arbitrary order; only the latest is
            // ever accepted, every superseded one is rejected.
            for &idx in &resolve_order {
                if let Some(&g) = generations.get(idx % generations.len()) {
                    let accepted = coord.accept(g);
                    prop_assert_eq!(accepted, g == latest);
                }
            }
            // The coordinator's current generation is the latest submitted.
            prop_assert_eq!(coord.current(), Some(latest));
        }
    }

    /// One of the four mappable resource kinds, used to drive the generator
    /// below so each generated resource carries the right `type` and the
    /// optional attributes that kind supports.
    #[derive(Debug, Clone, Copy)]
    enum Kind {
        Song,
        Album,
        Artist,
        Playlist,
    }

    impl Kind {
        fn type_name(self) -> &'static str {
            match self {
                Kind::Song => "songs",
                Kind::Album => "albums",
                Kind::Artist => "artists",
                Kind::Playlist => "playlists",
            }
        }
    }

    /// A generated resource together with the attribute values that were
    /// *present* when it was built, so the test can assert the mapped
    /// Domain_Object preserved exactly those and substituted the documented
    /// defaults for the absent ones (Requirement 12.1, 12.2).
    #[derive(Debug, Clone)]
    struct GenResource {
        kind: Kind,
        id: String,
        name: String,
        /// `Some` when `artistName` was present in the resource; `None` when it
        /// was omitted. Only songs/albums carry an artist name.
        artist_name: Option<String>,
        /// `Some` when `durationInMillis` was present (songs only).
        duration_ms: Option<u64>,
        /// `Some` when an `artwork.url` was present.
        artwork_url: Option<String>,
        /// `Some(flag)` when `isPlayable` was present (songs only).
        is_playable: Option<bool>,
    }

    impl GenResource {
        /// Build the JSON resource the mapper consumes, including only the
        /// attributes that were marked present.
        fn to_json(&self) -> Value {
            let mut attributes = serde_json::Map::new();
            attributes.insert("name".to_string(), json!(self.name));
            if let Some(ref artist) = self.artist_name {
                attributes.insert("artistName".to_string(), json!(artist));
            }
            if let Some(duration) = self.duration_ms {
                attributes.insert("durationInMillis".to_string(), json!(duration));
            }
            if let Some(ref url) = self.artwork_url {
                attributes.insert("artwork".to_string(), json!({ "url": url }));
            }
            if let Some(flag) = self.is_playable {
                attributes.insert("isPlayable".to_string(), json!(flag));
            }
            json!({
                "id": self.id,
                "type": self.kind.type_name(),
                "attributes": Value::Object(attributes),
            })
        }

        /// The Domain_Object the mapper must produce: present attributes are
        /// preserved verbatim, absent text attributes become the empty string,
        /// an absent duration becomes `None`, and an absent `isPlayable`
        /// defaults to `true` (Requirement 12.1, 12.2).
        fn expected(&self) -> Domain_Object {
            match self.kind {
                Kind::Song => Domain_Object::Song {
                    id: self.id.clone(),
                    name: self.name.clone(),
                    artist_name: self.artist_name.clone().unwrap_or_default(),
                    duration_ms: self.duration_ms,
                    artwork_url: self.artwork_url.clone(),
                    is_playable: self.is_playable.unwrap_or(true),
                },
                Kind::Album => Domain_Object::Album {
                    id: self.id.clone(),
                    name: self.name.clone(),
                    artist_name: self.artist_name.clone().unwrap_or_default(),
                    artwork_url: self.artwork_url.clone(),
                },
                Kind::Artist => Domain_Object::Artist {
                    id: self.id.clone(),
                    name: self.name.clone(),
                    artwork_url: self.artwork_url.clone(),
                },
                Kind::Playlist => Domain_Object::Playlist {
                    id: self.id.clone(),
                    name: self.name.clone(),
                    artwork_url: self.artwork_url.clone(),
                },
            }
        }
    }

    /// A strategy for non-empty strings (so `id`/`name` are always *present*),
    /// including non-ASCII so the mapper is exercised on multi-byte text.
    fn non_empty_text() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9 é★日本語_-]{1,24}".prop_filter("must be non-empty", |s| !s.is_empty())
    }

    /// A strategy producing a resource that always has a non-empty id and name
    /// (so it is never dropped) and randomly includes or omits each optional
    /// attribute supported by its kind. Artist/duration/isPlayable only apply
    /// to the kinds that carry them, so those are forced to `None` elsewhere to
    /// match what the mapper can ever populate.
    fn resource_strategy() -> impl Strategy<Value = GenResource> {
        let kind = prop_oneof![
            Just(Kind::Song),
            Just(Kind::Album),
            Just(Kind::Artist),
            Just(Kind::Playlist),
        ];
        (
            kind,
            non_empty_text(),
            non_empty_text(),
            proptest::option::of(".{0,24}"),
            proptest::option::of(0u64..1_000_000),
            // A non-empty url, since the mapper treats an empty url as absent.
            proptest::option::of(non_empty_text()),
            proptest::option::of(any::<bool>()),
        )
            .prop_map(
                |(kind, id, name, artist, duration, artwork, playable)| {
                    // Restrict each optional attribute to the kinds that carry
                    // it so `expected()` matches the mapper's variant shape.
                    let (artist_name, duration_ms, is_playable) = match kind {
                        Kind::Song => (artist, duration, playable),
                        Kind::Album => (artist, None, None),
                        Kind::Artist | Kind::Playlist => (None, None, None),
                    };
                    GenResource {
                        kind,
                        id,
                        name,
                        artist_name,
                        duration_ms,
                        artwork_url: artwork,
                        is_playable,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 25: Domain_Object mapping preserves present attributes
        // Validates: Requirements 12.1, 12.2
        //
        // For any list of successful-response resources that each have an id and
        // a name, mapping produces exactly one Domain_Object per resource (none
        // is ever dropped), in input order, that preserves every present
        // attribute, substitutes an empty string for each missing text
        // attribute, and `None` for a missing duration.
        #[test]
        fn property_mapping_preserves_present_attributes(
            resources in prop::collection::vec(resource_strategy(), 0..12)
        ) {
            let json_resources: Vec<Value> =
                resources.iter().map(GenResource::to_json).collect();

            let outcome = map_resources(&json_resources);

            // Every id/name-bearing resource is included: none dropped, count
            // and order preserved.
            prop_assert!(
                outcome.dropped.is_empty(),
                "no id/name-bearing resource should be dropped, got {:?}",
                outcome.dropped
            );
            prop_assert_eq!(outcome.objects.len(), resources.len());

            for (generated, mapped) in resources.iter().zip(outcome.objects.iter()) {
                // The mapped object preserves present attributes and applies the
                // documented substitutions for absent ones.
                prop_assert_eq!(mapped, &generated.expected());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_fully_populated_song_preserving_all_attributes() {
        let resource = json!({
            "id": "1001",
            "type": "songs",
            "attributes": {
                "name": "Clair de Lune",
                "artistName": "Claude Debussy",
                "durationInMillis": 300000u64,
                "artwork": { "url": "https://example.com/art.jpg" },
                "isPlayable": true
            }
        });

        let outcome = map_resources(&[resource]);

        assert!(outcome.dropped.is_empty());
        assert_eq!(
            outcome.objects,
            vec![Domain_Object::Song {
                id: "1001".to_string(),
                name: "Clair de Lune".to_string(),
                artist_name: "Claude Debussy".to_string(),
                duration_ms: Some(300000),
                artwork_url: Some("https://example.com/art.jpg".to_string()),
                is_playable: true,
            }]
        );
    }

    #[test]
    fn missing_text_attributes_become_empty_string_and_missing_duration_is_none() {
        // A song with only id/name/type: artistName, duration, and artwork all
        // absent (Requirement 12.2).
        let resource = json!({
            "id": "2002",
            "type": "songs",
            "attributes": { "name": "Untitled" }
        });

        let outcome = map_resources(&[resource]);

        assert!(outcome.dropped.is_empty());
        assert_eq!(
            outcome.objects,
            vec![Domain_Object::Song {
                id: "2002".to_string(),
                name: "Untitled".to_string(),
                artist_name: String::new(),
                duration_ms: None,
                artwork_url: None,
                is_playable: true,
            }]
        );
    }

    #[test]
    fn maps_album_artist_and_playlist_variants() {
        let resources = vec![
            json!({
                "id": "a1",
                "type": "albums",
                "attributes": {
                    "name": "Nocturnes",
                    "artistName": "Chopin",
                    "artwork": { "url": "https://example.com/album.jpg" }
                }
            }),
            json!({
                "id": "ar1",
                "type": "artists",
                "attributes": { "name": "Chopin" }
            }),
            json!({
                "id": "p1",
                "type": "playlists",
                "attributes": {
                    "name": "Focus",
                    "artwork": { "url": "https://example.com/pl.jpg" }
                }
            }),
        ];

        let outcome = map_resources(&resources);

        assert!(outcome.dropped.is_empty());
        assert_eq!(
            outcome.objects,
            vec![
                Domain_Object::Album {
                    id: "a1".to_string(),
                    name: "Nocturnes".to_string(),
                    artist_name: "Chopin".to_string(),
                    artwork_url: Some("https://example.com/album.jpg".to_string()),
                },
                Domain_Object::Artist {
                    id: "ar1".to_string(),
                    name: "Chopin".to_string(),
                    artwork_url: None,
                },
                Domain_Object::Playlist {
                    id: "p1".to_string(),
                    name: "Focus".to_string(),
                    artwork_url: Some("https://example.com/pl.jpg".to_string()),
                },
            ]
        );
    }

    #[test]
    fn drops_resource_missing_id_and_logs_it() {
        let resource = json!({
            "type": "songs",
            "attributes": { "name": "No Id" }
        });

        let outcome = map_resources(&[resource]);

        assert!(outcome.objects.is_empty());
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].reason, DropReason::MissingId);
        assert_eq!(outcome.dropped[0].index, 0);
        assert_eq!(outcome.dropped[0].resource_type.as_deref(), Some("songs"));
    }

    #[test]
    fn drops_resource_missing_name_and_logs_it() {
        let resource = json!({
            "id": "3003",
            "type": "songs",
            "attributes": { "artistName": "Someone" }
        });

        let outcome = map_resources(&[resource]);

        assert!(outcome.objects.is_empty());
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].reason, DropReason::MissingName);
        assert_eq!(outcome.dropped[0].id.as_deref(), Some("3003"));
    }

    #[test]
    fn empty_string_id_or_name_counts_as_missing() {
        let resources = vec![
            json!({ "id": "", "type": "songs", "attributes": { "name": "X" } }),
            json!({ "id": "4004", "type": "songs", "attributes": { "name": "" } }),
        ];

        let outcome = map_resources(&resources);

        assert!(outcome.objects.is_empty());
        assert_eq!(outcome.dropped.len(), 2);
        assert_eq!(outcome.dropped[0].reason, DropReason::MissingId);
        assert_eq!(outcome.dropped[1].reason, DropReason::MissingName);
    }

    #[test]
    fn keeps_well_formed_resources_and_drops_malformed_ones() {
        // Mix of well-formed and malformed resources (Requirement 12.3): the
        // good ones are returned, the bad ones dropped, order preserved.
        let resources = vec![
            json!({ "id": "1", "type": "songs", "attributes": { "name": "Good A" } }),
            json!({ "type": "songs", "attributes": { "name": "No Id" } }),
            json!({ "id": "2", "type": "albums", "attributes": { "name": "Good B", "artistName": "Band" } }),
            json!({ "id": "3", "type": "songs", "attributes": {} }),
            json!("not an object"),
        ];

        let outcome = map_resources(&resources);

        assert_eq!(outcome.objects.len(), 2);
        assert_eq!(outcome.dropped.len(), 3);
        // Order of the kept objects is preserved.
        match &outcome.objects[0] {
            Domain_Object::Song { name, .. } => assert_eq!(name, "Good A"),
            other => panic!("expected Song, got {other:?}"),
        }
        match &outcome.objects[1] {
            Domain_Object::Album { name, .. } => assert_eq!(name, "Good B"),
            other => panic!("expected Album, got {other:?}"),
        }
        assert_eq!(outcome.dropped[0].reason, DropReason::MissingId);
        assert_eq!(outcome.dropped[1].reason, DropReason::MissingName);
        assert_eq!(outcome.dropped[2].reason, DropReason::NotAnObject);
    }

    #[test]
    fn unknown_type_is_dropped() {
        let resource = json!({
            "id": "5005",
            "type": "music-videos",
            "attributes": { "name": "A Video" }
        });

        let outcome = map_resources(&[resource]);

        assert!(outcome.objects.is_empty());
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].reason, DropReason::UnknownType);
    }

    #[test]
    fn numeric_id_is_accepted_and_stringified() {
        let resource = json!({
            "id": 6006,
            "type": "songs",
            "attributes": { "name": "Numeric Id" }
        });

        let outcome = map_resources(&[resource]);

        assert_eq!(outcome.dropped.len(), 0);
        match &outcome.objects[0] {
            Domain_Object::Song { id, .. } => assert_eq!(id, "6006"),
            other => panic!("expected Song, got {other:?}"),
        }
    }

    #[test]
    fn negative_or_non_integer_duration_is_treated_as_absent() {
        let resource = json!({
            "id": "7007",
            "type": "songs",
            "attributes": { "name": "Bad Duration", "durationInMillis": -5 }
        });

        let outcome = map_resources(&[resource]);

        match &outcome.objects[0] {
            Domain_Object::Song { duration_ms, .. } => assert_eq!(*duration_ms, None),
            other => panic!("expected Song, got {other:?}"),
        }
    }

    #[test]
    fn map_response_accepts_data_envelope() {
        let body = json!({
            "data": [
                { "id": "1", "type": "songs", "attributes": { "name": "A" } }
            ]
        });

        let outcome = map_response(&body);

        assert_eq!(outcome.objects.len(), 1);
        assert!(outcome.dropped.is_empty());
    }

    #[test]
    fn map_response_accepts_bare_array() {
        let body = json!([
            { "id": "1", "type": "songs", "attributes": { "name": "A" } }
        ]);

        let outcome = map_response(&body);

        assert_eq!(outcome.objects.len(), 1);
        assert!(outcome.dropped.is_empty());
    }

    #[test]
    fn map_response_reports_unparseable_body_as_dropped() {
        let body = json!("totally wrong shape");

        let outcome = map_response(&body);

        assert!(outcome.objects.is_empty());
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].reason, DropReason::NotAnObject);
    }
}

#[cfg(test)]
mod mapping_property_tests {
    //! Property-based tests for the pure resource mapping layer (task 7.6).
    //! Each test exercises one design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    /// The four known resource types that map to a `Domain_Object` variant.
    const KNOWN_TYPES: [&str; 4] = ["songs", "albums", "artists", "playlists"];

    /// One generated resource, carrying whether it is well-formed so the test
    /// can count expectations independently of the mapper under test.
    #[derive(Debug, Clone)]
    struct GeneratedResource {
        value: Value,
        /// `true` when the resource has both a non-empty `id` and a non-empty
        /// `name` and a known `type`, so it should map to a `Domain_Object`;
        /// `false` when it is missing an `id` or a `name`, so it is dropped.
        well_formed: bool,
    }

    /// How a generated resource is made malformed (missing a required field).
    #[derive(Debug, Clone, Copy)]
    enum Malformation {
        /// Omit the `id` field entirely.
        MissingId,
        /// Present but empty `id` (counts as missing, Requirement 12.3).
        EmptyId,
        /// Omit the `name` attribute entirely.
        MissingName,
        /// Present but empty `name` (counts as missing, Requirement 12.3).
        EmptyName,
    }

    /// Strategy for a well-formed resource: a known type with a non-empty id and
    /// a non-empty name (plus arbitrary optional attributes that never affect
    /// the kept/dropped decision).
    fn well_formed_resource() -> impl Strategy<Value = GeneratedResource> {
        (
            prop::sample::select(KNOWN_TYPES.as_slice()),
            "[a-zA-Z0-9]{1,8}",
            "[a-zA-Z0-9 ]{1,12}",
        )
            .prop_map(|(kind, id, name)| GeneratedResource {
                value: json!({
                    "id": id,
                    "type": kind,
                    "attributes": { "name": name }
                }),
                well_formed: true,
            })
    }

    /// Strategy for a malformed resource: a known type whose `id` or `name` is
    /// missing or empty. Everything else is well-formed so the *only* reason the
    /// resource is dropped is the missing required field.
    fn malformed_resource() -> impl Strategy<Value = GeneratedResource> {
        let malformation = prop_oneof![
            Just(Malformation::MissingId),
            Just(Malformation::EmptyId),
            Just(Malformation::MissingName),
            Just(Malformation::EmptyName),
        ];
        (
            prop::sample::select(KNOWN_TYPES.as_slice()),
            "[a-zA-Z0-9]{1,8}",
            "[a-zA-Z0-9 ]{1,12}",
            malformation,
        )
            .prop_map(|(kind, id, name, how)| {
                let value = match how {
                    Malformation::MissingId => json!({
                        "type": kind,
                        "attributes": { "name": name }
                    }),
                    Malformation::EmptyId => json!({
                        "id": "",
                        "type": kind,
                        "attributes": { "name": name }
                    }),
                    Malformation::MissingName => json!({
                        "id": id,
                        "type": kind,
                        "attributes": {}
                    }),
                    Malformation::EmptyName => json!({
                        "id": id,
                        "type": kind,
                        "attributes": { "name": "" }
                    }),
                };
                GeneratedResource {
                    value,
                    well_formed: false,
                }
            })
    }

    /// Strategy for an arbitrary list mixing well-formed and malformed
    /// resources in any order and proportion (including all-good and all-bad).
    fn mixed_resources() -> impl Strategy<Value = Vec<GeneratedResource>> {
        prop::collection::vec(
            prop_oneof![well_formed_resource(), malformed_resource()],
            0..24,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: apple-music-tui, Property 26: Malformed resources are dropped, the rest returned
        // Validates: Requirements 12.3
        //
        // For any successful response mixing well-formed resources with
        // resources missing an id or name, the mapped result contains exactly
        // the well-formed resources, and the number of omitted resources equals
        // the number missing an id or name.
        #[test]
        fn property_malformed_resources_dropped_rest_returned(resources in mixed_resources()) {
            let expected_kept = resources.iter().filter(|r| r.well_formed).count();
            let expected_dropped = resources.iter().filter(|r| !r.well_formed).count();

            let values: Vec<Value> = resources.iter().map(|r| r.value.clone()).collect();
            let outcome = map_resources(&values);

            // Exactly the well-formed resources are returned...
            prop_assert_eq!(outcome.objects.len(), expected_kept);
            // ...and the number of omitted resources equals the number missing
            // an id or name.
            prop_assert_eq!(outcome.dropped.len(), expected_dropped);
            // Every input resource is accounted for exactly once.
            prop_assert_eq!(outcome.objects.len() + outcome.dropped.len(), values.len());
            // Every dropped resource was omitted for a missing id or name.
            for dropped in &outcome.dropped {
                prop_assert!(matches!(
                    dropped.reason,
                    DropReason::MissingId | DropReason::MissingName
                ));
            }
        }
    }
}

#[cfg(test)]
mod pagination_tests {
    //! Unit tests for search pagination (task 9.2): per-section offset tracking,
    //! more-available detection, append-without-moving-selection semantics, and
    //! the load-more error state that retries on the next end-of-list navigation
    //! (Requirement 2.10, 2.11).
    use super::*;
    use serde_json::json;

    fn song(id: &str) -> Domain_Object {
        Domain_Object::Song {
            id: id.to_string(),
            name: format!("Song {id}"),
            artist_name: String::new(),
            duration_ms: None,
            artwork_url: None,
            is_playable: true,
        }
    }

    fn first_page(songs: usize, more: bool) -> SearchPagination {
        let query = validate_query("daft punk").unwrap();
        let request = SearchRequest::new(&query, "us");
        let results = SearchResults {
            songs: (0..songs).map(|i| song(&format!("s{i}"))).collect(),
            ..SearchResults::default()
        };
        let section_more = SectionMore {
            songs: more,
            ..SectionMore::default()
        };
        SearchPagination::new(&request, results, section_more)
    }

    #[test]
    fn parse_section_more_reads_next_markers() {
        let body = json!({
            "results": {
                "songs": { "data": [], "next": "/v1/catalog/us/search?offset=25" },
                "albums": { "data": [] },
                "artists": { "data": [], "next": "" },
            }
        });
        let more = parse_section_more(&body);
        // A non-empty `next` means more available; an empty or absent one does not.
        assert!(more.songs);
        assert!(!more.albums);
        assert!(!more.artists);
        assert!(!more.playlists);
    }

    #[test]
    fn new_initialises_loaded_counts_and_more_flags() {
        let page = first_page(25, true);
        let state = page.section_state(SearchSection::Songs);
        assert_eq!(state.loaded, 25);
        assert!(state.more_available);
        assert!(!state.loading);
        assert!(!state.load_error);
        // A section with no results reports nothing to load.
        assert_eq!(page.section_state(SearchSection::Albums).loaded, 0);
        assert!(!page.section_state(SearchSection::Albums).more_available);
    }

    #[test]
    fn reach_end_requests_next_page_with_offset_equal_to_loaded() {
        let mut page = first_page(25, true);
        match page.reach_end_of_section(SearchSection::Songs) {
            EndOfSection::RequestNextPage(req) => {
                // Offset is the current item count so the next 25 are requested.
                assert_eq!(req.offset, 25);
                assert_eq!(req.term, "daft punk");
                assert_eq!(req.storefront, "us");
                // The request URL carries the offset param.
                assert!(req.url().contains("offset=25"), "url {}", req.url());
            }
            EndOfSection::Idle => panic!("expected a next-page request"),
        }
        // The section is now marked loading so a duplicate end-of-list does not
        // launch a second request.
        assert!(page.section_state(SearchSection::Songs).loading);
        assert_eq!(
            page.reach_end_of_section(SearchSection::Songs),
            EndOfSection::Idle
        );
    }

    #[test]
    fn reach_end_is_idle_when_no_more_available() {
        let mut page = first_page(10, false);
        assert_eq!(
            page.reach_end_of_section(SearchSection::Songs),
            EndOfSection::Idle
        );
    }

    #[test]
    fn apply_page_appends_to_tail_and_advances_offset() {
        let mut page = first_page(25, true);
        let _ = page.reach_end_of_section(SearchSection::Songs);
        let next: Vec<Domain_Object> = (25..40).map(|i| song(&format!("s{i}"))).collect();
        let more = SectionMore {
            songs: false,
            ..SectionMore::default()
        };
        page.apply_page(SearchSection::Songs, next, more);

        let songs = &page.results().songs;
        assert_eq!(songs.len(), 40);
        // The appended items are at the tail in order; the head is unchanged.
        match &songs[0] {
            Domain_Object::Song { id, .. } => assert_eq!(id, "s0"),
            other => panic!("unexpected {other:?}"),
        }
        match &songs[39] {
            Domain_Object::Song { id, .. } => assert_eq!(id, "s39"),
            other => panic!("unexpected {other:?}"),
        }
        let state = page.section_state(SearchSection::Songs);
        assert_eq!(state.loaded, 40);
        assert!(!state.more_available);
        assert!(!state.loading);
        assert!(!state.load_error);
    }

    #[test]
    fn failure_keeps_results_sets_error_and_retries_on_next_end_of_list() {
        let mut page = first_page(25, true);
        // First end-of-list triggers a request.
        assert!(matches!(
            page.reach_end_of_section(SearchSection::Songs),
            EndOfSection::RequestNextPage(_)
        ));
        // The request fails.
        page.record_failure(SearchSection::Songs);
        // Existing results are kept and a load-more error is shown.
        assert_eq!(page.results().songs.len(), 25);
        assert!(page.has_load_error(SearchSection::Songs));
        assert!(!page.section_state(SearchSection::Songs).loading);
        // more_available stays set so navigation re-arms the request.
        assert!(page.section_state(SearchSection::Songs).more_available);

        // Next end-of-list navigation retries (Requirement 2.11) and clears the
        // stale error while the retry is in flight.
        match page.reach_end_of_section(SearchSection::Songs) {
            EndOfSection::RequestNextPage(req) => assert_eq!(req.offset, 25),
            EndOfSection::Idle => panic!("expected retry request"),
        }
        assert!(!page.has_load_error(SearchSection::Songs));
        assert!(page.section_state(SearchSection::Songs).loading);
    }

    #[test]
    fn successful_retry_after_failure_appends_page() {
        let mut page = first_page(25, true);
        let _ = page.reach_end_of_section(SearchSection::Songs);
        page.record_failure(SearchSection::Songs);
        let _ = page.reach_end_of_section(SearchSection::Songs);
        let next: Vec<Domain_Object> = (25..30).map(|i| song(&format!("s{i}"))).collect();
        page.apply_page(SearchSection::Songs, next, SectionMore::default());
        assert_eq!(page.results().songs.len(), 30);
        assert!(!page.has_load_error(SearchSection::Songs));
    }

    #[test]
    fn pagination_tracks_sections_independently() {
        let query = validate_query("rock").unwrap();
        let request = SearchRequest::new(&query, "us");
        let results = SearchResults {
            songs: (0..25).map(|i| song(&format!("s{i}"))).collect(),
            albums: (0..25).map(|i| song(&format!("a{i}"))).collect(),
            ..SearchResults::default()
        };
        let mut page = SearchPagination::new(
            &request,
            results,
            SectionMore {
                songs: true,
                albums: false,
                ..SectionMore::default()
            },
        );
        // Songs can page; albums cannot, even at its end-of-list.
        assert!(matches!(
            page.reach_end_of_section(SearchSection::Songs),
            EndOfSection::RequestNextPage(_)
        ));
        assert_eq!(
            page.reach_end_of_section(SearchSection::Albums),
            EndOfSection::Idle
        );
    }
}

// ---------------------------------------------------------------------------
// Library browsing (task 9.3): section + track pagination at 100/page, refresh
// that re-fetches and resets the selection, failure handling that keeps prior
// items and leaves the Queue/playback untouched, and token-rejection handling
// that stops the sections and prompts re-sign-in (Requirements 3.1, 3.2, 3.3,
// 3.6, 3.7, 3.9). Everything below the networked `CatalogApi` methods is pure
// so it can be property-/unit-tested without a network, mirroring the search
// pagination design above.
// ---------------------------------------------------------------------------

/// Maximum items requested per Library page (Requirement 3.2–3.4). Library
/// sections and album/playlist tracks are both paged 100 items at a time, in
/// contrast to catalog search's [`SEARCH_SECTION_LIMIT`] of 25.
pub const LIBRARY_PAGE_LIMIT: usize = 100;

/// Maximum time to wait for a single Library request (Requirement 3.7). The
/// retry/backoff policy around this timeout is layered on in task 9.4; here it
/// only bounds one request, matching [`SEARCH_TIMEOUT`].
pub const LIBRARY_TIMEOUT: Duration = Duration::from_secs(10);

/// The four Library sections the TUI offers (Requirement 3.1).
///
/// The order matches the Requirement 3.1 labelling (playlists, albums, artists,
/// songs). Each variant names both the `/v1/me/library/<kind>` path segment and
/// the resource `type` the mapper expects, so a section and its contents stay
/// consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibrarySection {
    /// The user's saved playlists.
    Playlists,
    /// The user's saved albums.
    Albums,
    /// The user's saved artists.
    Artists,
    /// The user's saved songs.
    Songs,
}

impl LibrarySection {
    /// Every section in the Requirement 3.1 display order.
    pub const ALL: [LibrarySection; 4] = [
        LibrarySection::Playlists,
        LibrarySection::Albums,
        LibrarySection::Artists,
        LibrarySection::Songs,
    ];

    /// The `/v1/me/library/<segment>` path segment for this section.
    pub fn path_segment(self) -> &'static str {
        match self {
            LibrarySection::Playlists => "playlists",
            LibrarySection::Albums => "albums",
            LibrarySection::Artists => "artists",
            LibrarySection::Songs => "songs",
        }
    }
}

/// A fully-formed Library request for one page of a section or of an album's or
/// playlist's tracks (Requirement 3.2–3.4).
///
/// Built purely so the exact URL — path plus percent-encoded query string — can
/// be asserted without sending anything, mirroring [`SearchRequest`]. The
/// networked [`CatalogApi::fetch_library`] turns this into a `reqwest` call with
/// the Music_User_Token attached. Library requests carry no storefront: the
/// `/v1/me/library` endpoints are scoped to the signed-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRequest {
    /// The `/v1/me/library` path below the common prefix, e.g.
    /// `albums` for a section or `albums/<id>/tracks` for an album's tracks.
    path_suffix: String,
    /// Zero-based item offset for pagination (0 for the first page).
    pub offset: usize,
    /// Items requested for this page; always [`LIBRARY_PAGE_LIMIT`].
    pub limit: usize,
}

impl LibraryRequest {
    /// Build the request for page `offset` of a Library `section`
    /// (Requirement 3.2, 3.3). The first page uses `offset` 0.
    pub fn section(section: LibrarySection, offset: usize) -> Self {
        LibraryRequest {
            path_suffix: section.path_segment().to_string(),
            offset,
            limit: LIBRARY_PAGE_LIMIT,
        }
    }

    /// Build the request for page `offset` of the tracks of a Library album
    /// (`kind` = [`LibrarySection::Albums`]) or playlist
    /// ([`LibrarySection::Playlists`]) identified by `id` (Requirement 3.4).
    ///
    /// Only albums and playlists have a tracks relationship; passing another
    /// section is a programming error and panics in debug builds, but the method
    /// still produces a well-formed `<kind>/<id>/tracks` suffix so a release
    /// build degrades to an empty/benign request rather than a malformed URL.
    pub fn tracks(kind: LibrarySection, id: &str, offset: usize) -> Self {
        debug_assert!(
            matches!(kind, LibrarySection::Albums | LibrarySection::Playlists),
            "only albums and playlists have a tracks relationship"
        );
        LibraryRequest {
            path_suffix: format!("{}/{}/tracks", kind.path_segment(), id),
            offset,
            limit: LIBRARY_PAGE_LIMIT,
        }
    }

    /// The request path, e.g. `/v1/me/library/albums` or
    /// `/v1/me/library/playlists/p.123/tracks`.
    pub fn path(&self) -> String {
        format!("/v1/me/library/{}", self.path_suffix)
    }

    /// The query parameters as `(key, value)` pairs, before URL encoding
    /// (Requirement 3.2–3.4). `limit` caps the page at [`LIBRARY_PAGE_LIMIT`]
    /// and `offset` advances pagination (omitted on the first page).
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![("limit".to_string(), self.limit.to_string())];
        if self.offset > 0 {
            pairs.push(("offset".to_string(), self.offset.to_string()));
        }
        pairs
    }

    /// The absolute request URL with a percent-encoded query string.
    pub fn url(&self) -> String {
        let query = self
            .query_pairs()
            .into_iter()
            .map(|(k, v)| format!("{}={}", percent_encode(&k), percent_encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{API_HOST}{}?{}", self.path(), query)
    }
}

/// One page of a Library response: the mapped items in stored order, the
/// dropped-resource diagnostics, and whether the API reports a further page
/// (Requirement 3.2–3.4, reusing the Requirement 12 mapping).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryPage {
    /// The successfully mapped items for this page, in the order the API
    /// returned them (stored order, Requirement 3.4).
    pub items: Vec<Domain_Object>,
    /// Diagnostics for each resource omitted during mapping (Requirement 12.3).
    pub dropped: Vec<DroppedResource>,
    /// `true` when the API advertises a further page via a `next` marker
    /// (Requirement 3.3).
    pub more_available: bool,
}

/// Parse a successful (2xx) Library response body into a [`LibraryPage`]
/// (Requirement 3.2–3.4).
///
/// The Apple Music `/v1/me/library` responses place the resources under a
/// top-level `data` array and, when more pages exist, a top-level `next` URL
/// string. Each resource is mapped with [`map_resources`] so malformed
/// resources are dropped and logged (Requirement 12.3) while the rest are
/// returned in stored order. Each page is capped defensively at
/// [`LIBRARY_PAGE_LIMIT`] so the 100-per-page invariant holds regardless of the
/// server. Pure and side-effect free for property testing.
pub fn parse_library_response(body: &Value) -> LibraryPage {
    let data = body.get("data").and_then(Value::as_array);
    let (mut items, dropped) = match data {
        Some(resources) => {
            let outcome = map_resources(resources);
            (outcome.objects, outcome.dropped)
        }
        None => (Vec::new(), Vec::new()),
    };
    items.truncate(LIBRARY_PAGE_LIMIT);
    let more_available = body
        .get("next")
        .and_then(Value::as_str)
        .map(|url| !url.is_empty())
        .unwrap_or(false);
    LibraryPage {
        items,
        dropped,
        more_available,
    }
}

/// An error from a networked Library request (Requirement 3.7, 3.9).
///
/// The message never contains a token value (Requirement 1.13). The variants
/// map one-to-one onto the Requirement 3 error policy: a transport failure or
/// timeout keeps prior items and leaves the Queue/playback unchanged
/// (Requirement 3.7, handled by the caller via [`LibraryPagination`]), while a
/// token rejection stops the sections and prompts re-sign-in (Requirement 3.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryError {
    /// The request failed or exceeded the 10-second timeout (Requirement 3.7).
    /// `cause` is a token-free description for logging. The caller keeps the
    /// section's previously displayed items and leaves playback unchanged.
    Transport { cause: String },
    /// The Apple Music API rejected the Music_User_Token with HTTP 401/403
    /// (Requirement 3.9). The TUI stops displaying the Library sections and
    /// shows "you must sign in again"; this mirrors the Auth_Manager's
    /// [`crate::auth::AuthOutcome::SessionExpired`] path for a
    /// [`crate::auth::AuthScope::WithMusicUserToken`] request (task 8.3).
    SessionExpired,
    /// The API returned a non-success, non-auth HTTP status. Carries the code
    /// for the error message; the detailed 429/5xx policy lands in task 9.4.
    Status { code: u16 },
}

impl LibraryError {
    /// The user-facing message shown when the Library could not be loaded
    /// (Requirement 3.7) or the session expired (Requirement 3.9). Token-free.
    pub fn message(&self) -> String {
        match self {
            LibraryError::Transport { .. } => {
                "the library could not be loaded".to_string()
            }
            LibraryError::SessionExpired => "you must sign in again".to_string(),
            LibraryError::Status { code } => {
                format!("the library could not be loaded: the Apple Music API returned status {code}")
            }
        }
    }

    /// Whether this error means the Music_User_Token was rejected and the TUI
    /// must stop the Library sections and prompt re-sign-in (Requirement 3.9).
    pub fn is_session_expired(&self) -> bool {
        matches!(self, LibraryError::SessionExpired)
    }
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for LibraryError {}

/// Classify the HTTP status of a Library response into success or a
/// [`LibraryError`] (Requirement 3.9).
///
/// A 401/403 maps to [`LibraryError::SessionExpired`] because every Library
/// request carries the Music_User_Token, so a rejection means that token is no
/// longer accepted (Requirement 3.9, mirroring the Auth_Manager's
/// [`crate::auth::AuthScope::WithMusicUserToken`] session-expiry path). Any
/// other non-2xx status maps to [`LibraryError::Status`]. Pure so the mapping
/// can be unit-tested without a network.
pub fn classify_library_status(code: u16) -> Result<(), LibraryError> {
    match code {
        200..=299 => Ok(()),
        401 | 403 => Err(LibraryError::SessionExpired),
        other => Err(LibraryError::Status { code: other }),
    }
}

/// The per-section/per-list bookkeeping the Library pagination state machine
/// keeps (Requirement 3.2, 3.3, 3.6, 3.7).
///
/// Tracks how many items are loaded (`loaded`, which is also the offset of the
/// next page), whether the API reports more pages (`more_available`), whether a
/// request is in flight (`loading`, so a second end-of-list navigation does not
/// duplicate a request), whether the last request failed (`load_error`, which
/// drives the "library could not be loaded" message and leaves prior items in
/// place per Requirement 3.7), and whether the Library has been stopped because
/// the token was rejected (`stopped`, Requirement 3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LibraryListState {
    /// Number of items loaded so far; also the offset for the next page.
    pub loaded: usize,
    /// `true` while the API still advertises a further page.
    pub more_available: bool,
    /// `true` while a request for this list is in flight.
    pub loading: bool,
    /// `true` when the most recent request failed with a transport/other error
    /// and prior items were kept (Requirement 3.7).
    pub load_error: bool,
    /// `true` once a token rejection stopped this list (Requirement 3.9).
    pub stopped: bool,
}

/// Drives pagination and refresh for one Library list — a section, or an
/// album's/playlist's track list (Requirement 3.2, 3.3, 3.6, 3.7, 3.9).
///
/// The state machine is pure: it owns the accumulated items and the
/// [`LibraryListState`]. The TUI opens a list with [`new`](LibraryList::new),
/// and when the selection reaches the last loaded item calls
/// [`reach_end`](LibraryList::reach_end); if that returns a [`LibraryRequest`]
/// the TUI performs the networked fetch and reports the result with
/// [`apply_page`](LibraryList::apply_page) on success or
/// [`record_failure`](LibraryList::record_failure) on failure.
///
/// Successful pages are appended in stored order and never move the selection
/// (Requirement 3.3). A transport/other failure keeps the already-displayed
/// items untouched (Requirement 3.7). A refresh re-fetches the first page and
/// resets the selection to the first item (Requirement 3.6). A token rejection
/// stops the list so the TUI can hide the Library and prompt re-sign-in
/// (Requirement 3.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryList {
    /// The request builder for this list: a section or an album/playlist tracks
    /// relationship. Its offset is overwritten per page; the rest is stable.
    base: LibraryRequest,
    /// The accumulated items displayed so far, in stored order.
    items: Vec<Domain_Object>,
    state: LibraryListState,
}

impl LibraryList {
    /// Open a Library list for `request` with no pages loaded yet
    /// (Requirement 3.2). The caller immediately issues the first page via
    /// [`first_page_request`](LibraryList::first_page_request).
    pub fn new(request: LibraryRequest) -> Self {
        // Normalise the base request to the first page; per-page offsets are
        // derived from `loaded` when a page is requested.
        let base = LibraryRequest {
            offset: 0,
            ..request
        };
        LibraryList {
            base,
            items: Vec::new(),
            state: LibraryListState::default(),
        }
    }

    /// The accumulated items displayed so far, in stored order.
    pub fn items(&self) -> &[Domain_Object] {
        &self.items
    }

    /// The current list state (loaded/more/loading/error/stopped).
    pub fn state(&self) -> LibraryListState {
        self.state
    }

    /// `true` when the list is empty, so the TUI shows the empty-state message
    /// (Requirement 3.8). Only meaningful once a page has been loaded.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// `true` when the last request failed and prior items were kept
    /// (Requirement 3.7), so the TUI shows "the library could not be loaded".
    pub fn has_load_error(&self) -> bool {
        self.state.load_error
    }

    /// `true` once a token rejection stopped this list (Requirement 3.9).
    pub fn is_stopped(&self) -> bool {
        self.state.stopped
    }

    /// The request for the first page of this list (Requirement 3.2). Marks the
    /// list as loading so an overlapping trigger does not duplicate the fetch.
    pub fn first_page_request(&mut self) -> LibraryRequest {
        self.state.loading = true;
        self.state.load_error = false;
        LibraryRequest {
            offset: 0,
            ..self.base.clone()
        }
    }

    /// React to the selection reaching the last loaded item (Requirement 3.3,
    /// 3.7).
    ///
    /// Returns `Some(LibraryRequest)` with the next page's offset when a further
    /// page is available and no request is in flight and the list is not stopped
    /// — this is the same decision whether or not a previous attempt failed, so
    /// a prior load error is retried here (Requirement 3.7). Otherwise returns
    /// `None`. Calling this marks the list loading and clears any stale error.
    pub fn reach_end(&mut self) -> Option<LibraryRequest> {
        if self.state.stopped || !self.state.more_available || self.state.loading {
            return None;
        }
        self.state.loading = true;
        self.state.load_error = false;
        Some(LibraryRequest {
            offset: self.state.loaded,
            ..self.base.clone()
        })
    }

    /// The request that re-fetches the first page for a refresh (Requirement
    /// 3.6).
    ///
    /// Refresh does not clear the currently displayed items yet — they stay in
    /// place until [`apply_refresh`](LibraryList::apply_refresh) replaces them,
    /// so a failed refresh (via [`record_failure`](LibraryList::record_failure))
    /// leaves the prior items untouched (Requirement 3.7). Marks the list
    /// loading.
    pub fn refresh_request(&mut self) -> LibraryRequest {
        self.state.loading = true;
        self.state.load_error = false;
        LibraryRequest {
            offset: 0,
            ..self.base.clone()
        }
    }

    /// Append a successfully fetched next page in stored order (Requirement 3.3).
    ///
    /// The page's items are appended to the end of the list — the caller's
    /// current selection is never touched (Requirement 3.3) — the loaded count
    /// grows, the more-available flag is refreshed from the page, and the
    /// in-flight/error flags are cleared.
    pub fn apply_page(&mut self, page: LibraryPage) {
        let added = page.items.len();
        self.items.extend(page.items);
        self.state.loaded += added;
        self.state.more_available = page.more_available;
        self.state.loading = false;
        self.state.load_error = false;
    }

    /// Replace all displayed items with a freshly fetched first page and reset
    /// the selection to the first item (Requirement 3.6).
    ///
    /// Returns the selection index the TUI should move to: `0` (the first item)
    /// whenever any items were fetched, matching "move the selection to the
    /// first item"; an empty refreshed list leaves nothing to select.
    pub fn apply_refresh(&mut self, page: LibraryPage) -> Option<usize> {
        let count = page.items.len();
        self.items = page.items;
        self.state.loaded = count;
        self.state.more_available = page.more_available;
        self.state.loading = false;
        self.state.load_error = false;
        // Selection resets to the first item (Requirement 3.6); none if empty.
        (count > 0).then_some(0)
    }

    /// Record that a request for this list failed with a transport/other error
    /// (Requirement 3.7).
    ///
    /// The already-displayed items are kept untouched, the load error is set so
    /// the TUI shows "the library could not be loaded", and the in-flight flag
    /// is cleared while `more_available` stays set so the next end-of-list
    /// navigation re-issues the request. The caller leaves the Queue and current
    /// playback unchanged (a view/queue concern outside this state machine).
    pub fn record_failure(&mut self) {
        self.state.loading = false;
        self.state.load_error = true;
    }

    /// Stop this list because the Music_User_Token was rejected (Requirement
    /// 3.9). No further pages are requested and the TUI hides the Library and
    /// prompts re-sign-in. Clears the in-flight flag.
    pub fn record_session_expired(&mut self) {
        self.state.loading = false;
        self.state.load_error = false;
        self.state.stopped = true;
    }
}

impl CatalogApi {
    /// Fetch one page of a Library section or track list (Requirement 3.2–3.4,
    /// 3.9).
    ///
    /// Every Library request carries the Music_User_Token, so `music_user_token`
    /// is required (unlike catalog search, where it is optional). The
    /// Developer_Token is attached as the `Authorization: Bearer` header and the
    /// Music_User_Token as the `Music-User-Token` header. A 401/403 maps to
    /// [`LibraryError::SessionExpired`] (Requirement 3.9); the 10-second timeout
    /// is enforced by the client and surfaces as [`LibraryError::Transport`]
    /// (Requirement 3.7). The 2xx body is parsed into a [`LibraryPage`] whose
    /// items are in stored order and capped at 100 (Requirement 3.4), with the
    /// dropped-resource diagnostics returned for the caller to log
    /// (Requirement 12.3).
    pub async fn fetch_library(
        &self,
        request: &LibraryRequest,
        developer_token: &SecretString,
        music_user_token: &SecretString,
    ) -> Result<LibraryPage, LibraryError> {
        let builder = self
            .client
            .get(request.url())
            .bearer_auth(developer_token.expose())
            .header("Music-User-Token", music_user_token.expose());

        let response = builder.send().await.map_err(map_library_error)?;
        classify_library_status(response.status().as_u16())?;
        let body: Value = response.json().await.map_err(map_library_error)?;
        Ok(parse_library_response(&body))
    }
}

/// Map a `reqwest` transport error to a token-free [`LibraryError`]
/// (Requirement 3.7, 1.13).
fn map_library_error(err: reqwest::Error) -> LibraryError {
    LibraryError::Transport {
        cause: err.to_string(),
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;
    use serde_json::json;

    fn song(id: &str) -> Domain_Object {
        Domain_Object::Song {
            id: id.to_string(),
            name: format!("Song {id}"),
            artist_name: String::new(),
            duration_ms: None,
            artwork_url: None,
            is_playable: true,
        }
    }

    fn page_body(count: usize, kind: &str, next: bool) -> Value {
        let data: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": format!("{kind}-{i}"),
                    "type": kind,
                    "attributes": { "name": format!("{kind} {i}") }
                })
            })
            .collect();
        if next {
            json!({ "data": data, "next": "/v1/me/library/songs?offset=100" })
        } else {
            json!({ "data": data })
        }
    }

    #[test]
    fn section_request_builds_me_library_path_and_limit() {
        let req = LibraryRequest::section(LibrarySection::Albums, 0);
        assert_eq!(req.path(), "/v1/me/library/albums");
        let pairs = req.query_pairs();
        // 100-per-page cap expressed as the limit param (Requirement 3.2).
        assert!(pairs.contains(&("limit".to_string(), "100".to_string())));
        // No offset on the first page.
        assert!(!pairs.iter().any(|(k, _)| k == "offset"));
    }

    #[test]
    fn section_request_second_page_carries_offset() {
        let req = LibraryRequest::section(LibrarySection::Songs, 100);
        let pairs = req.query_pairs();
        assert!(pairs.contains(&("offset".to_string(), "100".to_string())));
        assert!(req
            .url()
            .starts_with("https://api.music.apple.com/v1/me/library/songs?"));
    }

    #[test]
    fn tracks_request_builds_relationship_path() {
        let album = LibraryRequest::tracks(LibrarySection::Albums, "l.abc", 0);
        assert_eq!(album.path(), "/v1/me/library/albums/l.abc/tracks");
        let playlist = LibraryRequest::tracks(LibrarySection::Playlists, "p.123", 100);
        assert_eq!(playlist.path(), "/v1/me/library/playlists/p.123/tracks");
        assert!(playlist
            .query_pairs()
            .contains(&("offset".to_string(), "100".to_string())));
    }

    #[test]
    fn parse_library_response_maps_items_in_order_and_detects_more() {
        let body = page_body(3, "songs", true);
        let page = parse_library_response(&body);
        assert_eq!(page.items.len(), 3);
        assert!(page.more_available);
        assert!(page.dropped.is_empty());
        // Stored order preserved (Requirement 3.4).
        if let Domain_Object::Song { id, .. } = &page.items[0] {
            assert_eq!(id, "songs-0");
        } else {
            panic!("expected a song");
        }
    }

    #[test]
    fn parse_library_response_no_next_means_no_more() {
        let page = parse_library_response(&page_body(2, "albums", false));
        assert!(!page.more_available);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn parse_library_response_missing_data_is_empty_not_error() {
        let page = parse_library_response(&json!({}));
        assert!(page.items.is_empty());
        assert!(page.dropped.is_empty());
        assert!(!page.more_available);
    }

    #[test]
    fn parse_library_response_drops_malformed_and_keeps_rest() {
        let body = json!({ "data": [
            { "id": "s1", "type": "songs", "attributes": { "name": "Good" } },
            { "type": "songs", "attributes": { "name": "No Id" } },
        ]});
        let page = parse_library_response(&body);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.dropped.len(), 1);
        assert_eq!(page.dropped[0].reason, DropReason::MissingId);
    }

    #[test]
    fn parse_library_response_caps_page_at_100() {
        let page = parse_library_response(&page_body(150, "songs", true));
        assert_eq!(page.items.len(), LIBRARY_PAGE_LIMIT);
    }

    #[test]
    fn classify_library_status_maps_401_403_to_session_expired() {
        assert_eq!(classify_library_status(200), Ok(()));
        assert_eq!(classify_library_status(204), Ok(()));
        assert_eq!(
            classify_library_status(401),
            Err(LibraryError::SessionExpired)
        );
        assert_eq!(
            classify_library_status(403),
            Err(LibraryError::SessionExpired)
        );
        assert_eq!(
            classify_library_status(500),
            Err(LibraryError::Status { code: 500 })
        );
    }

    #[test]
    fn library_error_messages_are_token_free_and_match_requirements() {
        let transport = LibraryError::Transport {
            cause: "connection reset".to_string(),
        };
        assert_eq!(transport.message(), "the library could not be loaded");
        assert!(!transport.is_session_expired());

        let expired = LibraryError::SessionExpired;
        assert_eq!(expired.message(), "you must sign in again");
        assert!(expired.is_session_expired());

        let status = LibraryError::Status { code: 500 };
        assert!(status.message().contains("500"));
    }

    #[test]
    fn list_loads_first_page_then_appends_without_resetting() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let req = list.first_page_request();
        assert_eq!(req.offset, 0);
        assert!(list.state().loading);

        let first: Vec<Domain_Object> = (0..100).map(|i| song(&format!("s{i}"))).collect();
        list.apply_page(LibraryPage {
            items: first,
            dropped: vec![],
            more_available: true,
        });
        assert_eq!(list.items().len(), 100);
        assert!(!list.state().loading);
        assert!(list.state().more_available);

        // End-of-list navigation requests the next page at offset 100.
        let next = list.reach_end().expect("more available -> request");
        assert_eq!(next.offset, 100);
        let more: Vec<Domain_Object> = (100..150).map(|i| song(&format!("s{i}"))).collect();
        list.apply_page(LibraryPage {
            items: more,
            dropped: vec![],
            more_available: false,
        });
        // Appended below existing items, nothing reset (Requirement 3.3).
        assert_eq!(list.items().len(), 150);
        assert!(list.reach_end().is_none());
    }

    #[test]
    fn reach_end_is_idle_when_no_more_or_loading() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Albums, 0));
        list.apply_page(LibraryPage {
            items: vec![song("a0")],
            dropped: vec![],
            more_available: false,
        });
        // No more pages -> no request.
        assert!(list.reach_end().is_none());
    }

    #[test]
    fn failure_keeps_prior_items_and_retries_on_next_end() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        list.apply_page(LibraryPage {
            items: (0..100).map(|i| song(&format!("s{i}"))).collect(),
            dropped: vec![],
            more_available: true,
        });
        let _ = list.reach_end();
        list.record_failure();
        // Prior items untouched (Requirement 3.7).
        assert_eq!(list.items().len(), 100);
        assert!(list.has_load_error());
        assert!(!list.state().loading);
        // Next end-of-list navigation retries and clears the stale error.
        let retry = list.reach_end().expect("retry request");
        assert_eq!(retry.offset, 100);
        assert!(!list.has_load_error());
    }

    #[test]
    fn refresh_refetches_first_page_and_resets_selection() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Playlists, 0));
        list.apply_page(LibraryPage {
            items: (0..100).map(|i| song(&format!("p{i}"))).collect(),
            dropped: vec![],
            more_available: true,
        });
        let _ = list.reach_end();
        list.apply_page(LibraryPage {
            items: (100..120).map(|i| song(&format!("p{i}"))).collect(),
            dropped: vec![],
            more_available: false,
        });
        assert_eq!(list.items().len(), 120);

        // Refresh re-fetches the first page.
        let req = list.refresh_request();
        assert_eq!(req.offset, 0);
        let refreshed: Vec<Domain_Object> = (0..100).map(|i| song(&format!("r{i}"))).collect();
        let selection = list.apply_refresh(LibraryPage {
            items: refreshed,
            dropped: vec![],
            more_available: true,
        });
        // Replaced items, selection reset to the first item (Requirement 3.6).
        assert_eq!(list.items().len(), 100);
        assert_eq!(selection, Some(0));
        assert_eq!(list.state().loaded, 100);
    }

    #[test]
    fn refresh_of_empty_section_has_no_selection() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        let selection = list.apply_refresh(LibraryPage::default());
        assert_eq!(selection, None);
        assert!(list.is_empty());
    }

    #[test]
    fn session_expiry_stops_the_list() {
        let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
        list.apply_page(LibraryPage {
            items: (0..100).map(|i| song(&format!("s{i}"))).collect(),
            dropped: vec![],
            more_available: true,
        });
        list.record_session_expired();
        assert!(list.is_stopped());
        // A stopped list never requests further pages (Requirement 3.9).
        assert!(list.reach_end().is_none());
    }
}

#[cfg(test)]
mod library_property_tests {
    //! Property-based tests for the pure Library pagination logic (task 9.3).
    use super::*;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    fn library_body(count: usize, next: bool) -> Value {
        let data: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": format!("s-{i}"),
                    "type": "songs",
                    "attributes": { "name": format!("Song {i}") }
                })
            })
            .collect();
        if next {
            json!({ "data": data, "next": "/v1/me/library/songs?offset=100" })
        } else {
            json!({ "data": data })
        }
    }

    proptest! {
        // Library pages are capped at 100 and preserve the API's stored order.
        // Validates: Requirements 3.2, 3.3, 3.4
        #[test]
        fn property_library_page_capped_and_ordered(count in 0usize..250, next in any::<bool>()) {
            let page = parse_library_response(&library_body(count, next));
            prop_assert!(page.items.len() <= LIBRARY_PAGE_LIMIT);
            prop_assert_eq!(page.items.len(), count.min(LIBRARY_PAGE_LIMIT));
            prop_assert_eq!(page.more_available, next);
            // Stored order preserved: the i-th mapped item is resource `s-i`.
            for (i, obj) in page.items.iter().enumerate() {
                if let Domain_Object::Song { id, .. } = obj {
                    prop_assert_eq!(id, &format!("s-{i}"));
                } else {
                    prop_assert!(false, "expected a song");
                }
            }
        }
    }

    proptest! {
        // Appending successive pages never reorders or drops earlier items and
        // keeps the loaded count equal to the total appended (Requirement 3.3).
        #[test]
        fn property_appending_pages_preserves_order(
            page_sizes in prop::collection::vec(0usize..100, 0..6),
        ) {
            let mut list = LibraryList::new(LibraryRequest::section(LibrarySection::Songs, 0));
            let _ = list.first_page_request();
            let mut expected = 0usize;
            let mut counter = 0usize;
            for (idx, size) in page_sizes.iter().enumerate() {
                let items: Vec<Domain_Object> = (0..*size)
                    .map(|_| {
                        let id = format!("s{counter}");
                        counter += 1;
                        Domain_Object::Song {
                            id,
                            name: String::new(),
                            artist_name: String::new(),
                            duration_ms: None,
                            artwork_url: None,
                            is_playable: true,
                        }
                    })
                    .collect();
                let more = idx + 1 < page_sizes.len();
                list.apply_page(LibraryPage { items, dropped: vec![], more_available: more });
                expected += size;
            }
            prop_assert_eq!(list.items().len(), expected);
            prop_assert_eq!(list.state().loaded, expected);
            // Items are in insertion order s0, s1, ... with no gaps.
            for (i, obj) in list.items().iter().enumerate() {
                if let Domain_Object::Song { id, .. } = obj {
                    prop_assert_eq!(id, &format!("s{i}"));
                }
            }
        }
    }
}
// ---------------------------------------------------------------------------
// Transport error handling (task 9.4): 10-second timeout with retry, 429
// rate-limit backoff computed from the Retry-After header, 5xx/other-4xx status
// surfacing, and single-shot retry per user selection (Requirements 2.5, 12.4,
// 12.5, 12.6, 12.7, 12.8).
//
// The policy is factored so its arithmetic core — the 429 backoff schedule — is
// a PURE function with no I/O, letting Property 27 (task 9.5) exercise it
// directly. The networked driver ([`send_with_retry`]) layers the async waits
// and request resends on top of that pure core.
// ---------------------------------------------------------------------------

/// The per-request timeout applied to every catalog and Library request
/// (Requirements 2.5, 3.7, 12.4). This matches [`SEARCH_TIMEOUT`] and
/// [`LIBRARY_TIMEOUT`]; it is named once here so the retry driver and the
/// `reqwest` client builders agree on a single value.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The wait used for a 429 when the `Retry-After` header is absent or is not a
/// non-negative integer number of seconds (Requirement 12.6).
pub const DEFAULT_BACKOFF_SECONDS: u64 = 1;

/// The maximum wait honoured for a single 429 attempt (Requirement 12.6). A
/// `Retry-After` larger than this is clamped down so one backoff never blocks
/// longer than a minute.
pub const MAX_BACKOFF_SECONDS: u64 = 60;

/// The maximum number of 429 retry attempts for one request (Requirement 12.6,
/// 12.7). After this many retries still return 429, the TUI shows the
/// rate-limit error instead of waiting again.
pub const MAX_RETRY_ATTEMPTS: u32 = 3;

/// Compute the backoff wait for one 429 response from its `Retry-After` header
/// value (Requirement 12.6, Property 27).
///
/// `retry_after` is the raw header value, if the response carried one. The wait
/// is the header value in seconds when it parses as a non-negative integer, and
/// [`DEFAULT_BACKOFF_SECONDS`] (1 second) otherwise — that is, when the header
/// is missing, empty, negative, fractional, or non-numeric (an HTTP-date
/// `Retry-After` is treated as "not a non-negative integer" and falls back to
/// the default, matching the requirement's wording). The result is always
/// clamped to at most [`MAX_BACKOFF_SECONDS`] (60 seconds) per attempt.
///
/// Pure and side-effect free: it performs no I/O and no sleeping, so Property 27
/// can assert the schedule across arbitrary header values.
pub fn backoff_delay(retry_after: Option<&str>) -> Duration {
    let seconds = parse_retry_after_seconds(retry_after).unwrap_or(DEFAULT_BACKOFF_SECONDS);
    Duration::from_secs(seconds.min(MAX_BACKOFF_SECONDS))
}

/// Parse a `Retry-After` header into a non-negative integer number of seconds,
/// or `None` when it is absent or not a non-negative integer (Requirement
/// 12.6).
///
/// Only the delay-seconds form is accepted. A leading/trailing-whitespace-padded
/// integer is accepted (headers are commonly space-padded); anything else —
/// empty, negative, fractional, an HTTP-date, or otherwise non-numeric — yields
/// `None` so the caller substitutes the 1-second default.
fn parse_retry_after_seconds(retry_after: Option<&str>) -> Option<u64> {
    retry_after?.trim().parse::<u64>().ok()
}

/// The full sequence of backoff waits a request would observe if every attempt
/// returned 429 with the given `Retry-After` header (Requirement 12.6, Property
/// 27).
///
/// Produces exactly [`MAX_RETRY_ATTEMPTS`] (3) waits — one before each retry —
/// each equal to [`backoff_delay`] for `retry_after`. Exposing the whole
/// schedule as a pure value lets Property 27 assert both the per-attempt wait
/// and the "at most three retries" bound without running the async driver.
pub fn backoff_schedule(retry_after: Option<&str>) -> Vec<Duration> {
    let wait = backoff_delay(retry_after);
    vec![wait; MAX_RETRY_ATTEMPTS as usize]
}

/// A transport-layer outcome classified from an HTTP status code (Requirements
/// 12.6, 12.7, 12.8).
///
/// This is the pure decision the retry driver makes after each response: a 2xx
/// succeeds, a 429 triggers a backoff-and-resend (up to [`MAX_RETRY_ATTEMPTS`]),
/// and a 5xx or other 4xx (excluding the 401/403 handled per-subsystem) is a
/// terminal status error naming the code and request. 401/403 are left to the
/// caller's auth/session policy (Requirement 1.9, 3.9) and reported as
/// [`TransportStatus::Auth`] so the driver does not retry or wrap them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    /// A 2xx success; the body should be parsed.
    Success,
    /// A 429 rate-limit response; the driver backs off and resends while retry
    /// attempts remain, then surfaces [`TransportError::RateLimited`].
    RateLimited,
    /// A 401/403 that the subsystem's auth/session policy handles, not the
    /// transport retry loop (Requirement 1.9, 3.9).
    Auth,
    /// A 5xx or other 4xx (not 401/403/429). Carries the status code so the TUI
    /// can name it in the error (Requirement 12.8).
    Status(u16),
}

/// Classify an HTTP status code for the shared transport policy (Requirements
/// 12.6, 12.7, 12.8).
///
/// - 200–299 → [`TransportStatus::Success`].
/// - 429 → [`TransportStatus::RateLimited`] (Requirement 12.6).
/// - 401/403 → [`TransportStatus::Auth`], deferred to the subsystem's session
///   policy (Requirement 1.9, 3.9).
/// - any other status → [`TransportStatus::Status`] carrying the code
///   (Requirement 12.8; covers 5xx and the remaining 4xx).
///
/// Pure so it can be unit-tested without a network.
pub fn classify_transport_status(code: u16) -> TransportStatus {
    match code {
        200..=299 => TransportStatus::Success,
        429 => TransportStatus::RateLimited,
        401 | 403 => TransportStatus::Auth,
        other => TransportStatus::Status(other),
    }
}

/// A failed transport attempt surfaced to the TUI after the retry policy has run
/// (Requirements 2.5, 12.4, 12.7, 12.8).
///
/// Every variant is token-free (Requirement 1.13) and carries a human-readable
/// description of the request that failed so the TUI can name it alongside the
/// retry action. The TUI resends the originating request exactly once per retry
/// selection (Requirement 12.5); this type describes only the failure, not the
/// retry bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The request failed to connect or no complete response arrived within the
    /// 10-second timeout (Requirements 2.5, 12.4). `request` names the failed
    /// request; `cause` is a token-free description for logging.
    Unreachable { request: String, cause: String },
    /// The API still returned 429 after [`MAX_RETRY_ATTEMPTS`] retries
    /// (Requirement 12.7). `request` names the rate-limited request.
    RateLimited { request: String },
    /// The API returned a 5xx or other 4xx status (not 401/403/429); the TUI
    /// shows the status code and the failed request (Requirement 12.8).
    Status { code: u16, request: String },
}

impl TransportError {
    /// The user-facing, token-free message for this failure (Requirements 2.5,
    /// 12.7, 12.8).
    pub fn message(&self) -> String {
        match self {
            TransportError::Unreachable { request, .. } => {
                format!("the Apple Music API is unreachable (request: {request})")
            }
            TransportError::RateLimited { request } => {
                format!("the Apple Music API rate limit was exceeded (request: {request})")
            }
            TransportError::Status { code, request } => {
                format!(
                    "the Apple Music API returned status {code} (request: {request})"
                )
            }
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for TransportError {}

/// The classified result of a single HTTP attempt, handed to the retry driver.
///
/// Keeps the pure classification ([`TransportStatus`]) together with the
/// `Retry-After` header needed to compute a 429 backoff, so the driver can be
/// written against a small, side-effect-free description of each attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutcome {
    /// How the attempt's status code classifies (Requirements 12.6–12.8).
    pub status: TransportStatus,
    /// The raw `Retry-After` header value, when the response carried one; used
    /// only for a [`TransportStatus::RateLimited`] attempt.
    pub retry_after: Option<String>,
}

/// Drive the shared transport retry policy over an async attempt function
/// (Requirements 2.5, 12.4, 12.6, 12.7).
///
/// `request_label` names the request for any surfaced [`TransportError`].
/// `attempt` performs one HTTP attempt (already timeout-bounded by the
/// `reqwest` client) and reports either a connection/timeout failure (`Err`) or
/// a classified [`AttemptOutcome`] (`Ok`). `sleep` awaits a backoff
/// [`Duration`]; production passes `tokio::time::sleep`, while tests pass a
/// no-op recorder so the schedule can be asserted without real waiting.
///
/// The loop makes the initial attempt and then up to [`MAX_RETRY_ATTEMPTS`]
/// retries, sleeping [`backoff_delay`] for the attempt's `Retry-After` before
/// each retry. It returns:
/// - `Ok(TransportStatus)` for a non-retryable classification (`Success`,
///   `Auth`, or a `Status(code)`), so the caller parses the body, runs its auth
///   policy, or surfaces the status respectively;
/// - `Err(TransportError::Unreachable)` on a connection/timeout failure
///   (Requirements 2.5, 12.4);
/// - `Err(TransportError::RateLimited)` when 429 persists after the retry budget
///   is exhausted (Requirement 12.7).
///
/// Single-shot retry on a user's retry selection (Requirement 12.5) is the
/// caller's concern: it invokes this driver afresh once per selection.
pub async fn send_with_retry<A, Fut, S, SFut>(
    request_label: &str,
    mut attempt: A,
    mut sleep: S,
) -> Result<TransportStatus, TransportError>
where
    A: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<AttemptOutcome, String>>,
    S: FnMut(Duration) -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    let mut retries_left = MAX_RETRY_ATTEMPTS;
    loop {
        let outcome = attempt().await.map_err(|cause| TransportError::Unreachable {
            request: request_label.to_string(),
            cause,
        })?;

        match outcome.status {
            TransportStatus::RateLimited => {
                if retries_left == 0 {
                    return Err(TransportError::RateLimited {
                        request: request_label.to_string(),
                    });
                }
                retries_left -= 1;
                sleep(backoff_delay(outcome.retry_after.as_deref())).await;
            }
            other => return Ok(other),
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn retry_after_integer_is_used_verbatim_when_under_cap() {
        assert_eq!(backoff_delay(Some("5")), Duration::from_secs(5));
        assert_eq!(backoff_delay(Some("  30 ")), Duration::from_secs(30));
        assert_eq!(backoff_delay(Some("0")), Duration::from_secs(0));
    }

    #[test]
    fn retry_after_is_capped_at_sixty_seconds() {
        assert_eq!(backoff_delay(Some("60")), Duration::from_secs(60));
        assert_eq!(backoff_delay(Some("61")), Duration::from_secs(60));
        assert_eq!(backoff_delay(Some("100000")), Duration::from_secs(60));
    }

    #[test]
    fn missing_or_invalid_retry_after_defaults_to_one_second() {
        assert_eq!(backoff_delay(None), Duration::from_secs(1));
        assert_eq!(backoff_delay(Some("")), Duration::from_secs(1));
        assert_eq!(backoff_delay(Some("   ")), Duration::from_secs(1));
        // Negative, fractional, and non-numeric (HTTP-date) all fall back.
        assert_eq!(backoff_delay(Some("-5")), Duration::from_secs(1));
        assert_eq!(backoff_delay(Some("1.5")), Duration::from_secs(1));
        assert_eq!(
            backoff_delay(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn schedule_has_exactly_three_waits() {
        let schedule = backoff_schedule(Some("2"));
        assert_eq!(schedule.len(), MAX_RETRY_ATTEMPTS as usize);
        assert!(schedule.iter().all(|w| *w == Duration::from_secs(2)));
    }

    #[test]
    fn classify_status_covers_success_rate_limit_auth_and_status() {
        assert_eq!(classify_transport_status(200), TransportStatus::Success);
        assert_eq!(classify_transport_status(299), TransportStatus::Success);
        assert_eq!(classify_transport_status(429), TransportStatus::RateLimited);
        assert_eq!(classify_transport_status(401), TransportStatus::Auth);
        assert_eq!(classify_transport_status(403), TransportStatus::Auth);
        assert_eq!(classify_transport_status(500), TransportStatus::Status(500));
        assert_eq!(classify_transport_status(404), TransportStatus::Status(404));
        assert_eq!(classify_transport_status(400), TransportStatus::Status(400));
    }

    #[test]
    fn transport_error_messages_name_the_request_and_are_token_free() {
        let unreachable = TransportError::Unreachable {
            request: "GET /v1/catalog/us/search".to_string(),
            cause: "connection reset".to_string(),
        };
        assert!(unreachable.message().contains("unreachable"));
        assert!(unreachable.message().contains("/v1/catalog/us/search"));

        let rate_limited = TransportError::RateLimited {
            request: "GET /v1/me/library/songs".to_string(),
        };
        assert!(rate_limited.message().contains("rate limit"));

        let status = TransportError::Status {
            code: 503,
            request: "GET /v1/catalog/us/search".to_string(),
        };
        assert!(status.message().contains("503"));
        assert!(status.message().contains("/v1/catalog/us/search"));
    }

    // A deterministic driver harness: the attempt function replays a scripted
    // list of outcomes and the sleep function records the waits instead of
    // actually sleeping, so the retry loop can be asserted without a network or
    // real time.
    async fn run_driver(
        outcomes: Vec<Result<AttemptOutcome, String>>,
    ) -> (Result<TransportStatus, TransportError>, Vec<Duration>) {
        use std::cell::RefCell;
        let script = RefCell::new(outcomes.into_iter());
        let waits = RefCell::new(Vec::new());
        let result = send_with_retry(
            "GET /test",
            || {
                let next = script.borrow_mut().next().expect("attempt beyond script");
                async move { next }
            },
            |d| {
                waits.borrow_mut().push(d);
                async {}
            },
        )
        .await;
        (result, waits.into_inner())
    }

    fn rate_limited(retry_after: Option<&str>) -> Result<AttemptOutcome, String> {
        Ok(AttemptOutcome {
            status: TransportStatus::RateLimited,
            retry_after: retry_after.map(str::to_string),
        })
    }

    fn ok(status: TransportStatus) -> Result<AttemptOutcome, String> {
        Ok(AttemptOutcome {
            status,
            retry_after: None,
        })
    }

    #[tokio::test]
    async fn driver_returns_success_without_sleeping() {
        let (result, waits) = run_driver(vec![ok(TransportStatus::Success)]).await;
        assert_eq!(result, Ok(TransportStatus::Success));
        assert!(waits.is_empty());
    }

    #[tokio::test]
    async fn driver_defers_auth_and_status_without_retry() {
        let (auth, auth_waits) = run_driver(vec![ok(TransportStatus::Auth)]).await;
        assert_eq!(auth, Ok(TransportStatus::Auth));
        assert!(auth_waits.is_empty());

        let (status, status_waits) = run_driver(vec![ok(TransportStatus::Status(500))]).await;
        assert_eq!(status, Ok(TransportStatus::Status(500)));
        assert!(status_waits.is_empty());
    }

    #[tokio::test]
    async fn driver_backs_off_then_succeeds() {
        let (result, waits) = run_driver(vec![
            rate_limited(Some("2")),
            rate_limited(Some("3")),
            ok(TransportStatus::Success),
        ])
        .await;
        assert_eq!(result, Ok(TransportStatus::Success));
        // One wait before each of the two retries, using each attempt's header.
        assert_eq!(waits, vec![Duration::from_secs(2), Duration::from_secs(3)]);
    }

    #[tokio::test]
    async fn driver_gives_up_after_three_retries() {
        // Initial attempt + 3 retries all 429 -> RateLimited, 3 waits.
        let (result, waits) = run_driver(vec![
            rate_limited(Some("1")),
            rate_limited(Some("1")),
            rate_limited(Some("1")),
            rate_limited(Some("1")),
        ])
        .await;
        assert_eq!(
            result,
            Err(TransportError::RateLimited {
                request: "GET /test".to_string()
            })
        );
        assert_eq!(waits.len(), MAX_RETRY_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn driver_maps_attempt_failure_to_unreachable() {
        let (result, waits) = run_driver(vec![Err("timed out".to_string())]).await;
        assert_eq!(
            result,
            Err(TransportError::Unreachable {
                request: "GET /test".to_string(),
                cause: "timed out".to_string(),
            })
        );
        assert!(waits.is_empty());
    }
}

#[cfg(test)]
mod backoff_property_tests {
    //! Property-based test for the rate-limit backoff schedule (task 9.5).
    //!
    //! Exercises the single design correctness property at >=100 iterations.
    use super::*;
    use proptest::prelude::*;

    /// A generated `Retry-After` header value paired with the number of seconds
    /// it should be interpreted as under the Requirement 12.6 rule *before*
    /// defaulting and capping: `Some(n)` for a non-negative integer header and
    /// `None` for an absent header or any value that is not a non-negative
    /// integer (negative, fractional, non-numeric, or an HTTP-date).
    #[derive(Debug, Clone)]
    struct RetryAfterCase {
        /// The raw header value, or `None` to model an absent header.
        header: Option<String>,
        /// `Some(seconds)` when the header is a non-negative integer, else
        /// `None` (the "default to 1 second" case).
        parsed_seconds: Option<u64>,
    }

    /// Generate the spread of `Retry-After` header values called out by the
    /// task: non-negative integers, negatives, fractional values, non-numeric
    /// text, HTTP-dates, and the absent header.
    fn retry_after_case() -> impl Strategy<Value = RetryAfterCase> {
        // A handful of real HTTP-date strings (the other accepted RFC 7231 form
        // of Retry-After), all of which must fall back to the default.
        let http_dates = prop::sample::select(vec![
            "Wed, 21 Oct 2015 07:28:00 GMT",
            "Fri, 31 Dec 1999 23:59:59 GMT",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        ])
        .prop_map(|s| s.to_string());

        prop_oneof![
            // Non-negative integer, optionally padded with surrounding spaces
            // (headers are commonly space-padded and must still be accepted).
            (any::<u64>(), 0usize..3, 0usize..3).prop_map(|(n, lead, trail)| {
                let header = format!("{}{n}{}", " ".repeat(lead), " ".repeat(trail));
                RetryAfterCase {
                    header: Some(header),
                    parsed_seconds: Some(n),
                }
            }),
            // Negative integers -> not a non-negative integer -> default.
            (1i64..=1_000_000).prop_map(|n| RetryAfterCase {
                header: Some(format!("-{n}")),
                parsed_seconds: None,
            }),
            // Fractional values -> default.
            (0u64..1000, 1u64..1000).prop_map(|(whole, frac)| RetryAfterCase {
                header: Some(format!("{whole}.{frac}")),
                parsed_seconds: None,
            }),
            // Arbitrary non-numeric text -> default. Constrained to characters
            // that never form a bare integer so the expected value is stable.
            "[A-Za-z_][A-Za-z0-9 _-]{0,20}".prop_map(|s| RetryAfterCase {
                header: Some(s),
                parsed_seconds: None,
            }),
            // Empty / whitespace-only header -> default.
            (0usize..5).prop_map(|n| RetryAfterCase {
                header: Some(" ".repeat(n)),
                parsed_seconds: None,
            }),
            // HTTP-date form -> treated as "not a non-negative integer" -> default.
            http_dates.prop_map(|s| RetryAfterCase {
                header: Some(s),
                parsed_seconds: None,
            }),
            // Absent header -> default.
            Just(RetryAfterCase {
                header: None,
                parsed_seconds: None,
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        // Feature: apple-music-tui, Property 27: Rate-limit backoff schedule
        // Validates: Requirements 12.6
        #[test]
        fn property_rate_limit_backoff_schedule(case in retry_after_case()) {
            let header = case.header.as_deref();

            // Expected per-attempt wait: the header value in seconds when it is
            // a non-negative integer, 1 second otherwise, then capped at 60.
            let expected_secs = case
                .parsed_seconds
                .unwrap_or(DEFAULT_BACKOFF_SECONDS)
                .min(MAX_BACKOFF_SECONDS);
            let expected = Duration::from_secs(expected_secs);

            // Single-attempt wait matches the rule exactly.
            prop_assert_eq!(backoff_delay(header), expected);

            // The computed wait never exceeds the 60-second per-attempt cap.
            prop_assert!(backoff_delay(header) <= Duration::from_secs(MAX_BACKOFF_SECONDS));

            // When the header carries no non-negative integer, the wait is the
            // 1-second default (before capping, which cannot lower 1s).
            if case.parsed_seconds.is_none() {
                prop_assert_eq!(backoff_delay(header), Duration::from_secs(DEFAULT_BACKOFF_SECONDS));
            }

            // The schedule holds at most three retry attempts, each equal to the
            // per-attempt wait.
            let schedule = backoff_schedule(header);
            prop_assert!(schedule.len() <= MAX_RETRY_ATTEMPTS as usize);
            prop_assert_eq!(schedule.len(), MAX_RETRY_ATTEMPTS as usize);
            for wait in &schedule {
                prop_assert_eq!(*wait, expected);
                prop_assert!(*wait <= Duration::from_secs(MAX_BACKOFF_SECONDS));
            }
        }
    }
}
