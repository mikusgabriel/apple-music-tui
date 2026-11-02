//! Integration test: MPRIS_Adapter against a session D-Bus (task 15.2).
//!
//! Exercises the real zbus-backed `MprisAdapter` (`apple_music_tui::mpris`) over
//! a live D-Bus **session** bus, driving it the way an MPRIS client (a desktop
//! environment, `playerctl`, a media key) would:
//!
//!   - Registration: when enabled, the adapter claims the
//!     `org.mpris.MediaPlayer2.apple_music_tui` well-known name and serves the
//!     `org.mpris.MediaPlayer2` + `org.mpris.MediaPlayer2.Player` interfaces at
//!     `/org/mpris/MediaPlayer2` (Requirement 11.1).
//!   - Command forwarding: `Play` / `Pause` / `PlayPause` / `Next` / `Previous`
//!     / `Seek` calls on the bus are forwarded to the Playback_Engine over the
//!     IPC socket, and the framed JSON-RPC requests a **stub engine** reads back
//!     match the equivalent keybinding action (Requirement 11.2).
//!   - Metadata / status publishing: the `Player` interface publishes track
//!     metadata (`xesam:title`/`artist`/`album`, `mpris:length`, `mpris:trackid`)
//!     and the `PlaybackStatus` string reflecting the mirrored Playback_State
//!     (Requirement 11.3).
//!   - Bus-name release: `MprisAdapter::shutdown` releases the well-known name so
//!     a fresh registration can reclaim it (Requirement 11.7).
//!
//! Environment gating: these tests need a D-Bus session bus. The harness first
//! tries to launch a **private, isolated** session bus with `dbus-daemon
//! --session` (so tests never touch the developer's real desktop bus); if that
//! daemon is not installed and no `DBUS_SESSION_BUS_ADDRESS` is already set, the
//! tests **skip gracefully** (print a reason and return) rather than fail. See
//! the printed `[mpris-it]` lines in the test output for which path ran.
//!
//! _Requirements: 11.1, 11.2, 11.3, 11.7_
//! _Design: Testing Strategy (Integration and Smoke Tests), MPRIS_Adapter_

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use apple_music_tui::domain::{PlaybackStatus, RepeatMode};
use apple_music_tui::ipc::IpcClient;
use apple_music_tui::mpris::{MprisAdapter, RegistrationOutcome, BUS_NAME, OBJECT_PATH};
use apple_music_tui::now_playing::Playback_State;
use apple_music_tui::queue::{Queue, QueueTrack};

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use zbus::zvariant::OwnedValue;

// ---------------------------------------------------------------------------
// Private session-bus harness (keeps the test isolated from the real desktop)
// ---------------------------------------------------------------------------

/// Serializes the bus-using tests. The adapter registers with zbus'
/// `Builder::session()`, which reads the **process-global**
/// `DBUS_SESSION_BUS_ADDRESS` env var; cargo runs a test crate's tests in
/// parallel threads of one process, so two tests pointing that var at their own
/// private daemons would race. Each test holds this lock for its whole body, so
/// only one owns the env var (and thus the bus) at a time. The lock is held
/// across `.await`s, which is sound because `#[tokio::test]` uses a
/// current-thread runtime (no `Send` requirement on the guard).
static BUS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A private session bus spawned for the duration of a test, plus a guard that
/// restores the ambient `DBUS_SESSION_BUS_ADDRESS` and reaps the daemon on drop.
struct PrivateBus {
    child: Child,
    previous_addr: Option<std::ffi::OsString>,
}

impl PrivateBus {
    /// Try to start a private `dbus-daemon --session` and point
    /// `DBUS_SESSION_BUS_ADDRESS` at it. Returns `None` when no daemon binary is
    /// available, so the caller can fall back or skip.
    ///
    /// The daemon prints its bus address on stdout (`--print-address`); we read
    /// that line and export it so zbus `session()` connects to *this* bus and
    /// not the developer's real one.
    fn spawn() -> Option<PrivateBus> {
        let mut child = Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--print-address=1",
                "--nopidfile",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Read the single address line the daemon prints on startup.
        let stdout = child.stdout.take()?;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        use std::io::BufRead;
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let address = line.trim().to_string();

        let previous_addr = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address);

