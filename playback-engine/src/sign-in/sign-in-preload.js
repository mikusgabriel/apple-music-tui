// Preload bridge for the Sign_In_Window renderer.
//
// The sign-in page runs with contextIsolation + sandbox and no Node access; this
// preload exposes a single, narrow API on `window.signIn` so the page can:
//   - ask the main process for the Developer_Token (and receive it),
//   - report a captured Music_User_Token, a cancellation, or an error.
//
// Nothing here logs token values (Requirement 1.13). The channel names mirror the
// constants in sign-in-window.js.

'use strict';

const { contextBridge, ipcRenderer } = require('electron');

const CHANNEL_READY = 'signin:ready';
const CHANNEL_TOKEN = 'signin:token';
const CHANNEL_CANCELLED = 'signin:cancelled';
const CHANNEL_ERROR = 'signin:error';
const CHANNEL_CONFIG = 'signin:config';

contextBridge.exposeInMainWorld('signIn', {
  /**
   * Signal that the renderer is ready and request the sign-in configuration
   * (the Developer_Token). The main process replies over `onConfig`.
   */
  ready() {
    ipcRenderer.send(CHANNEL_READY);
  },

  /**
   * Register a one-shot listener for the sign-in configuration delivered by the
   * main process.
   *
   * @param {(config: {developerToken: string}) => void} handler
   */
  onConfig(handler) {
    ipcRenderer.once(CHANNEL_CONFIG, (_event, config) => handler(config));
  },

  /**
   * Report a captured Music_User_Token to the main process.
   *
   * @param {string} musicUserToken
   */
  reportToken(musicUserToken) {
    ipcRenderer.send(CHANNEL_TOKEN, musicUserToken);
  },

  /** Report that the user cancelled sign-in from within the page. */
  reportCancelled() {
    ipcRenderer.send(CHANNEL_CANCELLED);
  },

  /**
   * Report that the sign-in flow errored.
   *
   * @param {string} reason
   */
  reportError(reason) {
    ipcRenderer.send(CHANNEL_ERROR, reason);
  },
});
