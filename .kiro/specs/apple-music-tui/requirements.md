# Requirements Document

## Introduction

Apple Music TUI is a lightweight, keyboard-driven terminal application for Linux that lets an Apple Music subscriber search the catalog, browse a personal library, and play music without keeping the Apple Music web player open in a full browser.

### Platform Constraints

- The application targets Linux. Native Music.app and AppleScript control are unavailable.
- The Apple Music API requires a Developer_Token for catalog requests, and a Music_User_Token for library access and full-length playback.
- The user has no Apple Developer account. The application obtains the Developer_Token automatically from the public Apple Music web player (music.apple.com) assets instead of signing one with a MusicKit private key.
- Reusing the web player's Developer_Token is unofficial. Apple may rotate, restrict, or block this token at any time, which would break catalog access until the application adapts.
- A Music_User_Token is issued only through Apple's MusicKit sign-in flow, which runs in a web context (MusicKit JS). The application runs this flow in the Sign_In_Window, where the user signs in with a personal Apple ID.
- Full-length Apple Music streams are DRM-protected. On Linux, the only known playback path is MusicKit JS running in a Chromium-based runtime with the Widevine content decryption module. The TUI therefore needs a separate Playback_Engine process, and the memory budget in Requirement 10 must account for it.
- Without a Music_User_Token, the Apple Music API only exposes 30-second previews, which are unencrypted.

### Open Questions

1. **Playback_Engine runtime:** Is a headless Chromium/Electron runtime with Widevine acceptable as a background dependency, given that it adds memory overhead compared with a pure terminal process?
2. **Memory budget:** Are the limits in Requirement 10 (TUI process 100 MB, all processes combined 400 MB during playback) acceptable targets?
3. **Preview-only mode:** Should the application offer a preview-only mode (30-second clips, no DRM, no sign-in) as a fallback when the Playback_Engine is unavailable?
4. **Scope of library features:** Are creating and editing playlists, adding songs to the library, and rating songs in scope for the first version, or is read-only library browsing sufficient?
5. **Lyrics and artwork:** Are lyrics or terminal-rendered album artwork desired in the first version?
6. **Desktop integration:** Should MPRIS media key support (Requirement 11) be included in the first version?

## Glossary

- **TUI**: The Apple Music terminal user interface application process that renders views and handles keyboard input.
- **Developer_Token**: A JWT that authorizes requests to the Apple Music API. The Auth_Manager obtains the Developer_Token automatically from the public Apple Music web player (music.apple.com) assets; the user does not supply it.
- **Music_User_Token**: A token issued by Apple after the user signs in with an Apple ID that authorizes access to that user's library and subscription playback.
- **Sign_In_Window**: A small embedded web view that runs Apple's MusicKit sign-in flow, in which the user signs in with a personal Apple ID so the Auth_Manager can capture the Music_User_Token.
- **Auth_Manager**: The component that obtains, stores, validates, and refreshes the Developer_Token and Music_User_Token.
- **Token_Store**: Local storage for tokens, restricted to the current Linux user.
- **API_Client**: The component that sends requests to the Apple Music API and converts responses into Domain_Objects.
- **Domain_Object**: An in-memory representation of an Apple Music resource: Song, Album, Artist, or Playlist.
- **Library**: The signed-in user's saved songs, albums, artists, and playlists in Apple Music.
- **Catalog**: The full public Apple Music catalog for the user's storefront.
- **Storefront**: The Apple Music country or region used for catalog requests.
- **Playback_Engine**: The component that decodes and plays audio streams, including DRM-protected streams, and reports playback state to the TUI.
- **Playback_State**: The current track, position, duration, play/pause status, volume, shuffle mode, and repeat mode.
- **Queue**: The ordered list of tracks scheduled for playback, with a pointer to the current track.
- **Now_Playing_Bar**: The persistent area of the TUI that displays Playback_State.
- **Config_File**: A TOML file at `$XDG_CONFIG_HOME/apple-music-tui/config.toml` (default `~/.config/apple-music-tui/config.toml`) containing user settings.
- **Config_Parser**: The component that reads the Config_File into a Configuration object.
- **Config_Printer**: The component that writes a Configuration object back to Config_File format.
- **Session_State**: Persisted data restored between runs: Queue, current track position, volume, shuffle mode, and repeat mode.
- **Keybinding**: A mapping from a key or key sequence to a TUI action.
- **MPRIS_Adapter**: An optional component that exposes playback controls over the D-Bus MPRIS interface for desktop media keys.

