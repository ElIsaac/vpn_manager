# Requirements Document

## Introduction

This feature ports the existing Rust terminal UI (TUI) VPN manager, project `vpn-tui`, to a graphical desktop application built with Tauri. The graphical application uses a React + TypeScript frontend rendered in the Tauri webview and a Tauri (Rust) backend. The backend reuses the already-decoupled VPN core logic from the existing `vpn-tui` project as a shared Rust library, so a single implementation drives both the existing TUI and the new graphical application.

The graphical application manages the same three VPN technologies as the TUI: OpenVPN3 (D-Bus based sessions that survive terminal close), OpenFortiVPN (a privileged process controlled by signals), and Tailscale (its own daemon with operator mode). The new application is created as a separate project (a distinct crate and workspace) so that the existing TUI remains fully functional and both applications share the same on-disk SQLite database.

The core behaviors of the TUI must be preserved: periodic reconciliation of real system state, adoption of externally started sessions, closing only sessions the application itself opened on exit, never prompting for a password mid-connection, per-profile traffic statistics, profile management, OpenVPN3 session management, IPv6 toggling, Tailscale control, live logging, and database persistence with legacy configuration migration. The target platform is Linux with systemd, matching the TUI.

## Glossary

- **GUI_App**: The new Tauri desktop application, comprising the React + TypeScript frontend and the Tauri (Rust) backend.
- **Frontend**: The React + TypeScript user interface rendered inside the Tauri webview.
- **Backend**: The Rust side of the Tauri application that exposes commands and emits events.
- **Core_Library**: The reusable Rust library crate containing the VPN management logic currently in the `vpn-tui` modules (`db`, `vpn::mod`, `vpn::openvpn3`, `vpn::fortivpn`, `vpn::tailscale`, and the reconciliation logic).
- **TUI_App**: The existing `vpn-tui` terminal application that must remain functional.
- **Shared_Database**: The SQLite database file located at `~/.local/share/vpn-tui/vpn-tui.db`, used by both the TUI_App and the GUI_App, created with file permission mode 600.
- **Tauri_Command**: A backend function invokable from the Frontend over Tauri inter-process communication (IPC).
- **Tauri_Event**: A message emitted by the Backend and received by the Frontend over Tauri's event channel.
- **Snapshot**: A data structure describing the observed real system state of all managed VPNs at a point in time, including per-profile connection state, traffic counters, and traffic rate samples.
- **Reconciler**: The Backend component that periodically observes real system state and produces a Snapshot.
- **Owned_Session**: A VPN connection that the GUI_App itself started during the current run.
- **Adopted_Session**: A VPN connection that existed before the GUI_App started, or was started outside the GUI_App, that the GUI_App displays and manages without claiming ownership.
- **OpenVPN3_Profile**: A stored profile referencing an `.ovpn` configuration file for OpenVPN3.
- **FortiVPN_Profile**: A stored profile containing host, port, username, and either a password or a password environment variable name for OpenFortiVPN.
- **Poll_Interval**: The time between reconciliation cycles, stored as `poll_interval_ms`, defaulting to 2000 milliseconds, with a minimum of 250 milliseconds.
- **Setup_Wizard**: The first-run guided configuration flow shown when the Shared_Database contains no profiles and no settings.
- **Log_Entry**: A timestamped message with a severity level of Info, Ok, Warn, or Error.
- **IPv6_Scripts**: The installed `root:root`-owned enable and disable scripts referenced by the settings `ipv6_enable_script` and `ipv6_disable_script`.
- **Privileged_Operation**: An operation requiring elevated privileges, executed with `sudo -n` against pre-authorized NOPASSWD sudoers rules.
- **Native_File_Dialog**: The operating system file-selection dialog provided through the Tauri dialog plugin.

## Requirements

### Requirement 1: Reuse the VPN core as a shared library

**User Story:** As a maintainer, I want the GUI_App to reuse the existing VPN logic as a library, so that both applications share one implementation and bug fixes apply to both.

#### Acceptance Criteria

