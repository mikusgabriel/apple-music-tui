//! IPC client (length-prefixed JSON-RPC over a Unix domain socket).
//!
//! The TUI is the client in the two-process architecture. It connects to the
//! Playback_Engine's Unix domain socket, issues JSON-RPC 2.0 *requests*
//! (load/play/pause/seek/setVolume/openSignIn/closeSignIn/shutdown), and
//! receives JSON-RPC *notifications* for asynchronous state
//! (stateChanged/trackFinished/playbackError/trackUnavailable/signInResult/
//! signInError).
//!
//! Transport (design "IPC Protocol"): a Unix domain socket at
//! `$XDG_RUNTIME_DIR/apple-music-tui/engine.sock`, falling back to a stable
//! per-user path under the OS temp dir when `XDG_RUNTIME_DIR` is unset or empty.
//! Framing: a 4-byte big-endian length prefix followed by a UTF-8 JSON-RPC 2.0
//! object. This matches the Electron IPC server (`playback-engine`), so the two
//! sides agree on the wire format and the socket path.
//!
//! This module provides:
//! - [`resolve_socket_path`]: the shared socket-path resolution.
//! - [`encode_frame`] / [`FrameDecoder`]: the 4-byte length-prefix framing.
//! - [`Request`] / [`RequestMethod`] / [`RequestParams`]: the typed request
//!   methods the TUI encodes.
//! - [`Notification`]: the typed notifications the TUI decodes.
//! - [`IpcClient`]: an async client that connects, sends requests, and yields
//!   decoded notifications.

use std::io;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::domain::PlaybackStatus;

/// The JSON-RPC version string carried on every message.
const JSONRPC_VERSION: &str = "2.0";

/// The socket file name, shared with the Electron IPC server.
const SOCKET_FILE: &str = "engine.sock";

/// The per-user directory name under the runtime/temp root.
const APP_DIR: &str = "apple-music-tui";

/// Resolve the Unix domain socket path shared with the Playback_Engine.
///
/// Primary location is `$XDG_RUNTIME_DIR/apple-music-tui/engine.sock`. When
/// `XDG_RUNTIME_DIR` is unset or empty, fall back to a stable per-user path
/// under the OS temp dir (`<temp>/apple-music-tui/engine.sock`) so the client
/// derives the same path as the server (design "IPC Protocol").
pub fn resolve_socket_path() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    base.join(APP_DIR).join(SOCKET_FILE)
}

/// The maximum frame payload length the decoder will accept, as a guard against
/// a corrupt or hostile length prefix allocating unbounded memory. JSON-RPC
/// control messages are tiny; 16 MiB is far above any legitimate message.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Encode a JSON-serializable message as a framed buffer: a 4-byte big-endian
/// length prefix followed by the UTF-8 JSON payload.
///
/// This mirrors the Electron server's `encodeFrame`.
pub fn encode_frame<T: Serialize>(message: &T) -> serde_json::Result<Vec<u8>> {
    let payload = serde_json::to_vec(message)?;
    let len = payload.len() as u32;
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(&payload);
    Ok(framed)
}

/// Incremental decoder for 4-byte big-endian length-prefixed frames.
///
/// Bytes arrive in arbitrary chunks over the socket; this buffers them and
/// yields whole payloads as they complete. A single chunk may contain zero,
/// one, or many frames, and a frame may span multiple chunks. Mirrors the
/// Electron server's `FrameDecoder`.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes to the internal buffer.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Extract the next complete frame payload (without the length prefix), if
    /// one is fully buffered.
    ///
    /// Returns `Ok(None)` when more bytes are needed, and an error when the
    /// declared length exceeds [`MAX_FRAME_LEN`].
    pub fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame length {len} exceeds maximum {MAX_FRAME_LEN}"),
            ));
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buffer[4..4 + len].to_vec();
        self.buffer.drain(..4 + len);
        Ok(Some(payload))
    }
}

