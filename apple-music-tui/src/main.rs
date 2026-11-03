//! Apple Music TUI binary entry point.
//!
//! Task 16.1 wires the end-to-end startup and shutdown flow (design
//! "Architecture", "Sign-In Sequence"). On start the TUI:
//!
//! 1. ensures the Config_File exists and parses it, printing any warnings and
//!    exiting non-zero on a parse/validation error (Requirements 8.1–8.3);
//! 2. restores the previous Session_State, logging a restore warning without
//!    disturbing the file (Requirements 9.4, 9.6);
//! 3. resolves the Keybindings from the Configuration and logs any resolution
//!    warnings, then drives the loop from the real resolved bindings
//!    (Requirements 7.4, 7.5, 7.8);
//! 4. reads the stored tokens through the Auth_Manager / Token_Store;
//! 5. spawns and connects the Electron Playback_Engine over the Unix socket,
//!    exporting `AMT_PARENT_PID` so the engine self-terminates if the TUI dies
//!    (Requirement 10.7);
//! 6. opens the Sign_In_Window when there is no Music_User_Token (Requirement
//!    1.1), or attaches the stored token otherwise (Requirements 1.2, 1.6);
//! 7. registers the MPRIS_Adapter when enabled in the config (Requirement 11);
//! 8. runs the render/input loop so the main view is visible within the startup
//!    deadline (Requirement 10.1), feeding the engine's notification stream into
//!    the mirrored Playback_State, and tears everything down cleanly on quit.
//!
//! The Playback_Engine spawn/connect is isolated behind [`start_engine`], which
//! returns an [`EngineStartup`] (an optional live connection plus the child
//! handle). This is the seam task 16.2 extends to add the preview-only fallback
//! when the engine cannot start; this task does not implement that fallback and
//! simply runs without an engine connection when the spawn/connect fails.
//!
//! Exit codes (Requirements 7.6, 7.9, 8.3, 12.11):
//! - `0` on a clean quit (quit key or SIGINT/SIGTERM/SIGHUP) with the session
//!   saved;
//! - non-zero when the Config_File fails to parse/validate (printed to stderr,
//!   file left unmodified, Requirement 8.3);
//! - non-zero when saving the session fails on quit (logged to the app log);
//! - non-zero on an unrecoverable error, after the terminal is restored and the
//!   error is logged with token values excluded.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use apple_music_tui::auth::AuthManager;
use apple_music_tui::config::{self, Configuration};
use apple_music_tui::api::{
    validate_query, CatalogApi, LibraryList, LibraryRequest, SearchPagination, SectionMore,
};
use apple_music_tui::ipc::{
    resolve_socket_path, IpcClient, IpcSender, Notification, RequestMethod, SignInStatus,
};
use apple_music_tui::keybindings::{self, Resolution};
use apple_music_tui::mpris::{self, MprisAdapter, RegistrationOutcome, SharedState};
use apple_music_tui::preview::{EngineErrorState, EngineFailure};
use apple_music_tui::session;
use apple_music_tui::token_store::{SecretString, TokenStore};
use apple_music_tui::ui::{
    self, App, FetchOutcome, FetchRequest, KeybindingActionResolver, LoopOutcome, TerminalGuard,
};
use tokio::process::{Child, Command};

/// How long to wait for the Playback_Engine to bind its socket after we spawn
/// it, before giving up and continuing without an engine connection. The engine
/// binds the socket asynchronously during Electron startup, so we poll
/// [`IpcClient::connect`] until it succeeds or this budget elapses.
const ENGINE_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Delay between connection attempts while the engine is still starting up.
const ENGINE_CONNECT_RETRY: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() -> ExitCode {
    // Install a panic hook that restores the terminal and logs the panic with
    // tokens excluded before the process unwinds, so an unrecoverable error
    // never leaves the terminal in raw mode (Requirement 12.11).
    install_panic_hook();

    // Load and validate the Configuration *before* touching the terminal. A
    // config error must print to stderr on a normal screen and exit non-zero
    // with the file left unmodified (Requirement 8.3).
    let config = match load_configuration() {
        Ok(config) => config,
        Err(code) => return code,
    };

    match run(config).await {
        Ok(LoopOutcome::Quit) => ExitCode::SUCCESS,
        // Session save failed on quit (Requirement 7.9): already logged.
        Ok(LoopOutcome::QuitSaveFailed) => ExitCode::FAILURE,
        // Startup/terminal error before or during the loop. The terminal guard
        // restored the terminal on drop; record the error with tokens excluded
        // (Requirement 12.11) and exit non-zero.
        Err(err) => {
            ui::log_unrecoverable(&err.to_string());
            ExitCode::FAILURE
        }
    }
}

