// Sign_In_Window — a visible BrowserWindow that hosts the MusicKit sign-in web
// flow and captures the Music_User_Token.
//
// Per design "Sign-In Sequence" and "Playback_Engine": opened on `openSignIn`
// (carrying the Developer_Token), it loads an Apple-hosted MusicKit sign-in page,
// runs the MusicKit JS authorize flow, captures a non-empty Music_User_Token, and
// reports completion to the TUI via the IPC `signInResult` / `signInError`
// notifications:
//
//   signInResult { status: "ok", musicUserToken }   — a non-empty token captured
//   signInResult { status: "cancelled" }             — the user closed/cancelled
//   signInError  { reason }                           — the window failed to open,
//                                                        or the flow errored
//
// When the window closes, every process started for the sign-in is terminated
// (the BrowserWindow is destroyed, which tears down its renderer), within 5
// seconds, while keeping an already-obtained token (Requirements 1.3, 1.4, 1.14,
// 10.8).
//
// This module keeps hardened webPreferences (contextIsolation on, nodeIntegration
// off, sandbox on, a narrow preload bridge). The Developer_Token and the captured
// Music_User_Token are passed over the preload IPC bridge and are never logged
// (Requirement 1.13).

'use strict';

const path = require('path');

// Lazily required so the module can be unit-tested without a running Electron
// main process. Tests inject a fake `electron` dependency instead.
function defaultElectron() {
  // eslint-disable-next-line global-require
  return require('electron');
}

// The renderer document and preload script that drive the MusicKit JS sign-in.
const SIGN_IN_HTML = path.join(__dirname, 'sign-in', 'sign-in.html');
const SIGN_IN_PRELOAD = path.join(__dirname, 'sign-in', 'sign-in-preload.js');

// IPC channel names used between the sign-in renderer and this main-process host.
// Kept in one place so the preload bridge and this host stay in sync.
const CHANNEL_READY = 'signin:ready'; // renderer -> main: ask for the developer token
const CHANNEL_TOKEN = 'signin:token'; // renderer -> main: a captured Music_User_Token
const CHANNEL_CANCELLED = 'signin:cancelled'; // renderer -> main: user cancelled in-page
const CHANNEL_ERROR = 'signin:error'; // renderer -> main: the flow errored
const CHANNEL_CONFIG = 'signin:config'; // main -> renderer: deliver the developer token

/**
 * How long, after the window is asked to open, we wait for it to actually become
 * ready to show before declaring an open failure (Requirement 1.14: 10 seconds).
 */
const OPEN_TIMEOUT_MS = 10_000;

/**
 * Normalize a captured token: a Music_User_Token counts only when it is a
 * non-empty string after trimming (Requirements 1.3 / design "Sign-In Sequence").
 *
 * @param {unknown} token
 * @returns {string|null} the trimmed token, or null when empty/absent/non-string
 */
function normalizeToken(token) {
  if (typeof token !== 'string') {
    return null;
  }
  const trimmed = token.trim();
  return trimmed.length > 0 ? trimmed : null;
}

class SignInWindow {
  /**
   * @param {{
   *   electron?: object,
   *   onResult?: (payload: {status: 'ok'|'cancelled', musicUserToken?: string}) => void,
   *   onError?: (payload: {reason: string}) => void,
   *   openTimeoutMs?: number,
   * }} [deps]
   *
   * `onResult` / `onError` are the callbacks the IPC server forwards as the
   * `signInResult` / `signInError` notifications. They default to no-ops so the
   * module is safe to construct before wiring (e.g. the main.js scaffold).
   */
  constructor(deps = {}) {
    this._electron = deps.electron || null;
    this._onResult = typeof deps.onResult === 'function' ? deps.onResult : () => {};
    this._onError = typeof deps.onError === 'function' ? deps.onError : () => {};
    this._openTimeoutMs =
      typeof deps.openTimeoutMs === 'number' ? deps.openTimeoutMs : OPEN_TIMEOUT_MS;

    /** @type {import('electron').BrowserWindow|null} */
    this.window = null;
    /** The Developer_Token for the in-flight sign-in (never logged). */
    this._developerToken = null;
    /** A non-empty Music_User_Token captured during the current flow, if any. */
    this._capturedToken = null;
    /** Guards against reporting a result/error more than once per flow. */
    this._settled = false;
    /** Timer that fires if the window never becomes ready (Requirement 1.14). */
    this._openTimer = null;
    /** The per-flow IPC handlers, so they can be detached on close. */
    this._ipcHandlers = null;
  }

  /**
   * Set (or replace) the result/error callbacks. Allows a host (main.js) that
   * constructed the window before the IPC server existed to wire the forwarding
   * after the fact.
   *
   * @param {{onResult?: Function, onError?: Function}} callbacks
   */
  setCallbacks(callbacks = {}) {
    if (typeof callbacks.onResult === 'function') {
      this._onResult = callbacks.onResult;
    }
    if (typeof callbacks.onError === 'function') {
      this._onError = callbacks.onError;
    }
  }

