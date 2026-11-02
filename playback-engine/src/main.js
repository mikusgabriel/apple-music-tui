// Playback_Engine — Electron main process entry point.
//
// This is the disposable Chromium/Electron child process supervised by the Rust
// TUI parent. Its only responsibilities are DRM-protected playback (MusicKit JS)
// and hosting the Sign_In_Window. All durable state (Queue, Config, Session,
// tokens) lives in the Rust TUI parent; this process stays stateless about queue
// ordering and communicates over a length-prefixed JSON-RPC Unix-domain socket.
//
// Scope of this file (task 1.3): scaffolding only — apply the reduced-feature
// launch flags and wire the placeholder modules into the app lifecycle. The IPC
// protocol, MusicKit playback, and sign-in logic are implemented in later tasks
// (11.2, 12.1, 12.2).

'use strict';

const { app } = require('electron');

const { IpcServer } = require('./ipc-server');
const { PlayerWindow } = require('./player-window');
const { SignInWindow } = require('./sign-in-window');
const { ParentWatcher } = require('./parent-watcher');

// ---------------------------------------------------------------------------
// Reduced-feature launch flags (Requirement 10.3, design "Technology Stack").
//
// Electron/Chromium is the only heavy component, so it is launched with reduced
// features to stay within the combined 400 MB memory budget: GPU disabled,
// extensions disabled, a single renderer, and audio-only features. These must be
// appended before the "ready" event fires.
// ---------------------------------------------------------------------------
function applyLaunchFlags() {
  // Disable the GPU process — there is no visible rendering workload for the
  // hidden player window, so the GPU process is pure overhead.
  app.disableHardwareAcceleration();
  app.commandLine.appendSwitch('disable-gpu');
  app.commandLine.appendSwitch('disable-gpu-compositing');

  // Disable Chrome extensions — none are needed and they add memory/attack surface.
  app.commandLine.appendSwitch('disable-extensions');

  // Keep everything in a single renderer process rather than spawning a renderer
  // per window, to minimize process count and resident memory.
  app.commandLine.appendSwitch('process-per-site');
  app.commandLine.appendSwitch('renderer-process-limit', '1');

  // Audio-only posture: we never show video, so disable video-oriented features
  // and background throttling that could interrupt audio position reporting.
  app.commandLine.appendSwitch('disable-features', 'HardwareMediaKeyHandling,MediaSessionService');
  app.commandLine.appendSwitch('disable-background-timer-throttling');
  app.commandLine.appendSwitch('disable-renderer-backgrounding');
  app.commandLine.appendSwitch('autoplay-policy', 'no-user-gesture-required');
}

applyLaunchFlags();

// Enforce a single instance of the engine; a second launch should exit rather
// than racing over the same Unix socket.
const gotSingleInstanceLock = app.requestSingleInstanceLock();
if (!gotSingleInstanceLock) {
  app.quit();
}

// Placeholder module instances. These are wired together here so the lifecycle
// is in place; their behavior is filled in by later tasks.
let ipcServer = null;
let playerWindow = null;
let signInWindow = null;
let parentWatcher = null;

/**
 * Terminate the engine and all of its child processes (Requirement 10.7).
 *
 * `app.exit()` tears down the Electron process tree — the GPU/renderer/utility
 * child processes Electron spawned — and runs the `will-quit` path. We also stop
 * the IPC server and close any sign-in window explicitly in case the exit path
 * is reached before `before-quit` cleanup. Called when parent-loss is detected.
 */
function terminateEngine() {
  console.error('[engine] parent TUI lost; self-terminating (Requirement 10.7)');
  try {
    if (ipcServer) {
      ipcServer.stop();
    }
    // Close the Sign_In_Window and its child processes (design: terminate all
    // sign-in processes). We call into sign-in-window without rewriting it.
    if (signInWindow && typeof signInWindow.close === 'function') {
      signInWindow.close();
    }
  } catch (err) {
    console.error('[engine] error during termination cleanup:', err && err.message ? err.message : err);
  }
  // app.exit() (not app.quit()) force-terminates the whole Electron process
  // tree, including all child processes, without waiting on window close
  // handlers that could stall past the 5-second budget.
  app.exit(0);
}

app.whenReady().then(() => {
  // The player emits asynchronous playback state (stateChanged / trackFinished /
  // playbackError / trackUnavailable) through this callback, which the IPC
  // server (assigned below) forwards to the TUI as JSON-RPC notifications
  // (task 12.1). The closure defers reading `ipcServer` until a real event
  // fires, by which point it is set.
  playerWindow = new PlayerWindow({
    notify: (method, params) => {
      if (ipcServer) ipcServer.notify(method, params);
    },
  });
  signInWindow = new SignInWindow();

  // Parent-loss detection (task 12.3, Requirement 10.7): poll the parent PID and
  // react to IPC disconnects; self-terminate within 5 seconds of parent loss.
  parentWatcher = new ParentWatcher({
    terminate: terminateEngine,
  });

  ipcServer = new IpcServer({
    playerWindow,
    signInWindow,
    // The TUI parent owns the engine lifetime; a `shutdown` request quits the
    // app, which triggers the before-quit cleanup below.
    onShutdown: () => app.quit(),
    // Bridge the IPC connection lifecycle into parent-loss detection: a live
    // connection means the parent is present; a closed socket is one of the two
    // loss signals the watcher combines with parent-PID liveness.
    onClientConnected: () => parentWatcher.setIpcConnected(true),
    onClientDisconnected: () => parentWatcher.setIpcConnected(false),
  });

  // Start the JSON-RPC server: resolve and bind the Unix socket, accept the
  // TUI's connection, decode length-prefixed frames, and dispatch requests to
  // the player/sign-in handlers (task 11.2). The MusicKit playback bodies
  // (task 12.1) and sign-in bodies (task 12.2) fill in the handler stubs.
  ipcServer.start();

  // Create the hidden MusicKit player BrowserWindow (task 12.1).
  playerWindow.create();

  // Begin watching for parent loss once the socket is bound and the TUI can
  // connect. The watcher polls the parent PID and reacts to socket closes,
  // self-terminating the engine and its children within 5 seconds.
  parentWatcher.start();
});

// This is a headless audio engine with no dock/taskbar presence, so closing all
// windows must NOT keep the process alive the way a normal desktop app would.
// The parent TUI controls the lifetime via the `shutdown` IPC request and via
// parent-loss detection (task 12.3).
app.on('window-all-closed', () => {
  // Intentionally do not quit here during normal operation: the hidden player
  // window may be recreated. Lifetime is owned by the parent TUI.
});

app.on('before-quit', () => {
  // TODO(task 11.2 / 12.x): gracefully stop the IPC server, close windows, and
  // terminate any sign-in child processes (Requirement 10.8).
  if (parentWatcher) {
    parentWatcher.stop();
  }
  if (ipcServer) {
    ipcServer.stop();
  }
});

module.exports = { applyLaunchFlags };
