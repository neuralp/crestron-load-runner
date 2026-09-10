# Crestron Load Runner

A native Rust/egui utility for discovering Crestron devices, keeping an address book, inspecting devices over SSH, and loading processor or touchpanel projects over SFTP.

## Features

- Crestron UDP autodiscovery on port 41794
- Manual and autodiscovery-to-address-book workflows with processor/touchpanel classification
- Address-book import and export as portable JSON files
- Dedicated background worker thread per SSH device
- App-specific trust-on-first-use SSH host-key verification
- Persistent `.lpz` processor assignments for program slots 1–10
- Persistent processor configuration-file assignments for slots 1–10
- Persistent `.vtz` touchpanel project assignments
- Network, program, IP table, and optional Cresnet detail views
- Multi-device loading from the top action bar
- Resizable device/details split with a one-third initial device-list width
- Saved default SSH credentials, with per-device credentials taking precedence
- Firmware editor with a persistent per-model catalog and managed local firmware copies

## Build and run

```sh
cargo run --release
```

The current address book is stored in the platform user configuration directory. Each device's trusted SSH host-key fingerprint is stored on that device's address-book entry, so it is included in portable address-book JSON files alongside assigned program, configuration, and touchpanel file paths. Address-book changes remain in memory until saved; the status bar appends `*` to the current file while it has unsaved changes and reports load/save results without opening a notice dialog. Per-device passwords are held in memory only. A default username and password can be saved in **File → Preferences** and are used when the corresponding per-device credential is blank.

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

Storage is a `firmware` subdirectory beside the local `address-book.json` settings file:

- Linux: `~/.config/crestronloadrunner/firmware/` (or `$XDG_CONFIG_HOME/crestronloadrunner/firmware/`)
- Windows: `%APPDATA%\WorldDomination\CrestronLoadRunner\config\firmware\`

This directory contains `catalog.json` and checksum-named `.firmware` copies. Their contents are unchanged; the original filenames are recorded in the JSON. Back up the entire directory, not just the catalog. The editor displays its storage location and reports missing files or import/save errors. Exiting is blocked until an active import finishes.

## Device commands

The application uses Crestron console commands over SSH:

- Details: `hostname`, `ver`, `ipconfig`, `proginf`, `ipt -t`, `REPORTCRESNET`
- Processor load: SFTP upload followed by `progload -p:<slot> <file>`
- Touchpanel load: SFTP upload followed by `projectload <file>`

Command availability and output vary by Crestron firmware generation. Verify load behavior on a non-production device before deploying broadly.

## Security

The first connection presents the device's SHA-256 host-key fingerprint. Trust it only after comparing it with a known-good fingerprint. A changed key is rejected. Per-device passwords are never written to portable address-book files; the optional default password is stored in the local application settings as plain text.