1. THE Core_Library SHALL expose the database persistence, VPN wrapper, and reconciliation logic currently contained in the `vpn-tui` modules as a reusable Rust library crate.
2. THE Backend SHALL depend on the Core_Library for all VPN management operations.
3. THE Core_Library SHALL be free of terminal-UI dependencies so that it can be consumed by both the TUI_App and the GUI_App.
4. WHERE a VPN management behavior exists in both the TUI_App and the GUI_App, THE Core_Library SHALL provide the single implementation of that behavior.

### Requirement 2: Separate project leaving the TUI intact with a shared database

**User Story:** As a user, I want the new graphical application to be a separate project that shares my data, so that my existing terminal tool keeps working and my profiles are available in both.

#### Acceptance Criteria

1. THE GUI_App SHALL be implemented as a project separate from the TUI_App binary.
2. THE TUI_App SHALL remain buildable and runnable after the GUI_App is introduced.
3. THE GUI_App SHALL read from and write to the Shared_Database at `~/.local/share/vpn-tui/vpn-tui.db`.
4. WHEN the GUI_App creates the Shared_Database, THE GUI_App SHALL set its file permission mode to 600.
5. WHERE the Shared_Database already contains profiles created by the TUI_App, THE GUI_App SHALL present those profiles without modification to their stored values.

### Requirement 3: Expose core operations as Tauri commands

**User Story:** As a frontend developer, I want the core operations available over IPC, so that the React interface can drive VPN actions.

#### Acceptance Criteria

1. THE Backend SHALL expose Tauri_Commands for connecting a single profile, disconnecting a single profile, connecting all profiles, and disconnecting all profiles.
2. THE Backend SHALL expose Tauri_Commands for creating, editing, and deleting OpenVPN3_Profiles and FortiVPN_Profiles.
3. THE Backend SHALL expose Tauri_Commands for reading and updating application settings.
4. THE Backend SHALL expose Tauri_Commands for the OpenVPN3 session management, IPv6 toggle, and Tailscale control operations defined in this document.
5. WHEN a Tauri_Command completes successfully, THE Backend SHALL return a result describing the outcome of the operation.
6. IF a Tauri_Command fails, THEN THE Backend SHALL return a descriptive error identifying the failed operation.

### Requirement 4: Push reconciled state to the frontend via events

**User Story:** As a user, I want the interface to reflect the real state of my VPNs automatically, so that I see accurate status without manual refresh.

#### Acceptance Criteria

1. THE Reconciler SHALL produce a Snapshot of real system state on each reconciliation cycle.
2. WHEN the Reconciler produces a Snapshot, THE Backend SHALL emit a Tauri_Event carrying that Snapshot to the Frontend.
3. WHEN the Frontend receives a Snapshot Tauri_Event, THE Frontend SHALL update the displayed connection state, traffic counters, and traffic rate to match the Snapshot.
4. THE Snapshot SHALL include, for each managed profile, the connection state, cumulative traffic counters, and the instantaneous traffic rate.

### Requirement 5: Never prompt for a password mid-connection

**User Story:** As a user, I want privileged actions to run without interactive password prompts, so that connecting and disconnecting are seamless.

#### Acceptance Criteria

1. WHEN the GUI_App performs a Privileged_Operation, THE GUI_App SHALL execute that operation using `sudo -n` against the pre-authorized NOPASSWD sudoers rules.
2. THE GUI_App SHALL perform Privileged_Operations without displaying an interactive password prompt during a connection or disconnection.
3. IF a Privileged_Operation is denied because the required NOPASSWD sudoers rule is absent, THEN THE GUI_App SHALL record a Log_Entry at Error level identifying the denied operation.

### Requirement 6: Reconcile real system state periodically

**User Story:** As a user, I want the application to reflect actual system state rather than optimistic state, so that the status I see is trustworthy.

#### Acceptance Criteria

