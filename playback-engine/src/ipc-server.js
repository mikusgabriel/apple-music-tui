// IPC server — JSON-RPC 2.0 over a Unix domain socket.
//
// Transport (per design "IPC Protocol"): a Unix domain socket at
// `$XDG_RUNTIME_DIR/apple-music-tui/engine.sock` (fallback to a temp dir).
// Framing: a 4-byte big-endian length prefix followed by a UTF-8 JSON-RPC 2.0
// object. The TUI is the client and issues requests; this engine issues
// notifications for asynchronous state.
//
// Requests handled (TUI -> engine):
//   load, play, pause, seek, setVolume, openSignIn, closeSignIn, shutdown
// Notifications emitted (engine -> TUI):
//   stateChanged, trackFinished, playbackError, trackUnavailable,
//   signInResult, signInError
//
// Scope (task 11.2): the real framing, socket binding, request dispatch, and
// notification emission. The request handlers call into the player and sign-in
// window modules whose playback/sign-in bodies are filled in by tasks 12.1 and
// 12.2 — this module only wires dispatch to those interfaces.

'use strict';

const net = require('net');
const os = require('os');
const path = require('path');
const fs = require('fs');

// Volume is clamped to this inclusive range (design "IPC Protocol": setVolume
// result is clamped 0-100).
const VOLUME_MIN = 0;
const VOLUME_MAX = 100;

/**
 * Resolve the Unix domain socket path.
 *
 * Primary location is `$XDG_RUNTIME_DIR/apple-music-tui/engine.sock`; when
 * XDG_RUNTIME_DIR is unset we fall back to a stable per-user directory under the
 * OS temp dir so the TUI client can derive the same path (design "IPC Protocol").
 *
 * @returns {string} the absolute socket path
 */
function resolveSocketPath() {
  const runtimeDir = process.env.XDG_RUNTIME_DIR;
  const baseDir =
    runtimeDir && runtimeDir.length > 0
      ? path.join(runtimeDir, 'apple-music-tui')
      : path.join(os.tmpdir(), 'apple-music-tui');
  return path.join(baseDir, 'engine.sock');
}

/**
 * Encode a JSON-serializable message into a framed buffer: a 4-byte big-endian
 * length prefix followed by the UTF-8 JSON payload.
 *
 * @param {object} message a JSON-RPC 2.0 object
 * @returns {Buffer}
 */
function encodeFrame(message) {
  const payload = Buffer.from(JSON.stringify(message), 'utf8');
  const prefix = Buffer.allocUnsafe(4);
  prefix.writeUInt32BE(payload.length, 0);
  return Buffer.concat([prefix, payload]);
}

/**
 * Incremental decoder for 4-byte-big-endian length-prefixed frames.
 *
 * Bytes arrive in arbitrary chunks over the socket; this buffers them and yields
 * whole payloads as they complete. A single chunk may contain zero, one, or many
 * frames, and a frame may span multiple chunks.
 */
class FrameDecoder {
  constructor() {
    this._buffer = Buffer.alloc(0);
  }

  /**
   * Append received bytes and extract every complete frame now available.
   *
   * @param {Buffer} chunk
   * @returns {Buffer[]} payloads (without the length prefix), in order
   */
  push(chunk) {
    this._buffer = this._buffer.length === 0 ? chunk : Buffer.concat([this._buffer, chunk]);
    const frames = [];
    while (this._buffer.length >= 4) {
      const len = this._buffer.readUInt32BE(0);
      if (this._buffer.length < 4 + len) {
        break; // the full payload has not arrived yet
      }
      frames.push(this._buffer.subarray(4, 4 + len));
      this._buffer = this._buffer.subarray(4 + len);
    }
    return frames;
  }
}

class IpcServer {
  /**
   * @param {{ playerWindow: object, signInWindow: object, socketPath?: string }} deps
   */
  constructor(deps) {
    this.playerWindow = deps.playerWindow;
    this.signInWindow = deps.signInWindow;
    this.socketPath = deps.socketPath || resolveSocketPath();

    /** @type {import('net').Server|null} */
    this.server = null;
    /** @type {import('net').Socket|null} */
    this.socket = null;
    this._decoder = new FrameDecoder();

    // Callback invoked when a `shutdown` request is received, so the main
    // process can quit the app. Settable by the caller (main.js).
    this.onShutdown = typeof deps.onShutdown === 'function' ? deps.onShutdown : null;

    // Connection lifecycle callbacks used by parent-loss detection (task 12.3):
    // the TUI is the sole client, so an accepted connection means the parent is
    // present and a closed socket means the parent may be gone.
    this.onClientConnected =
      typeof deps.onClientConnected === 'function' ? deps.onClientConnected : null;
    this.onClientDisconnected =
      typeof deps.onClientDisconnected === 'function' ? deps.onClientDisconnected : null;
  }

