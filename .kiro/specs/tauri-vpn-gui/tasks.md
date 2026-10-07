# Implementation Plan: Tauri VPN GUI

## Overview

This plan ports the existing `vpn-tui` terminal application to a Tauri desktop
application through **extraction, not rewrite**, following the hexagonal
(ports & adapters) architecture in the design. Work proceeds bottom-up and
test-driven:

1. Stand up a Cargo workspace and scaffold `vpn-core` without breaking the TUI.
2. Extract the existing modules into `vpn-core` as the domain + outbound ports +
   outbound adapters, preserving every existing unit test.
3. Build the pure application core (`VpnManagerService` + `reconcile()`).
4. Lock the core behavior down with the 15 property-based tests from the design.
5. Refactor the existing TUI binary to depend on `vpn-core`.
6. Scaffold and build the `vpn-gui` Tauri crate (composition root, commands,
   events, dialog, wizard predicate).
7. Build the React + TypeScript frontend and its tests.
8. Package and extend the installer.

Each task builds on the previous ones and ends by wiring new code into the
existing build. Test sub-tasks are marked optional with `*`. Property-based
tests use `proptest` with the `Feature: tauri-vpn-gui, Property N: ...` tag
convention and run against fake adapters.

## Tasks

- [ ] 1. Convert the repository into a Cargo workspace and scaffold `vpn-core`
  - Replace the root `Cargo.toml` package manifest with a `[workspace]` manifest declaring `members = ["vpn-core", "vpn-tui", "vpn-gui"]`, and move the current `vpn-tui` package manifest into `vpn-tui/Cargo.toml`.
  - Relocate the existing `src/` tree under `vpn-tui/src/` so the `vpn` binary still builds unchanged; preserve the `[[bin]] name = "vpn"` target and the `[profile.release]` settings at the workspace root.
  - Create the `vpn-core/` library crate (`vpn-core/Cargo.toml`, `vpn-core/src/lib.rs`) with dependencies for the domain and adapters (serde, serde_json, toml, anyhow, dirs, chrono, rusqlite with `bundled`) and explicitly **no** `ratatui` or `tauri`.
  - Verify `cargo build` succeeds for the whole workspace and the TUI binary still runs.
  - _Requirements: 1.1, 1.3, 2.1, 2.2_

- [ ] 2. Extract the pure domain into `vpn-core::domain`
  - [ ] 2.1 Move profile and state domain types into `vpn-core/src/domain/`
    - Move `Kind`, `State`, `Stats`, `OpenVpnProfile`, `FortiProfile`, `General`, `Session`, `TailscaleStatus`, `Iface`, `Profile`, `Snapshot`, `LogEntry`, and `Level` into `domain/profile.rs`, `domain/state.rs`, and `domain/snapshot.rs`.
    - Move the pure helpers `display_name`, `validate_profile_name`, `human_bytes`, `human_duration`, and the per-profile rate history (capped at 80 samples) into the domain, keeping their existing semantics.
    - Move the existing unit tests for these items into the same modules so they keep passing.
    - _Requirements: 1.1, 1.3, 4.4, 9.3, 9.4, 9.5, 9.6_
  - [ ]* 2.2 Preserve and run the migrated domain unit tests
    - Confirm the moved `display_name`, `validate_profile_name`, and `human_bytes/duration` tests pass under `vpn-core`.
    - _Requirements: 1.4_
  - [ ] 2.3 Implement the `reconcile()` ownership/adoption engine in `domain/reconcile.rs`
    - Implement `reconcile(Snapshot) -> ReconcileOutcome`: adopt externally observed sessions/PIDs, track owned vs adopted, reflect observed (not optimistic) state, reset link fields when a session/PID disappears, derive per-profile rates from the clock, and emit log entries into the outcome.
    - Define `ReconcileOutcome` (updated per-profile views + produced `LogEntry` list) and `ProfileRef`.
    - _Requirements: 4.1, 4.4, 6.3, 7.1, 7.3, 8.1, 8.2_