/// Ensure the Config_File exists, then parse it into a [`Configuration`]
/// (Requirements 8.1, 8.2, 8.3).
///
/// A missing file is created with the default content and startup continues on
/// the defaults (Requirement 8.2); a write failure is a non-fatal warning. A
/// parse/validation error is fatal: it is printed to stderr and the process
/// exits non-zero with the file left unmodified (Requirement 8.3).
fn load_configuration() -> Result<Configuration, ExitCode> {
    // Create the default Config_File when absent (Requirement 8.2); a failure to
    // write is a warning, not fatal — we continue using parsed/defaults.
    match config::ensure_config_file() {
        Ok(Some(path)) => {
            eprintln!("created default config file at {}", path.display());
        }
        Ok(None) => {}
        Err(warning) => {
            // Requirement 8.2: warn and continue on write failure.
            eprintln!("warning: {warning}");
        }
    }

    // Read and parse the file. A missing file (e.g. the create above failed)
    // yields the default Configuration so startup can proceed.
    let text = match config::config_file_path() {
        Some(path) => std::fs::read_to_string(&path).unwrap_or_default(),
        None => String::new(),
    };

    match config::parse_config(&text) {
        Ok((configuration, warnings)) => {
            // Requirement 8.8: unknown settings are ignored with a warning.
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            Ok(configuration)
        }
        Err(err) => {
            // Requirement 8.3: name the line/setting, leave the file unmodified,
            // and exit non-zero.
            eprintln!("error: {err}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Wire the full startup flow and run the render/input loop.
///
/// The ordering mirrors the design's Architecture/Sign-In sequence: restore the
/// session, resolve keybindings, read tokens, spawn+connect the engine, open the
/// Sign_In_Window or attach the stored token, register MPRIS, then run the loop.
async fn run(config: Configuration) -> std::io::Result<LoopOutcome> {
    // 1. Restore the previous session (Requirement 9.4). A missing file yields
    //    an empty session; a corrupt/out-of-range file yields an empty session
    //    plus a warning we log without disturbing the file (Requirement 9.6).
    let restored = session::restore_session();
    if let Some(warning) = &restored.warning {
        ui::log_unrecoverable(&warning.to_string());
    }

    // 2. Resolve the Keybindings from the Configuration (Requirement 7.4) and
    //    log any conflict/unknown warnings (Requirements 7.5, 7.8). The loop is
    //    then driven by the real resolved bindings.
    let resolution: Resolution = keybindings::resolve_keybindings(&config);
    for warning in resolution.warnings() {
        ui::log_unrecoverable(&format!("keybinding warning: {warning}"));
    }
    let resolver = KeybindingActionResolver::new(resolution);

    // 3. Read the stored tokens through the Auth_Manager / Token_Store
    //    (Requirement 1.5). The store never surfaces token values, and the
    //    Auth_Manager only tells us whether a Music_User_Token is present.
    let store = TokenStore::new(config.token_store_backend);
    let auth = AuthManager::new(store);
    let has_music_user_token = auth.has_music_user_token();

    let mut app = App::new(restored.state);
    // Fetch the Developer_Token up front so the engine player can authorize DRM
    // playback (MusicKit.configure needs it). A failure is non-fatal here: it is
    // logged token-free and playback will surface a DRM/auth error until a token
    // is available; catalog/library fetches fetch their own token as needed.
    let startup_developer_token = match auth.developer_token().await {
        Ok(token) => Some(token.expose().to_string()),
        Err(err) => {
            ui::log_unrecoverable(&format!("could not obtain Developer_Token at startup: {err}"));
            None
        }
    };
    // Supply the dispatch-time configuration: the seek step (Requirement 4.6),
    // the stored Music_User_Token so transport actions can authorize engine
    // `load` requests right away for a returning user (Requirements 1.2, 1.6),
    // and the Developer_Token the engine player needs for DRM playback.
    app.configure(
        config.seek_step_seconds,
        auth.music_user_token().map(|t| t.expose().to_string()),
        startup_developer_token,
    );

    // 4. Spawn and connect the Playback_Engine. On a failure to spawn/connect
    //    (the engine binary is absent or Widevine/Chromium is unavailable) the
    //    TUI degrades to preview-only mode (Requirement 4.10, Open Question 3,
    //    task 16.2): it surfaces the start-failure error with a restart action,
    //    keeps the Queue and current track, and plays 30-second previews
    //    labelled as such. The Queue/current track are already loaded into
    //    `app` from the restored session, so entering preview mode does not
    //    disturb them.
    let mut engine = start_engine().await;
    if engine.is_none() {
        app.enter_preview_mode(EngineFailure::StartFailure);
    }
    if let Some(startup) = &engine {
        // 5. Drive the Sign-In sequence (design "Sign-In Sequence").
        if has_music_user_token {
            // Requirements 1.2, 1.6: a stored non-empty Music_User_Token is
            // attached to engine loads without opening sign-in. We do not start
            // playback here; the restored session is paused with no audio
            // (Requirement 9.4) until the user acts.
            let _ = has_music_user_token;
        } else {
            // Requirement 1.1: no Music_User_Token — open the Sign_In_Window
            // within 3 seconds of startup, passing the Developer_Token the
            // window needs to run the MusicKit sign-in flow.
            open_sign_in(&startup.sender, &auth, &config.storefront).await;
        }
    }

    // 6. Register the MPRIS_Adapter when enabled (Requirement 11). When the
    //    engine is connected its sender forwards MPRIS transport commands;
    //    otherwise MPRIS still registers but its commands are no-ops until the
    //    engine connects. The adapter is held for the life of the loop and its
    //    bus name released on shutdown (Requirement 11.7).
    let mpris_state = mpris::shared_state();
    let mpris_adapter = register_mpris(&config, engine.as_ref(), mpris_state.clone()).await;

    // Enter raw mode + alternate screen. The guard restores the terminal on
    // drop, so every exit path below leaves the terminal usable (Requirements
    // 7.6, 12.11).
    let mut guard = TerminalGuard::enter()?;

    // 7. Pump the engine's notification stream into the mirrored Playback_State
    //    in the background so the Now_Playing_Bar tracks engine events
    //    (Requirement 4.9). The task owns the read half of the connection; the
    //    loop keeps the sender for issuing requests.
    let engine_sender = engine.as_ref().map(|e| e.sender.clone());
    if let Some(startup) = engine.take() {
        spawn_notification_pump(startup.client, mpris_state.clone());
    }

    // 8. Run the render/input loop so the main view is visible within the
    //    startup deadline (Requirement 10.1). On quit/signal it saves the
    //    session, tells the engine to shut down, and restores the terminal.
    //
    //    Two task-16.2 hooks are supplied:
    //    - `restart_engine`: invoked when the user presses the restart key on
    //      the engine-error banner (Requirement 4.10). It respawns and
    //      reconnects the Playback_Engine and, on success, loads the current
    //      track at its last reported position (the engine loads it paused,
    //      Requirement 4.14), keeping the Queue and current track.
    //    - `engine_watch`: polled on each tick; reports a lost connection the
    //      moment the notification pump flags it (Requirement 4.10), so the loop
    //      degrades to preview-only mode mid-session.
    let watch_state = mpris_state.clone();
    let engine_watch = move || {
        // A blocking `try_lock` keeps the tick non-awaiting; a momentarily
        // contended lock simply defers detection to the next tick.
        match watch_state.try_lock() {
            Ok(guard) if guard.engine_lost => Some(EngineFailure::LostConnection),
            _ => None,
        }
    };

    let restart_auth = &auth;
    let restart_state = mpris_state.clone();
    let on_restart = move |error: EngineErrorState| {
        let restart_state = restart_state.clone();
        async move { restart_engine(error, restart_auth, restart_state).await }
    };

    // Poll the shared state for a Music_User_Token captured by a mid-session
    // sign-in (set by the notification pump on a successful `signInResult`) so
    // the render loop can authorize engine `load` requests after the user signs
    // in (Requirements 1.3, 1.6). A `try_lock` keeps the tick non-blocking;
    // the token is taken once so it is handed over exactly once, and persisted
    // to the Token_Store so the user stays signed in across launches
    // (Requirement 1.3). A save failure is logged (token-free) and does not
    // disturb the in-memory token used for this session (Requirement 1.15).
    let token_state = mpris_state.clone();
    let token_auth = &auth;
    let token_watch = move || {
        let token = match token_state.try_lock() {
            Ok(mut guard) => guard.captured_music_user_token.take(),
            Err(_) => None,
        }?;
        if let Err(err) = token_auth.store_music_user_token(&SecretString::new(token.clone())) {
            ui::log_unrecoverable(&format!("failed to persist Music_User_Token: {err}"));
        }
        Some(token)
    };

    // The catalog/library API client, built once over a shared connection pool
    // for the storefront in the config. The fetch closure runs search and
    // library requests on the loop's behalf, attaching the Developer_Token and
    // (for library) the Music_User_Token, and returns a view-ready outcome.
    let catalog_api = CatalogApi::new(config.storefront.clone()).ok();
    let catalog_api_ref = catalog_api.as_ref();
    let fetch_auth = &auth;
    let fetch = move |request: FetchRequest| async move {
        match catalog_api_ref {
            Some(api) => run_fetch(api, fetch_auth, request).await,
            // No HTTP client could be built: surface a benign message.
            None => match request {
                FetchRequest::Search { .. } => {
                    FetchOutcome::Search(Err("search is unavailable".to_string()))
                }
                FetchRequest::LibrarySection { .. } | FetchRequest::LibraryTracks { .. } => {
                    FetchOutcome::Library {
                        result: Err("the library could not be loaded".to_string()),
                        is_track_list: matches!(request, FetchRequest::LibraryTracks { .. }),
                    }
                }
            },
        }
    };

    let outcome = ui::run(
        &mut guard,
        &mut app,
        &resolver,
        engine_sender.as_ref(),
        on_restart,
        engine_watch,
        token_watch,
        fetch,
    )
    .await?;

    // 9. Release the MPRIS bus name on a clean exit (Requirement 11.7).
    if let Some(adapter) = mpris_adapter {
        let _ = adapter.shutdown().await;
    }

    outcome_ok(outcome)
}

/// Wrap a [`LoopOutcome`] in `Ok` for the `?`-friendly return type.
fn outcome_ok(outcome: LoopOutcome) -> std::io::Result<LoopOutcome> {
    Ok(outcome)
}

/// A live Playback_Engine connection plus the child process handle.
///
/// Holding the [`Child`] keeps the spawned engine owned by the TUI; dropping it
/// does not kill the engine (the engine self-terminates on parent loss,
/// Requirement 10.7), but keeping the handle lets a future task reap or signal
/// it. The [`IpcClient`] owns the read half (consumed by the notification pump)
/// and vends a cloneable [`IpcSender`] for issuing requests.
struct EngineStartup {
    /// The connected IPC client; its read half is moved into the notification
    /// pump and its sender is cloned for request issuing.
    client: IpcClient,
    /// A cloneable sender for issuing requests to the engine.
    sender: IpcSender,
    /// The spawned engine child process. Kept alive for the life of the TUI.
    #[allow(dead_code)]
    child: Child,
}

/// Spawn the Electron Playback_Engine and connect to its Unix socket.
///
/// Returns `Some(EngineStartup)` when the engine was spawned and the socket
/// accepted a connection within [`ENGINE_CONNECT_TIMEOUT`]; `None` when the
/// engine could not be spawned or did not come up in time. This is the seam
/// task 16.2 extends: on `None` it will fall back to preview-only playback and
/// surface the engine start-failure error with a restart action. This task
/// simply logs and continues without an engine connection.
async fn start_engine() -> Option<EngineStartup> {
    // A stale socket file can survive an unclean engine exit (the engine was
    // SIGKILL/SIGTERM'd before it could unlink the socket, or a prior `cargo
    // run` crashed). The freshly spawned engine unlinks a stale socket before
    // binding, but there is a race: the TUI may `connect()` to the leftover
    // socket file before the new engine rebinds, and connecting to a socket
    // whose listener is gone fails with ENXIO ("No such device or address",
    // os error 6). That makes the connect loop burn its whole budget against a
    // dead socket, the TUI gives up, and the engine-driven sign-in never runs —
    // so no Music_User_Token is ever captured or persisted and the user has to
    // sign in on every launch. Clear the stale socket up front so the new
    // engine always binds a fresh one and the connect loop targets it.
    remove_stale_engine_socket().await;

    let child = match spawn_engine_process() {
        Ok(child) => child,
        Err(err) => {
            // The engine binary/project may be absent in some environments
            // (e.g. tests, a minimal install). Log and continue; task 16.2 turns
            // this into the preview-only fallback.
            ui::log_unrecoverable(&format!("failed to spawn Playback_Engine: {err}"));
            return None;
        }
    };

    match connect_engine(ENGINE_CONNECT_TIMEOUT).await {
        Some(client) => {
            let sender = client.sender();
            Some(EngineStartup {
                client,
                sender,
                child,
            })
        }
        None => {
            ui::log_unrecoverable(
                "Playback_Engine did not accept a socket connection in time; continuing without it",
            );
            None
        }
    }
}

/// Remove a stale engine socket file left by an unclean engine exit.
///
/// The engine binds a Unix domain socket at [`resolve_socket_path`]. On a clean
/// shutdown it unlinks that file, but a crash or a `SIGKILL`/`SIGTERM` (for
/// example when a previous `cargo run` is interrupted) can leave the socket file
/// behind with no process listening on it. Connecting to such a file fails with
/// `ENXIO` ("No such device or address", os error 6), which is exactly the
/// failure that makes [`start_engine`] time out and drop into preview-only mode
/// — preventing the engine-driven sign-in and thus the one-time persistence of
/// the Music_User_Token.
///
/// This probes the socket: if a connection succeeds, a live engine is already
/// serving and the file is left untouched (the single-instance engine will
/// handle it). If the file exists but the probe fails, the socket is stale and
/// is unlinked so the freshly spawned engine can bind a clean one. A missing
/// file is a no-op. Any unlink error is logged (token-free) and ignored; the
/// engine's own pre-bind unlink is the backstop.
async fn remove_stale_engine_socket() {
    let path = resolve_socket_path();
    // Nothing to clean up if the file is not present.
    if !path.exists() {
        return;
    }
    // If something is actively listening, a live engine owns the socket; leave
    // it alone rather than yanking the path out from under a running engine.
    if IpcClient::connect_to(&path).await.is_ok() {
        return;
    }
    // The file exists but no listener accepted the connection: it is stale.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        // A concurrent cleanup (e.g. the engine's own pre-bind unlink) already
        // removed it; that is fine.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            ui::log_unrecoverable(&format!(
                "failed to remove stale Playback_Engine socket: {err}"
            ));
        }
    }
}

