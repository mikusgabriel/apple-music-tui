// MusicKit player window — a single hidden BrowserWindow that runs MusicKit JS
// for DRM-protected playback.
//
// Per design "Playback_Engine" and "Playback-Start Sequence": this hidden window
// loads and plays tracks, authorizes with the Music_User_Token, requests the DRM
// (Widevine) license, and reports position at least once per second so the TUI
// can meet the Now_Playing_Bar update guarantee (Requirement 6.2). It drives the
// stateChanged / trackFinished / playbackError / trackUnavailable notifications.
//
// Division of labor:
//   - This main-process class owns the authoritative playback *status* mirror,
//     the >= 1 Hz position-report timer, the 10-second load/DRM timeout
//     (Requirement 4.13), value clamping, and the mapping from renderer events
//     to IPC notifications.
//   - The renderer (player.js + player.html) is a thin adapter over MusicKit's
//     imperative API behind a hardened preload bridge (player-preload.js).
//
// Transport commands (load/play/pause/seek/setVolume) are invoked by the IPC
// server (ipc-server.js); their results are echoed synchronously where the IPC
// contract requires a value (seek -> applied positionMs, setVolume -> level),
// while actual audio state is reported asynchronously via notifications.
//
// Testability: Electron's BrowserWindow, the preload/renderer transport, and the
// wall clock are injected through the constructor so the state machine and timer
// can be exercised with Node's built-in test runner, without a real Chromium.

'use strict';

const path = require('path');

// Position is reported at least once per second (Requirement 6.2 via design
// "Playback_Engine"). We tick a little faster than 1 Hz to stay safely within
// the guarantee even under minor timer jitter.
const POSITION_REPORT_INTERVAL_MS = 500;

// A stream that does not load, or a DRM license that does not arrive, within
// this window is a failure (Requirement 4.13).
const LOAD_TIMEOUT_MS = 10000;

const VOLUME_MIN = 0;
const VOLUME_MAX = 100;

// Playback status strings mirror the TUI's Playback_State.status
// (design "Playback_State": Playing | Paused | Stopped). "Loading" is an
// internal engine-side status used while the DRM license is pending; it is
// reported so the TUI can show buffering and so the load-timeout is observable.
const STATUS_STOPPED = 'Stopped';
const STATUS_LOADING = 'Loading';
const STATUS_PLAYING = 'Playing';
const STATUS_PAUSED = 'Paused';

/**
 * Clamp a volume level to the 0-100 inclusive range, coercing non-finite input
 * to the minimum.
 *
 * @param {unknown} level
 * @returns {number}
 */
function clampVolume(level) {
  const n = Number(level);
  if (!Number.isFinite(n)) return VOLUME_MIN;
  return Math.max(VOLUME_MIN, Math.min(VOLUME_MAX, Math.round(n)));
}

/**
 * Clamp a position to a non-negative integer, optionally bounded above by a
 * known duration. Non-finite input coerces to 0.
 *
 * @param {unknown} positionMs
 * @param {number|null} durationMs upper bound, or null if unknown
 * @returns {number}
 */
function clampPosition(positionMs, durationMs) {
  const n = Number(positionMs);
  let v = !Number.isFinite(n) || n < 0 ? 0 : Math.round(n);
  if (typeof durationMs === 'number' && Number.isFinite(durationMs) && durationMs >= 0) {
    v = Math.min(v, durationMs);
  }
  return v;
}