- [ ] 3. Define the outbound port traits in `vpn-core::ports`
  - [ ] 3.1 Define persistence ports in `ports/repository.rs`
    - Declare `ProfileRepository` (openvpn/forti list, insert, update, delete) and `SettingsRepository` (`general`, `set_setting`) exactly as in the design.
    - _Requirements: 1.1, 16.1, 16.2_
  - [ ] 3.2 Define VPN and system ports
    - Declare `OpenVpnPort`, `FortiPort`, `TailscalePort` in `ports/vpn.rs`; `SystemProbePort` in `ports/probe.rs`; `ClockPort` in `ports/clock.rs`; and `EventSink` (`log`, `snapshot`) in `ports/sink.rs`, each with an `available()` method where the design specifies one.
    - _Requirements: 1.1, 1.3, 5.1, 6.2, 13.1, 13.2, 14.1, 18.2_

- [ ] 4. Port the SQLite persistence adapter into `vpn-core::adapters::sqlite`
  - [ ] 4.1 Implement `SqliteProfileRepository` and `SqliteSettingsRepository` (ported from `db.rs`)
    - Port `open_at(path)`: create parent dir, open connection, run `init_schema`, `migrate_name_nullable`, apply `chmod 600` (`restrict_permissions`), and on a new file run the legacy `config.toml` import (`find_legacy_toml` + `migrate_from_toml`) and the IPv6-script-path upgrade.
    - Implement `general()` with the clamps (`poll_interval_ms` → `max(250)`, `log_capacity` → `max(100)`, defaults 2000/2000) and `set_setting`, plus profile CRUD matching the existing signatures with the `name` column nullable.
    - Default DB path `~/.local/share/vpn-tui/vpn-tui.db`, overridable with `VPN_TUI_DB`.
    - _Requirements: 1.1, 2.3, 2.4, 2.5, 6.4, 6.5, 15.4, 16.1, 16.2, 16.3, 16.4_
  - [ ]* 4.2 Preserve and run the migrated `db.rs` tests
    - Confirm the migration, CRUD, optional-name, schema-upgrade, and IPv6-path-upgrade tests pass under `vpn-core`.
    - _Requirements: 1.4, 2.3, 16.3_

- [ ] 5. Port the VPN and probe outbound adapters into `vpn-core::adapters`
  - [ ] 5.1 Implement `Openvpn3Adapter` (ported from `vpn/openvpn3.rs`)
    - Implement `OpenVpnPort`: connect (returns session path), disconnect, `all_sessions`, `disconnect_all`, `stats`, and `available()`; keep the existing session/stats parsing.
    - _Requirements: 1.1, 6.2, 12.1, 12.2, 12.3, 18.2_
  - [ ]* 5.2 Preserve and run the migrated `openvpn3.rs` parsing tests
    - _Requirements: 1.4_
  - [ ] 5.3 Implement `FortiAdapter` (ported from `vpn/fortivpn.rs`)
    - Implement `FortiPort`: `spawn` writing credentials to a `chmod 600` temp file (never on argv), the layered `signal` strategy (`sudo pkill -x` → `sudo kill` → `kill`), `running_pids`, and `available()`.
    - On a privilege-denied result, return an error that names the fix (`run sudo ./install.sh`).
    - _Requirements: 1.1, 5.1, 5.3, 6.2, 16.4, 18.2_
  - [ ]* 5.4 Preserve and run the migrated `fortivpn.rs` host/port + classify tests
    - _Requirements: 1.4_
  - [ ] 5.5 Implement `TailscaleAdapter` (ported from `vpn/tailscale.rs`)
    - Implement `TailscalePort`: `status` (from `tailscale status --json`), `up`, `down`, `set_exit_node`, `set_advertise_routes`, and `available()`; keep operator-mode-first with `sudo -n` fallback and one-time-fix guidance.
    - _Requirements: 1.1, 5.1, 14.1, 14.2, 14.3, 14.4, 14.5, 18.2_
  - [ ]* 5.6 Preserve and run the migrated `tailscale.rs` status/prefs parsing tests
    - _Requirements: 1.4_
  - [ ] 5.7 Implement `SysfsProbeAdapter` and `SystemClock`
    - Implement `SystemProbePort`: `tunnel_interfaces` (from `/sys/class/net`, `/proc`, `ip`), `ipv6_disabled`, and `run_script` (privileged IPv6 scripts by fixed absolute path via `sudo -n`). Implement `SystemClock` for `ClockPort`.
    - _Requirements: 1.1, 5.1, 6.2, 13.1, 13.2, 13.4_