## Requirements

### Requirement 1: Authentication

**User Story:** As an Apple Music subscriber, I want to sign in once and stay signed in, so that I can access my library and play full tracks from the terminal.

#### Acceptance Criteria

1. WHEN the TUI starts and the Token_Store holds no Music_User_Token, or holds a Music_User_Token that is empty, THE Auth_Manager SHALL open the Sign_In_Window within 3 seconds of TUI startup so the user can sign in with a personal Apple ID.
2. WHILE the Sign_In_Window is open, THE TUI SHALL display a status message stating that sign-in is in progress in the Sign_In_Window, and THE TUI SHALL keep the quit key binding active.
3. WHEN the user completes sign-in in the Sign_In_Window and the MusicKit sign-in flow returns a non-empty Music_User_Token, THE Auth_Manager SHALL save the Music_User_Token in the Token_Store, close the Sign_In_Window, and show the TUI's main view within 2 seconds of the token being returned.
4. IF the user closes or cancels the Sign_In_Window before the MusicKit sign-in flow returns a Music_User_Token, THEN THE TUI SHALL display a message stating that sign-in was cancelled, leave the Token_Store unchanged, and offer a retry action that reopens the Sign_In_Window.
5. THE Token_Store SHALL restrict read and write permission on stored Developer_Token and Music_User_Token values to the current Linux user (file mode 0600 or an equivalent OS keyring entry), so that no other Linux user account can read or modify them.
6. WHEN the TUI starts and the Token_Store holds a non-empty Music_User_Token, THE Auth_Manager SHALL attach that Music_User_Token to API requests without opening the Sign_In_Window, until the Apple Music API rejects that token as described in criterion 11.
7. WHEN the Auth_Manager requires a Developer_Token and the Token_Store holds no cached Developer_Token, THE Auth_Manager SHALL fetch the Developer_Token from the public Apple Music web player (music.apple.com) assets, waiting at most 10 seconds for the fetch to complete, and cache the Developer_Token in the Token_Store.
8. WHEN the Auth_Manager requires a Developer_Token and the cached Developer_Token cannot be decoded as a JWT, lacks an exp claim, or has an exp claim earlier than the current time plus 60 seconds, THE Auth_Manager SHALL discard the cached Developer_Token and fetch a new Developer_Token from the Apple Music web player assets as described in criterion 7.
9. IF the Apple Music API rejects a Catalog request with HTTP status 401 or 403, THEN THE Auth_Manager SHALL discard the cached Developer_Token, fetch a new Developer_Token from the Apple Music web player assets, and resend the rejected request exactly once; IF the resent request is also rejected with HTTP status 401 or 403, THEN THE TUI SHALL display the error described in criterion 10 and SHALL NOT resend the request again.
10. IF the Developer_Token fetch from the Apple Music web player assets fails because of a network error, the 10-second wait expiring, or the downloaded assets containing no Developer_Token, THEN THE TUI SHALL display an error stating that the Developer_Token could not be obtained from the Apple Music web player, keep the stored Music_User_Token, and offer a retry action that runs the fetch again.
11. IF the Apple Music API rejects a request that carries the Music_User_Token with HTTP status 401 or 403, and the request still fails after the Developer_Token refresh and single resend described in criterion 9, THEN THE Auth_Manager SHALL delete the stored Music_User_Token from the Token_Store and THE TUI SHALL display a message stating that the session expired and open the Sign_In_Window.
12. WHEN the user selects the sign-out action, THE Auth_Manager SHALL delete the Music_User_Token from the Token_Store, keep the cached Developer_Token, and THE TUI SHALL display a signed-out view that offers a sign-in action that opens the Sign_In_Window.
13. THE TUI SHALL exclude Developer_Token and Music_User_Token values from log files, standard output, standard error, and error messages shown in the TUI.
14. IF the Sign_In_Window fails to open within 10 seconds, for example because the embedded web view runtime is missing or no graphical display is available, THEN THE TUI SHALL display an error stating that the Sign_In_Window could not be opened and the reason, and offer a retry action that reopens the Sign_In_Window.
15. IF the Auth_Manager fails to save a Music_User_Token or Developer_Token in the Token_Store, THEN THE TUI SHALL display an error stating that the token could not be saved and that sign-in will be required at the next start, and THE Auth_Manager SHALL keep using the token in memory for the current session.

