# Restore native Linux compilation

## Goal
Fix the build-script dependency mismatch that prevents this checkout from compiling on Linux, while preserving Windows executable-icon support.

## Current context / assumptions

- Workspace: `/home/npepper/development/crestron-load-runner`; branch `main`; inspected HEAD `75e0b484f5c576c4b96dfee4cc32125ae90befbe`. The worktree was clean before this plan was written.
- This is a Rust 2024 native eframe/egui binary. `src/main.rs` starts the GUI; `src/app.rs` owns the application. SSH/device functionality is unrelated to this fix and must remain untouched.
- The inspected host is Linux (`rustc --print cfg` reports `target_os="linux"`), with Rust/Cargo 1.95.0.
- **Concrete source-level defect:** `Cargo.toml:23–24` declares `winresource` only under `[target."cfg(windows)".build-dependencies]`, but `build.rs:26` unconditionally references `winresource::WindowsResource`.
- `build.rs:18–20` returns early when `CARGO_CFG_TARGET_OS` is not Windows. That is a runtime check in the build-script executable, not conditional compilation. Rust must resolve/type-check the later `winresource` reference before it can execute that return. Linux builds do not receive the dependency and therefore cannot compile this build script.
- `git log -p -- Cargo.toml` traces this mismatch to `1c826ed` (the introduction of the application icon), not the latest SSH-console change.
- `Cargo.lock` already contains `winresource` 0.1.31. The manifest and lockfile contain matching inspected versions of eframe, flate2, serde, and sha2; do not guess that their versions are typos or downgrade them.
- The user has not supplied their failing command or compiler output. **No compilation was run in plan mode**, because Cargo writes build artifacts and can update caches. The diagnosis is grounded in source inspection, not a reproduced compiler transcript. Other errors may appear after this first blocker is fixed.
- No repository AGENTS.md/CLAUDE.md/.cursorrules guidance was found in the inspected workspace. There is no tracked `.github` workflow directory.

## Architecture / proposed approach

Make `winresource` a normal build dependency in `Cargo.toml`, so the build-script reference resolves on every host. Keep the existing `CARGO_CFG_TARGET_OS` early return in `build.rs`: Linux targets still skip all icon generation/resource compilation, while Windows targets retain their current behavior. This is smaller than reorganizing the icon code behind conditional compilation, and does not conflate the build-script host OS with the application's target OS when cross-compiling.

## Step-by-step tasks

Commands below are for the implementer after leaving plan mode. Run from the workspace root. Do not clean `target`, update dependencies, install system packages, touch credentials, launch the GUI, or contact a Crestron device as part of this fix.

### Task 1 — Establish the compile regression (2–5 minutes, plus compiler time)

**Files inspected:** `Cargo.toml`, `build.rs`, `Cargo.lock`; no edits yet.

```sh
cd /home/npepper/development/crestron-load-runner
git status --short
git rev-parse --short HEAD
rustc --version
cargo --version
cargo check --locked --offline --all-targets
```

Expected results:

- HEAD is `75e0b48`, unless the user has since advanced it; preserve any new user edits.
- With dependencies cached, compilation fails with `E0433` at the `winresource::WindowsResource::new()` expression in `build.rs`, describing an unresolved module/unlinked crate `winresource`.
- If the offline command instead reports a missing cached dependency, that does **not** reproduce the Rust error. With network access authorized for implementation, run `cargo check --locked --all-targets`; otherwise report the dependency-cache blocker and stop.
- If a different compiler/dependency error occurs first, preserve its exact output and diagnose it before applying speculative edits. Do not claim the failure above was observed unless it actually was.

**TDD:** The failing Linux compile is the regression test here. Adding an application unit test cannot exercise a build script that fails before the test binary exists, and a string-matching manifest test would test formatting rather than behavior. Use the identical Cargo command before and after the change.

### Task 2 — Make the build dependency available to the build script (2–5 minutes)

**Only implementation file:** `Cargo.toml`.

Replace the final build-dependency section with exactly:

```toml
[build-dependencies]
flate2 = "1.1.10"
winresource = "0.1"
```

Equivalently, the entire intended implementation diff is:

```diff
 [build-dependencies]
 flate2 = "1.1.10"
-
-[target."cfg(windows)".build-dependencies]
 winresource = "0.1"
```

Do not change `build.rs`, `src/logo.rs`, dependency versions, or runtime application code. Do not replace the existing runtime OS check with `cfg!(windows)`; that also does not exclude code from type checking.

Run the same regression command:

```sh
cargo check --locked --offline --all-targets
git diff --check
git diff -- Cargo.toml Cargo.lock build.rs
```

