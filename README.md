# Crestron Load Runner

A native Rust/egui utility for discovering Crestron devices, keeping an address book, inspecting devices over SSH, and loading processor or touchpanel projects over SFTP.

## Features

- Crestron UDP autodiscovery on port 41794
- Manual and autodiscovery-to-address-book workflows with processor/touchpanel classification
- Device-list text search across model, hostname, IP address, MAC address, and firmware
- Address-book import and export as portable JSON files
- Dedicated background worker thread per SSH device, each driving an asynchronous `russh` session
- App-specific trust-on-first-use SSH host-key verification, with **Forget SSH host key** on a device's right-click menu to ask again
- `--config-dir` for an isolated settings, address-book, and firmware profile
- Persistent `.lpz` processor assignments for program slots 1–10
- Program signatures uploaded automatically: a `.sig` file beside the assigned `.lpz` is sent to the same slot directory as `.zig`
- Persistent processor configuration-file assignments for slots 1–10
- Persistent `.vtz` touchpanel project assignments
- Network, program, IP table, and optional Cresnet detail views
- Success/fail indicator on each device card, with the operation's own text kept in the device log
- In-memory device log of every line sent to and received from devices, opened with **Device log…** on the details panel
- Multi-device loading from the top action bar: **Load Assigned Program**, **Load Assigned Config**, **Load Assigned Touchpanel**, and **Load Firmware**
- Resizable device/details split with a one-third initial device-list width
- Saved default SSH credentials, with per-device credentials taking precedence
- Firmware editor with a persistent per-model catalog and managed local firmware copies

## Build and run

```sh
cargo run --release
```

### Command-line options

- `--config-dir <DIR>`: keep settings, the address book, and the firmware library in `DIR` instead of the platform user configuration directory. Use it for a throwaway profile — screenshots, demos, or trying an address book — without touching the real one. The directory is created on first save.
- `-h`, `--help`: print usage and exit.

The current address book is stored in the platform user configuration directory, or in `--config-dir` when that is given. Each device's trusted SSH host-key fingerprint is stored on that device's address-book entry, so it is included in portable address-book JSON files alongside assigned program, configuration, and touchpanel file paths. Address-book changes remain in memory until saved; the status bar appends `*` to the current file while it has unsaved changes and reports load/save results without opening a notice dialog. Per-device passwords are held in memory only. A default username and password can be saved in **File → Preferences** and are used when the corresponding per-device credential is blank.

## Saving and operation safety

- Opening another address book or exiting (including the window close button) prompts **Save / Discard / Cancel** when there are unsaved changes. Failed saves leave the confirmation open.
- Device removal, address-book switching, and exit are blocked while device operations are queued or running. Wait for them to finish; there is no force-cancel during an upload.
- Manual additions reuse matching discovered endpoints. Imports reject duplicate host/port pairs. Rediscovery updates changed IP addresses on discovered-only cards without rewriting manually configured hosts.
- Portable JSON files cannot overwrite the internal configuration file, including through symlink aliases. Trusting a key on an address-book device marks the address book as changed; save it to persist the fingerprint. A discovered-only device's key remains trusted for the session and is persisted if the device is subsequently added to the address book and saved.
- SSH handshake, authentication, and SFTP calls have a 20-second blocking-call timeout. Load-command calls allow up to five minutes. A timeout does not prove a device-side load stopped; check the device before retrying.

## Firmware library

Open **File → Firmware Editor…** and select a discovered model or enter a model manually, then choose **Choose firmware file…** to assign or replace its firmware. Models are remembered across restarts and are not removed by **Clear Devices**. The editor does not infer models from filenames.

Files are copied and SHA-256 verified in a background worker. The catalog is saved automatically, independently of address-book edits, with each model's original filename, relative stored filename, byte count, and SHA-256 checksum. You can move or delete the original source file after a successful import. **Remove assignment** also deletes the stored copy; content shared by another model is retained until its last assignment is removed.

Select address-book targets and choose **Load Firmware** to upload the firmware assigned to each target's model. PUF files are uploaded to the device firmware directory and applied with the Crestron `puf` command; ZIP updates use `pushupdate full`. Firmware installation can restart or temporarily disconnect a device, so verify the model assignment before loading.

Storage is a `firmware` subdirectory beside the local `address-book.json` settings file, so `--config-dir` moves it too:

- Linux: `~/.config/crestronloadrunner/firmware/` (or `$XDG_CONFIG_HOME/crestronloadrunner/firmware/`)
- Windows: `%APPDATA%\WorldDomination\CrestronLoadRunner\config\firmware\`

This directory contains `catalog.json` and checksum-named `.firmware` copies. Their contents are unchanged; the original filenames are recorded in the JSON. Back up the entire directory, not just the catalog. The editor displays its storage location and reports missing files or import/save errors. Exiting is blocked until an active import finishes.

## Device commands

The application uses Crestron console commands over SSH:

- Details: `hostname`, `ver`, `ipconfig`, `proginf`, `ipt -t`, `REPORTCRESNET`
- Processor load: SFTP upload into `/program<NN>` for the target slot, with the program's `.sig` file uploaded alongside it as `.zig`, followed by `progload -p:<slot>`
- Configuration load: SFTP upload into `/user`; no console command is issued
- Touchpanel load: SFTP upload into the panel's `/display` directory followed by `projectload`

Command availability and output vary by Crestron firmware generation. Verify load behavior on a non-production device before deploying broadly.

The SFTP namespace is not the one the SSH console presents. A program the console addresses as `\SIMPLpp01` is written over SFTP to `/program01`, so these paths are SFTP paths and cannot be read off a console directory listing.

## Device log

Every console command sent to a device and every response it returns is recorded in memory for the session, along with app-side notes for connection attempts, SFTP transfers and failures. Open it with **Device log…** on the Device details panel. The window filters to the selected device by default, and offers **Copy all** and **Clear**.

A device card shows only whether the last operation succeeded or failed; the text it produced goes to the log. Hovering the indicator shows the last message.

The log holds 2000 entries, dropping the oldest and reporting how many were dropped, and truncates any single entry over 8 KB. It is never written to disk, and timestamps are UTC because resolving a local time zone would mean adding a dependency.

## SSH implementation

SSH and SFTP use [`russh`](https://crates.io/crates/russh) with the `ring` backend: a pure-Rust client that needs no OpenSSL, Perl, or NASM to build. This matters for Crestron hardware. 4-Series devices advertise `ssh-rsa` alongside `ecdsa-sha2-nistp256` but some of them only complete a handshake with the ECDSA key, and the previous libssh2 client fell back to Windows CNG, whose host-key support is RSA-only. It therefore negotiated the one algorithm such a device could not honor, the device closed the connection, and the failure surfaced as `Unable to exchange encryption keys`.

Because the negotiated host key depends on which algorithms the client supports, a fingerprint trusted by an older build of this app can stop matching even though the device is unchanged — a device commonly holds several host keys. That is reported as a changed host key and the connection is refused; use **Forget SSH host key** on the device's right-click menu and trust the new fingerprint when prompted.

## Security

The first connection presents the device's SHA-256 host-key fingerprint. Trust it only after comparing it with a known-good fingerprint. A changed key is rejected. Per-device passwords are never written to portable address-book files; the optional default password is stored in the local application settings as plain text.