### Requirement 2: Catalog Search

**User Story:** As a listener, I want to search the Apple Music catalog, so that I can find songs, albums, artists, and playlists to play.

#### Acceptance Criteria

1. WHEN the user submits a search query of 1 to 200 characters after trimming leading and trailing whitespace, THE API_Client SHALL request up to 25 matching songs, up to 25 albums, up to 25 artists, and up to 25 playlists from the Catalog for the configured Storefront.
2. IF the user submits a search query that is empty, contains only whitespace, or exceeds 200 characters, THEN THE TUI SHALL keep the query in the input field, send no request, and display a message stating that the query must be 1 to 200 characters.
3. WHEN search results are received, THE TUI SHALL display the results within 2 seconds of query submission on a connection with under 200 ms latency, grouped into separate sections for songs, albums, artists, and playlists, with each song showing title, artist name, and duration, each album showing name and artist name, each artist showing name, and each playlist showing name.
4. WHEN a search returns zero results across all four resource types, THE TUI SHALL display a message stating that no results matched the query, and THE TUI SHALL omit the section for any single resource type that returned zero results.
5. IF a search request fails or exceeds the 10-second timeout, THEN THE TUI SHALL display an error stating that the search failed, keep the submitted query in the input field, and offer a retry action that resubmits the same query.
6. WHEN the user submits a new search query while an earlier search request is still pending, THE TUI SHALL display only the results of the most recently submitted query.
7. WHEN the user selects a song in the search results, THE TUI SHALL replace the Queue with the selected song as its only entry and start playback of that song.
8. WHEN the user selects an album or playlist in the search results, THE TUI SHALL display the tracks of the selected album or playlist in their Catalog order with title, artist name, and duration.
9. WHEN the user selects an artist in the search results, THE TUI SHALL display the artist's albums and the artist's top songs as two separate sections.
10. WHEN the user moves the selection to the last item of a result section that has more results available in the Catalog, THE API_Client SHALL request the next page of up to 25 results for that resource type and THE TUI SHALL append the returned results to the end of that section without changing the current selection.
11. IF a next-page request fails, THEN THE TUI SHALL keep the already displayed results, display an error stating that more results could not be loaded, and request the page again when the user next moves the selection to the last item of that section.

### Requirement 3: Library Browsing

**User Story:** As a subscriber, I want to browse my Apple Music library, so that I can play my saved music and playlists.

#### Acceptance Criteria

1. WHILE a valid Music_User_Token is available, THE TUI SHALL display four Library sections labeled playlists, albums, artists, and songs, each selectable from the keyboard.
2. WHEN the user opens a Library section, THE API_Client SHALL retrieve the first page of up to 100 Library items for that section from the Apple Music API, and THE TUI SHALL show a loading indicator in that section until the page is displayed or the request fails.
3. WHEN the user moves the selection to the last loaded item of a Library section and the Apple Music API reports further pages for that section, THE API_Client SHALL retrieve the next page of up to 100 items, and THE TUI SHALL append those items below the already loaded items in the order the Apple Music API returned them.
4. WHEN the user opens a Library playlist or album, THE API_Client SHALL retrieve all tracks of that playlist or album in pages of up to 100 tracks, and THE TUI SHALL display the tracks in their stored order, with each row showing title, artist, and duration formatted as m:ss for durations under 1 hour and h:mm:ss for durations of 1 hour or more.
5. WHEN the user selects a track inside an album or playlist, THE TUI SHALL replace the Queue with all tracks of that album or playlist in their stored order and start playback at the selected track's position in the Queue.
6. WHEN the user triggers the refresh action in a Library section, THE API_Client SHALL re-fetch the first page of up to 100 items of that section from the Apple Music API, and THE TUI SHALL replace the displayed items of that section with the re-fetched items and move the selection to the first item.
7. IF a Library request from the API_Client fails or receives no response within 10 seconds, THEN THE TUI SHALL display an error message indicating that the Library could not be loaded, keep any previously displayed items of that section unchanged, and leave the Queue and current playback unchanged.
8. IF the Apple Music API returns zero items for a Library section, playlist, or album, THEN THE TUI SHALL display an empty-state message indicating that the section, playlist, or album contains no items, in place of the item list.
9. IF the Apple Music API rejects the Music_User_Token during a Library request, THEN THE TUI SHALL stop displaying the Library sections and display a message indicating that the user must sign in again.