/// Parameters for each request method (TUI → Playback_Engine).
///
/// Serialized to the `params` object of the JSON-RPC request, with the exact
/// field names the Electron server reads (design "IPC Protocol").
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestParams {
    /// `load` — `{trackId, positionMs, musicUserToken}`.
    Load {
        #[serde(rename = "trackId")]
        track_id: String,
        #[serde(rename = "positionMs")]
        position_ms: u64,
        #[serde(rename = "musicUserToken")]
        music_user_token: String,
    },
    /// `seek` — `{positionMs}`.
    Seek {
        #[serde(rename = "positionMs")]
        position_ms: u64,
    },
    /// `setVolume` — `{level}`.
    SetVolume { level: u8 },
    /// `openSignIn` — `{developerToken}`.
    OpenSignIn {
        #[serde(rename = "developerToken")]
        developer_token: String,
    },
    /// Methods with no parameters: `play`, `pause`, `closeSignIn`, `shutdown`.
    Empty {},
}

/// The request methods the TUI can issue (design "IPC Protocol").
#[derive(Debug, Clone, PartialEq)]
pub enum RequestMethod {
    /// Load a track at a start position, authorizing with the Music_User_Token.
    Load {
        track_id: String,
        position_ms: u64,
        music_user_token: String,
    },
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Seek to a position in milliseconds (the engine clamps to track bounds).
    Seek { position_ms: u64 },
    /// Set the volume level (clamped 0–100 by the engine).
    SetVolume { level: u8 },
    /// Open the Sign_In_Window, passing the Developer_Token.
    OpenSignIn { developer_token: String },
    /// Close the Sign_In_Window.
    CloseSignIn,
    /// Ask the engine to shut down.
    Shutdown,
}

impl RequestMethod {
    /// The JSON-RPC method name sent on the wire.
    pub fn method_name(&self) -> &'static str {
        match self {
            RequestMethod::Load { .. } => "load",
            RequestMethod::Play => "play",
            RequestMethod::Pause => "pause",
            RequestMethod::Seek { .. } => "seek",
            RequestMethod::SetVolume { .. } => "setVolume",
            RequestMethod::OpenSignIn { .. } => "openSignIn",
            RequestMethod::CloseSignIn => "closeSignIn",
            RequestMethod::Shutdown => "shutdown",
        }
    }

    /// The serialized parameters for this method.
    fn params(&self) -> RequestParams {
        match self {
            RequestMethod::Load {
                track_id,
                position_ms,
                music_user_token,
            } => RequestParams::Load {
                track_id: track_id.clone(),
                position_ms: *position_ms,
                music_user_token: music_user_token.clone(),
            },
            RequestMethod::Seek { position_ms } => RequestParams::Seek {
                position_ms: *position_ms,
            },
            RequestMethod::SetVolume { level } => RequestParams::SetVolume { level: *level },
            RequestMethod::OpenSignIn { developer_token } => RequestParams::OpenSignIn {
                developer_token: developer_token.clone(),
            },
            RequestMethod::Play
            | RequestMethod::Pause
            | RequestMethod::CloseSignIn
            | RequestMethod::Shutdown => RequestParams::Empty {},
        }
    }
}

/// A JSON-RPC 2.0 request message (TUI → Playback_Engine).
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: RequestParams,
}

impl Request {
    /// Build a request with the given id from a [`RequestMethod`].
    pub fn new(id: u64, method: &RequestMethod) -> Self {
        Request {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.method_name(),
            params: method.params(),
        }
    }
}

/// A notification the TUI decodes from the engine (design "IPC Protocol").
///
/// The `status` of `stateChanged` reuses the shared [`PlaybackStatus`] enum,
/// lower-cased on the wire to match the engine (`playing`/`paused`/`stopped`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// `stateChanged` — `{status, positionMs, durationMs, trackId}`.
    StateChanged {
        status: PlaybackStatus,
        position_ms: u64,
        duration_ms: Option<u64>,
        track_id: Option<String>,
    },
    /// `trackFinished` — `{trackId}`.
    TrackFinished { track_id: String },
    /// `playbackError` — `{trackId, kind}`.
    PlaybackError {
        track_id: String,
        kind: PlaybackErrorKind,
    },
    /// `trackUnavailable` — `{trackId}`.
    TrackUnavailable { track_id: String },
    /// `signInResult` — `{status, musicUserToken?}`.
    SignInResult {
        status: SignInStatus,
        music_user_token: Option<String>,
    },
    /// `signInError` — `{reason}`.
    SignInError { reason: String },
}

