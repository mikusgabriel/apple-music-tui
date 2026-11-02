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

app.whenReady().then(() => {
  playerWindow = new PlayerWindow();
  signInWindow = new SignInWindow();
  ipcServer = new IpcServer({ playerWindow, signInWindow });

  // TODO(task 11.2): start the JSON-RPC server, accept the TUI's socket
  // connection, decode framed requests, and dispatch to the player/sign-in
  // handlers.
  ipcServer.start();

  // TODO(task 12.1): create the hidden MusicKit player BrowserWindow.
  playerWindow.create();

  // TODO(task 12.3): detect loss of the parent TUI process and self-terminate
  // (the engine and all its child processes) within 5 seconds (Requirement 10.7).
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
  if (ipcServer) {
    ipcServer.stop();
  }
});

module.exports = { applyLaunchFlags };
