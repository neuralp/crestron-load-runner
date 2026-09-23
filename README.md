# Crestron Load Runner

A native Rust/egui utility for discovering Crestron devices, keeping an address book, inspecting devices over SSH, and loading processor or touchpanel projects over SFTP.

## Features

- Crestron UDP autodiscovery on port 41794
- Manual and autodiscovery-to-address-book workflows with processor/touchpanel classification
- Device-list text search across model, hostname, IP address, MAC address, and firmware
- The address book is an ordinary JSON file you name and own: **New**, **Open**, **Save**, **Save as…**, and an **Open Recent** list of the five most recent books
- Dedicated background worker thread per SSH device, each holding one `russh` session open and shared by everything that device does
- App-specific trust-on-first-use SSH host-key verification, with **Forget SSH host key** on a device's right-click menu to ask again
- `--config-dir` for an isolated preferences and firmware profile
- Persistent `.lpz` processor assignments for program slots 1–10
- Program signatures uploaded automatically: a `.sig` file beside the assigned `.lpz` is sent to the same slot directory as `.zig`
- Persistent processor configuration-file assignments for slots 1–10
- Persistent `.vtz` touchpanel project assignments
- Network, program, IP table, and optional Cresnet detail views
- Running program summary drawn from `progcomments`: source file, program file, compile time, and programmer
- Cresnet report parsed into a table of ID, model, firmware, and serial, for processors
- Success/fail indicator on each device card, with the operation's own text kept in the device log
- In-memory device log of every line sent to and received from devices, opened with **Device log…** on the details panel
- Interactive SSH console for one device in its own window, opened with **Connect SSH…**
- Multi-device loading from the top action bar: **Load Assigned Program**, **Load Assigned Config**, **Load Assigned Touchpanel**, and **Load Firmware**
- Resizable device/details split with a one-third initial device-list width
- Script and firmware editors open as separate operating-system windows, so they stay usable beside the main one
- Dialogs that block the main window frost and dim what is behind them
- A drawn application mark, used in **Help → About**, on the window, and as the executable's icon
- Saved default SSH credentials, with per-device credentials taking precedence, a session-only prompt when neither supplies one, and a choice of what to open at startup
- Firmware editor with a persistent per-model catalog, managed local firmware copies, and `.puf` package details read from the file

## Build and run

```sh
cargo run --release
```

### The application mark

The mark is described in `src/logo.rs` as three triangles in a unit square, not stored as an image. The application draws it in **Help → About** and rasters it for the window icon; `build.rs` includes the same file to raster the executable's icon into a six-size `.ico`, so the two cannot drift apart and there is no image file to keep in step with the source.

Attaching an icon to the executable needs a resource compiler from the Windows SDK. Without one the build prints a warning and carries on: only the icon on the file in Explorer is missing, and the running window still shows the mark.

### Command-line options

- `--config-dir <DIR>`: keep the preferences and the firmware library in `DIR` instead of the platform user configuration directory. Use it for a throwaway profile — screenshots, demos, or trying an address book — without touching the real preferences. The directory is created on first save.
- `-h`, `--help`: print usage and exit.

## The address book

The address book is a JSON file you choose. Nothing about it is kept in the configuration directory: **File → New address book**, **Open address book…**, **Save address book**, and **Save address book as…** behave as they do in any editor, and saving a book that has never been written asks where to put it. **File → Open Recent** lists the five most recently opened or saved books, newest first; an entry that no longer exists is reported and dropped from the list, while one that is merely malformed stays so it can be repaired.