Expected: exit 0 from the check and whitespace validation; no `E0433`; only the manifest change above. If task 1 required the online command, use `cargo check --locked --all-targets` instead. `Cargo.lock` and `build.rs` should remain unchanged. If Cargo says the lockfile needs updating, investigate that explicitly rather than dropping `--locked` without explanation.

If further errors are exposed, capture their exact diagnostics and extend the plan with evidence-backed fixes before expanding scope. A successful manifest edit alone is not completion.

### Task 3 — Validate executable compilation and existing tests (2–5 minutes of focused work, plus build/test time)

**Files under validation:** `Cargo.toml`, `build.rs`, all existing `src/*.rs` tests and `tests/fixtures/ip_table.txt`; no new test files required.

```sh
cargo build --locked --offline
cargo build --locked --offline --release
cargo test --locked --offline
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
```

Expected:

- Both debug and release builds finish successfully; this verifies compilation and linking without opening the GUI or invoking device discovery.
- The existing test suite exits 0, with zero failures. HEAD's commit message reports 146 passing tests and two ignored live-network tests; this is a historical reference, **not** a newly verified count. Report the actual result.
- Leave live-network tests ignored: do not pass `--ignored` or set `CRESTRON_SSH_PROBE_HOST`. Ordinary SSH tests use a loopback test server and may require a sandbox that permits loopback sockets.
- Formatting and Clippy exit 0. Report pre-existing failures separately instead of doing unrelated cleanup.
- If offline caches remain insufficient, use the same commands without `--offline` only when network access is authorized. Missing Rust components or native libraries are environment blockers to report, not justification for broad source changes.

**Windows preservation check, if a Windows runner is available:** on a native Windows checkout containing this change, run:

```sh
cargo check --locked --all-targets
cargo build --locked --release
```

Expected: exit 0. When the resource compiler is absent, the current documented warning about the executable icon is acceptable; successful resource embedding cannot be claimed from that warning. No Windows runner was inspected in this planning turn. If none is available, state that Windows compilation was not executed rather than claiming cross-platform verification. Do not add a new CI platform or install a cross-toolchain merely to land this narrowly scoped fix.

### Task 4 — Review and checkpoint the fix (2–5 minutes)

**File staged:** `Cargo.toml` only.

```sh
git diff --check
git diff --stat
git diff -- Cargo.toml Cargo.lock build.rs
git status --short
```

Expected: the implementation diff only removes the target-specific table boundary from `Cargo.toml`; the plan is separate. No unintended lockfile or application-source changes.

After successful verification, and only if commits are authorized for the implementation session:

```sh
git add -- Cargo.toml
git commit -m "Fix Linux build script dependency on winresource"
git show --stat --oneline HEAD
git status --short
```

Expected: one focused fix commit containing only `Cargo.toml`. Do not stage the entire worktree, commit this plan automatically, or push. This fix warrants one atomic commit, not a deliberately broken intermediate commit. If commits are not authorized, leave the verified edit uncommitted.

## Tests / validation summary

The red/green test is `cargo check --locked --offline --all-targets` on Linux before/after the manifest edit. Debug/release builds test linking, existing unit tests test runtime regressions without accessing real hardware, and formatting/Clippy test repository conventions. Acceptance requires real successful Linux compilation and actual reported test outcomes, not merely removal of the predicted error message.

## Risks, tradeoffs, and open questions

- **Exact user failure is unknown:** the source defect explains native Linux compilation, but a Windows-only failure or a different command may have a separate cause. Request the exact command and complete first error only if implementation reproduction does not match.
- **Dependency cost:** non-Windows hosts now compile the resource-helper crate even though non-Windows targets skip its runtime work. This is the deliberate small cost of a minimal fix that preserves cross-target behavior; do not reorganize icon generation merely to optimize build time.
- **Host versus target:** build scripts execute on the host. Blindly wrapping the icon code in `#[cfg(windows)]` checks that host and can skip Windows-target icon generation when cross-compiling from Linux. Keeping target detection via `CARGO_CFG_TARGET_OS` avoids that change in behavior.
- **Additional blockers:** source inspection cannot prove the rest of the application compiles. Missing caches/toolchains/native dependencies and subsequent Rust diagnostics must be reported honestly and handled based on actual output.
- **Windows behavior:** the implementation leaves resource logic untouched, but native Windows verification remains conditional on runner availability.
- **No scope creep:** no SSH changes, GUI changes, dependency upgrades, icon rewrites, CI setup, credential reads, or production-device interaction.