### Requirement 4: Playback Control

**User Story:** As a listener, I want to control playback with the keyboard, so that I can listen without leaving the terminal.

#### Acceptance Criteria

1. WHEN the user starts playback of a track, THE Playback_Engine SHALL begin audio output within 3 seconds of the user action, measured on a connection with at least 5 Mbit/s bandwidth.
2. WHEN the user triggers the play/pause action while a track is loaded, THE Playback_Engine SHALL switch the play/pause status from playing to paused or from paused to playing, keeping the playback position unchanged.
3. WHEN the user triggers the next-track action, THE Playback_Engine SHALL start playback of the track after the current track in the Queue from position 0; IF the current track is the last track in the Queue, THEN THE Playback_Engine SHALL start the first track in the Queue when repeat mode is repeat all, and stop playback in paused status at position 0 of the last track when repeat mode is off or repeat one.
4. WHEN the user triggers the previous-track action and the current position is 3 seconds or less, THE Playback_Engine SHALL start playback of the track before the current track in the Queue from position 0; IF the current track is the first track in the Queue, THEN THE Playback_Engine SHALL restart the current track from position 0.
5. WHEN the user triggers the previous-track action and the current position is greater than 3 seconds, THE Playback_Engine SHALL restart the current track from position 0.
6. WHEN the user triggers a seek-forward or seek-backward action, THE Playback_Engine SHALL move the playback position forward or backward by the configured seek step (an integer from 1 to 300 seconds, default 10 seconds), clamping the result to the range 0 to the track duration, and a forward seek that reaches the track duration SHALL be handled as the current track finishing.
7. WHEN the user changes the volume, THE Playback_Engine SHALL set the volume to the requested integer level, clamping values below 0 to 0 percent and values above 100 to 100 percent.
8. WHEN the current track finishes, THE Playback_Engine SHALL restart the same track from position 0 if repeat mode is repeat one, start the next track in the Queue if one exists, start the first track in the Queue if the finished track is the last track and repeat mode is repeat all, and stop playback in paused status if the finished track is the last track and repeat mode is off.
9. WHILE the TUI is running, THE Playback_Engine SHALL continue playback without interruption when the TUI switches between views.
10. IF the Playback_Engine fails to start within 10 seconds or does not respond to the TUI for 5 seconds, THEN THE TUI SHALL display an error naming the failure (start failure or lost connection) and offer a restart action for the Playback_Engine, keeping the Queue and current track unchanged.
11. IF a track is unavailable for playback in the user's Storefront, THEN THE Playback_Engine SHALL skip to the next track in the Queue and THE TUI SHALL display the title of the skipped track.
12. IF every remaining track in the Queue is unavailable for playback in the user's Storefront, THEN THE Playback_Engine SHALL stop playback in paused status and THE TUI SHALL display a message stating that no playable tracks remain in the Queue.
13. IF the stream of the current track fails to load within 10 seconds or the DRM license request for the track fails, THEN THE Playback_Engine SHALL set the play/pause status to paused at the current position and THE TUI SHALL display an error naming the track and the failure type (network or DRM license).
14. WHEN the TUI sends a restart action for the Playback_Engine and the restarted Playback_Engine connects, THE Playback_Engine SHALL load the current track at the last reported position in paused status.

### Requirement 5: Queue Management

**User Story:** As a listener, I want to view and edit what plays next, so that I can control my listening session.

#### Acceptance Criteria