Each device's trusted SSH host-key fingerprint is stored on that device's entry, so it travels with the file alongside assigned program, configuration, and touchpanel file paths. An assigned file that sits in the book's own folder, or below it, is stored as a path relative to the book and spelled with forward slashes, so a book and the files it names can be moved together — onto another machine, another drive letter, or a memory stick — and still find them. A file kept anywhere else can only be named in full, so its complete path is what travels. Reading does the reverse: a relative path in a book is resolved against the book's own folder rather than against whatever directory the application happens to have been started in, so an existing book that already held relative paths is now read the way it reads. Saving always names the files from whichever book is being written, so **Save address book as…** into another folder rewrites them for their new home; what the application is working with stays absolute throughout. Changes stay in memory until saved; the status bar names the current file, shows `Untitled` before there is one, appends `*` while there are unsaved changes, and reports load/save results without opening a notice dialog. Per-device passwords are held in memory only.

**File → Preferences** holds a default username and password, used when the corresponding per-device credential is blank, and a choice of what to open at startup: start with an empty address book, reopen the most recent one, or always open a specific file. A specific file that has gone missing is reported in the status bar and left set as the preference, because it may be on a share that is offline rather than deleted.

When a device has no username or password of its own and the defaults above are blank, connecting asks for a **session username and password** rather than failing. They are used for every device that has none of its own, for as long as the application is running: they are never written to disk, and they survive opening another address book, which does discard per-device passwords. The operation that raised the prompt runs as soon as it is answered. A device that refuses what was sent asks again, but only when the prompt is what supplied it — a wrong per-device or default password is corrected where it is kept. **Preferences** shows whether session credentials are set and clears them, which makes the next connection ask again.

Preferences live in `preferences.json` in the platform user configuration directory, or in `--config-dir` when that is given.

> **Upgrading from a build before 2026.9.10.** Earlier versions kept the address book inside the configuration directory, in `address-book.json` beside the settings. This build cannot read that file, and leaves it in place. To recover its devices, copy it, rename the `"address_book"` key to `"devices"`, add `"version": 1` alongside it, and open the result. The default username and password are not carried across and need entering once in **Preferences**.

## Saving and operation safety

- Starting a new address book, opening another, or exiting (including the window close button) prompts **Save / Discard / Cancel** when there are unsaved changes. The prompt names what is unsaved. Exiting also covers unsaved scripts, so **Save and quit** writes the address book and the script library, and **Discard all changes and quit** abandons both. Failed saves leave the confirmation open, and so does cancelling the dialog that asks where an untitled book should go.
- Device removal, address-book switching, and exit are blocked while device operations are queued or running. Wait for them to finish; there is no force-cancel during an upload.
- Autodiscovered devices are not part of the document, so they stay on screen across **New** and **Open**.
- Every save is read back and compared with what is in memory before it is called a success — the file is the only copy of the data.
- Manual additions reuse matching discovered endpoints. Loading a book rejects duplicate host/port pairs. Rediscovery updates changed IP addresses on discovered-only cards without rewriting manually configured hosts.
- An address book cannot overwrite the preferences file or anything in the firmware storage directory, including through symlink aliases. Trusting a key on an address-book device marks the address book as changed; save it to persist the fingerprint. A discovered-only device's key remains trusted for the session and is persisted if the device is subsequently added to the address book and saved.
- SSH handshake, authentication, and SFTP calls have a 20-second blocking-call timeout. Load-command calls allow up to five minutes. A timeout does not prove a device-side load stopped; check the device before retrying.

## Firmware library

Open **Devices → Firmware Editor…**, which opens in its own window: a tree of the catalog on the left and the selected model on the right. The tree branches on the assigned firmware file, so every model sharing a file sits under it and models with nothing assigned stay at the root. Select a model there or enter one above, then choose **Choose firmware file…** to assign or replace its firmware. Models are remembered across restarts and are not removed by **Clear Devices**. The editor does not infer models from filenames.

The detail pane reports the assigned file's name, size, import time, and SHA-256, and then reads the firmware itself. A `.puf` is a zip carrying a `~.package.ini`, whose `[Package]` section is shown as a table — the name, version, build date, and whatever else the manufacturer put there. A `.zip` update describes nothing inside, so it reports its file count and the date of its newest entry instead. Only the archive index and that one description are read, never the firmware image, and anything unreadable is reported in place rather than hidden.

