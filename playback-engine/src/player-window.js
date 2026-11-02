// MusicKit player window (placeholder) — a single hidden BrowserWindow that runs
// MusicKit JS for DRM-protected playback.
//
// Per design "Playback_Engine" and "Playback-Start Sequence": this hidden window
// loads and plays tracks, authorizes with the Music_User_Token, requests the DRM
// (Widevine) license, and reports position at least once per second so the TUI
// can meet the Now_Playing_Bar update guarantee (Requirement 6.2). It drives the
// stateChanged / trackFinished / playbackError / trackUnavailable notifications.
//
// Scope: this is a stub. The BrowserWindow is not created and no MusicKit logic
// runs here yet. Actual playback is implemented in task 12.1.

'use strict';

class PlayerWindow {
  constructor() {
    // TODO(task 12.1): hold the Electron BrowserWindow instance and current
    // playback metadata (trackId, status, position).
    this.window = null;
  }

  /**
   * Create the hidden BrowserWindow that hosts MusicKit JS.
   * TODO(task 12.1): construct a `show: false` BrowserWindow with a hardened
   * webPreferences (contextIsolation on, nodeIntegration off, a preload bridge)
   * and load the MusicKit JS player document.
   */
  create() {
    // Placeholder — no-op until task 12.1.
  }

  /**
   * Load a track at a position and authorize playback.
   * TODO(task 12.1): setQueue + authorize(musicUserToken) + request DRM license.
   */
  load(/* trackId, positionMs, musicUserToken */) {
    // Placeholder — no-op until task 12.1.
  }

  /** TODO(task 12.1): start playback. */
  play() {}

  /** TODO(task 12.1): pause playback at the current position. */
  pause() {}

  /** TODO(task 12.1): seek to a clamped position. */
  seek(/* positionMs */) {}

  /** TODO(task 12.1): set the clamped (0-100) output volume. */
  setVolume(/* level */) {}

  /**
   * Destroy the hidden window and release media resources.
   * TODO(task 12.1): close the BrowserWindow and stop playback.
   */
  destroy() {
    // Placeholder — no-op until task 12.1.
  }
}

module.exports = { PlayerWindow };