  /**
   * Open the visible sign-in web view and run the MusicKit sign-in flow.
   *
   * Creates a visible, hardened BrowserWindow, loads the MusicKit sign-in page
   * with the Developer_Token, and observes for the Music_User_Token. Completion
   * is reported asynchronously via the result/error callbacks. If a sign-in is
   * already in progress, the existing window is focused instead of opening a
   * second one.
   *
   * @param {string} developerToken the Developer_Token used to configure MusicKit
   */
  open(developerToken) {
    // A sign-in is already in flight: focus the existing window rather than
    // racing two web views for the same token.
    if (this.window) {
      try {
        this.window.focus();
      } catch (_err) {
        // A window mid-teardown may reject focus(); nothing actionable here.
      }
      return;
    }

    this._developerToken = typeof developerToken === 'string' ? developerToken : '';
    this._capturedToken = null;
    this._settled = false;

    const electron = this._electron || defaultElectron();
    const { BrowserWindow, ipcMain } = electron;

    let win;
    try {
      win = new BrowserWindow({
        width: 480,
        height: 720,
        show: false, // shown once ready-to-show, so no white flash
        title: 'Sign in to Apple Music',
        autoHideMenuBar: true,
        webPreferences: {
          // Hardened defaults: the Apple-hosted page runs with no Node access and
          // only the narrow preload bridge below.
          contextIsolation: true,
          nodeIntegration: false,
          sandbox: true,
          preload: SIGN_IN_PRELOAD,
        },
      });
    } catch (err) {
      // Construction can fail when no display is available or the web-view
      // runtime is missing (Requirement 1.14).
      this._reportError(this._describeError(err) || 'window creation failed');
      return;
    }

    this.window = win;
    this._attachIpc(ipcMain, win);
    this._attachWindowEvents(win);

    // Open-timeout guard: if the window never becomes ready-to-show within the
    // deadline, report an open failure and tear down (Requirement 1.14).
    this._openTimer = setTimeout(() => {
      if (!this._settled) {
        this._reportError('sign-in window did not open within 10 seconds');
        this._teardown();
      }
    }, this._openTimeoutMs);
    // Do not let the timer keep the event loop alive on its own.
    if (this._openTimer && typeof this._openTimer.unref === 'function') {
      this._openTimer.unref();
    }

    win.once('ready-to-show', () => {
      this._clearOpenTimer();
      if (!win.isDestroyed()) {
        win.show();
      }
    });

    win.webContents.on('did-fail-load', (_event, errorCode, errorDescription) => {
      // A failed navigation to the sign-in page is an open/flow error; ignore
      // benign aborts (-3) that Chromium emits for superseded navigations.
      if (errorCode === -3) {
        return;
      }
      this._reportError(
        errorDescription ? `failed to load sign-in page: ${errorDescription}` : 'failed to load sign-in page'
      );
      this._teardown();
    });

    const loadResult = win.loadFile(SIGN_IN_HTML);
    // loadFile returns a promise in modern Electron; surface a rejection as an
    // open error rather than an unhandled rejection.
    if (loadResult && typeof loadResult.catch === 'function') {
      loadResult.catch((err) => {
        this._reportError(this._describeError(err) || 'failed to load sign-in page');
        this._teardown();
      });
    }
  }

  /**
   * Close the sign-in web view.
   *
   * Terminates every process started for the sign-in by destroying the
   * BrowserWindow (which tears down its renderer process) and detaching the IPC
   * handlers, while keeping an already-obtained Music_User_Token. Reports a
   * cancellation only if no token was captured and the flow has not already
   * settled (Requirement 1.4). Idempotent.
   */
  close() {
    if (!this.window && this._settled) {
      return;
    }
    // If the flow never produced a token, a close is a cancellation.
    if (!this._settled && !this._capturedToken) {
      this._reportResult({ status: 'cancelled' });
    }
    this._teardown();
  }

  // -------------------------------------------------------------------------
  // Internal wiring
  // -------------------------------------------------------------------------