Files are copied and SHA-256 verified in a background worker. The catalog is saved automatically, independently of address-book edits, with each model's original filename, relative stored filename, byte count, and SHA-256 checksum. You can move or delete the original source file after a successful import. **Remove assignment** also deletes the stored copy; content shared by another model is retained until its last assignment is removed.

Select address-book targets and choose **Load Firmware** to upload the firmware assigned to each target's model. Before any upload, the application reads `[Package] Version` from `~.package.ini`, runs a fresh `ver -v` on that device, and compares against the `PUF:` line (not the control-engine or Cab version). Only strictly newer versions proceed: downgrades and same-version reinstalls are blocked, with both versions in the error. Components are compared numerically, ignoring zero padding. Missing, malformed, or ambiguous versions and failed device queries block the load before SFTP. This also blocks plain ZIP updates without package-version metadata rather than bypassing the check. After passing, firmware is staged in the device firmware directory; a ZIP update is applied with `pushupdate full`.

A PUF is applied with `puf`, which takes no file name — the device finds what was staged. It reports as it works, then restarts, which ends the session mid-command; that is expected, and neither the dropped connection nor a missing exit status is treated as a failure. The device is then logged into again every 10 seconds for up to 15 minutes, with the wait shown on the device card. A device that never answers ends the operation as a failure; an untrusted host key ends it at once rather than retrying for the full 15 minutes. Once it is back — the port answers before the console does, so the query is repeated until it produces a report — `puf -results` is read and its component table parsed. The table goes to the device log in full, and the card shows a tally of what the device called each component's result. Nothing here decides what counts as success: the device's own wording is carried through.

Firmware installation restarts the device, so verify the model assignment before loading. Exiting and switching address books stay blocked for the whole update, including the restart wait.

Firmware images are large and can be fetched again from the vendor, so the library is kept with the local data rather than beside the preferences. On Windows that keeps it out of a roaming profile, where it would be copied back and forth at every sign-in:

- Linux: `~/.local/share/crestronloadrunner/firmware/` (or `$XDG_DATA_HOME/crestronloadrunner/firmware/`)
- Windows: `%LOCALAPPDATA%\WorldDomination\CrestronLoadRunner\data\firmware\`

`--config-dir` still gathers every stored file into the directory it names, firmware included, so a throwaway profile cannot reach the real library. A library an earlier build left beside the preferences is moved here at startup. Where the two sit on different volumes the rename cannot happen; rather than copy gigabytes during startup the application reports it in the status bar and goes on using the old directory until you move it yourself.

This directory contains `catalog.json` and checksum-named `.firmware` copies. Their contents are unchanged; the original filenames are recorded in the JSON. Back up the entire directory, not just the catalog. The editor displays its storage location and reports missing files or import/save errors. Exiting is blocked until an active import finishes.

## Scripts

Open **Devices → Script Editor…**, choose **New script**, give it a unique name, optionally a **Model**, and enter Crestron console commands, one per line. Blank lines and full-line `#` comments are ignored. The editor opens in its own window: a tree of the library on the left, the selected script on the right, and **New script**, **Delete script**, **Save scripts**, and **Discard changes** across the top. The tree branches on model, so every script sharing a model sits under it; scripts without one stay at the root. **Save scripts** saves the whole library, including edits, renames, and deletions; **Discard changes** restores the saved library. Closing the editor window keeps drafts in memory, and quitting asks whether to save or abandon them. Only saved scripts can run.

Select address-book devices with their **Target** checkboxes, then click **Run Script** after **Load Firmware** in the top bar. Choose a saved script, fill in any variables, review the rendered commands for every target, and click **Run on these devices**. Alternatively, right-click a device and choose **Run Script…** to target only that device, regardless of the checkboxes. The preview snapshots both the targets and script; later selection or editor changes do not alter that run.

