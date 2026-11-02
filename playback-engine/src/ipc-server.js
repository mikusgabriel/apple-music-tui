// IPC server (placeholder) — JSON-RPC 2.0 over a Unix domain socket.
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
// Scope: this is a stub. The real framing, socket binding, request dispatch,
// and notification emission are implemented in task 11.2.

'use strict';

class IpcServer {
  /**
   * @param {{ playerWindow: object, signInWindow: object }} deps
   */
  constructor(deps) {
    this.playerWindow = deps.playerWindow;
    this.signInWindow = deps.signInWindow;
    // TODO(task 11.2): hold the net.Server, the active socket, and a read
    // buffer for reassembling length-prefixed frames.
  }

  /**
   * Bind the Unix domain socket and begin accepting the TUI connection.
   * TODO(task 11.2): resolve the socket path, bind, and wire the frame decoder
   * to request dispatch.
   */
  start() {
    // Placeholder — no-op until task 11.2.
  }

  /**
   * Dispatch a decoded JSON-RPC request to the player / sign-in handlers.
   * TODO(task 11.2): route load/play/pause/seek/setVolume to the player window
   * and openSignIn/closeSignIn to the sign-in window; handle shutdown.
   */
  handleRequest(/* request */) {
    // Placeholder — no-op until task 11.2.
  }

  /**
   * Emit a JSON-RPC notification to the TUI over the framed socket.
   * TODO(task 11.2): serialize, length-prefix, and write to the socket.
   */
  notify(/* method, params */) {
    // Placeholder — no-op until task 11.2.
  }

  /**
   * Close the socket and release the socket-path resource.
   * TODO(task 11.2): close connections and unlink the socket file.
   */
  stop() {
    // Placeholder — no-op until task 11.2.
  }
}

module.exports = { IpcServer };