  /**
   * Bind the Unix domain socket and begin accepting the TUI connection.
   *
   * The engine is launched as a single instance per socket, so a single client
   * connection is expected; a new connection replaces the previous one.
   */
  start() {
    const dir = path.dirname(this.socketPath);
    fs.mkdirSync(dir, { recursive: true });

    // A stale socket file from a previous crashed run would make bind fail with
    // EADDRINUSE, so remove it first. (Only the file; the dir is kept.)
    try {
      fs.unlinkSync(this.socketPath);
    } catch (err) {
      if (err && err.code !== 'ENOENT') {
        throw err;
      }
    }

    this.server = net.createServer((socket) => {
      this._attachSocket(socket);
    });

    this.server.on('error', (err) => {
      // Surface bind/accept errors; there is no recovery path for a failed bind.
      console.error('[ipc] server error:', err && err.message ? err.message : err);
    });

    this.server.listen(this.socketPath);
  }

  /**
   * Wire a freshly accepted client socket: reset the decoder, and route data,
   * close, and error events.
   *
   * @param {import('net').Socket} socket
   */
  _attachSocket(socket) {
    // Only the most recent client is retained; drop any prior connection.
    if (this.socket && this.socket !== socket) {
      this.socket.destroy();
    }
    this.socket = socket;
    this._decoder = new FrameDecoder();

    // Notify parent-loss detection that the TUI parent is present.
    if (this.onClientConnected) {
      this.onClientConnected();
    }

    socket.on('data', (chunk) => {
      let frames;
      try {
        frames = this._decoder.push(chunk);
      } catch (err) {
        console.error('[ipc] frame decode error:', err && err.message ? err.message : err);
        return;
      }
      for (const payload of frames) {
        this._handleFrame(payload);
      }
    });

    socket.on('error', (err) => {
      console.error('[ipc] socket error:', err && err.message ? err.message : err);
    });

    socket.on('close', () => {
      if (this.socket === socket) {
        this.socket = null;
        // The TUI parent dropped its connection; let parent-loss detection
        // (task 12.3) decide whether this means the parent is gone.
        if (this.onClientDisconnected) {
          this.onClientDisconnected();
        }
      }
    });
  }

  /**
   * Parse one decoded frame payload as a JSON-RPC request and dispatch it.
   *
   * @param {Buffer} payload UTF-8 JSON bytes (no length prefix)
   */
  _handleFrame(payload) {
    let request;
    try {
      request = JSON.parse(payload.toString('utf8'));
    } catch (err) {
      // A malformed frame has no id to answer against; log and drop it.
      console.error('[ipc] invalid JSON request:', err && err.message ? err.message : err);
      return;
    }
    this.handleRequest(request);
  }