1. WHEN the user opens the Queue view, THE TUI SHALL display every track in the Queue in playback order, each row showing its 1-based position, title, artist, and duration, with the current track highlighted.
2. WHEN the user adds a track with the play-next action, THE Queue SHALL insert the track at the position directly after the current track, or, if the Queue has no current track, insert it as the only track and set it as the current track without starting playback.
3. WHEN the user adds a track with the add-to-queue action, THE Queue SHALL append the track after the last track in the Queue, or, if the Queue has no current track, append it and set it as the current track without starting playback.
4. WHEN the user removes a track other than the current track from the Queue, THE Queue SHALL delete that one track, reduce the Queue length by 1, preserve the relative order of the remaining tracks, and keep the current-track pointer on the same track.
5. WHEN the user moves a track to a target position in the Queue view, THE Queue SHALL place the track at the target position, bounded by position 1 and the Queue length, preserve the relative order of the other tracks, and keep the current-track pointer on the same track.
6. WHEN the user enables shuffle mode, THE Queue SHALL keep the current track and all tracks before it at their positions and reorder the tracks after the current track as a permutation of those same tracks, leaving the Queue unchanged if 1 or fewer tracks follow the current track.
7. WHEN the user disables shuffle mode, THE Queue SHALL restore the order the tracks had before shuffle mode was enabled, excluding tracks removed while shuffle mode was enabled, placing tracks added while shuffle mode was enabled after the restored tracks in the order they were added, and keeping the current-track pointer on the same track.
8. WHEN the user cycles the repeat mode, THE Queue SHALL switch from off to repeat all, from repeat all to repeat one, and from repeat one to off.
9. FOR ALL sequences of add, remove, reorder, and shuffle operations, THE Queue SHALL keep the current-track pointer on a track that exists in the Queue, or on no track when the Queue is empty, and SHALL keep the Queue length equal to the starting length plus the number of added tracks minus the number of removed tracks (invariant).
10. IF the user opens the Queue view while the Queue contains no tracks, THEN THE TUI SHALL display a message stating that the Queue is empty.
11. IF the user removes the current track from the Queue, THEN THE Queue SHALL move the current-track pointer to the track that followed it, or to no track if it was the last track, and THE Playback_Engine SHALL start playback of the new current track if playback was active, or stop playback if no current track remains.
12. IF an add action would make the Queue exceed 10,000 tracks, THEN THE Queue SHALL reject the action and leave its contents and current-track pointer unchanged, and THE TUI SHALL display a message stating that the Queue limit of 10,000 tracks was reached.

### Requirement 6: Now Playing Display

**User Story:** As a listener, I want to see what is playing, so that I know the track and progress at a glance.

#### Acceptance Criteria

1. WHILE a track is loaded, THE Now_Playing_Bar SHALL display the Playback_State fields: track title, artist, album, elapsed time, total duration, play/pause status, volume as an integer percentage from 0 to 100, shuffle mode as on or off, and repeat mode as one of off, repeat all, or repeat one.
2. WHILE a track is playing, THE Now_Playing_Bar SHALL update the elapsed time at least once per second, and the displayed elapsed time SHALL differ from the Playback_Engine's reported position by no more than 1 second.
3. WHILE no track is loaded, THE Now_Playing_Bar SHALL display a message stating that nothing is playing in place of the track title, artist, album, elapsed time, and total duration, and SHALL continue to display volume, shuffle mode, and repeat mode.
4. IF the terminal width in columns is less than the width needed to render all Now_Playing_Bar fields on a single line, THEN THE Now_Playing_Bar SHALL truncate the album, then the artist, then the title text, each ending with a single ellipsis character, and SHALL keep the elapsed time, total duration, and play/pause status fully visible.
5. THE Now_Playing_Bar SHALL format elapsed time and total duration as m:ss for values under 1 hour and as h:mm:ss for values of 1 hour or more.
6. WHEN the Playback_State changes play/pause status, volume, shuffle mode, repeat mode, or current track, THE Now_Playing_Bar SHALL display the new value within 100 ms.
7. IF the current track has no artist, album, or total duration available, THEN THE Now_Playing_Bar SHALL display a placeholder indicating the value is unknown in that field and SHALL keep displaying all other fields.

### Requirement 7: Keyboard Navigation

**User Story:** As a terminal user, I want to operate the whole application from the keyboard, so that I never need a mouse.

#### Acceptance Criteria