        Some(PrivateBus {
            child,
            previous_addr,
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        match &self.previous_addr {
            Some(v) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v),
            None => std::env::remove_var("DBUS_SESSION_BUS_ADDRESS"),
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A connection to the session bus for the test to drive the adapter with.
///
/// Prefers a freshly spawned private bus (fully isolated); otherwise reuses an
/// ambient `DBUS_SESSION_BUS_ADDRESS` if one is set. Returns `None` to signal
/// the test should skip gracefully (no daemon available, no ambient bus).
///
/// `_bus` keeps the private daemon alive, and `_env_guard` keeps the global
/// `DBUS_SESSION_BUS_ADDRESS` stable, for the lifetime of the returned value.
struct BusHandle {
    _bus: Option<PrivateBus>,
    _env_guard: std::sync::MutexGuard<'static, ()>,
}

impl BusHandle {
    fn acquire(tag: &str) -> Option<BusHandle> {
        // Serialize with every other bus-using test before touching the global
        // address env var (poison is irrelevant here — the guarded data is ()).
        let env_guard = BUS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(bus) = PrivateBus::spawn() {
            println!("[mpris-it:{tag}] using a private dbus-daemon session bus");
            return Some(BusHandle {
                _bus: Some(bus),
                _env_guard: env_guard,
            });
        }
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            println!(
                "[mpris-it:{tag}] dbus-daemon unavailable; using ambient \
                 DBUS_SESSION_BUS_ADDRESS"
            );
            return Some(BusHandle {
                _bus: None,
                _env_guard: env_guard,
            });
        }
        println!(
            "[mpris-it:{tag}] SKIP: no D-Bus session bus available \
             (dbus-daemon not found and DBUS_SESSION_BUS_ADDRESS unset)"
        );
        None
    }
}

// ---------------------------------------------------------------------------
// Stub Playback_Engine over a Unix socket (captures forwarded commands)
// ---------------------------------------------------------------------------

/// A unique, self-cleaning socket path for one test.
struct TestSocket {
    dir: PathBuf,
    path: PathBuf,
}

impl TestSocket {
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "amt-mpris-it-{}-{tag}-{stamp}",
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

/// A thread-safe accumulator of the framed requests a stub engine has read.
type Recorded = std::sync::Arc<tokio::sync::Mutex<Vec<Value>>>;

/// Spawn a stub engine that accepts one connection and pushes every framed
/// request it receives into `recorded` as it arrives. The task runs until the
/// connection closes; the test reads forwarded requests from `recorded` by
/// polling, so it never has to wait for the socket to reach EOF.
fn spawn_stub_engine(path: PathBuf, recorded: Recorded) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        loop {
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).await.is_err() {
                break; // connection closed
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            if stream.read_exact(&mut payload).await.is_err() {
                break;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&payload) {
                recorded.lock().await.push(value);
            }
        }
    })
}