/// Launch the Electron Playback_Engine child process.
///
/// The engine lives in the sibling `playback-engine` npm project and is started
/// with `npm start` (which runs `electron .`). `AMT_PARENT_PID` is exported so
/// the engine's parent-loss watcher terminates it within 5 seconds if the TUI
/// dies (Requirement 10.7). The engine resolves the same socket path we connect
/// to from `XDG_RUNTIME_DIR` (ipc.rs / the engine's `resolveSocketPath` agree).
fn spawn_engine_process() -> std::io::Result<Child> {
    let dir = playback_engine_dir();
    Command::new("npm")
        .arg("start")
        .arg("--silent")
        .current_dir(&dir)
        // Export our PID so the engine watches the right parent (Requirement
        // 10.7); it falls back to `process.ppid` when this is absent.
        .env("AMT_PARENT_PID", std::process::id().to_string())
        // Keep the engine's stdout/stderr off our terminal; it would corrupt the
        // alternate screen. Inherit nothing by piping to null.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false)
        .spawn()
}

/// Resolve the `playback-engine` project directory relative to the running
/// binary's workspace. Falls back to a path relative to the current directory
/// so a `cargo run` from the workspace root also finds it.
fn playback_engine_dir() -> PathBuf {
    // In the workspace layout the engine is a sibling of the `apple-music-tui`
    // crate at `<workspace>/playback-engine`. `CARGO_MANIFEST_DIR` points at the
    // crate dir at build time; its parent is the workspace root.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace) = manifest_dir.parent() {
        let candidate = workspace.join("playback-engine");
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("playback-engine")
}

