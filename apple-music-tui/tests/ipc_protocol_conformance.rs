//! Integration test: IPC protocol conformance (task 11.3).
//!
//! Verifies the JSON-RPC request/notification *schema* on the wire between the
//! Rust IPC client (`apple_music_tui::ipc`) and a **stub Playback_Engine**: a
//! Unix domain socket listener in the test that speaks the same transport as
//! the real Electron engine (`playback-engine/src/ipc-server.js`) — a 4-byte
//! big-endian length prefix followed by a UTF-8 JSON-RPC 2.0 object.
//!
//! The stub deliberately re-derives the framing and schema expectations from
//! the design's "IPC Protocol" table rather than reusing the client's encoder,
//! so a drift in either side's wire format would fail the test:
//!   requests:  load / play / pause / seek / setVolume / openSignIn /
//!              closeSignIn / shutdown
//!   notifs:    stateChanged / trackFinished / playbackError /
//!              trackUnavailable / signInResult / signInError
//!
//! _Requirements: 4.2_
//! _Design: Testing Strategy (Integration and Smoke Tests), IPC Protocol_

use std::path::PathBuf;

use apple_music_tui::domain::PlaybackStatus;
use apple_music_tui::ipc::{
    IpcClient, Notification, PlaybackErrorKind, RequestMethod, SignInStatus,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// A unique, self-cleaning socket path for one test, under the OS temp dir.
struct TestSocket {
    dir: PathBuf,
    path: PathBuf,
}

impl TestSocket {
    fn new(tag: &str) -> Self {
        // Include pid + a per-test tag + a nanosecond stamp so parallel tests
        // never collide on the same socket file.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "amt-ipc-conformance-{}-{tag}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.sock");
        let _ = std::fs::remove_file(&path);
        TestSocket { dir, path }
    }
}

impl Drop for TestSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Frame an outbound JSON value exactly as the engine does: a 4-byte big-endian
/// length prefix followed by the UTF-8 JSON payload. Re-implemented here (not
/// imported from the client) so the test independently pins the wire format.
fn stub_encode_frame(message: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(message).unwrap();
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);
    framed
}

/// Read exactly one length-prefixed frame from the socket, independently of the
/// client's decoder, and return the decoded JSON value. Reads the 4-byte prefix
/// then the declared number of payload bytes, so a single `write_all` of a
/// framed request is parsed back correctly regardless of chunking.
async fn stub_read_frame(stream: &mut UnixStream) -> Value {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    serde_json::from_slice(&payload).unwrap()
}

/// Assert the JSON-RPC envelope fields common to every request the client
/// sends: version "2.0", a positive integer id, and a method name.
fn assert_request_envelope(request: &Value, expected_method: &str) {
    assert_eq!(
        request["jsonrpc"], "2.0",
        "request must carry jsonrpc 2.0: {request}"
    );
    assert_eq!(
        request["method"], expected_method,
        "unexpected method name: {request}"
    );
    assert!(
        request["id"].as_u64().is_some(),
        "request must carry an integer id: {request}"
    );
}

// --------------------------------------------------------------------------
// Request schema conformance (TUI client -> stub engine)
// --------------------------------------------------------------------------

/// The client encodes every request method with the exact JSON-RPC envelope and
/// params field names the engine reads from the "IPC Protocol" table. The stub
/// accepts the connection, reads each framed request, and asserts its schema.
#[tokio::test]
async fn requests_conform_to_the_protocol_schema() {
    let socket = TestSocket::new("requests");
    let listener = UnixListener::bind(&socket.path).unwrap();

    // Stub engine: read the eight requests in the order the client sends them
    // and return each decoded frame to the test body for assertions.
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        for _ in 0..8 {
            received.push(stub_read_frame(&mut stream).await);
        }
        received
    });

    let client = IpcClient::connect_to(&socket.path).await.unwrap();
    let sender = client.sender();

    // Send one of every request method, exercising param-carrying and
    // empty-param variants.
    sender
        .send(&RequestMethod::Load {
            track_id: "track-1".to_string(),
            position_ms: 5_000,
            music_user_token: "mut-abc".to_string(),
        })
        .await
        .unwrap();
    sender.send(&RequestMethod::Play).await.unwrap();
    sender.send(&RequestMethod::Pause).await.unwrap();
    sender
        .send(&RequestMethod::Seek { position_ms: 42_000 })
        .await
        .unwrap();
    sender
        .send(&RequestMethod::SetVolume { level: 80 })
        .await
        .unwrap();
    sender
        .send(&RequestMethod::OpenSignIn {
            developer_token: "dev-jwt".to_string(),
        })
        .await
        .unwrap();
    sender.send(&RequestMethod::CloseSignIn).await.unwrap();
    sender.send(&RequestMethod::Shutdown).await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(received.len(), 8, "stub engine should read all 8 requests");

    // load {trackId, positionMs, musicUserToken}
    let load = &received[0];
    assert_request_envelope(load, "load");
    assert_eq!(load["params"]["trackId"], "track-1");
    assert_eq!(load["params"]["positionMs"], 5_000);
    assert_eq!(load["params"]["musicUserToken"], "mut-abc");

    // play {}
    let play = &received[1];
    assert_request_envelope(play, "play");
    assert_eq!(play["params"], json!({}));

    // pause {}
    let pause = &received[2];
    assert_request_envelope(pause, "pause");
    assert_eq!(pause["params"], json!({}));

    // seek {positionMs}
    let seek = &received[3];
    assert_request_envelope(seek, "seek");
    assert_eq!(seek["params"]["positionMs"], 42_000);

    // setVolume {level}
    let set_volume = &received[4];
    assert_request_envelope(set_volume, "setVolume");
    assert_eq!(set_volume["params"]["level"], 80);

    // openSignIn {developerToken}
    let open_sign_in = &received[5];
    assert_request_envelope(open_sign_in, "openSignIn");
    assert_eq!(open_sign_in["params"]["developerToken"], "dev-jwt");

    // closeSignIn {}
    let close_sign_in = &received[6];
    assert_request_envelope(close_sign_in, "closeSignIn");
    assert_eq!(close_sign_in["params"], json!({}));

    // shutdown {}
    let shutdown = &received[7];
    assert_request_envelope(shutdown, "shutdown");
    assert_eq!(shutdown["params"], json!({}));

    // Ids are monotonic and unique across the eight requests (client assigns
    // them 1..=8 in send order).
    let ids: Vec<u64> = received.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "request ids must be unique: {ids:?}");
}