/// Poll `recorded` until at least `want` requests have arrived or a short
/// deadline elapses, then return a snapshot. Command forwarding is asynchronous
/// (bus call -> adapter resolves -> IPC writes), so polling avoids both flakes
/// and an arbitrary fixed sleep.
async fn wait_for_requests(recorded: &Recorded, want: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        {
            let guard = recorded.lock().await;
            if guard.len() >= want {
                return guard.clone();
            }
        }
        if std::time::Instant::now() >= deadline {
            return recorded.lock().await.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

fn track(id: &str, title: &str, artist: &str, duration_ms: Option<u64>) -> QueueTrack {
    QueueTrack {
        track_id: id.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        duration_ms,
    }
}

/// A registered adapter together with the handles that keep its IPC transport
/// and recorded-request accumulator alive for the test's duration.
struct Harness {
    adapter: MprisAdapter,
    /// Requests the stub engine has read, in arrival order.
    recorded: Recorded,
    /// Kept alive so the adapter's IPC socket stays connected. Dropped at the
    /// end of each test with the rest of the harness.
    _client: IpcClient,
    _engine: tokio::task::JoinHandle<()>,
    _socket: TestSocket,
}

/// Build a registered adapter plus a stub engine recording forwarded requests,
/// or `None` if the bus could not be acquired (caller then skips).
///
/// The stub engine must already be listening before the adapter's IPC client
/// connects, so this wires the socket first, then registers on the bus. The
/// returned `IpcClient` is held (not dropped) so the socket the adapter writes
/// to stays open until the test finishes.
async fn register_adapter(tag: &str, bus: &BusHandle) -> Option<Harness> {
    let _ = bus; // Acquired by the caller; kept alive for the adapter's lifetime.
    let socket = TestSocket::new(tag);
    let recorded: Recorded = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let engine = spawn_stub_engine(socket.path.clone(), recorded.clone());

    let client = IpcClient::connect_to(&socket.path).await.unwrap();
    let sender = client.sender();

    let state = apple_music_tui::mpris::shared_state();
    match MprisAdapter::register(true, sender, state).await {
        RegistrationOutcome::Registered(adapter) => Some(Harness {
            adapter,
            recorded,
            _client: client,
            _engine: engine,
            _socket: socket,
        }),
        RegistrationOutcome::Unavailable(w) => {
            println!("[mpris-it:{tag}] registration unavailable: {w}");
            None
        }
        RegistrationOutcome::Disabled => {
            panic!("adapter registered with enabled=true should not be Disabled")
        }
    }
}

/// Set the adapter's mirrored Playback_State + Queue the way the render loop
/// would, then publish, so property gets over the bus observe the new state.
async fn set_state(adapter: &MprisAdapter, queue: Queue, playback: Playback_State) {
    {
        let shared = adapter.state();
        let mut guard = shared.lock().await;
        guard.queue = queue;
        guard.playback = playback;
    }
    adapter.publish_state().await.unwrap();
}

/// A minimal zbus proxy for the standard `org.mpris.MediaPlayer2.Player`
/// interface, used by the test as an MPRIS client would be.
#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait Player {
    fn play(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn seek(&self, offset: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

/// A minimal proxy for the root `org.mpris.MediaPlayer2` interface.
#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2 {
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
}

async fn player_proxy(conn: &zbus::Connection) -> PlayerProxy<'static> {
    PlayerProxy::builder(conn)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// 11.1 — Registration on the session bus
// ---------------------------------------------------------------------------

/// With MPRIS enabled the adapter claims the well-known name and serves both
/// MPRIS interfaces at the spec object path; a client can resolve the name and
/// read a known property off each interface (Requirement 11.1).
#[tokio::test]
async fn registers_well_known_name_and_serves_interfaces() {
    let tag = "register";
    let Some(bus) = BusHandle::acquire(tag) else {
        return; // graceful skip (reason already printed)
    };
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };

    let conn = zbus::Connection::session().await.unwrap();

    // The well-known name is owned on the bus.
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    let has_owner = dbus
        .name_has_owner(BUS_NAME.try_into().unwrap())
        .await
        .unwrap();
    assert!(has_owner, "the MPRIS well-known name should be owned");

    // The root interface answers Identity (Requirement 11.1).
    let root = MediaPlayer2Proxy::builder(&conn)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(root.identity().await.unwrap(), "Apple Music TUI");

    // The Player interface answers PlaybackStatus (defaults to Stopped).
    let player = player_proxy(&conn).await;
    assert_eq!(player.playback_status().await.unwrap(), "Stopped");

    harness.adapter.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 11.2 — Command forwarding to the Playback_Engine
// ---------------------------------------------------------------------------

/// `Play` / `Pause` / `PlayPause` on the bus forward the matching engine request
/// the equivalent keybinding would (Requirement 11.2). With a single track
/// playing: Play->play, Pause->pause, PlayPause(while playing)->pause.
#[tokio::test]
async fn forwards_play_pause_playpause_to_engine() {
    let tag = "transport";
    let Some(bus) = BusHandle::acquire(tag) else {
        return;
    };
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };

    // One track, currently playing.
    let mut queue = Queue::new();
    queue.tracks.push(track("t1", "Song", "Artist", Some(180_000)));
    queue.current = Some(0);
    let playback = Playback_State {
        track_id: Some("t1".to_string()),
        title: "Song".to_string(),
        artist: "Artist".to_string(),
        duration_ms: Some(180_000),
        status: PlaybackStatus::Playing,
        ..Default::default()
    };
    set_state(&harness.adapter, queue, playback).await;

    let conn = zbus::Connection::session().await.unwrap();
    let player = player_proxy(&conn).await;

    player.play().await.unwrap();
    player.pause().await.unwrap();
    player.play_pause().await.unwrap(); // playing -> pause

    let requests = wait_for_requests(&harness.recorded, 3).await;
    let methods: Vec<&str> = requests
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        vec!["play", "pause", "pause"],
        "Play/Pause/PlayPause(playing) should forward play,pause,pause: {methods:?}"
    );

    harness.adapter.shutdown().await.unwrap();
}

/// `Next` / `Previous` forward the Queue transition the TUI would compute: a
/// `load` of the resolved track at position 0 followed by a `play` (Requirement
/// 11.2). Starting on track 0 of three, Next advances to track 1.
#[tokio::test]
async fn forwards_next_as_load_then_play() {
    let tag = "next";
    let Some(bus) = BusHandle::acquire(tag) else {
        return;
    };
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };

    let mut queue = Queue::new();
    queue.tracks.push(track("t1", "One", "A", Some(180_000)));
    queue.tracks.push(track("t2", "Two", "B", Some(200_000)));
    queue.tracks.push(track("t3", "Three", "C", Some(220_000)));
    queue.current = Some(0);
    let playback = Playback_State {
        track_id: Some("t1".to_string()),
        duration_ms: Some(180_000),
        position_ms: 10_000,
        status: PlaybackStatus::Playing,
        repeat: RepeatMode::Off,
        ..Default::default()
    };
    set_state(&harness.adapter, queue, playback).await;

    let conn = zbus::Connection::session().await.unwrap();
    let player = player_proxy(&conn).await;
    player.next().await.unwrap();

    let requests = wait_for_requests(&harness.recorded, 2).await;
    assert_eq!(requests.len(), 2, "Next should forward load then play: {requests:?}");
    assert_eq!(requests[0]["method"], "load");
    assert_eq!(requests[0]["params"]["trackId"], "t2");
    assert_eq!(requests[0]["params"]["positionMs"], 0);
    assert_eq!(requests[1]["method"], "play");

    harness.adapter.shutdown().await.unwrap();
}