**Model** is a case-insensitive wildcard pattern for the device models a script applies to: `*` matches any run of characters and `?` exactly one, so `TSW-*` covers every TSW panel and `TSW-10??` only the ten-inch ones. Run Script preselects the first script in the library whose model matches every target's discovered model; a blank model, an undiscovered model, or a mixed selection preselects the first script instead. The model only preselects. Any saved script can still be chosen and run on any target, so check the picker and the preview before running.

Templates use `{{variable}}` substitutions. Built-ins are `{{device.name}}` (display name, falling back to the host), `{{device.host}}`, `{{device.port}}`, `{{device.model}}`, `{{device.mac}}`, `{{device.firmware}}`, and `{{device.kind}}`. Other names, such as `{{room}}`, become run-time input fields shared across that run's targets. The `device.` prefix is reserved; misspelled built-ins are rejected. Missing or empty values and control characters block execution. Values are inserted literally, without quoting or recursive expansion: review the preview, especially spaces or command separators. There are no loops, conditionals, or local shell execution.

Scripts run sequentially on each device over one authenticated SSH connection, using a separate exec channel per command. Different devices use their existing independent worker queues. Interactive prompts and persistent shell state between commands are not supported. Each command has a 20-second timeout. An SSH error, timeout, or nonzero remote exit status stops the remaining commands on that device, without rolling back earlier commands or stopping other devices. Some Crestron firmware reports command errors only as text without a failing exit status; inspect the device log. A timeout does not prove the remote command stopped. Host-key verification remains required; after trusting a new key, review and run the script again.

The library is `scripts.json` beside `preferences.json`, so `--config-dir` also isolates scripts. Saves use a temporary file, replacement, and read-back verification. Unreadable libraries are not overwritten, and address-book saves cannot overwrite the script library. Scripts and command logs are plain text: do not put passwords or other secrets in them. Run-time variable values are not saved in the library, but rendered commands are logged.

## The SSH console

**Connect SSH…**, on a device's right-click menu and on the details panel, opens an interactive console for that device in its own window. It connects as it opens, using the same credentials and the same trust-on-first-use host-key check as every other operation; a device whose key has not been trusted yet asks in the main window, and the console can be opened again once it has been.

A line is sent when it is entered, and the up and down arrows walk back through what has been entered before. **Auto-scroll** keeps the newest line in view and can be turned off to read back through the output while the device is still talking. Everything typed and everything received also reaches the device log, so a console session leaves the same record as a script or a load. Closing the window, or **Disconnect**, ends the session. A window whose session has ended — by either hand — offers **Reconnect** in the same place, which opens another and keeps what the last one said above a line marking where the new one begins. Choosing **Connect SSH…** again for a device whose window is already open does the same.

The session is asked for a terminal, so a device echoes what is typed the way it would to any terminal program. This is a line console rather than a terminal emulator: colour and cursor-movement sequences are removed rather than acted on, and the scrollback is plain text that can be selected and copied. A console does not queue behind the device's other operations and does not hold up exiting, since it lasts as long as it is wanted rather than as long as a command takes. It is a channel on the device's one connection rather than a connection of its own, so it opens without a second login and can be used while a load is running. It ends when the device it belongs to does: removing the device, opening another address book, or closing the application leaves the window in place, saying so, with **Reconnect** ready.

## VC-4 virtual servers

