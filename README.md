# Crestron Load Runner

A native Rust/egui utility for discovering Crestron devices, keeping an address book, inspecting devices over SSH, and loading processor or touchpanel projects over SFTP.

## Features

- Crestron UDP autodiscovery on port 41794
- Manual and autodiscovery-to-address-book workflows with processor/touchpanel classification
- Device-list text search across model, hostname, IP address, MAC address, and firmware
- The address book is an ordinary JSON file you name and own: **New**, **Open**, **Save**, **Save as…**, and an **Open Recent** list of the five most recent books
- Dedicated background worker thread per SSH device, each driving an asynchronous `russh` session
- App-specific trust-on-first-use SSH host-key verification, with **Forget SSH host key** on a device's right-click menu to ask again
- `--config-dir` for an isolated preferences and firmware profile
- Persistent `.lpz` processor assignments for program slots 1–10
- Program signatures uploaded automatically: a `.sig` file beside the assigned `.lpz` is sent to the same slot directory as `.zig`
- Persistent processor configuration-file assignments for slots 1–10
- Persistent `.vtz` touchpanel project assignments
- Network, program, IP table, and optional Cresnet detail views
- Success/fail indicator on each device card, with the operation's own text kept in the device log
- In-memory device log of every line sent to and received from devices, opened with **Device log…** on the details panel
- Multi-device loading from the top action bar: **Load Assigned Program**, **Load Assigned Config**, **Load Assigned Touchpanel**, and **Load Firmware**
- Resizable device/details split with a one-third initial device-list width
- Saved default SSH credentials, with per-device credentials taking precedence, and a choice of what to open at startup
- Firmware editor with a persistent per-model catalog and managed local firmware copies

## Build and run

```sh
cargo run --release
```

### Command-line options

- `--config-dir <DIR>`: keep the preferences and the firmware library in `DIR` instead of the platform user configuration directory. Use it for a throwaway profile — screenshots, demos, or trying an address book — without touching the real preferences. The directory is created on first save.
- `-h`, `--help`: print usage and exit.

## The address book

The address book is a JSON file you choose. Nothing about it is kept in the configuration directory: **File → New address book**, **Open address book…**, **Save address book**, and **Save address book as…** behave as they do in any editor, and saving a book that has never been written asks where to put it. **File → Open Recent** lists the five most recently opened or saved books, newest first; an entry that no longer exists is reported and dropped from the list, while one that is merely malformed stays so it can be repaired.

Each device's trusted SSH host-key fingerprint is stored on that device's entry, so it travels with the file alongside assigned program, configuration, and touchpanel file paths. Changes stay in memory until saved; the status bar names the current file, shows `Untitled` before there is one, appends `*` while there are unsaved changes, and reports load/save results without opening a notice dialog. Per-device passwords are held in memory only.

**File → Preferences** holds a default username and password, used when the corresponding per-device credential is blank, and a choice of what to open at startup: start with an empty address book, reopen the most recent one, or always open a specific file. A specific file that has gone missing is reported in the status bar and left set as the preference, because it may be on a share that is offline rather than deleted.

Preferences live in `preferences.json` in the platform user configuration directory, or in `--config-dir` when that is given.

> **Upgrading from a build before 2026.9.10.** Earlier versions kept the address book inside the configuration directory, in `address-book.json` beside the settings. This build cannot read that file, and leaves it in place. To recover its devices, copy it, rename the `"address_book"` key to `"devices"`, add `"version": 1` alongside it, and open the result. The default username and password are not carried across and need entering once in **Preferences**.

## Saving and operation safety

- Starting a new address book, opening another, or exiting (including the window close button) prompts **Save / Discard / Cancel** when there are unsaved changes. Failed saves leave the confirmation open, and so does cancelling the dialog that asks where an untitled book should go.
- Device removal, address-book switching, and exit are blocked while device operations are queued or running. Wait for them to finish; there is no force-cancel during an upload.
- Autodiscovered devices are not part of the document, so they stay on screen across **New** and **Open**.
- Every save is read back and compared with what is in memory before it is called a success — the file is the only copy of the data.
- Manual additions reuse matching discovered endpoints. Loading a book rejects duplicate host/port pairs. Rediscovery updates changed IP addresses on discovered-only cards without rewriting manually configured hosts.
- An address book cannot overwrite the preferences file or anything in the firmware storage directory, including through symlink aliases. Trusting a key on an address-book device marks the address book as changed; save it to persist the fingerprint. A discovered-only device's key remains trusted for the session and is persisted if the device is subsequently added to the address book and saved.
- SSH handshake, authentication, and SFTP calls have a 20-second blocking-call timeout. Load-command calls allow up to five minutes. A timeout does not prove a device-side load stopped; check the device before retrying.

## Firmware library