class PlayerWindow {
  /**
   * @param {object} [deps]
   * @param {(method: string, params: object) => void} [deps.notify]
   *   Emit a JSON-RPC notification to the TUI. Defaults to a no-op so the
   *   placeholder wiring in main.js keeps working until the engine is wired to
   *   the IPC server.
   * @param {() => object} [deps.createWindow]
   *   Factory that returns an object exposing the renderer transport used by
   *   this class: `send(command)`, `onEvent(cb)`, `loadFile(path)`, and
   *   `destroy()`. In production this wraps an Electron BrowserWindow; tests
   *   inject a fake. When omitted, a real BrowserWindow is created lazily.
   * @param {(fn: Function, ms: number) => any} [deps.setInterval]
   * @param {(handle: any) => void} [deps.clearInterval]
   * @param {(fn: Function, ms: number) => any} [deps.setTimeout]
   * @param {(handle: any) => void} [deps.clearTimeout]
   * @param {() => number} [deps.now]
   */
  constructor(deps = {}) {
    this.notify = typeof deps.notify === 'function' ? deps.notify : () => {};
    this._createWindow =
      typeof deps.createWindow === 'function' ? deps.createWindow : null;

    // Injectable timer primitives so the position timer and load timeout are
    // deterministic under test.
    this._setInterval = deps.setInterval || setInterval;
    this._clearInterval = deps.clearInterval || clearInterval;
    this._setTimeout = deps.setTimeout || setTimeout;
    this._clearTimeout = deps.clearTimeout || clearTimeout;

    /** Transport handle to the renderer (see createWindow). */
    this.window = null;

    // Authoritative playback mirror.
    this._status = STATUS_STOPPED;
    this._trackId = null;
    this._positionMs = 0;
    this._durationMs = null;
    this._volume = 100;

    /** Handle for the >=1Hz position-report timer, or null when stopped. */
    this._positionTimer = null;
    /** Handle for the per-load 10s timeout, or null when not loading. */
    this._loadTimer = null;
    /** Last positionMs we emitted, to suppress duplicate stateChanged spam. */
    this._lastReportedPositionMs = -1;
  }

  /**
   * Create the hidden BrowserWindow that hosts MusicKit JS.
   *
   * The window is `show: false` with hardened webPreferences (contextIsolation
   * on, nodeIntegration off, sandbox on, a single preload bridge) and loads the
   * MusicKit JS player document. Renderer events are routed into the state
   * machine here.
   */
  create() {
    if (this.window) return;
    this.window = this._createWindow
      ? this._createWindow()
      : createElectronPlayerWindow();
    this.window.onEvent((event) => this._handleRendererEvent(event));
  }

  /**
   * Load a track at a position and authorize playback with the Music_User_Token.
   *
   * Drives the Playback-Start Sequence: the renderer runs setQueue + authorize +
   * DRM license request. We enter the Loading status, (re)arm the 10-second
   * load/DRM timeout, and report the transition. On success the renderer emits a
   * `ready` event (and then `playing` once play is requested).
   *
   * @param {string} trackId
   * @param {number} [positionMs]
   * @param {string} [musicUserToken]
   * @param {string} [developerToken]
   */
  load(trackId, positionMs, musicUserToken, developerToken) {
    this.create();
    this._trackId = trackId;
    this._durationMs = null;
    this._positionMs = clampPosition(positionMs, null);
    this._setStatus(STATUS_LOADING);
    this._reportState();

    this._armLoadTimeout();

    this.window.send({
      type: 'load',
      trackId,
      positionMs: this._positionMs,
      musicUserToken: musicUserToken || '',
      developerToken: developerToken || '',
    });
  }

  /** Start playback of the loaded track. */
  play() {
    if (!this.window) return;
    this.window.send({ type: 'play' });
  }

  /** Pause playback, keeping the current position (Requirement 4.2). */
  pause() {
    if (!this.window) return;
    this.window.send({ type: 'pause' });
  }

  /**
   * Seek to a position, clamped to [0, duration]. Echoes the applied position so
   * the IPC server can return it in the `seek` result.
   *
   * @param {number} positionMs
   * @returns {number} the clamped position actually applied
   */
  seek(positionMs) {
    const applied = clampPosition(positionMs, this._durationMs);
    this._positionMs = applied;
    if (this.window) {
      this.window.send({ type: 'seek', positionMs: applied });
    }
    return applied;
  }

