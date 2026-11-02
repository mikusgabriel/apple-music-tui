// Sign_In_Window (placeholder) — a visible BrowserWindow that hosts the MusicKit
// sign-in web flow and captures the Music_User_Token.
//
// Per design "Sign-In Sequence" and "Playback_Engine": opened on `openSignIn`,
// it runs the Apple-hosted MusicKit sign-in, captures a non-empty
// Music_User_Token, and reports `signInResult` (ok/cancelled) or
// `signInError(reason)`. When the window closes, every process started for the
// sign-in is terminated within 5 seconds, while keeping the obtained token
// (Requirement 10.8).
//
// Scope: this is a stub. The BrowserWindow is not created and no sign-in logic
// runs here yet. Actual sign-in and token capture are implemented in task 12.2.

'use strict';

class SignInWindow {
  constructor() {
    // TODO(task 12.2): hold the visible BrowserWindow instance and the captured
    // token (if any).
    this.window = null;
  }

  /**
   * Open the visible sign-in web view and run the MusicKit sign-in flow.
   * TODO(task 12.2): create a visible BrowserWindow, load the MusicKit sign-in
   * page with the developerToken, and observe for the Music_User_Token.
   * Completion is reported to the TUI via the `signInResult` notification.
   */
  open(/* developerToken */) {
    // Placeholder — no-op until task 12.2.
  }

  /**
   * Close the sign-in web view.
   * TODO(task 12.2): close the window and terminate all sign-in child processes
   * within 5 seconds while keeping the obtained token (Requirement 10.8).
   */
  close() {
    // Placeholder — no-op until task 12.2.
  }
}

module.exports = { SignInWindow };