/// The failure class of a `playbackError` notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackErrorKind {
    Network,
    Drm,
}

/// The outcome of a `signInResult` notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInStatus {
    Ok,
    Cancelled,
}

/// An error decoding an incoming message into a [`Notification`].
#[derive(Debug)]
pub enum DecodeError {
    /// The payload was not valid JSON.
    Json(serde_json::Error),
    /// The message was valid JSON but not a recognized notification (e.g. a
    /// response to a request, or an unknown method).
    Unrecognized(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Json(e) => write!(f, "invalid JSON message: {e}"),
            DecodeError::Unrecognized(m) => write!(f, "unrecognized message: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Notification {
    /// Decode a raw JSON payload into a [`Notification`].
    ///
    /// Returns [`DecodeError::Unrecognized`] for messages that are valid JSON
    /// but not a known notification (for instance request responses, which the
    /// client handles separately, or methods outside the protocol).
    pub fn decode(payload: &[u8]) -> Result<Notification, DecodeError> {
        let value: Value = serde_json::from_slice(payload).map_err(DecodeError::Json)?;
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeError::Unrecognized(truncate_for_error(&value)))?;
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "stateChanged" => Ok(Notification::StateChanged {
                status: parse_status(params.get("status"))?,
                position_ms: as_u64(params.get("positionMs")).unwrap_or(0),
                duration_ms: as_u64(params.get("durationMs")),
                track_id: as_string(params.get("trackId")),
            }),
            "trackFinished" => Ok(Notification::TrackFinished {
                track_id: require_string(&params, "trackId", method)?,
            }),
            "playbackError" => Ok(Notification::PlaybackError {
                track_id: require_string(&params, "trackId", method)?,
                kind: parse_error_kind(params.get("kind"))?,
            }),
            "trackUnavailable" => Ok(Notification::TrackUnavailable {
                track_id: require_string(&params, "trackId", method)?,
            }),
            "signInResult" => Ok(Notification::SignInResult {
                status: parse_sign_in_status(params.get("status"))?,
                music_user_token: as_string(params.get("musicUserToken")),
            }),
            "signInError" => Ok(Notification::SignInError {
                reason: as_string(params.get("reason")).unwrap_or_default(),
            }),
            other => Err(DecodeError::Unrecognized(format!("method {other}"))),
        }
    }
}

fn parse_status(value: Option<&Value>) -> Result<PlaybackStatus, DecodeError> {
    match value.and_then(Value::as_str) {
        Some("playing") => Ok(PlaybackStatus::Playing),
        Some("paused") => Ok(PlaybackStatus::Paused),
        Some("stopped") => Ok(PlaybackStatus::Stopped),
        other => Err(DecodeError::Unrecognized(format!(
            "stateChanged status {other:?}"
        ))),
    }
}

fn parse_error_kind(value: Option<&Value>) -> Result<PlaybackErrorKind, DecodeError> {
    match value.and_then(Value::as_str) {
        Some("network") => Ok(PlaybackErrorKind::Network),
        Some("drm") => Ok(PlaybackErrorKind::Drm),
        other => Err(DecodeError::Unrecognized(format!(
            "playbackError kind {other:?}"
        ))),
    }
}

fn parse_sign_in_status(value: Option<&Value>) -> Result<SignInStatus, DecodeError> {
    match value.and_then(Value::as_str) {
        Some("ok") => Ok(SignInStatus::Ok),
        Some("cancelled") => Ok(SignInStatus::Cancelled),
        other => Err(DecodeError::Unrecognized(format!(
            "signInResult status {other:?}"
        ))),
    }
}

