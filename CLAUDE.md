# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Meeting is a COSMIC desktop applet that displays the next upcoming meeting in the system panel or dock. It integrates with GNOME Evolution Data Server via D-Bus to fetch calendar events.

- **Language**: Rust (Edition 2024)
- **Framework**: libcosmic (Pop!_OS COSMIC desktop)
- **App ID**: `com.dangrover.next-meeting-app`
- **Distribution**: Native packages and Flatpak

## Compatibility

This app targets all Linux distributions running COSMIC DE, not just Pop!_OS. Keep these considerations in mind:

### Multi-Distro Support
- **systemd**: Most features assume systemd (e.g., `org.freedesktop.login1` for sleep/wake detection). Always implement graceful fallbacks for non-systemd distros.
- **D-Bus services**: Don't assume specific D-Bus services are available. Check for availability and degrade gracefully.
- **File paths**: Use XDG base directories, not hardcoded paths.

### Flatpak Compatibility
- **Sandbox restrictions**: Flatpak apps are sandboxed. Any new D-Bus access requires updating `com.dangrover.next-meeting-app.json`:
  - Session bus: `--talk-name=org.example.Service`
  - System bus: `--system-talk-name=org.example.Service`
- **Filesystem access**: Limited to declared paths. Currently has read-only access to `~/.config/cosmic` and `~/.config/evolution`.
- **Test both**: Features must work in both native installs and Flatpak. If a capability isn't available in Flatpak, implement a fallback.

### D-Bus Permissions

The app uses these D-Bus services (declared in `com.dangrover.next-meeting-app.json`):

| Service | Bus | Purpose |
|---------|-----|---------|
| `org.gnome.evolution.dataserver.Calendar8` | Session | Read calendar events from EDS |
| `org.gnome.evolution.dataserver.Sources5` | Session | List available calendar sources |
| `org.gnome.OnlineAccounts` | Session | Access GNOME Online Accounts integration |
| `org.freedesktop.login1` | System | Detect system wake and session unlock to refresh events immediately |
| `com.dangrover.next-meeting-app` (own) | Session | Serve a `Control` interface so a `--open-menu` invocation (e.g. a keyboard shortcut) can open the panel popup |

The `login1` permission allows the app to listen for `PrepareForSleep` and session `Unlock` signals, so calendar data refreshes as soon as the user returns to their computer (after sleep or screen lock). This provides a better experience than waiting for the next polling interval.

The applet *owns* its own bus name (`--own-name` in the manifest) to serve a small `Control` interface (`OpenMenu`). The CLI flags `--join-next`, `--scheduling-helper`, and `--open-menu` are the actions exposed on the keyboard-shortcut page; the first two run and exit, while `--open-menu` signals the already-running applet over this interface (see `src/ipc.rs`).

## Build Commands

```bash
just dev              # Build, install, and reload panel - USE THIS FOR TESTING
just                  # Build release (default)
just build-debug      # Debug build
just run              # Build and run for testing
just check            # Run clippy linter (pedantic)
just install          # Install to ~/.local
```

**Important:** Always use `just dev` when testing changes. The panel loads the installed binary from `~/.local/bin/cosmic-ext-applet-next-meeting`, not from `target/`. Running only `cargo build` will not update what the panel displays.

## Architecture

### Core Modules

- **main.rs**: Entry point - initializes i18n and launches COSMIC applet runtime
- **app.rs**: Application model implementing `cosmic::Application` trait with message-based updates
- **calendar.rs**: D-Bus integration with Evolution Data Server for calendar queries
- **config.rs**: Configuration using `cosmic_config` derive macros
- **i18n.rs**: Fluent-based localization via `i18n-embed`

### Data Flow

1. COSMIC panel launches applet via desktop entry (`X-CosmicApplet=true`)
2. `AppModel::init()` loads config and fetches initial meeting
3. Background subscription refreshes meetings every 60 seconds
4. D-Bus queries Evolution Data Server → parses iCalendar → returns next meeting

### Calendar Integration

The app reads calendar sources from `~/.config/evolution/sources/*.source`, opens each via D-Bus (`org.gnome.evolution.dataserver.Calendar8`), fetches events as iCalendar objects, parses them, filters to future events, and returns the soonest.

### COSMIC Application Pattern