- [ ] 6. Checkpoint - workspace builds and all preserved tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 7. Implement the application core `VpnManagerService` in `vpn-core::app`
  - [ ] 7.1 Define `VpnManager` and construct the service over its ports
    - Define the `VpnManager` inbound trait and implement `VpnManagerService::new(repo, settings, openvpn, forti, tailscale, probe, clock, sink)` generic over the port traits.
    - _Requirements: 1.1, 1.2, 1.4, 3.5, 3.6_
  - [ ] 7.2 Implement connection control (connect/disconnect single/all)
    - Mark `Connecting`, call the appropriate port, record owned (not adopted) sessions, and emit `Info`/`Error` logs; map failures to `AppError{operation, message}`. When `ipv6_on_connect` is set, run the IPv6 disable script exactly once on connect.
    - _Requirements: 3.1, 3.6, 5.3, 13.3, 15.1_
  - [ ] 7.3 Implement profile CRUD with the optional-name rule
    - Implement `list_profiles`, `create/update/delete_openvpn`, `create/update/delete_forti`, enforcing `validate_profile_name` so a single profile of a kind may be unnamed but a second requires names.
    - _Requirements: 3.2, 9.1, 9.2, 9.3, 9.4, 9.5, 9.6_
  - [ ] 7.4 Implement settings read/update and the setup-wizard predicate
    - Implement `settings`, `update_settings` (clamp `poll_interval_ms` ≥ 250 and `log_capacity` ≥ 100 in the domain), and `needs_setup_wizard` (true iff no profiles and no settings).
    - _Requirements: 3.3, 6.4, 6.5, 11.1, 11.3, 16.1, 16.2_
  - [ ] 7.5 Implement OpenVPN3 session management, IPv6 toggle, and Tailscale control
    - Implement `list_sessions` (unfiltered), `kill_session`, `kill_all_sessions` (attempt all even on individual failure), `set_ipv6_disabled`, and the four `tailscale_*` methods, each logging and mapping errors.
    - _Requirements: 3.4, 12.1, 12.2, 12.3, 13.1, 13.2, 14.2, 14.3, 14.4, 14.5_
  - [ ] 7.6 Implement `reconcile()` wiring, the bounded log ring buffer, shutdown, and graceful degradation
    - Wire `reconcile(snap)` through the domain engine and expose `ReconcileOutcome`; maintain a bounded most-recent log ring sized to `log_capacity` (discard oldest first); implement `shutdown()` to close only owned, up sessions and leave adopted ones; mark unavailable tools from `available()`, disable their operations, and emit a `Warn` log naming each missing tool.
    - _Requirements: 4.1, 6.1, 6.3, 7.1, 7.3, 8.1, 8.2, 15.3, 15.4, 18.2, 18.3_

- [ ] 8. Build the fake adapters for domain tests
  - [ ] 8.1 Implement in-memory and fake port adapters under `vpn-core` test support
    - Implement `InMemoryProfileRepository`, `InMemorySettingsRepository`, `FakeOpenVpnPort`, `FakeFortiPort`, `FakeTailscalePort` (recording calls, scriptable results including privilege-denied), `FakeProbe`, `FixedClock`, and `RecordingSink`.
    - _Requirements: 1.3_