/// Poll [`IpcClient::connect`] until the engine's socket accepts a connection or
/// `timeout` elapses. Returns the connected client, or `None` on timeout.
async fn connect_engine(timeout: Duration) -> Option<IpcClient> {
    let deadline = Instant::now() + timeout;
    loop {
        match IpcClient::connect().await {
            Ok(client) => return Some(client),
            Err(_) if Instant::now() < deadline => {
                // Socket not bound yet; the engine is still starting up.
                tokio::time::sleep(ENGINE_CONNECT_RETRY).await;
            }
            Err(_) => return None,
        }
    }
}

/// Attempt to restart the Playback_Engine after a start-failure / lost-
/// connection error, in response to the user's restart action (Requirements
/// 4.10, 4.14, task 16.2).
///
/// On a successful respawn + reconnect this:
/// 1. clears the shared `engine_lost` flag so the loop's engine watch stops
///    reporting the stale lost connection;
/// 2. loads the current track at its last reported position (the engine loads
///    it paused, Requirement 4.14), when there is a current track and a
///    Music_User_Token to authorize it;
/// 3. spawns a fresh notification pump on the new connection so state keeps
///    flowing.
///
/// Returns `true` when the engine reconnected (the loop then leaves preview-only
/// mode and hides the error banner), `false` when the restart failed (the loop
/// stays in preview-only mode so the user can try again). The Queue and current
/// track are never touched here (Requirement 4.10): the reconnect load re-uses
/// the current track captured in `error`.
async fn restart_engine(
    error: EngineErrorState,
    auth: &AuthManager,
    state: SharedState,
) -> bool {
    // Respawn + reconnect using the same path as the initial startup.
    let startup = match start_engine().await {
        Some(startup) => startup,
        None => return false,
    };

    // The connection is live again: clear the stale lost-connection flag so the
    // engine watch does not immediately re-trigger preview mode.
    state.lock().await.engine_lost = false;

    // Requirement 4.14: load the current track at its last reported position;
    // the engine loads it paused. Only possible with a current track and a
    // stored Music_User_Token to authorize the load.
    if let Some(load) = error.reconnect_load() {
        if let Some(token) = auth.music_user_token() {
            // Fetch the Developer_Token so the reloaded track can authorize DRM
            // playback; a fetch failure sends an empty token (engine reports a
            // DRM/auth error rather than failing silently).
            let developer_token = auth
                .developer_token()
                .await
                .map(|t| t.expose().to_string())
                .unwrap_or_default();
            let request = RequestMethod::Load {
                track_id: load.track_id,
                position_ms: load.position_ms,
                music_user_token: token.expose().to_string(),
                developer_token,
            };
            if let Err(err) = startup.sender.send(&request).await {
                ui::log_unrecoverable(&format!(
                    "failed to reload current track after engine restart: {err}"
                ));
            }
        }
    }

    // Keep state flowing on the new connection. The child/sender are dropped
    // with `startup` at the end of this scope except for the pump, which owns
    // the read half; the engine self-terminates on parent loss if needed
    // (Requirement 10.7).
    spawn_notification_pump(startup.client, state);
    true
}