Messages flow through `update()`:
- `TogglePopup` / `PopupClosed` - popup visibility
- `MeetingUpdated(Option<Meeting>)` - new calendar data
- `UpdateConfig(Config)` - config changes

Subscriptions run in background: calendar refresh (60s interval) and config watcher.

## Localization

Translations use Fluent format in `i18n/<lang>/cosmic_ext_applet_next_meeting.ftl`. Add new languages by copying `i18n/en/` directory. Use `fl!("message-id")` macro in code.

### Honor the user's locale (applies to every feature)

Always respect the user's locale settings — never hardcode US/English conventions. When adding any feature that displays user-facing strings, dates, times, or numbers:

- **No hardcoded English** in code: every visible string goes through `fl!(...)` (Fluent), including text the user copies/shares (e.g. composed messages), not just static UI labels.
- **12h vs 24h clock**: format times according to the locale, not a fixed `AM/PM`. Use `crate::locale::format_time` / `hour_axis_label` (backed by POSIX `nl_langinfo(T_FMT)`), which fall back gracefully when the locale can't be read (e.g. a minimal Flatpak sandbox).
- **Localized day/month names**: use `crate::locale` helpers (`day_header`, `short_date`, `long_date`) rather than chrono's English `%a`/`%b`/`%A`.
- **Other locale-sensitive conventions** (first day of week, number/decimal formatting, etc.) should follow the locale too where relevant.

The `src/locale.rs` module centralizes this; extend it rather than re-deriving locale logic per feature.

## Key Dependencies

- `libcosmic` - COSMIC desktop framework (git dependency)
- `zbus` - D-Bus communication
- `ical` - iCalendar parsing
- `tokio` - Async runtime
- `chrono` - DateTime handling

## UI Patterns

The main popup follows the same pattern as other COSMIC applets (e.g., Power applet):

- Outer column: `.padding([8, 0])` (vertical padding only, no horizontal)
- Clickable items: `cosmic::applet::menu_button()`
- Non-interactive content (headings): `cosmic::applet::padded_control()`
- Dividers: `cosmic::applet::padded_control(widget::divider::horizontal::default()).padding([space_xxs, space_s])`

Settings pages use `widget::list_column()` for grouped items with dividers.

## Clippy Pedantic

This project uses `clippy::pedantic` warnings. Run `just check` before committing. Common patterns to follow:

- **Format strings**: Use inlined variables: `format!("{foo}")` not `format!("{}", foo)`
- **Option chains**: Use `.cloned()` instead of `.map(String::clone)` or `.map(|s| s.to_string())`
- **Option fallbacks**: Use `.map_or_else(default_fn, transform_fn)` instead of `.map(f).unwrap_or_else(g)`
- **Boolean checks**: Use `.is_some_and(predicate)` or `.is_ok_and(predicate)` instead of nested if-let with if, or `.map(predicate).unwrap_or(false)`
- **Let-else**: Use `let Ok(x) = expr else { return; }` instead of `match expr { Ok(x) => x, Err(_) => return }`
- **Match arms**: Merge arms with identical bodies; put wildcard pattern last
- **Doc comments**: Use backticks around identifiers in doc comments (e.g., `` `CalendarInfo` ``)
- **Method references**: Use `String::as_str` instead of `|s| s.as_str()` when possible

## Workflow

- **Branching**: Never make changes directly on the `main` branch. Always switch to `dev` (or create a feature branch) before making code changes. If you find yourself on `main`, stash changes and switch branches first.
- **Before committing**: Always run `cargo fmt` and `just check` before committing to ensure code passes CI formatting and linting checks.
- **Testing before pushing**: Do not push changes without letting the user test first. After making code changes, wait for the user to run `just dev` and verify the changes work correctly. Only push when explicitly asked, or when debugging CI issues.
- **Atomic commits**: Before starting new work, check for uncommitted changes and prompt to commit them first. Keep commits focused on single changes - don't let unrelated work pile up in a single commit.
- **Releases**: To trigger a release, create a tag with a `v` prefix (e.g., `v0.9.0`). Tags without the `v` prefix will not trigger the release workflow. Before tagging a release, always run `just update-flatpak-sources` to regenerate `cargo-sources.json` (the Flatpak vendored dependencies manifest). If dependencies changed since the last release and this file is stale, the Flatpak CI build will fail. Prefer using `just tag <version>` which handles version bumping, flatpak source regeneration, and tagging in one step.