1. THE TUI SHALL provide a Keybinding, consisting of a single key or a sequence of up to 2 keys, for every user action defined in Requirements 1 through 6 that the TUI renders, so that each action can be triggered without pointer input. Interactions inside the Apple-hosted sign-in page of the Sign_In_Window are excluded.
2. THE TUI SHALL provide default list-navigation Keybindings in which `j` and Down Arrow move the selection down one item, `k` and Up Arrow move the selection up one item, `l`, Right Arrow, and Enter open the selected item, and `h` and Left Arrow return to the previous view. At the first or last item of a list, the selection SHALL stay on that item.
3. WHEN the user presses the help Keybinding (default `?`), THE TUI SHALL display an overlay listing every Keybinding active in the current view and every global Keybinding, each with its action name. The overlay SHALL scroll with the list-navigation Keybindings when the list exceeds the terminal height, and SHALL close and restore the previous view unchanged when the user presses Esc or `?`.
4. WHERE the Config_File defines custom Keybindings, THE TUI SHALL use the custom Keybindings in place of the defaults for the listed actions, keep the default Keybindings for all unlisted actions, and show the custom Keybindings in the help overlay.
5. IF the Config_File assigns one key or key sequence to two actions in the same view, counting conflicts between custom Keybindings and the default Keybindings of unlisted actions, THEN THE TUI SHALL display a warning at startup naming the conflicting key and both actions, and SHALL use the default Keybindings for those actions.
6. WHEN the user presses the quit Keybinding (default `q`), THE TUI SHALL save the Session_State including the current playback position, stop playback, terminate the Playback_Engine, restore the terminal to its original mode, and exit with status code 0.
7. WHILE a text input field such as the search query field has focus, THE TUI SHALL insert printable keys as text instead of triggering their Keybindings, and SHALL return focus to the enclosing view when the user presses Esc.
8. IF the Config_File maps a Keybinding to an unknown action name or uses an unrecognized key name, THEN THE TUI SHALL display a warning at startup naming the invalid entry and use the default Keybinding for the affected action.
9. IF saving the Session_State fails after the user presses the quit Keybinding, THEN THE TUI SHALL write the error to the log file at `$XDG_STATE_HOME/apple-music-tui/app.log`, stop playback, terminate the Playback_Engine, restore the terminal to its original mode, and exit with a non-zero status code.

### Requirement 8: Configuration File

**User Story:** As a power user, I want to configure the application with a text file, so that I can set the Storefront, Keybindings, and defaults.

#### Acceptance Criteria

1. WHEN the TUI starts, THE Config_Parser SHALL read the Config_File into a Configuration object that holds a value for every setting, including the Storefront and every Keybinding, before the TUI displays its first view.
2. IF the Config_File does not exist when the TUI starts, THEN THE TUI SHALL create the Config_File and any missing parent directories, write the Config_Printer output of the default Configuration object to it, and continue startup using the default Configuration object.
3. IF the Config_File contains invalid TOML syntax, or a setting value of the wrong type or outside its permitted values (including a Storefront that is not a two-letter ISO 3166-1 alpha-2 country code), THEN THE Config_Parser SHALL return an error naming the 1-based line number and the setting name (the line number alone when a syntax error cannot be tied to a setting), and THE TUI SHALL print the error to standard error, leave the Config_File unmodified, and exit with a non-zero status code.
4. THE Config_Printer SHALL format any Configuration object into TOML that the Config_Parser accepts without error, and the output SHALL contain no access tokens, user tokens, or other credentials.
5. FOR ALL valid Configuration objects, parsing the output of the Config_Printer SHALL produce an equivalent Configuration object, where equivalent means every setting, including every Keybinding, has an equal value in both objects (round-trip property).
6. THE Config_Parser SHALL apply the default value for every setting absent from the Config_File, including when the Config_File is empty, without writing the defaults back to the Config_File.
7. IF the TUI cannot create or write the Config_File because of a file-system error, THEN THE TUI SHALL display a warning naming the Config_File path and the failure reason, and continue startup using the default Configuration object.
8. IF the Config_File contains a setting name that the Configuration object does not define, THEN THE Config_Parser SHALL ignore that setting, and THE TUI SHALL display a warning naming the setting and its 1-based line number and continue startup.

### Requirement 9: Session Persistence

**User Story:** As a listener, I want my queue and settings restored on the next launch, so that I can continue where I left off.

#### Acceptance Criteria