1. THE Reconciler SHALL observe real system state every Poll_Interval milliseconds.
2. THE Reconciler SHALL derive observed state from the OpenVPN3 sessions list, the presence of `openfortivpn` processes, the Tailscale status, and the tunnel interfaces reported under `/sys/class/net`.
3. THE Reconciler SHALL reflect observed real system state rather than optimistic predicted state.
4. WHEN the user sets the Poll_Interval below 250 milliseconds, THE GUI_App SHALL clamp the Poll_Interval to 250 milliseconds.
5. WHERE no Poll_Interval has been configured, THE GUI_App SHALL use a Poll_Interval of 2000 milliseconds.

### Requirement 7: Adopt externally started sessions

**User Story:** As a user, I want VPN sessions started outside the application to appear in it, so that I have a single view of all active connections.

#### Acceptance Criteria

1. WHEN the Reconciler observes a VPN session that the GUI_App did not start, THE GUI_App SHALL display that session as an Adopted_Session.
2. THE GUI_App SHALL manage an Adopted_Session using the same operations available for an Owned_Session.
3. THE GUI_App SHALL distinguish an Adopted_Session from an Owned_Session in its tracking of session ownership.

### Requirement 8: Close only owned sessions on exit

**User Story:** As a user, I want to keep my externally started connections alive after I close the app, so that quitting the interface does not drop VPNs I started elsewhere.

#### Acceptance Criteria

1. WHEN the GUI_App exits, THE GUI_App SHALL close every Owned_Session.
2. WHEN the GUI_App exits, THE GUI_App SHALL leave every Adopted_Session running.

### Requirement 9: Profile management with the optional-name rule

**User Story:** As a user, I want to add, edit, and delete VPN profiles, so that I can manage my connection configurations.

#### Acceptance Criteria

1. THE GUI_App SHALL allow the user to create, edit, and delete OpenVPN3_Profiles.
2. THE GUI_App SHALL allow the user to create, edit, and delete FortiVPN_Profiles.
3. THE GUI_App SHALL accept an OpenVPN3_Profile that specifies a configuration file path, host, port, username, password, password environment variable name, trusted certificate, and extra arguments.
4. THE GUI_App SHALL accept a FortiVPN_Profile that specifies host, port, username, and either a password or a password environment variable name.
5. WHERE only one profile of a given VPN type exists, THE GUI_App SHALL allow that profile to have no name.
6. IF the user creates a second profile of a VPN type that already has an unnamed profile, THEN THE GUI_App SHALL require a name for each profile of that VPN type.

### Requirement 10: Native file dialog for .ovpn selection

**User Story:** As a user, I want to pick my `.ovpn` file using a standard file dialog, so that selecting a configuration is familiar and reliable.

#### Acceptance Criteria

1. WHEN the user chooses to select an `.ovpn` configuration file, THE GUI_App SHALL open a Native_File_Dialog.
2. WHEN the user selects a file in the Native_File_Dialog, THE GUI_App SHALL store the selected file path as the OpenVPN3_Profile configuration path.
3. WHEN the user cancels the Native_File_Dialog, THE GUI_App SHALL leave the OpenVPN3_Profile configuration path unchanged.

### Requirement 11: First-run setup wizard

**User Story:** As a new user, I want a guided setup the first time I open the app, so that I can configure the application without prior knowledge.

#### Acceptance Criteria

1. WHEN the GUI_App starts and the Shared_Database contains no profiles and no settings, THE GUI_App SHALL present the Setup_Wizard.
2. WHEN the user completes the Setup_Wizard, THE GUI_App SHALL persist the entered profiles and settings to the Shared_Database.
3. WHERE the Shared_Database already contains profiles or settings, THE GUI_App SHALL start without presenting the Setup_Wizard.

### Requirement 12: OpenVPN3 session management

**User Story:** As a user, I want to see and terminate all live OpenVPN3 sessions, so that I can clean up orphaned connections.

#### Acceptance Criteria

1. THE GUI_App SHALL list every live OpenVPN3 session reported by the system, including sessions not associated with a stored OpenVPN3_Profile.
2. WHEN the user chooses to terminate a listed OpenVPN3 session, THE GUI_App SHALL terminate that session.
3. WHEN the user chooses to terminate all listed OpenVPN3 sessions, THE GUI_App SHALL terminate every listed OpenVPN3 session.

### Requirement 13: IPv6 toggle