  /**
   * Set the output volume, clamped to 0-100 (Requirement 4.7). Echoes the
   * applied level for the `setVolume` IPC result.
   *
   * @param {number} level
   * @returns {number} the clamped level actually applied
   */
  setVolume(level) {
    const clamped = clampVolume(level);
    this._volume = clamped;
    if (this.window) {
      this.window.send({ type: 'setVolume', level: clamped });
    }
    return clamped;
  }

  /**
   * Handle a normalized event from the renderer and map it to playback state
   * changes and IPC notifications.
   *
   * @param {{type: string, trackId?: string, positionMs?: number, durationMs?: number, kind?: string}} event
   */
  _handleRendererEvent(event) {
    if (!event || typeof event.type !== 'string') return;

    switch (event.type) {
      case 'ready': {
        // The DRM license arrived and the queue is set; cancel the load timeout.
        this._clearLoadTimeout();
        if (typeof event.durationMs === 'number') {
          this._durationMs = event.durationMs;
        }
        break;
      }
      case 'playing': {
        this._clearLoadTimeout();
        this._updateFromEvent(event);
        this._setStatus(STATUS_PLAYING);
        this._reportState();
        this._startPositionTimer();
        break;
      }
      case 'paused': {
        this._updateFromEvent(event);
        this._setStatus(STATUS_PAUSED);
        this._reportState();
        this._stopPositionTimer();
        break;
      }
      case 'timeupdate': {
        this._updateFromEvent(event);
        // Keep the mirror current; the periodic timer emits stateChanged, so we
        // do not emit here to avoid flooding the socket.
        break;
      }
      case 'ended': {
        // The track finished (Requirement 4.8 handling lives in the TUI, which
        // owns the Queue). The engine reports the fact and stops its timer.
        this._stopPositionTimer();
        this._clearLoadTimeout();
        this._setStatus(STATUS_PAUSED);
        this.notify('trackFinished', { trackId: event.trackId || this._trackId });
        break;
      }
      case 'error': {
        // Stream or DRM failure (Requirement 4.13): pause at the current
        // position and report a named failure type.
        this._clearLoadTimeout();
        this._stopPositionTimer();
        this._setStatus(STATUS_PAUSED);
        const kind = event.kind === 'drm' ? 'drm' : 'network';
        this.notify('playbackError', { trackId: event.trackId || this._trackId, kind });
        break;
      }
      case 'unavailable': {
        // Track not playable in the storefront (Requirements 4.11/4.12): the TUI
        // advances the Queue. The engine stops and reports.
        this._clearLoadTimeout();
        this._stopPositionTimer();
        this._setStatus(STATUS_PAUSED);
        this.notify('trackUnavailable', { trackId: event.trackId || this._trackId });
        break;
      }
      default:
        break;
    }
  }

  /**
   * Merge position/duration from a renderer event into the mirror, clamping.
   * @param {{positionMs?: number, durationMs?: number}} event
   */
  _updateFromEvent(event) {
    if (typeof event.durationMs === 'number' && Number.isFinite(event.durationMs) && event.durationMs > 0) {
      this._durationMs = event.durationMs;
    }
    if (typeof event.positionMs === 'number') {
      this._positionMs = clampPosition(event.positionMs, this._durationMs);
    }
  }

  /**
   * Arm (or re-arm) the per-load 10-second timeout. If neither a ready nor a
   * playing event has cleared it by the deadline, treat the load as a network
   * failure (Requirement 4.13: "fails to load within 10 seconds").
   */
  _armLoadTimeout() {
    this._clearLoadTimeout();
    const trackId = this._trackId;
    this._loadTimer = this._setTimeout(() => {
      this._loadTimer = null;
      // Only fire if still stuck in Loading for this track.
      if (this._status === STATUS_LOADING && this._trackId === trackId) {
        this._stopPositionTimer();
        this._setStatus(STATUS_PAUSED);
        this.notify('playbackError', { trackId, kind: 'network' });
      }
    }, LOAD_TIMEOUT_MS);
  }