/// Open the Sign_In_Window over IPC, passing the Developer_Token the MusicKit
/// sign-in flow needs (Requirement 1.1, design "Sign-In Sequence").
///
/// The Developer_Token is fetched/validated by the Auth_Manager. A fetch failure
/// is logged (token-free) and sign-in is skipped; task 16.2/later wiring adds
/// the user-facing web-player error with a retry action. The token value is
/// passed straight to the engine request and never logged.
async fn open_sign_in(sender: &IpcSender, auth: &AuthManager, storefront: &str) {
    let developer_token = match auth.developer_token().await {
        Ok(token) => token,
        Err(err) => {
            // The error type is written to exclude token values by construction.
            ui::log_unrecoverable(&format!("could not obtain Developer_Token: {err}"));
            return;
        }
    };

    let request = RequestMethod::OpenSignIn {
        developer_token: developer_token.expose().to_string(),
        storefront: storefront.to_string(),
    };
    if let Err(err) = sender.send(&request).await {
        ui::log_unrecoverable(&format!("failed to open Sign_In_Window: {err}"));
    }
}

/// Register the MPRIS_Adapter when enabled in the config (Requirement 11).
///
/// Returns the live adapter when registration succeeds so the caller can release
/// the bus name on exit (Requirement 11.7). A disabled config or an unavailable
/// D-Bus yields `None`: disabled never touches the bus (Requirement 11.5), and
/// an unavailable bus is logged as a degradation warning while every non-MPRIS
/// function continues (Requirement 11.4).
async fn register_mpris(
    config: &Configuration,
    engine: Option<&EngineStartup>,
    state: SharedState,
) -> Option<MprisAdapter> {
    // The adapter forwards transport commands to the engine. When the engine is
    // not connected we still register (so media keys are visible) with a sender
    // that currently has nowhere to go; to keep the types simple we only wire a
    // real sender when the engine is present.
    let sender = engine?.sender.clone();
    match MprisAdapter::register(config.mpris_enabled, sender, state).await {
        RegistrationOutcome::Registered(adapter) => Some(adapter),
        RegistrationOutcome::Disabled => None,
        RegistrationOutcome::Unavailable(warning) => {
            // Requirement 11.4: log and continue without MPRIS.
            ui::log_unrecoverable(&warning.to_string());
            None
        }
    }
}