A device whose model is **VC-4** uses the [Crestron Virtual Control REST API](https://docs.crestron.com/en-us/8314/Content/Topics/API-Reference/API-Reference.htm) for **Connect / refresh**, rather than the console commands used by physical processors. For a manually added server, set **Device model** to `VC-4` in Device details and save the address book. The device host is the server hostname/IP; its SSH port is separate from the API's HTTPS port (443 by default).

In the server's web interface, open **Settings → Tokens** and create a **read-only** token, following [Crestron's authentication instructions](https://docs.crestron.com/en-us/8314/Content/Topics/Quick-Start/Authentication.htm). Paste that token into **VC-4 API connection** in Device details, then click **Connect / refresh**. It is sent directly in the `Authorization` header, without a `Bearer` prefix. SSH credentials are not used for API access.

- **ProgramLibrary** lists the available programs, including ID, friendly name, application file, program type, compile time, notes, and tags.
- **ProgramInstances** lists the server's rooms/instances, including the instance ID, name, status, library ID, location, last-started time, runtime, and restart-required flag. Running and stopped instances are both shown, using the status returned by the server.
- Both tables have independent search and 50-row pages, with horizontal scrolling for wider columns. Hover a truncated cell to read its full value.
- Expand an instance with its arrow to retrieve **IPTableByPID** for that instance. The API path uses `ProgramInstanceId`, not the library ID or the server's collection key. The result is cached until **Refresh IP table** or a full **Connect / refresh**; expanding another instance fetches only that instance. Additional instance properties are available beneath the IP table.

Requests run sequentially on a background thread per server, with connection and request timeouts. Loading, empty tables, authentication failures, and API errors are shown separately. A full refresh discards old table data, including expanded IP tables. This feature only issues GET requests; it does not upload, start, stop, or delete VC-4 programs. Physical-processor slot assignments and load operations do not apply to VC-4. The explicit SSH console and script features remain separate from this read-only API view.

On first connection, the server's **SHA-256 HTTPS certificate fingerprint** is shown before any API token is sent. Compare it with a known-good value from the server administrator, then choose **Accept certificate and save**, or **Reject certificate**. No CA PEM is needed. Acceptance saves the exact leaf-certificate fingerprint and HTTPS endpoint in the address book and reconnects. An untitled book opens Save As; a discovered server is added to the book. This saves other pending address-book edits too. A cancelled or failed save is reported as unsaved and does not automatically reconnect.

Subsequent connections must present the accepted certificate for the same HTTPS host and port. A changed certificate blocks API access and shows both fingerprints for explicit replacement approval. Trust is based on the pinned certificate, not CA-chain, hostname, or expiry validation; TLS handshake signatures are still verified. Only approve fingerprints verified through a separate trusted channel. **Forget HTTPS certificate** removes the saved pin. SSH host-key trust remains separate. Redirects are not followed. Responses larger than 16 MiB are reported rather than truncated silently.

API tokens are stored **unencrypted in the address-book JSON**, alongside the approved HTTPS endpoint and certificate fingerprint, and restored when the book is reopened. Token edits mark the address book as modified; save normally or click **Save token**. **Forget token** clears it and saves the book. These buttons save other pending address-book edits too, add a discovered server to the book when necessary, and open Save As for an untitled book. Failed or cancelled saves remain unsaved. On Unix, address-book saves create owner-only files (mode `0600`); on Windows, protect the file with appropriate filesystem permissions. Do not share or commit a book containing tokens. Tokens are masked in the UI, redacted in debug output, and not written to device logs or preferences. Fetched data remains session-only. Changing connection settings clears cached results; connect again to use the new settings.

The IP-table API can return HTTP 200 with `StatusInfo: INVALID ID`; the app reports that explicitly rather than treating it as an empty table. Refresh ProgramInstances and retry in that case. Other unexpected response structures remain errors: diagnosing firmware-specific formats requires a redacted response JSON, never an API token.

## Device commands

The application uses Crestron console commands over SSH:

- Details: `hostname`, `ver`, `ipconfig`, `progcomments`, `ipt -t`, `REPORTCRESNET`
- Processor load: SFTP upload into `/program<NN>` for the target slot, with the program's `.sig` file uploaded alongside it as `.zig`, followed by `progload -p:<slot>`
- Configuration load: SFTP upload into `/user`; no console command is issued
- Touchpanel load: SFTP upload into the panel's `/display` directory followed by `projectload`
- Console: an interactive shell channel with a terminal requested, which is what **Connect SSH…** opens
- Firmware load: SFTP upload into `/firmware`, then `pushupdate full` for a ZIP, or `puf` for a PUF followed — after the device restarts and is reconnected to — by `puf -results`

Command availability and output vary by Crestron firmware generation. Verify load behavior on a non-production device before deploying broadly.

The SFTP namespace is not the one the SSH console presents. A program the console addresses as `\SIMPLpp01` is written over SFTP to `/program01`, so these paths are SFTP paths and cannot be read off a console directory listing.

## Device log

Every console command sent to a device and every response it returns is recorded in memory for the session, along with app-side notes for connection attempts, SFTP transfers and failures. Open it with **Device log…** on the Device details panel. The window filters to the selected device by default, and offers **Copy all** and **Clear**.

A device card shows only whether the last operation succeeded or failed; the text it produced goes to the log. Hovering the indicator shows the last message.

The log holds 2000 entries, dropping the oldest and reporting how many were dropped, and truncates any single entry over 8 KB. It is never written to disk, and timestamps are UTC because resolving a local time zone would mean adding a dependency.

## SSH implementation

SSH and SFTP use [`russh`](https://crates.io/crates/russh) with the `ring` backend: a pure-Rust client that needs no OpenSSL, Perl, or NASM to build. This matters for Crestron hardware. 4-Series devices advertise `ssh-rsa` alongside `ecdsa-sha2-nistp256` but some of them only complete a handshake with the ECDSA key, and the previous libssh2 client fell back to Windows CNG, whose host-key support is RSA-only. It therefore negotiated the one algorithm such a device could not honor, the device closed the connection, and the failure surfaced as `Unable to exchange encryption keys`.

Because the negotiated host key depends on which algorithms the client supports, a fingerprint trusted by an older build of this app can stop matching even though the device is unchanged — a device commonly holds several host keys. That is reported as a changed host key and the connection is refused; use **Forget SSH host key** on the device's right-click menu and trust the new fingerprint when prompted.

The client explicitly enables NIST ECDH key exchange ahead of DH group exchange, while retaining the modern default algorithms first. Russh 0.63 supports NIST ECDH but does not enable it by default; the default group-exchange path failed against an RMC3 with `Key exchange init failed`, while ECDH completed successfully. This compatibility setting does not enable SHA-1 key exchange or change host-key verification.

Each device is connected to once rather than once per operation. The connection is opened the first time something needs it and then held, so a refresh, a script and ten program slots are one conversation with the device instead of twelve logins; the console window shares it too. It is reopened only when the device has dropped it or when the settings that decide it — address, port, credentials, or trusted host key — have changed, which is what keeps **Forget SSH host key** honest.

A held connection sends nothing while it is idle, leaving the device free to close it on its own schedule. Two things bound that silence. The operating system's own TCP keepalive notices a device that has gone away without saying so, which SSH keepalives would otherwise be needed for and which the device's console never sees. And the connection is given up after twenty minutes unused, because a device permits only a few SSH sessions at once and shares them with every other tool on site: one this application is not using is one nobody else can have. The next operation simply opens another.

For a handshake-only diagnostic (no authentication or device commands), run `CRESTRON_SSH_PROBE_HOST=<host> cargo test live_ssh_handshake -- --ignored --nocapture`. With no trusted fingerprint it stops at host-key verification and prints the offered fingerprint. Set `CRESTRON_SSH_PROBE_FINGERPRINT` to a verified fingerprint to exercise the complete handshake. The probe does not save trust or change the address book.

## Security

The first connection presents the device's SHA-256 host-key fingerprint. Trust it only after comparing it with a known-good fingerprint. A changed key is rejected. Per-device passwords are never written to an address-book file; the optional default password is stored in `preferences.json` as plain text. Session credentials entered at the prompt are held in memory only — they are not written anywhere, and are redacted from debug output.
