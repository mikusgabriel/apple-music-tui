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

const { app, session, components } = require('electron');
const path = require('path');

const { IpcServer } = require('./ipc-server');
const { PlayerWindow } = require('./player-window');
const { SignInWindow } = require('./sign-in-window');
const { ParentWatcher } = require('./parent-watcher');
const { AssetServer } = require('./asset-server');
const { installOriginHeaders } = require('./origin-headers');

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
  // Another engine instance already holds the lock. This is the expected guard
  // against racing two engines over one socket — but if that other instance is
  // a stale orphan, the parent TUI that just spawned us will never connect and
  // degrades to preview-only. Log loudly so the cause is diagnosable, then exit.
  console.error(
    '[engine] another Playback_Engine instance already holds the single-instance ' +
      'lock; exiting. If the TUI reports "did not accept a socket connection in ' +
      'time", a stale engine from a previous run may be lingering.'
  );
  app.quit();
}

// Placeholder module instances. These are wired together here so the lifecycle
// is in place; their behavior is filled in by later tasks.
let ipcServer = null;
let playerWindow = null;
let signInWindow = null;
let parentWatcher = null;
let assetServer = null;

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
    if (assetServer) {
      assetServer.stop();
    }
  } catch (err) {
    console.error('[engine] error during termination cleanup:', err && err.message ? err.message : err);
  }
  // app.exit() (not app.quit()) force-terminates the whole Electron process
  // tree, including all child processes, without waiting on window close
  // handlers that could stall past the 5-second budget.
  app.exit(0);
}

// Bind the IPC socket shortly after startup. The socket server is plain Node
// and needs nothing from Chromium, while whenReady() only resolves after full
// Electron init. The hidden player window and the https origin interceptor both
// need Electron, so the header rewriter is installed in whenReady below. The
// loopback asset server is plain Node and is started here so the sign-in URL is
// set before the IPC socket accepts requests.
async function startIpcServer() {
  playerWindow = new PlayerWindow({
    notify: (method, params) => {
      if (ipcServer) ipcServer.notify(method, params);
    },
  });
  signInWindow = new SignInWindow();
  parentWatcher = new ParentWatcher({ terminate: terminateEngine });

  ipcServer = new IpcServer({
    playerWindow,
    signInWindow,
    onShutdown: () => app.quit(),
    onClientConnected: () => parentWatcher.setIpcConnected(true),
    onClientDisconnected: () => parentWatcher.setIpcConnected(false),
  });

  // Forward the Sign_In_Window's completion/error to the TUI as the
  // `signInResult` / `signInError` notifications. The window was constructed
  // before `ipcServer` existed, so its callbacks are wired here via
  // `setCallbacks`. Without this, a captured Music_User_Token is dropped on the
  // floor (the window's default callbacks are no-ops) and the TUI never learns
  // the user signed in — so it asks the user to sign in on every launch.
  signInWindow.setCallbacks({
    onResult: (payload) => {
      if (ipcServer) ipcServer.notify('signInResult', payload);
    },
    onError: (payload) => {
      if (ipcServer) ipcServer.notify('signInError', payload);
    },
  });

  // Serve the sign-in assets over a loopback HTTP origin (the page must not run
  // on a `file://` origin, which MusicKit rejects). We do NOT hijack the https
  // scheme — that breaks Apple's authorize popup. The Developer_Token's origin
  // requirement is satisfied separately by rewriting the Origin header on Apple
  // API calls (see origin-headers.js, installed in whenReady). Start the server
  // and set the URL before binding the IPC socket so no openSignIn is handled
  // before the window knows its URL.
  assetServer = new AssetServer({ root: path.join(__dirname, 'sign-in') });
  try {
    await assetServer.start();
    signInWindow.setSignInUrl(assetServer.signInUrl());
    console.error('[engine] sign-in asset server ready at', assetServer.baseUrl());
  } catch (err) {
    console.error(
      '[engine] failed to start sign-in asset server:',
      err && err.message ? err.message : err
    );
  }

  ipcServer.start();
  parentWatcher.start();
}

// Only the instance holding the single-instance lock may bind the socket; a
// second launch must not unlink the socket the running engine is serving.
if (gotSingleInstanceLock) {
  startIpcServer();

  app.whenReady().then(async () => {
    // Rewrite the Origin/Referer headers on Apple Music API requests to
    // `https://music.apple.com` — the origin the web-player Developer_Token is
    // bound to (`root_https_origin: ["apple.com"]`). Without this, MusicKit's
    // post-authorize `GET /v1/me/storefront` (and catalog calls) are rejected
    // with 401/403, surfacing as "Storefront Country Code error." /
    // "Unauthorized". This is scoped to the API hosts, so Apple's authorize
    // popup and its postMessage handshake are untouched. Requires whenReady().
    try {
      installOriginHeaders({ session: session.defaultSession });
      console.error('[engine] Apple API origin-header rewrite installed');
    } catch (err) {
      console.error(
        '[engine] failed to install origin-header rewrite:',
        err && err.message ? err.message : err
      );
    }

    // Wait for the Widevine CDM to be installed/ready before creating the
    // player window. Apple Music streams are Widevine-DRM-encrypted; without the
    // CDM the audio element cannot decode and playback is silent even though
    // MusicKit reports "playing". castLabs ECS installs the CDM via the
    // component updater on first launch; `components.whenReady()` resolves once
    // it is available (subsequent launches resolve immediately). This is why the
    // engine depends on castLabs electron-releases rather than stock Electron.
    if (components && typeof components.whenReady === 'function') {
      try {
        await components.whenReady();
        console.error(
          '[engine] Widevine components ready:',
          typeof components.status === 'function' ? JSON.stringify(components.status()) : '(status n/a)'
        );
      } catch (err) {
        // Without Widevine, DRM playback will fail; log and continue so the
        // engine still serves sign-in and preview-mode fallbacks.
        console.error(
          '[engine] Widevine components failed to initialize:',
          err && err.message ? err.message : err
        );
      }
    } else {
      console.error(
        '[engine] WARNING: components API unavailable — this is not a Widevine-enabled Electron; DRM playback will be silent'
      );
    }

    playerWindow.create();
  });
}

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
  if (assetServer) {
    assetServer.stop();
  }
});

module.exports = { applyLaunchFlags };
