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

## Build and run

```sh
cargo run --release
```

The current address book and trusted host-key fingerprints are stored in the platform user configuration directory. Address books can also be saved to and loaded from portable JSON files. Assigned program, configuration, and touchpanel file paths are part of the address-book data. Address-book changes remain in memory until saved; the status bar appends `*` to the current file while it has unsaved changes and reports load/save results without opening a notice dialog. Passwords are intentionally held in memory only and must be entered once per session.

## Saving and operation safety

- Opening another address book or exiting (including the window close button) prompts **Save / Discard / Cancel** when there are unsaved changes. Failed saves leave the confirmation open.
- Device removal, address-book switching, and exit are blocked while device operations are queued or running. Wait for them to finish; there is no force-cancel during an upload.
- Manual additions reuse matching discovered endpoints. Imports reject duplicate host/port pairs. Rediscovery updates changed IP addresses on discovered-only cards without rewriting manually configured hosts.
- Portable JSON files cannot overwrite the internal configuration file, including through symlink aliases. Trusting an SSH key persists only trust changes, not unsaved device edits.
- SSH handshake, authentication, and SFTP calls have a 20-second blocking-call timeout. Load-command calls allow up to five minutes. A timeout does not prove a device-side load stopped; check the device before retrying.

## Device commands

The application uses Crestron console commands over SSH:

- Details: `hostname`, `ver`, `ipconfig`, `proginf`, `ipt -t`, `REPORTCRESNET`
- Processor load: SFTP upload followed by `progload -p:<slot> <file>`
- Touchpanel load: SFTP upload followed by `projectload <file>`

Command availability and output vary by Crestron firmware generation. Verify load behavior on a non-production device before deploying broadly.

## Security

The first connection presents the device's SHA-256 host-key fingerprint. Trust it only after comparing it with a known-good fingerprint. A changed key is rejected. Passwords are never written to the address-book file.