- [ ] 9. Write the property-based tests for the 15 correctness properties (proptest, ≥100 cases)
  - [ ]* 9.1 Property test: shutdown closes exactly owned+up sessions
    - **Property 1: Shutdown closes exactly the owned, up sessions and leaves adopted ones**
    - Tag: `// Feature: tauri-vpn-gui, Property 1: Shutdown closes exactly the owned, up sessions and leaves adopted ones`
    - **Validates: Requirements 8.1, 8.2**
  - [ ]* 9.2 Property test: external sessions are adopted
    - **Property 2: External sessions are adopted**
    - **Validates: Requirements 7.1**
  - [ ]* 9.3 Property test: ownership not silently flipped for owned sessions
    - **Property 3: Ownership is never silently flipped for an owned session**
    - **Validates: Requirements 7.3**
  - [ ]* 9.4 Property test: reconcile reflects observed, not optimistic, state
    - **Property 4: Reconcile reflects observed state, not optimistic state**
    - **Validates: Requirements 6.3**
  - [ ]* 9.5 Property test: poll interval clamped to floor
    - **Property 5: Poll interval is clamped to the floor**
    - **Validates: Requirements 6.4**
  - [ ]* 9.6 Property test: optional-name rule
    - **Property 6: Optional-name rule**
    - **Validates: Requirements 9.5, 9.6**
  - [ ]* 9.7 Property test: profile persistence round-trip
    - **Property 7: Profile persistence round-trip**
    - **Validates: Requirements 9.1, 9.2, 9.3, 9.4, 16.1**
  - [ ]* 9.8 Property test: settings persistence round-trip with clamping
    - **Property 8: Settings persistence round-trip with clamping**
    - **Validates: Requirements 16.1, 16.2**
  - [ ]* 9.9 Property test: setup-wizard predicate
    - **Property 9: Setup-wizard predicate**
    - **Validates: Requirements 11.1, 11.3**
  - [ ]* 9.10 Property test: session listing is unfiltered
    - **Property 10: Session listing is unfiltered**
    - **Validates: Requirements 12.1**
  - [ ]* 9.11 Property test: kill-all terminates every listed session
    - **Property 11: Kill-all terminates every listed session**
    - **Validates: Requirements 12.3**
  - [ ]* 9.12 Property test: IPv6-on-connect side effect is conditional
    - **Property 12: IPv6-on-connect side effect is conditional**
    - **Validates: Requirements 13.3**
  - [ ]* 9.13 Property test: log buffer is a bounded most-recent ring
    - **Property 13: Log buffer is a bounded most-recent ring**
    - **Validates: Requirements 15.3, 15.4**
  - [ ]* 9.14 Property test: missing tools disable only their own operations
    - **Property 14: Missing tools disable only their own operations**
    - **Validates: Requirements 18.2, 18.3**
  - [ ]* 9.15 Property test: privileged-denial produces an identifying error log
    - **Property 15: Privileged-denial produces an identifying error log**
    - **Validates: Requirements 5.3**

- [ ] 10. Checkpoint - core complete and verified
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 11. Refactor the existing TUI binary into an inbound adapter on `vpn-core`
  - [ ] 11.1 Depend `vpn-tui` on `vpn-core` and delegate all VPN logic
    - Add the `vpn-core` dependency; reduce `app.rs` to view-state (selection, focus, form state, log scrollback); delegate all VPN actions and `reconcile()` to `VpnManagerService`; build the poll `Snapshot` from the outbound ports as the TUI's poll thread does today.
    - Implement a TUI `EventSink` that pushes log lines into the in-memory scrollback and triggers a redraw (mirroring the `Msg::Log` flow); keep ratatui rendering in `ui.rs` and the keyboard/tick threads in `event.rs`.
    - _Requirements: 1.2, 1.4, 2.2_
  - [ ]* 11.2 Confirm the preserved `app.rs` reconciliation/validation tests pass and the TUI builds/runs
    - _Requirements: 1.4, 2.2_

- [ ] 12. Scaffold the `vpn-gui` Tauri crate and composition root
  - [ ] 12.1 Create the `vpn-gui` crate, `tauri.conf.json`, and `AppState`
    - Create `vpn-gui/Cargo.toml` (depends on `vpn-core` + `tauri` + `tauri-plugin-dialog`), `build.rs`, `tauri.conf.json`, and `src/main.rs` with the Tauri builder.
    - Implement the composition root wiring the concrete adapters (`SqliteProfileRepository`, `SqliteSettingsRepository`, `Openvpn3Adapter`, `FortiAdapter`, `TailscaleAdapter`, `SysfsProbeAdapter`, `SystemClock`, `TauriEventSink`) into `VpnManagerService`, held behind `tauri::State<Mutex<Service>>`.
    - _Requirements: 1.2, 2.1, 2.3, 18.1_

