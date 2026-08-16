# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Windows-only native overlay that shows Warframe squad peer info (platform, name, host status, and optional country/region/ASN) anchored to the bottom of the game window. It detects the `Warframe.x64.exe`/`Warframe.exe` process, tails `%LOCALAPPDATA%\Warframe\EE.log`, and renders a borderless, click-through, always-on-top window with a tray icon for exit.

Privacy model (see README.md): log parsing and host detection are entirely local. If geo lookups are enabled (default), only the detected public IP is sent to `https://ipinfo.io`; results are cached on disk for 30 days. EE.log contents, squad names, and IPs are never logged or uploaded. `--no-geo` disables the external lookup entirely.

## Commands

```powershell
cargo build --release   # produces target\release\warframe-peer-overlay.exe
cargo run                # debug build + run (Windows only — uses windows-sys/tray-icon/winit)
cargo test                # runs all unit tests (every module has a #[cfg(test)] block)
cargo test <test_name>    # run a single test by name, e.g. `cargo test joins_member_ip_and_host_when_events_arrive_out_of_order`
cargo test --lib -- --ignored show_   # opt-in: pops a real desktop toast to verify notifications
cargo clippy --all-targets # lint
cargo fmt --all --check    # CI enforces this
```

The binary must be built/run on Windows — it links `windows-sys` APIs directly (window enumeration, positioning) and uses `tray-icon`/`winit` Windows integration.

CI (`.github/workflows/ci.yml`) runs fmt/clippy/test/build on `windows-latest` with `RUSTFLAGS: -D warnings`; keep all four green. Pushing a `v*` tag triggers `release.yml`, which attaches the exe to a GitHub Release.

## Licensing constraint

The project is released into the public domain under [The Unlicense](LICENSE), and the README states it is unaffiliated with Digital Extremes. That combination means **no game-derived asset may enter the repository** — art, extracted icons, ripped strings, or log samples containing real player names. `assets/icon.png` is original artwork drawn for this project (a four-node squad graph in the overlay's gold/navy palette), deliberately not Warframe imagery. Keep any future asset original for the same reason.

## Architecture

The app is a background monitor thread feeding a UI thread over an `mpsc` channel, with a strict pipeline for turning raw log lines into on-screen peer cards:

1. **`src/parser.rs` — `LogParser`**: a pure state machine over EE.log lines. It has no I/O; `process_line(&str) -> bool` returns whether the parseable peer state changed. It correlates several independently-arriving log line types (`AddSquadMember`, `Squad host address`, `Added remote player`, `VOIP: Registered remote player`, `Received ping from`) into a `Vec<Peer>`, because Warframe emits a peer's name, matchmaking ID, and IP on different lines in unpredictable order. `reconcile()` re-derives each peer's IP and host status every time new correlating data arrives; host detection falls back to "the locally logged-in user is host" only when no remote peer's IP matches `Squad host address`. Player names embed a private-use-area codepoint (e.g. `\u{e001}`) as a platform suffix — decoded in `parse_player_name`.

2. **`src/geo.rs` — `GeoResolver`**: given a public IP, queries `ipinfo.io` for country/region/org, skips private/loopback/documentation IPs, and caches results (with a `fetched_at` timestamp, 30-day TTL) to a JSON file under the OS cache dir (via `directories::ProjectDirs`, `com.synqark.WarframePeerOverlay`). `looks_like_hosting_provider` matches the ASN org name against a hardcoded list of cloud/hosting providers to flag likely relay/VPN peers (`is_hosting`) — this is a heuristic, not a definitive VPN detector. Entirely bypassed when `--no-geo` is passed.

3. **`src/monitor.rs` — `spawn`/`run`**: owns the background thread. On a 500ms loop it: (a) polls `sysinfo` for the Warframe process, (b) if running, finds its main visible window's client rect via `EnumWindows`/`GetClientRect`/`ClientToScreen` (used by the UI to position the overlay under the game window), (c) tails `EE.log` by tracking a byte offset and only feeding *complete* lines to `LogParser` (partial trailing lines are buffered in `pending` until the next newline arrives — see `read_appended_lines`), (d) resolves geo info for any new peer IPs via `GeoResolver`, and (e) builds a `Vec<PeerView>` (the parser's `Peer` plus resolved geo/UI fields) and sends a `MonitorSnapshot` through the channel — but only when a signature (debug-formatted state) differs from the last sent snapshot, to avoid needless UI churn. State resets (log offset, parser, geo failure cache) whenever Warframe stops running or the log file shrinks (log rotation).