Open **Devices → Firmware Editor…** and select a discovered model or enter a model manually, then choose **Choose firmware file…** to assign or replace its firmware. Models are remembered across restarts and are not removed by **Clear Devices**. The editor does not infer models from filenames.

Files are copied and SHA-256 verified in a background worker. The catalog is saved automatically, independently of address-book edits, with each model's original filename, relative stored filename, byte count, and SHA-256 checksum. You can move or delete the original source file after a successful import. **Remove assignment** also deletes the stored copy; content shared by another model is retained until its last assignment is removed.

Select address-book targets and choose **Load Firmware** to upload the firmware assigned to each target's model. PUF files are uploaded to the device firmware directory and applied with the Crestron `puf` command; ZIP updates use `pushupdate full`. Firmware installation can restart or temporarily disconnect a device, so verify the model assignment before loading.

Storage is a `firmware` subdirectory beside the `preferences.json` file, so `--config-dir` moves it too:

- Linux: `~/.config/crestronloadrunner/firmware/` (or `$XDG_CONFIG_HOME/crestronloadrunner/firmware/`)
- Windows: `%APPDATA%\WorldDomination\CrestronLoadRunner\config\firmware\`

This directory contains `catalog.json` and checksum-named `.firmware` copies. Their contents are unchanged; the original filenames are recorded in the JSON. Back up the entire directory, not just the catalog. The editor displays its storage location and reports missing files or import/save errors. Exiting is blocked until an active import finishes.

## Scripts

Open **Devices → Script Editor…**, choose **New script**, give it a unique name, and enter Crestron console commands, one per line. Blank lines and full-line `#` comments are ignored. **Save scripts** saves the whole library, including edits, renames, and deletions; **Discard changes** restores the saved library. Closing the editor keeps drafts in memory, and quitting requires saving or discarding them. Only saved scripts can run.

Select address-book devices with their **Target** checkboxes, then click **Run Script** after **Load Firmware** in the top bar. Choose a saved script, fill in any variables, review the rendered commands for every target, and click **Run on these devices**. Alternatively, right-click a device and choose **Run Script…** to target only that device, regardless of the checkboxes. The preview snapshots both the targets and script; later selection or editor changes do not alter that run.

Templates use `{{variable}}` substitutions. Built-ins are `{{device.name}}` (display name, falling back to the host), `{{device.host}}`, `{{device.port}}`, `{{device.model}}`, `{{device.mac}}`, `{{device.firmware}}`, and `{{device.kind}}`. Other names, such as `{{room}}`, become run-time input fields shared across that run's targets. The `device.` prefix is reserved; misspelled built-ins are rejected. Missing or empty values and control characters block execution. Values are inserted literally, without quoting or recursive expansion: review the preview, especially spaces or command separators. There are no loops, conditionals, or local shell execution.

Scripts run sequentially on each device over one authenticated SSH connection, using a separate exec channel per command. Different devices use their existing independent worker queues. Interactive prompts and persistent shell state between commands are not supported. Each command has a 20-second timeout. An SSH error, timeout, or nonzero remote exit status stops the remaining commands on that device, without rolling back earlier commands or stopping other devices. Some Crestron firmware reports command errors only as text without a failing exit status; inspect the device log. A timeout does not prove the remote command stopped. Host-key verification remains required; after trusting a new key, review and run the script again.

The library is `scripts.json` beside `preferences.json`, so `--config-dir` also isolates scripts. Saves use a temporary file, replacement, and read-back verification. Unreadable libraries are not overwritten, and address-book saves cannot overwrite the script library. Scripts and command logs are plain text: do not put passwords or other secrets in them. Run-time variable values are not saved in the library, but rendered commands are logged.

## Device commands

The application uses Crestron console commands over SSH:

- Details: `hostname`, `ver`, `ipconfig`, `proginfo`, `ipt -t`, `REPORTCRESNET`
- Processor load: SFTP upload into `/program<NN>` for the target slot, with the program's `.sig` file uploaded alongside it as `.zig`, followed by `progload -p:<slot>`
- Configuration load: SFTP upload into `/user`; no console command is issued
- Touchpanel load: SFTP upload into the panel's `/display` directory followed by `projectload`

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

For a handshake-only diagnostic (no authentication or device commands), run `CRESTRON_SSH_PROBE_HOST=<host> cargo test live_ssh_handshake -- --ignored --nocapture`. With no trusted fingerprint it stops at host-key verification and prints the offered fingerprint. Set `CRESTRON_SSH_PROBE_FINGERPRINT` to a verified fingerprint to exercise the complete handshake. The probe does not save trust or change the address book.

## Security

The first connection presents the device's SHA-256 host-key fingerprint. Trust it only after comparing it with a known-good fingerprint. A changed key is rejected. Per-device passwords are never written to an address-book file; the optional default password is stored in `preferences.json` as plain text.