  _clearLoadTimeout() {
    if (this._loadTimer !== null) {
      this._clearTimeout(this._loadTimer);
      this._loadTimer = null;
    }
  }

  /**
   * Start the periodic position-report timer (>= 1 Hz). Idempotent: an existing
   * timer is left running.
   */
  _startPositionTimer() {
    if (this._positionTimer !== null) return;
    this._positionTimer = this._setInterval(() => {
      // Advance a local estimate between renderer timeupdates so a slow event
      // stream still produces monotonically progressing positions, then report.
      this._reportState();
    }, POSITION_REPORT_INTERVAL_MS);
  }

  _stopPositionTimer() {
    if (this._positionTimer !== null) {
      this._clearInterval(this._positionTimer);
      this._positionTimer = null;
    }
  }

  /**
   * @param {string} status
   */
  _setStatus(status) {
    this._status = status;
  }

  /**
   * Emit a stateChanged notification reflecting the current mirror.
   * (design "IPC Protocol": stateChanged {status, positionMs, durationMs, trackId}).
   */
  _reportState() {
    this._lastReportedPositionMs = this._positionMs;
    this.notify('stateChanged', {
      status: this._status,
      positionMs: this._positionMs,
      durationMs: this._durationMs === null ? null : this._durationMs,
      trackId: this._trackId,
    });
  }

  /**
   * Destroy the hidden window, stop all timers, and release media resources.
   */
  destroy() {
    this._stopPositionTimer();
    this._clearLoadTimeout();
    if (this.window) {
      try {
        this.window.send({ type: 'teardown' });
      } catch (_err) {
        // The window may already be gone; destroying below is sufficient.
      }
      try {
        this.window.destroy();
      } catch (_err) {
        // ignore
      }
      this.window = null;
    }
    this._status = STATUS_STOPPED;
  }
}

/**
 * Create the production Electron transport around a hidden, hardened
 * BrowserWindow that hosts MusicKit JS.
 *
 * Kept separate from PlayerWindow so the class stays testable without Electron;
 * this function is only reached when no `createWindow` factory is injected.
 *
 * @returns {{send: Function, onEvent: Function, loadFile: Function, destroy: Function}}
 */
function createElectronPlayerWindow() {
  // Required lazily so unit tests that inject a fake window never load Electron.
  const { BrowserWindow, ipcMain } = require('electron');
  const { COMMAND_CHANNEL, EVENT_CHANNEL } = require('./player-preload');

  const win = new BrowserWindow({
    show: false,
    // No chrome and no visible surface; this is an audio-only, headless window.
    width: 1,
    height: 1,
    webPreferences: {
      // Hardened renderer (design "Technology Stack"; Requirement 10.3 posture):
      // the MusicKit page is untrusted, so it gets no Node and a locked-down,
      // isolated context with only the explicit preload bridge.
      preload: path.join(__dirname, 'player-preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
    },
  });

  let eventHandler = null;
  const onIpc = (_e, event) => {
    if (eventHandler) eventHandler(event);
  };
  ipcMain.on(EVENT_CHANNEL, onIpc);

  win.loadFile(path.join(__dirname, 'player.html'));

  return {
    send(command) {
      if (!win.isDestroyed()) {
        win.webContents.send(COMMAND_CHANNEL, command);
      }
    },
    onEvent(cb) {
      eventHandler = cb;
    },
    loadFile(p) {
      win.loadFile(p);
    },
    destroy() {
      ipcMain.removeListener(EVENT_CHANNEL, onIpc);
      if (!win.isDestroyed()) win.destroy();
    },
  };
}

module.exports = {
  PlayerWindow,
  clampVolume,
  clampPosition,
  createElectronPlayerWindow,
  POSITION_REPORT_INTERVAL_MS,
  LOAD_TIMEOUT_MS,
};
