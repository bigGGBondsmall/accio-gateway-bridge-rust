# Desktop verification — 2026-11-02

## Passed in isolated Linux cloud workspace

- Full native `cargo build --locked` (linked Tauri desktop executable, not only `cargo check`).
- `cargo test --locked`: 9 tests covering readiness, spawn failure, crash state, management allowlist, URL normalization rejection, encoded model IDs, and idempotent explicit cleanup while a reader retains another Arc.
- `cargo clippy --locked -- -D warnings`.
- UI `npm test`: 16 tests. One exercises the actual local Rust gateway through DOM UI handlers: connect, add/edit/default/disable/delete model, provider-group destination/key safety, no saved-key disclosure, no persisted runtime management token. Other tests include search/empty states, cancel, XSS-safe text, settings, local discovery prefill, outage, and mocked launch confirmation.
- UI `npm run check` (Prettier), `node build.mjs`.
- Runtime sidecar copied from verified core release, SHA-256: `4e2cde65c00ab9a6bfa8c0efacb031d373074530d57455bf364506f0c0799fe1`. The generated desktop debug directory contains the same sidecar hash. Source distribution excludes binaries; the preparation script rebuilds the sidecar from source with `--locked`.

Missing Linux build dependencies were recovered without system installation by downloading official signed Debian snapshot development/runtime packages, extracting under `/tmp/accio-sysroot`, and supplying `PKG_CONFIG_PATH`, `PKG_CONFIG_SYSROOT_DIR`, and `LIBRARY_PATH`. These temporary dependencies are not included in the source delivery.

## Not established by these checks

- **No Windows installer or WebView2 runtime test has run.** The manual-only `.github/workflows/windows-build.yml` provides a future Windows build/test/unsigned-NSIS artifact path. It was not pushed or triggered.
- **No desktop GUI launch, visual screenshot, or live Accio launch has been verified.** The supported cloud browser could not reach the isolated executor's localhost; a self-contained data preview was refused by browser URL policy. No alternate rendering route was used after that denial. DOM tests are not visual QA.
- No real account login, provider inference, or billable model request occurred. The Accio confirmation test uses a mock, and local discovery fixtures do not contact external providers.
- Process startup/crash state and explicit child ownership cleanup are unit-tested (the cleanup regression uses a fake termination callback, not a launched GUI); actual Tauri-window close → child-process termination is implemented and statically reviewed, but not established by a native GUI test.
- Provider keys remain in the local configuration file, not in an OS credential vault. Management tokens stay in Rust process memory and are not returned to the desktop JavaScript.