  /**
   * Dispatch a decoded JSON-RPC request to the player / sign-in handlers and
   * reply with the JSON-RPC result (requests carry an `id`; notifications from
   * the client, if any, do not and get no reply).
   *
   * Routes load/play/pause/seek/setVolume to the player window and
   * openSignIn/closeSignIn to the sign-in window; handles shutdown.
   *
   * @param {{ jsonrpc?: string, id?: (string|number|null), method?: string, params?: object }} request
   */
  handleRequest(request) {
    if (!request || typeof request.method !== 'string') {
      if (request && request.id !== undefined && request.id !== null) {
        this._reply(request.id, null, {
          code: -32600,
          message: 'Invalid Request',
        });
      }
      return;
    }

    const { id, method } = request;
    const params = request.params || {};
    const isCall = id !== undefined && id !== null;

    try {
      switch (method) {
        case 'load': {
          this.playerWindow.load(params.trackId, params.positionMs, params.musicUserToken);
          if (isCall) this._reply(id, { ok: true });
          break;
        }
        case 'play': {
          this.playerWindow.play();
          if (isCall) this._reply(id, { ok: true });
          break;
        }
        case 'pause': {
          this.playerWindow.pause();
          if (isCall) this._reply(id, { ok: true });
          break;
        }
        case 'seek': {
          // The engine clamps to the track bounds; it echoes the applied value.
          const applied = this.playerWindow.seek(params.positionMs);
          const positionMs =
            typeof applied === 'number' ? applied : clampNonNegative(params.positionMs);
          if (isCall) this._reply(id, { positionMs });
          break;
        }
        case 'setVolume': {
          const clamped = clampVolume(params.level);
          const applied = this.playerWindow.setVolume(clamped);
          const level = typeof applied === 'number' ? clampVolume(applied) : clamped;
          if (isCall) this._reply(id, { level });
          break;
        }
        case 'openSignIn': {
          // Completion is reported asynchronously via the signInResult
          // notification (design "IPC Protocol"); the call itself returns ok so
          // the client knows the request was accepted.
          this.signInWindow.open(params.developerToken);
          if (isCall) this._reply(id, { ok: true });
          break;
        }
        case 'closeSignIn': {
          this.signInWindow.close();
          if (isCall) this._reply(id, { ok: true });
          break;
        }
        case 'shutdown': {
          if (isCall) this._reply(id, { ok: true });
          if (this.onShutdown) {
            this.onShutdown();
          }
          break;
        }
        default: {
          if (isCall) {
            this._reply(id, null, {
              code: -32601,
              message: `Method not found: ${method}`,
            });
          }
          break;
        }
      }
    } catch (err) {
      const message = err && err.message ? err.message : String(err);
      console.error(`[ipc] handler error for ${method}:`, message);
      if (isCall) {
        this._reply(id, null, { code: -32603, message });
      }
    }
  }

  /**
   * Send a JSON-RPC response (result or error) for a request id.
   *
   * @param {string|number} id
   * @param {object|null} result
   * @param {{code: number, message: string}} [error]
   */
  _reply(id, result, error) {
    const message = error
      ? { jsonrpc: '2.0', id, error }
      : { jsonrpc: '2.0', id, result };
    this._write(message);
  }

  /**
   * Emit a JSON-RPC notification to the TUI over the framed socket.
   *
   * Notifications carry no `id` per JSON-RPC 2.0. Used for asynchronous state:
   * stateChanged / trackFinished / playbackError / trackUnavailable /
   * signInResult / signInError.
   *
   * @param {string} method
   * @param {object} params
   */
  notify(method, params) {
    this._write({ jsonrpc: '2.0', method, params: params || {} });
  }

  /**
   * Frame and write a JSON-RPC message to the active client socket.
   *
   * @param {object} message
   */
  _write(message) {
    if (!this.socket || this.socket.destroyed) {
      // No client attached; drop the message rather than buffering unbounded.
      return;
    }
    this.socket.write(encodeFrame(message));
  }

  /**
   * Close the socket and release the socket-path resource.
   */
  stop() {
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
    }
    if (this.server) {
      this.server.close();
      this.server = null;
    }
    try {
      fs.unlinkSync(this.socketPath);
    } catch (err) {
      if (err && err.code !== 'ENOENT') {
        console.error('[ipc] failed to unlink socket:', err && err.message ? err.message : err);
      }
    }
  }
}

/**
 * Clamp a volume level to the 0-100 inclusive range, coercing non-finite input
 * to the minimum.
 *
 * @param {unknown} level
 * @returns {number}
 */
function clampVolume(level) {
  const n = Number(level);
  if (!Number.isFinite(n)) {
    return VOLUME_MIN;
  }
  return Math.max(VOLUME_MIN, Math.min(VOLUME_MAX, Math.round(n)));
}

/**
 * Clamp a position to be a non-negative integer, coercing non-finite input to 0.
 *
 * @param {unknown} positionMs
 * @returns {number}
 */
function clampNonNegative(positionMs) {
  const n = Number(positionMs);
  if (!Number.isFinite(n) || n < 0) {
    return 0;
  }
  return Math.round(n);
}

module.exports = {
  IpcServer,
  FrameDecoder,
  encodeFrame,
  resolveSocketPath,
  clampVolume,
  clampNonNegative,
};
