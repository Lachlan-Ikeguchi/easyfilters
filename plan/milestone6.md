# Milestone 6 — Polish: run in the background, launch at login, packaging

Goal (from `docs/idea.md`, milestone 6): **startup behavior (launch at
login), packaging, and the leftover resolution/fps/camera-selection rough
edges.** Concretely, three pillars:

1. **Run in the background** — closing the GUI window hides it to the tray
   instead of quitting (the filtering keeps running; the tray is the control
   surface), plus a `--start-minimized` start-minimized mode and a
   single-instance lock so autostart + manual launch don't fight over the
   camera.
2. **Launch at login** — XDG autostart installed via
   `--install-autostart` / `--uninstall-autostart`, and an `autostart` option
   in the NixOS module (systemd user service).
3. **Packaging** — `.desktop` file + icon in the nix package, a version bump
   for the first packaged release — plus small pipeline rough edges: clear
   "camera busy" errors, requested-vs-negotiated logging, early `--format`
   validation.

Repo state at planning time: branch `feat/run-in-background`, clean, at
`c6f907b` (milestone 5 complete and verified live: iced GUI + ksni tray +
TOML config, pipeline thread driven by `Shared`, `--no-gui` headless at
1280x720@30, 29.8 fps, 0 drops; window close and tray "Quit" both currently
shut the app down — `src/main.rs` / `plan/milestone4.md`).

## Environment findings (carried over from M4/M5, verified on this machine, 2026-11-13)

- Session: KDE Plasma Wayland, D-Bus session bus with a StatusNotifierWatcher;
  `/dev/video0` = "EasyFilters Virtual Camera" loopback, `/dev/video1` = real
  uvcvideo camera, `/dev/video2` = metadata. 12 cores, 15 GiB RAM.
- `XDG_CONFIG_HOME`/`HOME` are set in the session, so the autostart path
  `~/.config/autostart/` is writable; `$XDG_RUNTIME_DIR` exists (per-session
  lockfile lives there).
- crates.io reachable; no new crates needed — `libc` is already in the tree
  (via `v4l2r::nix::libc`) for `flock`, and `.desktop` writing is plain
  `std::fs`. `nix build` / `nix flake check` stay green and pure.
- Release build matters (debug infer ~300 ms vs ~6-7 ms) — all live
  verification uses `--release`, same as M2-M5.

## Design decisions

### 1. Run in the background

- **New settings** (same precedence as everything else: CLI > config >
  default):
  - `Settings.close_to_tray: bool`, default **true** — when the GUI window
    is closed, hide it to the tray instead of quitting; filtering keeps
    running. Tray "Quit" (and SIGINT/SIGTERM) still stop everything.
  - `Settings.start_minimized: bool`, default **false** — start with the
    window hidden (tray only). CLI: `--start-minimized` sets it for this run
    (CLI > config, like the gaze flags: `--start-minimized` = `Some(true)`).
  - Config: `close_to_tray = true`, `start_minimized = false`
    (`#[serde(default)]` optionals). GUI: a "Close to tray" `toggler`.
- **GUI mechanics (iced 0.14):**
  - Start-minimized: launch with `window::Settings { visible: false, .. }`.
  - Close-to-tray: intercept `window::Event::CloseRequested` in the app's
    window-event handler; when `close_to_tray` is on, return
    `window::hide(id)` instead of exiting (when off, keep today's behavior:
    close = quit).
  - Tray "Show window" / "Hide window": the tray thread flips a
    `show_window: AtomicBool` in `Shared`; the GUI's 33 ms `Tick` sees the
    edge and issues `window::show(id)` / `window::hide(id)`. The tray menu
    label is updated via `tray.set_menu_item_label`-style API if ksni exposes
    it, otherwise a static "Show/Hide window" label is fine.
  - `--no-gui` headless is unchanged (no window, no tray) — it already *is*
    the server/background mode; `--start-minimized` is the desktop equivalent with
    a tray.