4. **`src/main.rs` — `OverlayApp` (eframe/egui)**: the UI thread. Each frame it drains the snapshot channel (keeping only the latest), repositions/resizes the window to hug the bottom of the game window using `window_rect` from the snapshot, and toggles window visibility only once a `window_rect` is known (to avoid flashing at a stale position). **All sizes in this file are authored against a 2560x1440 Warframe client area**: `ui_scale` derives a factor from `window_rect.height / 1440` and feeds it to `Context::set_pixels_per_point`, so fonts, margins and `OVERLAY_HEIGHT` scale as one. This deliberately overrides monitor DPI — the overlay's proportions follow the game's resolution, not the desktop scaling. Geometry sent via `ViewportCommand` is computed with that same factor rather than `Context::pixels_per_point()`, because egui-winit applies viewport commands using the *new* zoom (a fresh `set_pixels_per_point` does not land in `pixels_per_point()` until the next pass). It has two render modes: a full status panel (shown when Warframe isn't actively being monitored, e.g. "waiting for Warframe") and a compact peer-card row (shown once EE.log is being monitored and peers exist). Window chrome: undecorated, transparent, always-on-top, taskbar-skipped, and mouse-passthrough is enabled after the first frame so it never intercepts game input. A tray icon (via `tray-icon`) provides the only way to exit, since there's no visible window chrome. Japanese-capable fonts are loaded from `%WINDIR%\Fonts` (falls back through Noto Sans JP VF / Yu Gothic / Meiryo / MS Gothic) since peer/status text can be Japanese.

Data flow in one line: `EE.log` → `monitor::read_appended_lines` → `parser::LogParser::process_line` → `monitor::run` (adds geo via `GeoResolver`) → `MonitorSnapshot` over `mpsc::Receiver` → `OverlayApp::ui` renders `PeerView`s.

5. **`src/tray.rs`** — the tray icon and its `Exit` menu, on a thread of its own with a plain `GetMessageW` pump. The thread is load-bearing (see the gotcha below), not incidental. It takes an `on_exit` callback; `main.rs` passes one that closes the root viewport, which is safe to call from another thread because `egui::Context` is `Send + Sync`.

6. **`src/single_instance.rs`** and **`src/notify.rs`** — process-lifecycle concerns, both claimed in `main()` before `eframe::run_native`. `SingleInstance` holds a `Local\WarframePeerOverlay` named mutex whose guard must live for the whole process (dropping it frees the name); a refused launch shows a toast instead of exiting silently, because a release build has no console to print to. `notify::show` returns `Result` so callers choose how to handle failure.

`src/lib.rs` re-exports every module so `main.rs` and the test modules can use them as `warframe_peer_overlay::*`.

## Windows gotchas learned the hard way

- **Release builds set `windows_subsystem = "windows"`** via `#![cfg_attr(not(debug_assertions), ...)]` at the top of `main.rs`, so no console window appears behind the overlay. Debug builds keep the console for `cargo run`. Verify with the PE subsystem byte: `2` = GUI, `3` = console.
- **A toast is dispatched asynchronously.** If the process exits right after `notify::show`, WinRT drops it before delivery and nothing appears. The single-instance rejection path sleeps ~1s before returning for exactly this reason. To check delivery on a machine, watch `%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db-wal` for a write, or run `cargo test --lib -- --ignored show_`.
- **Toasts need a registered AppUserModelID**, which a portable exe has none of; `notify.rs` borrows `Toast::POWERSHELL_APP_ID`, the standard fallback. COM apartment init is not needed — `windows-core`'s factory cache falls back to `CoIncrementMTAUsage`.
- **The tray icon is `assets/icon.png`, embedded with `include_bytes!`** so the release exe stays a single portable file with no assets directory to ship. It is decoded with `image` (png feature only) into RGBA for `tray_icon::Icon::from_rgba`.
- **The tray icon must not share winit's thread.** `tray-icon` opens its context menu with `TrackPopupMenu`, which runs a modal message loop on the thread owning the tray window. Under eframe/winit that loop receives no mouse or keyboard input at all: the menu appears, nothing highlights on hover, and clicking an item just dismisses it without emitting a `MenuEvent`. The same menu driven by a bare `GetMessageW` pump works, and so does the menu when the tray lives on its own thread — which is why `tray.rs` owns one. Do not "simplify" it back onto the UI thread.
- **To test the tray menu without a mouse**, find the tray window by class name `tray_icon_app` for the process, `PostMessageW(hwnd, 6002, 0, WM_RBUTTONUP)` (6002 is tray-icon's `WM_USER_TRAYICON`) to open the menu, then drive it with `keybd_event` VK_DOWN/VK_RETURN. Injecting `WM_COMMAND` with the muda item id instead tests only the event wiring and bypasses the modal loop, so it passes even when the menu is broken — use both to tell the two failure modes apart.
- **Temp file names in tests must not contain `::`** — Windows rejects them, which is why the `monitor` tail test builds its name from the pid rather than the thread name.