- [ ] 13. Implement the DTO layer and command handlers
  - [ ] 13.1 Define serde DTOs and domain↔DTO mapping in `dto.rs`
    - Define `ProfileDto`, `ProfileRefDto`, `OpenVpnDraftDto`, `FortiDraftDto`, `SnapshotDto`, `SessionDto`, `TailscaleStatusDto`, `ExitNodeCandidateDto`, `LogEntryDto`, `SettingsDto`, and `AppErrorDto`, with `From`/`into_dto` conversions per the mapping table (password surfaced only as a set/unset flag; `connected`/`running` derived).
    - _Requirements: 3.5, 3.6, 4.4_
  - [ ] 13.2 Implement the command handlers in `commands.rs`
    - Implement every `#[tauri::command]` from the command table: `connect`, `disconnect`, `connect_all`, `disconnect_all`, profile CRUD, `get_settings`/`update_settings`, `list_sessions`/`kill_session`/`kill_all_sessions`, `set_ipv6`, the four `tailscale_*`, and `needs_setup_wizard`; each locks the service, maps DTOs↔domain, calls the inbound port, and returns `Result<T, AppErrorDto>`.
    - Register all commands in the Tauri builder `invoke_handler`.
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 9.1, 9.2, 11.1, 11.3, 12.1, 12.2, 12.3, 13.1, 13.2, 14.2, 14.3, 14.4, 14.5_
  - [ ]* 13.3 Write command example tests for DTO mapping and error propagation
    - Drive the service with fakes; assert DTO round-tripping and that failures surface as `AppErrorDto{operation, message}`.
    - _Requirements: 3.5, 3.6_

- [ ] 14. Implement the background reconciliation task and event emission
  - [ ] 14.1 Implement `TauriEventSink` in `sink.rs`
    - Implement `EventSink` forwarding `log(LogEntry)` as a `log` event (`LogEntryDto`) and `snapshot(SnapshotView)` as a `snapshot` event (`SnapshotDto`).
    - _Requirements: 4.2, 15.2_
  - [ ] 14.2 Implement the poll loop in `reconcile_task.rs`
    - Spawn a task that reads `poll_interval_ms` (clamped ≥ 250), assembles a `Snapshot` from the outbound ports, calls `reconcile(snapshot)` under the service lock, emits a `snapshot` event plus a `log` event per produced entry, then sleeps `poll_interval_ms` and repeats; reflect observed (not optimistic) state.
    - _Requirements: 4.1, 4.2, 6.1, 6.2, 6.3, 6.4, 6.5, 15.2_
  - [ ] 14.3 Wire the close-only-owned shutdown on window exit
    - On Tauri window `CloseRequested`/exit, call `service.shutdown()` so only owned sessions are closed and adopted ones are left running.
    - _Requirements: 8.1, 8.2_

- [ ] 15. Implement the native dialog and wizard-gating commands
  - [ ] 15.1 Implement `pick_ovpn_file` via `tauri-plugin-dialog`
    - Register the dialog plugin; open the file picker filtered to `.ovpn`; return `Some(path)` on selection and `None` on cancel, so the frontend leaves the config path unchanged on cancel and stores the path verbatim on selection (no file content read into the webview).
    - _Requirements: 10.1, 10.2, 10.3_

- [ ] 16. Build the React + TypeScript frontend foundation
  - [ ] 16.1 Scaffold the frontend and typed IPC client
    - Create `ui/` (`package.json`, `index.html`, `src/main.tsx`), `src/types.ts` mirroring `dto.rs`, and `src/ipc.ts` with one typed `invoke` wrapper per command plus `onSnapshot`/`onLog` listeners; `invoke` rejections carry `AppErrorDto`.
    - _Requirements: 3.5, 3.6, 4.2, 4.3_
  - [ ] 16.2 Implement the reconcile-driven state store
    - Implement the store holding `profiles`, `logs` (ring buffer capped at `log_capacity`), `tailscale`, `sessions`, `settings`, `ipv6Disabled`, `needsSetup`; dispatch `snapshot`/`log` events into it on mount; commands are fire-and-forget with no optimistic UI (the next `snapshot` updates state).
    - _Requirements: 4.3, 6.3, 15.3, 15.4_

- [ ] 17. Build the frontend components
  - [ ] 17.1 Implement `App`, `Dashboard`, `ProfileList`, and `ProfileRow`
    - Render per-profile state, cumulative counters, and instantaneous rate from `snapshot` events; connect/disconnect and connect-all/disconnect-all actions; sparkline from the rate series.
    - _Requirements: 3.1, 4.3, 4.4_
  - [ ] 17.2 Implement `ProfileForm` with native dialog and optional-name validation
    - Create/edit OpenVPN3 and Forti profiles; "Browse…" calls `pickOvpnFile()` and writes a returned path (cancel leaves it unchanged); enforce the optional-name rule client-side with the backend as source of truth.
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 10.1, 10.2, 10.3_
  - [ ] 17.3 Implement `SetupWizard` gated by `needs_setup_wizard`
    - Show only when the DB has no profiles and no settings; collect initial profiles/settings and persist via the CRUD/settings commands.
    - _Requirements: 11.1, 11.2, 11.3_
  - [ ] 17.4 Implement `LogPanel`, `TailscalePanel`, `Openvpn3SessionsManager`, and `SettingsForm`
    - `LogPanel`: append `log` events and trim to `log_capacity`. `TailscalePanel`: status, up/down, exit-node picker, advertised routes. `Openvpn3SessionsManager`: list every live session (including those with no stored profile) with kill-one/kill-all. `SettingsForm`: poll interval, log capacity, IPv6 scripts, `ipv6_on_connect`.
    - _Requirements: 12.1, 12.2, 12.3, 13.1, 13.2, 13.4, 14.1, 14.2, 14.3, 14.4, 14.5, 15.2, 15.3, 16.1, 16.2_