fn as_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn as_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|s| s.to_owned())
        .filter(|s| !s.is_empty())
}

fn require_string(params: &Value, field: &str, method: &str) -> Result<String, DecodeError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(|s| s.to_owned())
        .ok_or_else(|| DecodeError::Unrecognized(format!("{method} missing {field}")))
}

fn truncate_for_error(value: &Value) -> String {
    let s = value.to_string();
    if s.len() > 120 {
        format!("{}…", &s[..120])
    } else {
        s
    }
}

/// An async JSON-RPC client over the Playback_Engine's Unix domain socket.
///
/// Requests are written through a cloneable sender ([`IpcSender`]); incoming
/// frames are read by [`IpcClient::next_notification`], which yields decoded
/// notifications and silently skips request responses and any non-notification
/// messages.
pub struct IpcClient {
    reader: tokio::net::unix::OwnedReadHalf,
    sender: IpcSender,
    decoder: FrameDecoder,
}

/// The write half of an [`IpcClient`], used to send requests.
///
/// Cloneable and `Send`/`Sync` so the render loop and other tasks can issue
/// requests concurrently; writes are serialized by an internal mutex.
#[derive(Clone)]
pub struct IpcSender {
    write: std::sync::Arc<Mutex<OwnedWriteHalf>>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl IpcClient {
    /// Connect to the socket at [`resolve_socket_path`].
    pub async fn connect() -> io::Result<Self> {
        Self::connect_to(resolve_socket_path()).await
    }

    /// Connect to a specific socket path.
    pub async fn connect_to(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(IpcClient {
            reader,
            sender: IpcSender {
                write: std::sync::Arc::new(Mutex::new(writer)),
                next_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            },
            decoder: FrameDecoder::new(),
        })
    }

    /// A cloneable sender for issuing requests.
    pub fn sender(&self) -> IpcSender {
        self.sender.clone()
    }

    /// Await and decode the next notification from the engine.
    ///
    /// Non-notification frames (request responses, unknown methods) are skipped.
    /// Returns `Ok(None)` when the socket closes cleanly.
    pub async fn next_notification(&mut self) -> io::Result<Option<Notification>> {
        loop {
            // Drain any whole frames already buffered before reading more bytes.
            while let Some(payload) = self.decoder.next_frame()? {
                match Notification::decode(&payload) {
                    Ok(notification) => return Ok(Some(notification)),
                    Err(DecodeError::Unrecognized(_)) => continue,
                    Err(DecodeError::Json(e)) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                    }
                }
            }

            let mut buf = [0u8; 4096];
            let read = self.reader.read(&mut buf).await?;
            if read == 0 {
                return Ok(None);
            }
            self.decoder.push(&buf[..read]);
        }
    }
}

