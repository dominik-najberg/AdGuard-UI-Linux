# Building

Build and development instructions for **AdGuard UI for Linux**.

Verified against this development machine: **Ubuntu 26.04 LTS**, GNOME Shell 50.1, Wayland.

---

## 1. Prerequisites

### Already present on this machine

Nothing needs installing to start building:

| Component | Version |
| --- | --- |
| Rust / Cargo (rustup) | 1.97.0 |
| GTK4 + dev headers (`libgtk-4-dev`) | 4.22.4 |
| libadwaita + dev headers (`libadwaita-1-dev`) | 1.9.1 |
| GLib + dev headers | 2.88.0 |
| gcc / g++ | 15.2.0 |
| pkg-config, cmake, meson, ninja | 4.2.3 / 1.10.1 / 1.13.2 |
| `adguard-cli` | 1.4.13 |

Confirm the toolkit is visible to `pkg-config` before the first build:

```bash
pkg-config --modversion gtk4 libadwaita-1 glib-2.0
```

### On a clean Ubuntu 26.04

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev
```

Rust via [rustup](https://rustup.rs) — the distro `cargo` lags behind what the `gtk4` crates expect.

### Optional developer tools (not installed here)

```bash
sudo apt install gtk-4-examples blueprint-compiler
```

`gtk4-widget-factory` (from `gtk-4-examples`) is a useful reference for Adwaita widget behaviour. `blueprint-compiler` is only needed if the UI adopts `.blp` files instead of `.ui` XML — `gtk4-builder-tool` is already installed either way.

---

## 2. Build and run

```bash
cargo build --workspace
```

```bash
cargo run -p adguard-gui
```

That is the whole application: `adguard-ui` is the only binary, and it serves the tray icon too. `adguard-tray` is a library (see `architecture.md` §4), so there is nothing separate to start.

### Seeing the tray icon

The icon appears in the top bar next to the other indicators. It needs two things:

- **A real desktop session.** Under `Xvfb` the window renders but the icon has nowhere to appear — though it does still register on the session bus, which is how it can be tested headlessly (below).
- **An AppIndicator extension**, because GNOME has no native tray. `ubuntu-appindicators@ubuntu.com` ships enabled on Ubuntu. Without it the app prints one line to stderr and runs windowed; it does not fail.

Left-click the icon for the menu: start/stop the proxy, and the six Protection toggles as checkmarks. A toggle there and the switch on the Protection page are the same write, so they cannot disagree.

Two behaviours worth knowing:

- **While the tray is present, closing the window only hides it** and the app keeps running — "Quit" in the tray menu is how you exit. If the tray could not register, closing quits as usual, so you can never end up with a hidden app you cannot reach.
- Launching `adguard-ui` again does not start a second copy; it activates the running one. That matters because two copies would be two writers to `proxy.yaml`.

Confirm the icon registered without needing to look at the screen:

```bash
gdbus call --session --dest org.kde.StatusNotifierWatcher --object-path /StatusNotifierWatcher --method org.freedesktop.DBus.Properties.Get org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems
```

Our entry is `org.kde.StatusNotifierItem-<pid>-1`. The menu itself can be read, and even driven, over D-Bus — useful for testing it without a session:

```bash
gdbus call --session --dest org.kde.StatusNotifierItem-<pid>-1 --object-path /MenuBar --method com.canonical.dbusmenu.GetLayout 0 3 '[]'
```

Note `GetLayout`'s recursion depth is given as `3` rather than `-1`: `gdbus` reads a leading `-` as one of its own options, the same trap the `--` guard exists for in `cli-contract.md` §5.

### Starting it at login

```bash
adguard-ui --background
```

Registers the tray and presents no window. The window is built either way, so it appears without a pause the first time you ask for it — from the tray's "Open AdGuard UI", or by running `adguard-ui` again. A second `--background` launch while one is already running does *not* pull the window up; a launch without the flag does, which is what makes the dock icon behave.

Install the autostart entry to get that at login:

```bash
install -Dm644 data/autostart/*.desktop ~/.config/autostart/
```

It runs `adguard-ui --background` off `$PATH`, so it needs the `~/.local/bin` install in §4. Remove the file to undo it, or flip `X-GNOME-Autostart-enabled` in a startup-applications editor.

**`--background` is the one place where a tray that will not register is fatal.** Everywhere else a missing AppIndicator extension is one line on stderr and a windowed app. Here there is no window either, so the process would be running with nothing on screen and no way to reach or quit it — it says so and exits 1 instead. Started from the autostart entry that message goes to the session journal:

```bash
journalctl --user -b -g adguard-ui
```

A private session bus carries no `org.kde.StatusNotifierWatcher`, so that path can be provoked in one command:

```bash
dbus-run-session -- adguard-ui --background
```

Release build:

```bash
cargo build --release --workspace
```

### Logging

Use `RUST_LOG` for the app's own tracing, and raise the CLI's verbosity separately when debugging integration:

```bash
RUST_LOG=debug cargo run -p adguard-gui
```

```bash
adguard-cli config set log_level debug && adguard-cli restart
```

Remember to set `log_level` back to `info` — `proxy.log` reached 8 MB on this machine at default verbosity.

### GTK inspector

```bash
GTK_DEBUG=interactive cargo run -p adguard-gui
```

---

## 3. Tests

`adguard-core` is deliberately GTK-free so it tests headlessly:

```bash
cargo test -p adguard-core
```

Parsing tests must run against **recorded fixtures**, not a live CLI — output depends on machine state (licence, installed filters, whether the proxy is running). Capture fixtures with escapes intact, since the CLI emits ANSI unconditionally:

```bash
adguard-cli filters list --all > crates/adguard-core/tests/fixtures/filters-list-all.txt
```

Do not strip the escapes when recording — the stripper is part of what is under test.

### The `#[ignore]`d suites

Three suites are excluded from a plain `cargo test` because they invoke the real `adguard-cli`. They are the only tests that exercise the write path end to end — act → re-read → reconcile against the real binary — so run them after any change to `cli.rs`, and after an `adguard-cli` upgrade.

**Safe: writes only to a throwaway config.** The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`, so this one hands the real binary a copy of `proxy.yaml` in a temp directory and never touches your settings:

```bash
cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
```

That is where the dangerous behaviour is covered — exposing the proxy on `0.0.0.0`, blanking the proxy password, the `--` guard, the absent range checking. It also asserts, last, that the machine's `proxy.yaml` is byte-identical afterwards. A sandbox is unlicensed, so `status`/`license`/`filters` cannot run there; only the `config` family can (see `cli-contract.md` §5).

**Mutates this machine's real AdGuard configuration.** Both restore whatever they found, including on panic, but neither belongs in an unattended run:

```bash
cargo test -p adguard-core --test filters_mutate -- --ignored --nocapture
```

```bash
cargo test -p adguard-core --test config_mutate -- --ignored --nocapture
```

`config_mutate` is deliberately kept to one boolean round-trip plus the claim the whole no-YAML-writes rule rests on — that `config set` rewrites exactly one line and preserves every comment. Anything riskier belongs in `config_sandbox`.

The `*_live` suites are safe and run by default; they read the real `proxy.yaml` and filter databases, and **skip** rather than fail when AdGuard CLI is not installed.

### Driving the GUI against a fake config

The same `$XDG_DATA_HOME` trick works on the app, which is the only practical way to see how a page renders against a config you would never create on purpose — a port holding a float, a key missing outright, a value outside its enum:

```bash
mkdir -p /tmp/fake/adguard-cli && cp ~/.local/share/adguard-cli/proxy.yaml /tmp/fake/adguard-cli/
```

Edit `/tmp/fake/adguard-cli/proxy.yaml`, then:

```bash
XDG_DATA_HOME=/tmp/fake cargo run -p adguard-gui
```

Writes made in the app land in the fake config. Note the Filters page will fail to open its catalogue there unless the `agflm_*.db` files are copied across too — the CLI seeds a fresh data directory with its own bundled defaults on first use.

GUI code needs a display. Under Wayland, headless CI requires a compositor:

```bash
cargo test --workspace
```

For a quick look at the GUI without one, `Xvfb` is enough to render and screenshot it:

```bash
xvfb-run -n 99 -s "-screen 0 1000x820x24" env GDK_BACKEND=x11 ./target/debug/adguard-ui
```

To capture a frame, `ffmpeg`'s `x11grab` works against the virtual display — unlike against `:0`, where under Wayland it captures nothing because Xwayland windows are not drawn into the X root window:

```bash
ffmpeg -f x11grab -video_size 1000x820 -i :99 -frames:v 1 -y /tmp/shot.png
```

Make the virtual screen taller than the window if you want a whole `AdwPreferencesPage` in one frame; there is no way to scroll without `xdotool`, which is not installed here.

---

## 4. Local install

Matching the pattern already used for other tools on this machine (`~/.local/bin` + a desktop entry):

```bash
cargo build --release && install -Dm755 target/release/adguard-ui ~/.local/bin/adguard-ui
```

```bash
install -Dm644 data/*.desktop ~/.local/share/applications/
```

```bash
update-desktop-database ~/.local/share/applications
```

The autostart entry is a separate file in a separate directory, and the glob above deliberately does not reach it — installed among the launchers it would show up as a second, windowless entry in the app grid:

```bash
install -Dm644 data/autostart/*.desktop ~/.config/autostart/
```

**The desktop file, the GTK application ID, and `StartupWMClass` must all be the same reverse-DNS string.** If they diverge, GNOME shows a second, unbranded icon below the dock separator instead of grouping the window with its launcher.

The polkit action must go to a system path — this step needs root and is the only part that does:

```bash
sudo install -Dm644 data/*.policy /usr/share/polkit-1/actions/
```

---

## 5. Packaging

Assessed for this machine specifically:

| Route | Status here | Notes |
| --- | --- | --- |
| **Tarball + `.desktop`** | Ready | Least friction; matches existing habit for personal tools. |
| **`.deb`** | `dpkg-dev` 1.23.7 present; `debhelper` **not** installed | `dpkg-deb -b` on a hand-built tree works today. `sudo apt install debhelper devscripts` for a proper `debian/` setup. |
| **Flatpak** | **Not installed at all** | Needs `flatpak` + `flatpak-builder` + the `org.gnome.Platform//50` runtime/SDK (~1–2 GB download). The most "correct" GNOME distribution route, but greenfield here. |
| **Snap** | `snapd` 2.76.1 running; `snapcraft` **not** installed | Strict confinement would fight both `pkexec` and reaching `~/.local/bin/adguard-cli`. Not recommended. |
| **AppImage** | No tooling | Awkward for an app shipping a polkit policy. |

Recommendation: **tarball for personal use, `.deb` for release.** Both keep `pkexec` and the polkit action working, which confined formats do not.

Note that a packaged GUI still depends on `adguard-cli` being installed separately — declare it, and fail with a clear message rather than a crash when the binary is absent (see `adguard-core::paths`).

---

## 6. Verifying against a live CLI

The proxy must be running for status-related work:

```bash
adguard-cli status
```

If it reports "not running":

```bash
adguard-cli start && adguard-cli status
```

Before testing any code path that writes config, snapshot the real settings so a bug cannot cost you your setup:

```bash
adguard-cli export-settings -o ~/adguard-settings-backup.zip
```

Restore with `adguard-cli import-settings -i ~/adguard-settings-backup.zip`.

Also copy `proxy.yaml` aside — it carries the upstream explanatory comments, and confirming they survive a `config set` round-trip is a required regression check:

```bash
cp ~/.local/share/adguard-cli/proxy.yaml ~/proxy.yaml.orig
```

---

## 7. Documentation map

| File | Purpose |
| --- | --- |
| [`adguard-cli.md`](adguard-cli.md) | Reference for the underlying CLI — commands, options, config keys |
| [`cli-contract.md`](cli-contract.md) | **Measured** CLI behaviour as an automation target; read before writing wrapper code |
| [`architecture.md`](architecture.md) | Design of the GUI: crates, data flow, UI structure, privilege model |
| [`handoff.md`](handoff.md) | Current state, the next step, and the traps worth knowing before touching anything |
| `building.md` | This file |

Keep all five in `docs/`, versioned with the code, so a change to CLI-handling behaviour and its documentation land in the same commit. When bumping the supported `adguard-cli` version, re-verify `cli-contract.md` — every claim in it is a measurement that a new release could invalidate.