/// Pump the engine's notification stream into the mirrored Playback_State in the
/// background (Requirement 4.9).
///
/// This owns the read half of the IPC connection and applies each decoded
/// notification to the shared MPRIS state so the Now_Playing_Bar and any MPRIS
/// client track the engine. The task ends when the socket closes (the engine
/// exited) or a decode error occurs; a dropped connection does not crash the
/// loop.
fn spawn_notification_pump(mut client: IpcClient, state: SharedState) {
    tokio::spawn(async move {
        // Pump until the socket closes cleanly (engine exited) or a read error
        // occurs. The render loop keeps running regardless.
        while let Ok(Some(notification)) = client.next_notification().await {
            apply_notification(&state, notification).await;
        }
        // The connection is gone (socket closed or a read error). Flag the lost
        // connection so the render loop degrades to preview-only mode and
        // surfaces the engine error with a restart action (Requirement 4.10,
        // task 16.2). The Queue and current track in the mirror are untouched.
        state.lock().await.engine_lost = true;
    });
}

/// Apply one engine [`Notification`] to the shared [`mpris::MprisState`] so the
/// mirrored Playback_State reflects the engine (Requirement 4.9) and a captured
/// Music_User_Token reaches the render loop (Requirements 1.3, 1.6).
///
/// `stateChanged` updates the mirrored status/position/duration; a successful
/// `signInResult` records the captured Music_User_Token into the shared state
/// so the render loop can authorize engine `load` requests after the user signs
/// in. The richer handling of trackFinished/playbackError/trackUnavailable is
/// owned by later transport wiring.
async fn apply_notification(state: &SharedState, notification: Notification) {
    match notification {
        Notification::StateChanged {
            status,
            position_ms,
            duration_ms,
            ..
        } => {
            let mut guard = state.lock().await;
            guard.playback.status = status;
            guard.playback.position_ms = position_ms;
            if let Some(duration_ms) = duration_ms {
                guard.playback.duration_ms = Some(duration_ms);
            }
        }
        // A successful sign-in carrying a non-empty token: hand it to the render
        // loop via the shared state so post-login transport actions can
        // authorize `load` (Requirements 1.3, 1.6). The token value is stored in
        // memory only here and is never logged.
        Notification::SignInResult {
            status: SignInStatus::Ok,
            music_user_token: Some(token),
        } if !token.trim().is_empty() => {
            state.lock().await.captured_music_user_token = Some(token);
        }
        // A sign-in reported "ok" but with an empty/absent token: this is not a
        // usable sign-in, so nothing is captured and the user stays signed out.
        // Log it (token-free) so a silent "ok but no token" is diagnosable
        // rather than looking like a mysterious "sign in again" later.
        Notification::SignInResult {
            status: SignInStatus::Ok,
            ..
        } => {
            ui::log_unrecoverable(
                "sign-in reported success but carried no Music_User_Token; staying signed out",
            );
        }
        // The user dismissed the sign-in window without completing it. Recorded
        // so the library's "you must sign in again" message is explained.
        Notification::SignInResult {
            status: SignInStatus::Cancelled,
            ..
        } => {
            ui::log_unrecoverable("sign-in was cancelled; no Music_User_Token captured");
        }
        // The sign-in flow errored (window failed to open, MusicKit JS did not
        // load, or authorize() threw). The reason is already token-free.
        Notification::SignInError { reason } => {
            ui::log_unrecoverable(&format!("sign-in failed: {reason}"));
        }
        _ => {}
    }
}