**User Story:** As a user, I want to enable and disable IPv6 from the app, so that I can control IPv6 behavior around my VPN connections.

#### Acceptance Criteria

1. WHEN the user chooses to disable IPv6, THE GUI_App SHALL run the configured IPv6 disable script as a Privileged_Operation.
2. WHEN the user chooses to enable IPv6, THE GUI_App SHALL run the configured IPv6 enable script as a Privileged_Operation.
3. WHERE the setting `ipv6_on_connect` is enabled, THE GUI_App SHALL run the IPv6 disable script when a VPN connection is established.
4. THE GUI_App SHALL display the current IPv6 state as observed from the system.

### Requirement 14: Tailscale control

**User Story:** As a user, I want to control Tailscale from the app, so that I can manage my mesh connectivity in one place.

#### Acceptance Criteria

1. THE GUI_App SHALL display the Tailscale status obtained from `tailscale status --json`.
2. WHEN the user chooses to bring Tailscale up, THE GUI_App SHALL run the Tailscale up operation.
3. WHEN the user chooses to bring Tailscale down, THE GUI_App SHALL run the Tailscale down operation.
4. WHEN the user selects an exit node, THE GUI_App SHALL apply that exit node selection to Tailscale.
5. WHEN the user submits advertised routes, THE GUI_App SHALL apply those advertised routes to Tailscale.

### Requirement 15: Live log stream

**User Story:** As a user, I want a live log of actions and events, so that I can understand what the application is doing and diagnose problems.

#### Acceptance Criteria

1. THE Backend SHALL record a Log_Entry for each connection, disconnection, and error event, each with a severity level of Info, Ok, Warn, or Error.
2. WHEN the Backend records a Log_Entry, THE Backend SHALL deliver that Log_Entry to the Frontend for display.
3. WHEN the number of retained Log_Entries reaches the configured log capacity, THE GUI_App SHALL discard the oldest Log_Entry before retaining a new Log_Entry.
4. WHERE no log capacity has been configured, THE GUI_App SHALL use the default log capacity defined in the application settings.

### Requirement 16: Database persistence and legacy migration

**User Story:** As a user, I want my profiles and settings persisted and my old configuration imported, so that my data survives restarts and upgrades.

#### Acceptance Criteria

1. THE GUI_App SHALL persist OpenVPN3_Profiles, FortiVPN_Profiles, and application settings in the Shared_Database.
2. THE GUI_App SHALL persist the settings `poll_interval_ms`, `log_capacity`, `ipv6_disable_script`, `ipv6_enable_script`, and `ipv6_on_connect`.
3. WHEN the GUI_App starts and a legacy `config.toml` file exists while the Shared_Database contains no profiles, THE GUI_App SHALL import the legacy configuration into the Shared_Database.
4. THE GUI_App SHALL store the FortiVPN_Profile credentials in the Shared_Database without placing a password on a command line.

### Requirement 17: Packaging and distribution

**User Story:** As a user, I want to install the graphical application as a desktop app, so that I can launch it like any other application.

#### Acceptance Criteria

1. THE GUI_App SHALL be packaged as a Linux desktop application distributable as an AppImage and a Debian package.
2. THE GUI_App package SHALL install a desktop menu entry that launches the GUI_App.
3. THE GUI_App distribution SHALL configure the same permissions model used by the TUI_App, comprising the NOPASSWD sudoers rules and Tailscale operator mode.
4. THE GUI_App distribution SHALL coexist with the existing TUI_App desktop launcher without removing it.

### Requirement 18: Target platform Linux with systemd and graceful degradation

**User Story:** As a user on Linux, I want the app to work on my system and degrade gracefully when a VPN tool is missing, so that the parts I use remain functional.

#### Acceptance Criteria

1. THE GUI_App SHALL target Linux systems running systemd.
2. IF a required VPN command-line tool is absent from the system path, THEN THE GUI_App SHALL disable the operations that depend on that tool and record a Log_Entry at Warn level identifying the missing tool.
3. WHERE a VPN tool is absent, THE GUI_App SHALL continue to provide the operations for the VPN tools that are present.