impl IpcSender {
    /// Encode and send a request to the engine, returning the id assigned to it.
    ///
    /// Ids are monotonic per client; the result/notification correlation is
    /// handled by the caller (responses carry the same id).
    pub async fn send(&self, method: &RequestMethod) -> io::Result<u64> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = Request::new(id, method);
        let frame = encode_frame(&request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut guard = self.write.lock().await;
        guard.write_all(&frame).await?;
        guard.flush().await?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        // Snapshot and restore to avoid leaking env changes across tests.
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let path = resolve_socket_path();
        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/apple-music-tui/engine.sock")
        );
        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn socket_path_falls_back_to_temp_dir_when_unset() {
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::remove_var("XDG_RUNTIME_DIR");
        let path = resolve_socket_path();
        let expected = std::env::temp_dir().join("apple-music-tui").join("engine.sock");
        assert_eq!(path, expected);
        if let Some(v) = previous {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }

    #[test]
    fn encode_frame_prefixes_big_endian_length() {
        #[derive(Serialize)]
        struct Msg {
            a: u8,
        }
        let frame = encode_frame(&Msg { a: 1 }).unwrap();
        let payload = br#"{"a":1}"#;
        let mut expected = (payload.len() as u32).to_be_bytes().to_vec();
        expected.extend_from_slice(payload);
        assert_eq!(frame, expected);
    }

    #[test]
    fn decoder_yields_whole_frames_across_chunk_boundaries() {
        let frame = encode_frame(&serde_json::json!({"hello": "world"})).unwrap();
        let mut decoder = FrameDecoder::new();

        // Feed the frame one byte at a time; no frame until the last byte.
        for (i, byte) in frame.iter().enumerate() {
            decoder.push(&[*byte]);
            if i + 1 < frame.len() {
                assert!(decoder.next_frame().unwrap().is_none());
            }
        }
        let payload = decoder.next_frame().unwrap().expect("a full frame");
        let value: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["hello"], "world");
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn decoder_handles_two_frames_in_one_chunk() {
        let mut bytes = encode_frame(&serde_json::json!({"n": 1})).unwrap();
        bytes.extend(encode_frame(&serde_json::json!({"n": 2})).unwrap());
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);

        let first = decoder.next_frame().unwrap().unwrap();
        let second = decoder.next_frame().unwrap().unwrap();
        assert!(decoder.next_frame().unwrap().is_none());
        let a: Value = serde_json::from_slice(&first).unwrap();
        let b: Value = serde_json::from_slice(&second).unwrap();
        assert_eq!(a["n"], 1);
        assert_eq!(b["n"], 2);
    }

    #[test]
    fn decoder_rejects_oversized_frame_length() {
        let mut decoder = FrameDecoder::new();
        // A length prefix declaring more than MAX_FRAME_LEN bytes.
        let len = (MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        decoder.push(&len);
        decoder.push(&[0u8; 8]);
        assert!(decoder.next_frame().is_err());
    }

    #[test]
    fn request_encodes_load_with_expected_field_names() {
        let method = RequestMethod::Load {
            track_id: "123".to_string(),
            position_ms: 5000,
            music_user_token: "mut".to_string(),
        };
        let request = Request::new(7, &method);
        let value: Value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "load");
        assert_eq!(value["params"]["trackId"], "123");
        assert_eq!(value["params"]["positionMs"], 5000);
        assert_eq!(value["params"]["musicUserToken"], "mut");
    }

    #[test]
    fn request_encodes_empty_params_methods() {
        for (method, name) in [
            (RequestMethod::Play, "play"),
            (RequestMethod::Pause, "pause"),
            (RequestMethod::CloseSignIn, "closeSignIn"),
            (RequestMethod::Shutdown, "shutdown"),
        ] {
            let value = serde_json::to_value(Request::new(1, &method)).unwrap();
            assert_eq!(value["method"], name);
            assert_eq!(value["params"], serde_json::json!({}));
        }
    }

    #[test]
    fn request_encodes_seek_set_volume_and_open_sign_in() {
        let seek = serde_json::to_value(Request::new(1, &RequestMethod::Seek { position_ms: 42 }))
            .unwrap();
        assert_eq!(seek["method"], "seek");
        assert_eq!(seek["params"]["positionMs"], 42);

        let volume =
            serde_json::to_value(Request::new(2, &RequestMethod::SetVolume { level: 80 })).unwrap();
        assert_eq!(volume["method"], "setVolume");
        assert_eq!(volume["params"]["level"], 80);

        let sign_in = serde_json::to_value(Request::new(
            3,
            &RequestMethod::OpenSignIn {
                developer_token: "dev".to_string(),
            },
        ))
        .unwrap();
        assert_eq!(sign_in["method"], "openSignIn");
        assert_eq!(sign_in["params"]["developerToken"], "dev");
    }