1. WHEN the TUI exits normally (the user presses the quit Keybinding, or the TUI process receives SIGINT or SIGTERM), THE TUI SHALL write the Session_State to `$XDG_STATE_HOME/apple-music-tui/session.json` (default `~/.local/state/apple-music-tui/session.json` when `$XDG_STATE_HOME` is unset or empty), creating any missing parent directories, before the process exits.
2. WHEN the TUI writes the Session_State, THE TUI SHALL replace the session file only after the new content is completely written, so that the session file on disk always contains either the complete previous Session_State or the complete new Session_State.
3. THE TUI SHALL serialize and deserialize Session_State such that, for every valid Session_State value, serializing then deserializing produces a Session_State equal to the original in Queue track order, current track pointer, current track position (to the nearest second), volume, shuffle mode, and repeat mode (round-trip property).
4. WHEN the TUI starts and a valid saved Session_State exists, THE TUI SHALL restore the Queue in its saved order, the current track, the current track position, volume, shuffle mode, and repeat mode, and SHALL show the Playback_State as paused with no audio output until the user starts playback; IF the saved position is greater than the current track's duration, THEN THE TUI SHALL set the position to 0 seconds.
5. WHEN the TUI starts and no session file exists at the path in criterion 1, THE TUI SHALL start with an empty Queue and default Playback_State settings and SHALL NOT display a session warning.
6. IF the saved session file exists but cannot be read, cannot be parsed, or contains a field value outside its allowed range (including a current track pointer outside the Queue bounds or a volume outside the TUI's supported volume range), THEN THE TUI SHALL start with an empty Queue and default Playback_State settings, display a warning stating that the previous session could not be restored, and leave the existing session file unmodified until the next save.
7. IF one or more tracks in the restored Queue are no longer available from the Apple Music API at startup, THEN THE TUI SHALL remove those tracks from the Queue, keep the remaining tracks in their saved relative order, move the current track pointer to the next available track (or clear it if none remain), and display a warning stating how many tracks were removed.
8. IF writing the Session_State fails (for example, the directory is not writable or the disk is full), THEN THE TUI SHALL display or print an error message indicating that the session could not be saved, leave any existing session file unchanged, and still exit.

### Requirement 10: Resource Footprint

**User Story:** As a user who wants to avoid a heavy browser, I want the application to use few system resources, so that it stays lightweight.

#### Acceptance Criteria

1. WHEN the user launches the TUI with a stored Music_User_Token that the Apple Music API accepts, THE TUI SHALL render the main view layout (navigation and playback status areas visible, with loading placeholders allowed for remote content) within 2 seconds of process start, measured on a machine with an SSD, a 4-core x86_64 CPU, and at least 8 GB of RAM, excluding network response time for remote content.
2. WHILE playback is paused, no user input has been received for at least 30 seconds, and the Sign_In_Window is closed, THE TUI process SHALL use less than 100 MB of resident memory, excluding the Playback_Engine process and all of its child processes.
3. WHILE the Playback_Engine is playing audio, THE TUI process, the Playback_Engine process, and all child processes of the Playback_Engine combined SHALL use less than 400 MB of resident memory, measured at any point from 60 seconds after playback starts through at least 30 minutes of continuous playback.
4. WHILE playback is paused, no user input has been received for at least 30 seconds, and the Sign_In_Window is closed, THE TUI process SHALL use less than 1 percent of one CPU core averaged over a 60-second measurement window.
5. WHEN the TUI exits through the user quit command or on receiving SIGINT, SIGTERM, or SIGHUP, THE TUI SHALL terminate the Playback_Engine process and all child processes it started, so that none of these processes remain running 5 seconds after the exit begins.
6. WHILE playback is paused, no user input has been received for at least 30 seconds, and the Sign_In_Window is closed, THE Playback_Engine process and all of its child processes combined SHALL use less than 2 percent of one CPU core averaged over a 60-second measurement window.
7. IF the TUI process terminates without performing its exit cleanup (for example, through SIGKILL or a crash), THEN THE Playback_Engine SHALL detect the loss of the TUI process and terminate itself and all of its child processes within 5 seconds.
8. WHEN the Sign_In_Window closes, THE TUI SHALL terminate every process started for the Sign_In_Window within 5 seconds, while keeping the Music_User_Token obtained during sign-in.

### Requirement 11: Linux Desktop Integration

**User Story:** As a Linux desktop user, I want media keys and desktop widgets to control playback, so that I can control music while working in other windows.

#### Acceptance Criteria

1. WHERE MPRIS integration is enabled in the Config_File, WHEN the TUI starts, THE MPRIS_Adapter SHALL register the application on the D-Bus session bus as an MPRIS media player within 2 seconds of startup, under a bus name that identifies the application.
2. WHERE MPRIS integration is enabled, WHEN an MPRIS client sends a Play, Pause, PlayPause, Next, Previous, or Seek command, THE MPRIS_Adapter SHALL forward the command to the Playback_Engine, and the resulting Playback_State SHALL match the Playback_State produced by the equivalent TUI Keybinding action within 500 milliseconds of the command being received.
3. WHERE MPRIS integration is enabled, WHEN the Playback_State changes, THE MPRIS_Adapter SHALL publish over D-Bus, within 1 second of the change, the current track title, artist name, album name, track duration, and playback status (Playing, Paused, or Stopped), and SHALL publish the new position after every seek.
4. IF the D-Bus session bus is unavailable at startup or the connection to it is lost during the session, THEN THE MPRIS_Adapter SHALL write a warning naming the D-Bus failure to the application log, and THE TUI SHALL continue all non-MPRIS functions with the Playback_State unchanged and without retrying MPRIS registration until the next start.
5. WHERE MPRIS integration is disabled in the Config_File, THE MPRIS_Adapter SHALL NOT register any bus name on the D-Bus session bus.
6. IF an MPRIS client sends a command while the Queue has no current track, or sends a Seek that would move the position before 0 seconds or past the track duration, THEN THE MPRIS_Adapter SHALL leave the Playback_State unchanged for the no-current-track case, clamp the position to 0 seconds for a seek before the start, and advance to the next track in the Queue for a seek past the end.
7. WHERE MPRIS integration is enabled, WHEN the TUI exits, THE MPRIS_Adapter SHALL release its bus name on the D-Bus session bus before the TUI process terminates.

### Requirement 12: API Response Handling and Errors

**User Story:** As a user, I want clear feedback when something goes wrong, so that I understand failures and the application stays usable.

#### Acceptance Criteria

1. WHEN the API_Client receives an Apple Music API response with an HTTP status in the 200–299 range, THE API_Client SHALL convert each resource in the response into one Domain_Object containing the ID and name, plus the artist name (for Songs and Albums), the duration in milliseconds (for Songs), and the artwork URL, where each of those attributes is present in the resource.
2. IF a resource in a successful response lacks one or more optional attributes (artist name, duration, or artwork URL), THEN THE API_Client SHALL create the Domain_Object with an empty string for each missing text attribute and no value for a missing duration, and SHALL still include that Domain_Object in the result.
3. IF a resource in a successful response lacks an ID or a name, or the response body cannot be parsed, THEN THE API_Client SHALL omit each affected resource from the result, return the remaining Domain_Objects, and write an entry to the log file at `$XDG_STATE_HOME/apple-music-tui/app.log` identifying the omitted resource or the unparseable response.
4. IF a network request fails to connect, or no complete response arrives within 10 seconds of the request being sent, THEN THE TUI SHALL display an error stating that the Apple Music API is unreachable, offer a retry action, and keep any content already shown in the current view.
5. WHEN the user selects the retry action, THE API_Client SHALL resend the failed request exactly once per selection, and THE TUI SHALL replace the error with the response content if the resent request succeeds.
6. IF the Apple Music API returns HTTP status 429, THEN THE API_Client SHALL resend the request after the number of seconds given in the Retry-After header, or after 1 second if the header is missing or not a non-negative integer, with a maximum wait of 60 seconds per attempt and a maximum of 3 retry attempts per request.
7. IF the Apple Music API still returns HTTP status 429 after the third retry attempt, THEN THE TUI SHALL display an error stating that the Apple Music API rate limit was exceeded and offer a retry action.
8. IF the Apple Music API returns an HTTP status in the 500–599 range, or a status in the 400–499 range other than 401, 403, and 429, THEN THE TUI SHALL display an error stating the HTTP status code and the request that failed, offer a retry action, and keep any content already shown in the current view.
9. WHILE the network is unreachable, THE TUI SHALL process each key press in the views and the Now_Playing_Bar and render the result within 100 ms of the key press.
10. WHEN the terminal window is resized, THE TUI SHALL redraw all views and the Now_Playing_Bar to fit the new column and row dimensions within 100 ms of the resize event.
11. IF the TUI encounters an unrecoverable error (an unhandled exception or crash in the TUI process), THEN THE TUI SHALL restore the terminal to the mode it was in before the TUI started, write the error description to the log file at `$XDG_STATE_HOME/apple-music-tui/app.log` with token values excluded, and exit with a non-zero status code.