// --------------------------------------------------------------------------
// Notification schema conformance (stub engine -> TUI client)
// --------------------------------------------------------------------------

/// The stub engine emits every notification shape from the "IPC Protocol" table
/// using the engine's own framing, and the client decodes each into the correct
/// typed `Notification`. This exercises the 4-byte length-prefix framing end to
/// end (all six frames are written back-to-back into the stream).
#[tokio::test]
async fn notifications_conform_to_the_protocol_schema() {
    let socket = TestSocket::new("notifications");
    let listener = UnixListener::bind(&socket.path).unwrap();

    // Stub engine: on connect, emit one of every notification, back to back.
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let notifications = [
            json!({
                "jsonrpc": "2.0",
                "method": "stateChanged",
                "params": {
                    "status": "playing",
                    "positionMs": 1_500,
                    "durationMs": 210_000,
                    "trackId": "track-1"
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "trackFinished",
                "params": { "trackId": "track-1" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "playbackError",
                "params": { "trackId": "track-2", "kind": "network" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "playbackError",
                "params": { "trackId": "track-3", "kind": "drm" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "trackUnavailable",
                "params": { "trackId": "track-4" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "signInResult",
                "params": { "status": "ok", "musicUserToken": "mut-xyz" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "signInResult",
                "params": { "status": "cancelled" }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "signInError",
                "params": { "reason": "window failed to open" }
            }),
        ];

        // Write all frames into one buffer so they are flushed together,
        // exercising the client's incremental decoder across frame boundaries.
        let mut buf = Vec::new();
        for n in &notifications {
            buf.extend_from_slice(&stub_encode_frame(n));
        }
        stream.write_all(&buf).await.unwrap();
        stream.flush().await.unwrap();

        // Keep the connection open until the client has read everything.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });

    let mut client = IpcClient::connect_to(&socket.path).await.unwrap();

    let decoded = [
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
        client.next_notification().await.unwrap().unwrap(),
    ];

    assert_eq!(
        decoded[0],
        Notification::StateChanged {
            status: PlaybackStatus::Playing,
            position_ms: 1_500,
            duration_ms: Some(210_000),
            track_id: Some("track-1".to_string()),
        }
    );
    assert_eq!(
        decoded[1],
        Notification::TrackFinished {
            track_id: "track-1".to_string()
        }
    );
    assert_eq!(
        decoded[2],
        Notification::PlaybackError {
            track_id: "track-2".to_string(),
            kind: PlaybackErrorKind::Network,
        }
    );
    assert_eq!(
        decoded[3],
        Notification::PlaybackError {
            track_id: "track-3".to_string(),
            kind: PlaybackErrorKind::Drm,
        }
    );
    assert_eq!(
        decoded[4],
        Notification::TrackUnavailable {
            track_id: "track-4".to_string()
        }
    );
    assert_eq!(
        decoded[5],
        Notification::SignInResult {
            status: SignInStatus::Ok,
            music_user_token: Some("mut-xyz".to_string()),
        }
    );
    assert_eq!(
        decoded[6],
        Notification::SignInResult {
            status: SignInStatus::Cancelled,
            music_user_token: None,
        }
    );
    assert_eq!(
        decoded[7],
        Notification::SignInError {
            reason: "window failed to open".to_string()
        }
    );

    server.await.unwrap();
}

// --------------------------------------------------------------------------
// Full round-trip: request in, matching notification out
// --------------------------------------------------------------------------

/// A representative end-to-end exchange: the client issues a `load` request, the
/// stub engine parses it off the wire and replies with the `stateChanged`
/// notification a real engine would emit once playback starts. Confirms both
/// directions of the schema agree across a single connection.
#[tokio::test]
async fn request_then_matching_notification_round_trips() {
    let socket = TestSocket::new("roundtrip");
    let listener = UnixListener::bind(&socket.path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // Read the load request and echo its fields back in a stateChanged.
        let request = stub_read_frame(&mut stream).await;
        assert_eq!(request["method"], "load");
        let track_id = request["params"]["trackId"].as_str().unwrap().to_string();
        let position_ms = request["params"]["positionMs"].as_u64().unwrap();

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "stateChanged",
            "params": {
                "status": "playing",
                "positionMs": position_ms,
                "durationMs": 180_000,
                "trackId": track_id,
            }
        });
        stream
            .write_all(&stub_encode_frame(&notification))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });

    let mut client = IpcClient::connect_to(&socket.path).await.unwrap();
    client
        .sender()
        .send(&RequestMethod::Load {
            track_id: "track-42".to_string(),
            position_ms: 7_500,
            music_user_token: "mut".to_string(),
        })
        .await
        .unwrap();

    let notification = client.next_notification().await.unwrap().unwrap();
    assert_eq!(
        notification,
        Notification::StateChanged {
            status: PlaybackStatus::Playing,
            position_ms: 7_500,
            duration_ms: Some(180_000),
            track_id: Some("track-42".to_string()),
        }
    );

    server.await.unwrap();
}