    #[test]
    fn decode_state_changed_notification() {
        let payload = br#"{"jsonrpc":"2.0","method":"stateChanged","params":{"status":"playing","positionMs":1500,"durationMs":200000,"trackId":"t1"}}"#;
        let notification = Notification::decode(payload).unwrap();
        assert_eq!(
            notification,
            Notification::StateChanged {
                status: PlaybackStatus::Playing,
                position_ms: 1500,
                duration_ms: Some(200000),
                track_id: Some("t1".to_string()),
            }
        );
    }

    #[test]
    fn decode_track_finished_and_unavailable() {
        let finished =
            Notification::decode(br#"{"method":"trackFinished","params":{"trackId":"a"}}"#)
                .unwrap();
        assert_eq!(
            finished,
            Notification::TrackFinished {
                track_id: "a".to_string()
            }
        );

        let unavailable =
            Notification::decode(br#"{"method":"trackUnavailable","params":{"trackId":"b"}}"#)
                .unwrap();
        assert_eq!(
            unavailable,
            Notification::TrackUnavailable {
                track_id: "b".to_string()
            }
        );
    }

    #[test]
    fn decode_playback_error_kinds() {
        let network =
            Notification::decode(br#"{"method":"playbackError","params":{"trackId":"a","kind":"network"}}"#)
                .unwrap();
        assert_eq!(
            network,
            Notification::PlaybackError {
                track_id: "a".to_string(),
                kind: PlaybackErrorKind::Network,
            }
        );

        let drm =
            Notification::decode(br#"{"method":"playbackError","params":{"trackId":"a","kind":"drm"}}"#)
                .unwrap();
        assert_eq!(
            drm,
            Notification::PlaybackError {
                track_id: "a".to_string(),
                kind: PlaybackErrorKind::Drm,
            }
        );
    }

    #[test]
    fn decode_sign_in_result_and_error() {
        let ok = Notification::decode(
            br#"{"method":"signInResult","params":{"status":"ok","musicUserToken":"mut"}}"#,
        )
        .unwrap();
        assert_eq!(
            ok,
            Notification::SignInResult {
                status: SignInStatus::Ok,
                music_user_token: Some("mut".to_string()),
            }
        );

        let cancelled =
            Notification::decode(br#"{"method":"signInResult","params":{"status":"cancelled"}}"#)
                .unwrap();
        assert_eq!(
            cancelled,
            Notification::SignInResult {
                status: SignInStatus::Cancelled,
                music_user_token: None,
            }
        );

        let error =
            Notification::decode(br#"{"method":"signInError","params":{"reason":"boom"}}"#).unwrap();
        assert_eq!(
            error,
            Notification::SignInError {
                reason: "boom".to_string()
            }
        );
    }

    #[test]
    fn decode_skips_request_responses() {
        // A JSON-RPC response carries `id`/`result` and no `method`.
        let err = Notification::decode(br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
            .unwrap_err();
        assert!(matches!(err, DecodeError::Unrecognized(_)));
    }

    #[tokio::test]
    async fn client_round_trips_request_and_notification_over_a_socket() {
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("amt-ipc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("engine.sock");
        let _ = std::fs::remove_file(&socket);

        let listener = UnixListener::bind(&socket).unwrap();

        // Server side: accept, read one framed request, echo one notification.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();

            let mut decoder = FrameDecoder::new();
            let mut buf = [0u8; 1024];
            let request = loop {
                if let Some(frame) = decoder.next_frame().unwrap() {
                    break frame;
                }
                let n = read.read(&mut buf).await.unwrap();
                decoder.push(&buf[..n]);
            };
            let value: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(value["method"], "play");

            let notification = encode_frame(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "trackFinished",
                "params": {"trackId": "done"}
            }))
            .unwrap();
            write.write_all(&notification).await.unwrap();
            write.flush().await.unwrap();
        });

        let mut client = IpcClient::connect_to(&socket).await.unwrap();
        client.sender().send(&RequestMethod::Play).await.unwrap();
        let notification = client.next_notification().await.unwrap().unwrap();
        assert_eq!(
            notification,
            Notification::TrackFinished {
                track_id: "done".to_string()
            }
        );

        server.await.unwrap();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(&dir);
    }
}