/// A within-bounds `Seek` forwards a `seek` request carrying the absolute target
/// position in milliseconds the adapter resolved (Requirement 11.2). From 30s,
/// a +10s relative seek (10_000_000 µs) forwards seek{positionMs: 40_000}.
#[tokio::test]
async fn forwards_seek_with_resolved_position() {
    let tag = "seek";
    let Some(bus) = BusHandle::acquire(tag) else {
        return;
    };
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };

    let mut queue = Queue::new();
    queue.tracks.push(track("t1", "Song", "Artist", Some(180_000)));
    queue.current = Some(0);
    let playback = Playback_State {
        track_id: Some("t1".to_string()),
        duration_ms: Some(180_000),
        position_ms: 30_000,
        status: PlaybackStatus::Playing,
        ..Default::default()
    };
    set_state(&harness.adapter, queue, playback).await;

    let conn = zbus::Connection::session().await.unwrap();
    let player = player_proxy(&conn).await;
    player.seek(10_000_000).await.unwrap(); // +10s

    let requests = wait_for_requests(&harness.recorded, 1).await;
    assert_eq!(requests.len(), 1, "Seek should forward one seek: {requests:?}");
    assert_eq!(requests[0]["method"], "seek");
    assert_eq!(requests[0]["params"]["positionMs"], 40_000);

    harness.adapter.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 11.3 — Metadata and status publishing