- **Single-instance lock (`src/lock.rs`):** with autostart, a second manual
  launch would fail on the camera with a cryptic EBUSY. Take a non-blocking
  `flock(LOCK_EX|LOCK_NB)` on `$XDG_RUNTIME_DIR/easyfilters.lock` (fallback:
  the config dir) before starting the pipeline; write the pid into it. A
  second instance prints "easyfilters is already running (pid N, lock
  …); exiting" and returns Ok. `--list` and `--self-test` skip the lock
  (they don't touch devices). Pure path + `flock` logic, unit-tested on tmp
  paths (two locks on the same file: second must fail).

### 2. Launch at login

- **`src/autostart.rs`:** CLI `--install-autostart` / `--uninstall-autostart`
  short-circuit before any device/lock handling. Install writes
  `$XDG_CONFIG_HOME/autostart/easyfilters.desktop` (fallback
  `~/.config/autostart/`):
  ```ini
  [Desktop Entry]
  Type=Application
  Name=EasyFilters
  Exec=<current exe> --start-minimized
  Icon=easyfilters
  X-GNOME-Autostart-enabled=true
  ```
  Paths come from explicit function args (not env reads inside) so unit
  tests can point them at a tmp dir: content check, idempotent install,
  uninstall removes, missing dir is created.
- **NixOS module (`flake.nix`):** new `easyfilters.autostart` bool option
  (default false). When true, add a systemd user service:
  `systemd.user.services.easyfilters` — `ExecStart = <package>/bin/easyfilters
  --start-minimized`, `WantedBy = [ "graphical-session.target" ]`, restart on
  failure. (XDG autostart from the package would also work via
  `xdg.autostart.enable`, but the unit is explicit and logs to the journal.)
  Doc note: the user still needs the `video` group for `/dev/video*`.

### 3. Packaging + rough edges

- **flake package:** ship `packaging/easyfilters.desktop` (menu entry:
  `Exec=<bin>`, `Icon=easyfilters`, `Categories=AudioVideo;Video;`,
  `Terminal=false`) and `assets/easyfilters.svg` (hand-written green rounded
  square matching the in-code tray icon) into
  `$out/share/applications/` and `$out/share/icons/hicolor/scalable/apps/`.
  Bump `meta.version` to `0.2.0` — first packaged release. Extend
  `checks.default` to assert the two files exist in `$out` (the existing
  `--help` smoke test stays).
- **Pipeline rough edges (`src/pipeline.rs`):**
  - `negotiate()` silently picks nearest size / closest fps. Log
    requested vs negotiated explicitly at startup (`requested 1920x1080@30
    -> negotiated 1280x720@30 MJPG`) and surface the negotiated values in
    the GUI status line (extend `Stats` or reuse the existing info log).
  - Validate `--format` fourcc in `run()` *before* opening devices (today
    `parse_fourcc` only fails inside `negotiate`, after the retry loop has
    already started).
  - EBUSY: when `Device::open`/queue/stream fails with EBUSY, log a targeted
    hint once per run_stream ("camera held by another app? close Discord/
    Zoom or pass --input") instead of only the generic "stream failed;
    retrying in 2 s".
  - Auto-detect already filters `driver == "uvcvideo"`, so the loopback is
    never picked as input; keep, and log the chosen input/output at startup
    (exists via the "negotiated format" log).
- **No new crates, no new system deps.** `.desktop`/SVG are data files;
  `flock` uses the libc already linked.

## Implementation steps

1. `plan/milestone6.md` (this file) + `docs/idea.md` milestone-6 line points
   at it — commit as docs.
2. `src/settings.rs` / `src/config.rs` / `main.rs` — `close_to_tray`,
   `start_minimized`, `--start-minimized`, resolve precedence + serde roundtrip
   tests. Commit.
3. `src/lock.rs` + `src/autostart.rs` — flock single-instance (skip for
   `--list`/`--self-test`), autostart install/uninstall with tmp-path unit
   tests; CLI flags wired in `main.rs`. Commit.
4. `src/gui.rs` + `src/tray.rs` — close-to-tray window-event handling,
   start-minimized window, tray "Show/Hide window" via `Shared::show_window`,
   GUI "Close to tray" toggler; keep Quit = real quit. Build, commit.
5. `src/pipeline.rs` — requested-vs-negotiated log + GUI status, early
   `--format` validation, EBUSY hint. `cargo test`, commit.
6. `flake.nix` + `packaging/easyfilters.desktop` + `assets/easyfilters.svg` —
   install desktop file + icon, version 0.2.0, `checks.default` asserts the
   files; NixOS module `autostart` option (systemd user service). `nix build`
   + `nix flake check` green. Commit.
7. Verify (below) — close-to-tray live on Xwayland `:1`, `--start-minimized`,
   autostart file, double-launch lock, `--no-gui` regression. README
   milestone-6 section + findings appended here. Commit.

## Verify

- `cargo test --release` and `nix build` / `nix flake check` green; the
  package contains `share/applications/easyfilters.desktop` and
  `share/icons/hicolor/scalable/apps/easyfilters.svg`.
- Close-to-tray: launch the GUI on Xwayland `:1`, close the window
  (`import -window root` screenshot shows it gone), confirm via
  `scripts/debug_camera.py /dev/video0 --save` that the loopback feed is
  still filtered, tray "Show window" re-opens it (screenshot), tray "Quit"
  stops the pipeline cleanly (exit 0, stats log stops). With
  `close_to_tray = false` in the config, closing the window quits as in M4.
- `--start-minimized`: no window appears, tray icon registers (SNI watcher via
  `busctl --user`), feed on `/dev/video0` is filtered.
- `--install-autostart` writes `~/.config/autostart/easyfilters.desktop` with
  `Exec=<exe> --start-minimized`; re-running is idempotent; `--uninstall-autostart`
  removes it. NixOS module with `autostart = true` evaluates
  (`nix eval`) and the unit is a user service wanted by
  `graphical-session.target`.
- Single instance: start `--no-gui` in the background, run a second
  `easyfilters` — it prints "already running (pid N)" and exits 0 without
  touching the camera; after the first exits, a new launch acquires the lock.
- Regression: `--no-gui --mode blur` at 1280x720@30 on `/dev/video0` still
  ~29.8 fps, 0 drops; `--list`, `--self-test`, `--mode passthrough|overlay|
  mask`, gaze flags unchanged; headless fallback with no display unchanged.
- GUI smoke test on Xwayland `:1`: window + tray, preview ~27-30 fps, all
  M4/M5 controls still apply and persist.

## Risks / open questions

- **iced 0.14 window API drift:** `on_window_event` / `window::hide` /
  `window::show` / `window::Settings { visible }` signatures differ across
  minor versions (M4 already hit this with the executor `time` module).
  Budget a compile-fix cycle; the fallbacks are `iced::exit()` on close and
  no start-minimized.
- **ksni left-click ("Activate")**: could toggle the window too; optional —
  menu item is enough for M6.
- **Autostart on Wayland:** XDG autostart works on KDE/GNOME Wayland sessions;
  the session bus + tray are the only runtime deps (already required for the
  tray). Servers keep using `--no-gui`.
- **Lockfile location:** `$XDG_RUNTIME_DIR` is per-session and tmpfs — right
  scope (a stale lock must never survive reboot). If unset, fall back to the
  config dir and document that a crashed instance there needs manual
  removal… no — keep it simple: skip the lock with a `warn!` when
  `XDG_RUNTIME_DIR` is unset rather than risk a stale lock.
- **EBUSY hint accuracy:** match on the errno from the v4l2r error chain;
  if matching proves brittle, log the hint whenever the input device open
  fails, without errno matching.
- **Debug builds stay ~3 fps** — all live verification in `--release`.

## Implementation findings (appended after live verification, 2026-11-13)

- (to fill in during step 7, like M4/M5)
