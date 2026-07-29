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

Two suites are excluded from a plain `cargo test` because they invoke the real `adguard-cli` and mutate this machine's actual AdGuard configuration. Both restore whatever they found, but neither belongs in an unattended run:

```bash
cargo test -p adguard-core --test filters_mutate -- --ignored --nocapture
```

```bash
cargo test -p adguard-core --test config_mutate -- --ignored --nocapture
```

They are the only tests that exercise the write path end to end — act → re-read → reconcile against the real binary — so run them after any change to `cli.rs`, and after an `adguard-cli` upgrade. `config_mutate` also asserts the claim the whole no-YAML-writes rule rests on: that `config set` rewrites exactly one line and preserves every comment.

The `*_live` suites are safe and run by default; they read the real `proxy.yaml` and filter databases, and **skip** rather than fail when AdGuard CLI is not installed.

GUI code needs a display. Under Wayland, headless CI requires a compositor:

```bash
cargo test --workspace
```

For a quick look at the GUI without one, `Xvfb` is enough to render and screenshot it:

```bash
xvfb-run -n 99 -s "-screen 0 1000x820x24" env GDK_BACKEND=x11 ./target/debug/adguard-ui
```

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
| `building.md` | This file |

Keep all four in `docs/`, versioned with the code, so a change to CLI-handling behaviour and its documentation land in the same commit. When bumping the supported `adguard-cli` version, re-verify `cli-contract.md` — every claim in it is a measurement that a new release could invalidate.