// ---------------------------------------------------------------------------

/// The Player interface publishes the mirrored Playback_State as MPRIS metadata
/// and a status string a client reads off the bus (Requirement 11.3): title,
/// artist (array), album, length in microseconds, a valid trackid path, and the
/// `Playing` status.
#[tokio::test]
async fn publishes_metadata_and_status() {
    let tag = "metadata";
    let Some(bus) = BusHandle::acquire(tag) else {
        return;
    };
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };

    let mut queue = Queue::new();
    queue.tracks.push(track("abc", "Song", "Artist", Some(180_000)));
    queue.current = Some(0);
    let playback = Playback_State {
        track_id: Some("abc".to_string()),
        title: "Song".to_string(),
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        duration_ms: Some(180_000),
        status: PlaybackStatus::Playing,
        ..Default::default()
    };
    set_state(&harness.adapter, queue, playback).await;

    let conn = zbus::Connection::session().await.unwrap();
    let player = player_proxy(&conn).await;

    assert_eq!(player.playback_status().await.unwrap(), "Playing");

    let metadata = player.metadata().await.unwrap();

    // Owned values convert by value, so clone each entry out of the map before
    // converting to the concrete Rust type the client expects.
    let owned = |key: &str| -> OwnedValue {
        metadata
            .get(key)
            .unwrap_or_else(|| panic!("metadata must carry {key}"))
            .try_clone()
            .unwrap()
    };

    // mpris:trackid is a valid object path ending in the escaped id.
    let track_id_path: zbus::zvariant::ObjectPath = owned("mpris:trackid").try_into().unwrap();
    assert!(
        track_id_path.as_str().ends_with("/track/abc"),
        "trackid: {}",
        track_id_path.as_str()
    );

    let title: String = owned("xesam:title").try_into().unwrap();
    assert_eq!(title, "Song");

    let artists: Vec<String> = owned("xesam:artist").try_into().unwrap();
    assert_eq!(artists, vec!["Artist".to_string()]);

    let album: String = owned("xesam:album").try_into().unwrap();
    assert_eq!(album, "Album");

    let length: i64 = owned("mpris:length").try_into().unwrap();
    assert_eq!(length, 180_000_000, "mpris:length is microseconds");

    harness.adapter.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// 11.7 — Bus-name release on exit
// ---------------------------------------------------------------------------

/// `shutdown` releases the well-known name so a second adapter can reclaim it on
/// the same bus (Requirement 11.7). After shutdown the name has no owner; a
/// fresh registration then succeeds and owns it again.
#[tokio::test]
async fn releases_bus_name_on_shutdown() {
    let tag = "release";
    let Some(bus) = BusHandle::acquire(tag) else {
        return;
    };

    let conn = zbus::Connection::session().await.unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();

    // First registration owns the name.
    let Some(harness) = register_adapter(tag, &bus).await else {
        return;
    };
    assert!(
        dbus.name_has_owner(BUS_NAME.try_into().unwrap())
            .await
            .unwrap(),
        "name should be owned after first registration"
    );

    // Shutdown releases it. Destructure first so the adapter can be consumed by
    // `shutdown` while the IPC client / engine / socket are dropped together.
    let Harness {
        adapter,
        _client,
        _engine,
        _socket,
        ..
    } = harness;
    adapter.shutdown().await.unwrap();
    drop((_client, _engine, _socket));

    // The release propagates asynchronously on the bus; poll briefly.
    let mut owned_after = true;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        owned_after = dbus
            .name_has_owner(BUS_NAME.try_into().unwrap())
            .await
            .unwrap();
        if !owned_after {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!owned_after, "shutdown should release the well-known name");

    // A fresh registration reclaims the freed name, proving it was released.
    let Some(harness2) = register_adapter(tag, &bus).await else {
        panic!("a second registration should succeed on the freed name");
    };
    assert!(
        dbus.name_has_owner(BUS_NAME.try_into().unwrap())
            .await
            .unwrap(),
        "name should be owned again after re-registration"
    );

    harness2.adapter.shutdown().await.unwrap();
}
