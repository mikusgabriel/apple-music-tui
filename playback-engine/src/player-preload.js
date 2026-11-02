// Preload bridge for the hidden MusicKit player window.
//
// Runs in an isolated context (contextIsolation on, nodeIntegration off; see
// PlayerWindow's hardened webPreferences). It exposes a minimal, explicit bridge
// between the privileged main process and the untrusted MusicKit JS document:
//
//   window.playerBridge.onCommand(cb)   — receive transport commands from main
//   window.playerBridge.emit(event)     — send a player event up to main
//
// Nothing else from Node/Electron is reachable by the renderer, so a compromised
// MusicKit page cannot touch the filesystem, spawn processes, or read tokens
// beyond the single Music_User_Token it is explicitly handed for a `load`.

'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// Channel names are kept in one place so the renderer and main agree.
const COMMAND_CHANNEL = 'player:command';
const EVENT_CHANNEL = 'player:event';

contextBridge.exposeInMainWorld('playerBridge', {
  /**
   * Register a handler for transport commands pushed from the main process.
   * The command is a plain object: { type, ... } where type is one of
   * load/play/pause/seek/setVolume/teardown.
   *
   * @param {(command: object) => void} callback
   */
  onCommand(callback) {
    ipcRenderer.on(COMMAND_CHANNEL, (_event, command) => {
      callback(command);
    });
  },

  /**
   * Send a player event up to the main process. The event is a plain object:
   * { type, ... } where type is one of
   * ready/playing/paused/ended/timeupdate/error/unavailable.
   *
   * @param {object} event
   */
  emit(event) {
    ipcRenderer.send(EVENT_CHANNEL, event);
  },
});

module.exports = { COMMAND_CHANNEL, EVENT_CHANNEL };