  /**
   * Wire the per-flow IPC channels between the sign-in renderer and this host.
   *
   * @param {import('electron').IpcMain} ipcMain
   * @param {import('electron').BrowserWindow} win
   */
  _attachIpc(ipcMain, win) {
    const onReady = (event) => {
      // Deliver the Developer_Token to the renderer only for this window's
      // web contents, so an unrelated frame cannot phish it.
      if (!this._isFromWindow(event, win)) {
        return;
      }
      this._sendConfig(win);
    };

    const onToken = (event, token) => {
      if (!this._isFromWindow(event, win)) {
        return;
      }
      const normalized = normalizeToken(token);
      if (!normalized) {
        // An empty token is not a successful sign-in; keep waiting for a real one.
        return;
      }
      this._capturedToken = normalized;
      this._reportResult({ status: 'ok', musicUserToken: normalized });
      this._teardown();
    };

    const onCancelled = (event) => {
      if (!this._isFromWindow(event, win)) {
        return;
      }
      if (!this._capturedToken) {
        this._reportResult({ status: 'cancelled' });
      }
      this._teardown();
    };

    const onError = (event, reason) => {
      if (!this._isFromWindow(event, win)) {
        return;
      }
      this._reportError(typeof reason === 'string' && reason ? reason : 'sign-in failed');
      this._teardown();
    };

    ipcMain.on(CHANNEL_READY, onReady);
    ipcMain.on(CHANNEL_TOKEN, onToken);
    ipcMain.on(CHANNEL_CANCELLED, onCancelled);
    ipcMain.on(CHANNEL_ERROR, onError);

    this._ipcHandlers = { ipcMain, onReady, onToken, onCancelled, onError };
  }

  /**
   * Deliver the Developer_Token to the renderer over the preload bridge.
   *
   * @param {import('electron').BrowserWindow} win
   */
  _sendConfig(win) {
    if (win.isDestroyed()) {
      return;
    }
    win.webContents.send(CHANNEL_CONFIG, { developerToken: this._developerToken });
  }

  /**
   * React to the user closing the window via the window chrome.
   *
   * @param {import('electron').BrowserWindow} win
   */
  _attachWindowEvents(win) {
    win.on('closed', () => {
      // The OS/user closed the window. If nothing was captured and the flow is
      // unsettled, that is a cancellation (Requirement 1.4). Clear our reference
      // first so _teardown does not try to close an already-closed window.
      this.window = null;
      if (!this._settled && !this._capturedToken) {
        this._reportResult({ status: 'cancelled' });
      }
      this._teardown();
    });
  }

  /**
   * Only accept IPC from this sign-in window's own web contents.
   *
   * @param {{sender?: object}} event
   * @param {import('electron').BrowserWindow} win
   * @returns {boolean}
   */
  _isFromWindow(event, win) {
    if (!event || !event.sender || !win || win.isDestroyed()) {
      return false;
    }
    return event.sender === win.webContents;
  }

  /**
   * Fire the result callback exactly once per flow.
   *
   * @param {{status: 'ok'|'cancelled', musicUserToken?: string}} payload
   */
  _reportResult(payload) {
    if (this._settled) {
      return;
    }
    this._settled = true;
    this._onResult(payload);
  }

  /**
   * Fire the error callback exactly once per flow.
   *
   * @param {string} reason
   */
  _reportError(reason) {
    if (this._settled) {
      return;
    }
    this._settled = true;
    this._onError({ reason });
  }

  /**
   * Terminate all sign-in processes: destroy the window (tearing down its
   * renderer), detach IPC handlers, and clear the open timer — while keeping any
   * captured token in the reported result (Requirement 10.8). Idempotent.
   */
  _teardown() {
    this._clearOpenTimer();
    this._detachIpc();

    const win = this.window;
    this.window = null;
    if (win && !win.isDestroyed()) {
      try {
        win.destroy();
      } catch (_err) {
        // The window may already be mid-destroy; nothing more to do.
      }
    }
    // Drop the in-flight Developer_Token reference so it is not retained.
    this._developerToken = null;
  }

  _clearOpenTimer() {
    if (this._openTimer) {
      clearTimeout(this._openTimer);
      this._openTimer = null;
    }
  }

  _detachIpc() {
    if (!this._ipcHandlers) {
      return;
    }
    const { ipcMain, onReady, onToken, onCancelled, onError } = this._ipcHandlers;
    ipcMain.removeListener(CHANNEL_READY, onReady);
    ipcMain.removeListener(CHANNEL_TOKEN, onToken);
    ipcMain.removeListener(CHANNEL_CANCELLED, onCancelled);
    ipcMain.removeListener(CHANNEL_ERROR, onError);
    this._ipcHandlers = null;
  }

  /**
   * Extract a safe, token-free message from an error.
   *
   * @param {unknown} err
   * @returns {string}
   */
  _describeError(err) {
    if (!err) {
      return '';
    }
    if (typeof err === 'string') {
      return err;
    }
    return err && err.message ? String(err.message) : String(err);
  }
}

module.exports = {
  SignInWindow,
  normalizeToken,
  CHANNEL_READY,
  CHANNEL_TOKEN,
  CHANNEL_CANCELLED,
  CHANNEL_ERROR,
  CHANNEL_CONFIG,
  SIGN_IN_HTML,
  SIGN_IN_PRELOAD,
  OPEN_TIMEOUT_MS,
};