- [ ] 18. Frontend and end-to-end tests
  - [ ]* 18.1 Write frontend component tests
    - Snapshot/log event handling and store updates, `ProfileForm` optional-name validation, and `SetupWizard` gating (example/snapshot tests, not PBT).
    - _Requirements: 4.3, 9.5, 9.6, 11.1, 11.3, 15.3_
  - [ ]* 18.2 Write an end-to-end smoke test
    - Launch the Tauri app, verify a `snapshot` event arrives and the dashboard renders, and that `pick_ovpn_file` returns a path.
    - _Requirements: 4.2, 4.3, 10.1_

- [ ] 19. Packaging, distribution, and installer integration
  - [ ] 19.1 Configure the Tauri bundle and desktop entry
    - Set `tauri.conf.json` `bundle.targets = ["appimage", "deb"]`; add `vpn-gui.desktop` (`Categories=Network;System;`) launching the GUI with no terminal; declare `openvpn3`, `openfortivpn`, `tailscale` as `.deb` `Recommends` (not `Depends`).
    - _Requirements: 17.1, 17.2, 18.2, 18.3_
  - [ ] 19.2 Extend `install.sh` to build and install the GUI alongside the TUI
    - Add Tauri prerequisites (webkit2gtk, libayatana-appindicator, JS toolchain), build `vpn-gui`, install its bundle + `vpn-gui.desktop`; reuse the existing NOPASSWD sudoers rules, the `root:root` IPv6 scripts at fixed absolute paths, and Tailscale operator mode; leave the existing TUI install path, `/usr/local/bin/vpn`, and `vpn.desktop` untouched so both coexist.
    - _Requirements: 17.2, 17.3, 17.4, 2.2, 18.1_

- [ ] 20. Final checkpoint - full workspace builds, all tests pass, both apps run
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional test tasks and can be skipped for a faster MVP; core implementation tasks are never optional.
- Each task references specific requirement sub-clauses (and property numbers for PBT tasks) for traceability.
- Property-based tests (task 9) use `proptest` at ≥100 cases, run against the fake adapters from task 8, and each carries the `Feature: tauri-vpn-gui, Property N: ...` tag.
- The 15 property tests target the pure `vpn-core` domain only — no real D-Bus, sudo, or filesystem.
- Checkpoints (6, 10, 20) ensure incremental validation; extraction (tasks 1–5, 11) preserves every existing unit test to guard Requirements 1 and 2.2.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["2.1"] },
    { "id": 1, "tasks": ["2.2", "2.3", "3.1", "3.2"] },
    { "id": 2, "tasks": ["4.1", "5.1", "5.3", "5.5", "5.7"] },
    { "id": 3, "tasks": ["4.2", "5.2", "5.4", "5.6", "7.1"] },
    { "id": 4, "tasks": ["7.2", "7.3", "7.4", "7.5", "7.6", "8.1"] },
    { "id": 5, "tasks": ["9.1", "9.2", "9.3", "9.4", "9.5", "9.6", "9.7", "9.8", "9.9", "9.10", "9.11", "9.12", "9.13", "9.14", "9.15", "11.1", "12.1"] },
    { "id": 6, "tasks": ["11.2", "13.1"] },
    { "id": 7, "tasks": ["13.2", "14.1", "15.1", "16.1"] },
    { "id": 8, "tasks": ["13.3", "14.2", "14.3", "16.2"] },
    { "id": 9, "tasks": ["17.1", "17.2", "17.3", "17.4", "19.1"] },
    { "id": 10, "tasks": ["18.1", "18.2", "19.2"] }
  ]
}
```