/// Run a single [`FetchRequest`] against the catalog/library API, returning a
/// view-ready [`FetchOutcome`] (Requirements 2, 3).
///
/// Obtains the Developer_Token (fetching/refreshing as needed) and attaches the
/// stored Music_User_Token where required. Errors are mapped to token-free
/// user-facing messages; token values are never included.
async fn run_fetch(
    api: &CatalogApi,
    auth: &AuthManager,
    request: FetchRequest,
) -> FetchOutcome {
    // The Developer_Token is needed for every request; a fetch failure becomes a
    // user-facing message (token-free).
    let developer_token = match auth.developer_token().await {
        Ok(token) => token,
        Err(err) => {
            let message = format!("could not reach Apple Music: {err}");
            return match request {
                FetchRequest::Search { .. } => FetchOutcome::Search(Err(message)),
                FetchRequest::LibrarySection { .. } | FetchRequest::LibraryTracks { .. } => {
                    FetchOutcome::Library {
                        result: Err("the library could not be loaded".to_string()),
                        is_track_list: matches!(request, FetchRequest::LibraryTracks { .. }),
                    }
                }
            };
        }
    };
    let music_user_token = auth.music_user_token();

    match request {
        FetchRequest::Search { query } => {
            // Validate once so we can both build the request (for pagination) and
            // report an invalid-query message without sending anything.
            let valid = match validate_query(&query) {
                Ok(valid) => valid,
                Err(err) => return FetchOutcome::Search(Err(err.to_string())),
            };
            let search_request = api.request_for(&valid);
            match api
                .search(&query, &developer_token, music_user_token.as_ref())
                .await
            {
                Ok((results, _dropped)) => {
                    // The first page carries no load-more markers here; further
                    // pages are a later enhancement. Capped results still show.
                    let pagination =
                        SearchPagination::new(&search_request, results, SectionMore::default());
                    FetchOutcome::Search(Ok(pagination))
                }
                Err(err) => FetchOutcome::Search(Err(err.to_string())),
            }
        }
        FetchRequest::LibrarySection { section } => {
            fetch_library_list(api, &developer_token, music_user_token.as_ref(), LibraryRequest::section(section, 0), false).await
        }
        FetchRequest::LibraryTracks { kind, id } => {
            fetch_library_list(
                api,
                &developer_token,
                music_user_token.as_ref(),
                LibraryRequest::tracks(kind, &id, 0),
                true,
            )
            .await
        }
    }
}

/// Fetch the first page of a library list and wrap it as a [`FetchOutcome`].
///
/// A missing Music_User_Token or a rejected token both surface as a token-free
/// library error; a transport/status error does the same (Requirement 3.7/3.9).
async fn fetch_library_list(
    api: &CatalogApi,
    developer_token: &SecretString,
    music_user_token: Option<&SecretString>,
    request: LibraryRequest,
    is_track_list: bool,
) -> FetchOutcome {
    let Some(music_user_token) = music_user_token else {
        return FetchOutcome::Library {
            result: Err("you must sign in again".to_string()),
            is_track_list,
        };
    };
    match api
        .fetch_library(&request, developer_token, music_user_token)
        .await
    {
        Ok(page) => {
            let mut list = LibraryList::new(request);
            // Seed the first page through the state machine so items display.
            let _ = list.first_page_request();
            list.apply_page(page);
            FetchOutcome::Library {
                result: Ok(list),
                is_track_list,
            }
        }
        Err(err) => FetchOutcome::Library {
            result: Err(err.message()),
            is_track_list,
        },
    }
}

/// Install a panic hook that restores the terminal and logs the panic message
/// with token values excluded (Requirement 12.11).
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort terminal restore in case a guard is not in scope. The
        // `TerminalGuard`'s `Drop` also restores during unwind; doing it here as
        // well guarantees restoration even for a panic outside the guard's
        // lifetime.
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(
            stdout,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();

        // Log the panic with tokens excluded (Requirement 12.11).
        ui::log_unrecoverable(&info.to_string());

        // Preserve the default behavior (print the panic to stderr).
        default_hook(info);
    }));
}
