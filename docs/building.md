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

`make build` and `make run` wrap those two commands and nothing else — Cargo is still the build system (`architecture.md` §1). `make run ARGS=--background` passes the flag through, and a bare `make` lists the targets rather than picking one.

**A resident copy owns the launch.** The app is single-instance, so starting a fresh build while an older one is still running does not replace it: the new process hands its command line to the old one and exits. Normally that is the point, but while iterating it means the binary you just compiled never ran. Now that autostart keeps a copy resident from login, this is the common case, not the rare one:

```bash
pkill -x adguard-ui
```

A copy predating the `HANDLES_COMMAND_LINE` change refuses the handover outright, and the failure names neither cause nor cure:

```
GDBus.Error:org.freedesktop.DBus.Error.NotSupported: Application does not handle command line arguments
```

The *primary* decides whether command lines are accepted, so a process started before that flag existed rejects every launch of a build that has it. Kill it and the next launch becomes the primary. Two builds that both have the flag never see this.

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

Four suites are excluded from a plain `cargo test` because they invoke the real `adguard-cli`. They are the only tests that exercise the write path end to end — act → re-read → reconcile against the real binary — so run them after any change to `cli.rs`, and after an `adguard-cli` upgrade.

**Safe: writes only to a throwaway config.** The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`, so this one hands the real binary a copy of `proxy.yaml` in a temp directory and never touches your settings:

```bash
cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
```

That is where the dangerous behaviour is covered — exposing the proxy on `0.0.0.0`, blanking the proxy password, the `--` guard, the absent range checking. It also asserts, last, that the machine's `proxy.yaml` is byte-identical afterwards. A sandbox is unlicensed, so `status`/`license`/`filters` cannot run there; only the `config` family can (see `cli-contract.md` §5) — unless the licence is lent to it, which is what `Sandbox::licensed` does.

**Safe: writes only to a throwaway catalogue.** Custom filter install, against a sandbox holding a lent licence:

```bash
cargo test -p adguard-core --test filters_sandbox -- --ignored --nocapture
```

Every case installs a file the test wrote, through the `file://` leg `filters install` accepts on the same positional as a URL, so the suite reaches no network and cannot be broken by somebody else's list going down. It pins the boundary of AdGuard's one content check — HTML at the start is refused, JSON and prose and an empty file are not — and asserts that the machine's own custom filters are unchanged afterwards, which is one mistaken `Catalogue::open_set` away from being false: that call resolves `$XDG_DATA_HOME` from the *test* process, not the child's.

**Mutates this machine's real AdGuard configuration.** Both restore whatever they found, including on panic, but neither belongs in an unattended run:

```bash
cargo test -p adguard-core --test filters_mutate -- --ignored --nocapture
```

```bash
cargo test -p adguard-core --test config_mutate -- --ignored --nocapture
```

`config_mutate` is deliberately kept to one boolean round-trip plus the claim the whole no-YAML-writes rule rests on — that `config set` rewrites exactly one line and preserves every comment. Anything riskier belongs in `config_sandbox`.

The `*_live` suites are safe and run by default, and **skip** rather than fail when AdGuard CLI is not installed. `config_live` and `filters_live` read the real `proxy.yaml` and filter databases. `license_live` is the one that shells out — `adguard-cli license`, read-only, ~20 ms — because the licensed three-line reading cannot be captured in a sandbox: a sandbox is unlicensed by construction. It skips again when this install is not licensed, and it is written so that no assertion message can print the key.

### Continuous integration

`.github/workflows/ci.yml`, added 1 August 2026: `cargo build --workspace --locked` and `cargo test --workspace --locked` on push to `main`, on pull requests, and on demand. It runs no `cargo fmt --check` (§4 of `handoff.md` — the tree is hand-formatted and that check is deliberately dirty), no clippy gate, and never `--ignored`. The file says so at the top, so that nobody adds one of them back as an obvious omission.

**It runs in an `ubuntu:26.04` container rather than on the runner image.** `ubuntu-latest` was Ubuntu 24.04 when this was written, which ships **libadwaita 1.5** against a workspace that takes the crate's `v1_7` feature: a native job does not fail a test, it fails to build at all. The container also pins the distribution §1 describes, so a green run means what this document says it means.

**Two tests had to change to survive it, and the reason is worth keeping.** A container runs as **root** by default, so a file a test writes into `/tmp` is *root-owned* — which is the one thing `helper.rs`'s two user-owned cases exist to assert the absence of. They failed on the first clean run and nothing local could have predicted it: on a developer's machine the premise is true by construction. Both now skip when `geteuid() == 0`, printing why, which is the same answer their neighbours already give when `/bin/ls` or `/etc/hostname` is missing. The met case is unaffected — it reads `/usr/bin/passwd`, which no test process owns.

That is the whole argument for having CI on a project with one maintainer: not that the suite might break, but that a suite passing on the machine it was written on says nothing about a machine it was not.

The workflow can be rehearsed locally, which is how the above was found rather than discovered on a push:

```bash
docker run --rm -v "$PWD:/work" -w /work ubuntu:26.04 bash -c 'apt-get update -qq && apt-get install -y --no-install-recommends build-essential pkg-config libgtk-4-dev libadwaita-1-dev ca-certificates curl >/dev/null && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null && . "$HOME/.cargo/env" && cargo test --workspace --locked'
```

Copy the tree somewhere disposable first, or pass a `target/` the container can have: it builds as root and leaves root-owned artifacts behind in whatever directory it is given, which the next local `cargo` cannot remove.

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

That seeding is also why nothing should run two `adguard-cli` commands at once against a brand-new directory: one of them loses a race with the other's initialisation and exits 1 with `Filter manager initialization failed` (contract §3). The app itself no longer does this — `StatusPage::reload` sequences its two reads — but a script of your own easily can. Run any `adguard-cli` command against the directory first and it is settled for good.

A fake config is also an *unlicensed* one, which is the only way to see the Status page's activation flow. If you are going to press **Activate…** there, make sure the log-in link cannot reach a browser that is signed in to AdGuard — it is a real link, and completing it would bind a device slot to a throwaway install. `handoff.md` §4 has the two lines that arrange that.

### Reaching the first-run assistant

The assistant only appears when there is **no** `proxy.yaml`, so copying one across is exactly what hides it. Leave the directory empty instead:

```bash
rm -rf /tmp/firstrun && mkdir -p /tmp/firstrun/adguard-cli
```

But an empty directory is unlicensed, and `configure` is licence-gated — so the assistant would stop at its welcome page with AdGuard's complaint, which is worth seeing once and useless after that. **The licence lives in `adguard.conf`, and copying that one file carries it across** (contract §5):

```bash
cp ~/.local/share/adguard-cli/adguard.conf /tmp/firstrun/adguard-cli/
```

```bash
XDG_DATA_HOME=/tmp/firstrun cargo run -p adguard-gui
```

That is a licensed install with no configuration — the exact state the assistant exists for, and the only way to exercise the seeding path without resetting your own config. Two warnings go with it. The directory now holds your licence key, so delete it when you are done. And a walk of the resulting Status page carries the owner's e-mail like any other licensed install, which is the thing `handoff.md` §4's "just use a sandbox" advice no longer protects you from.

**That one file carries the certificate as well, which makes the deletion less optional than it sounds.** Measured while building the certificate check: `configure` in such a sandbox does not generate a new CA, it reproduces the machine's existing one — byte-identical, same fingerprint, dated weeks before the run (contract §8). For HTTPS filtering to work in the sandbox at all, `adguard.conf` must therefore carry the CA's **private key**, for a CA this system trusts. Copy it to `/tmp` and you have put that key somewhere it does not belong; `rm -rf` the sandbox as soon as you are finished with it.

It also means the assistant's certificate rows are *correctly* invisible in that sandbox — the certificate it seeds is already trusted here. To see the unmet branches, point the check somewhere empty:

```bash
mkdir -p /tmp/empty && : > /tmp/empty-bundle.crt && ADGUARD_CA_BUNDLE=/tmp/empty-bundle.crt SYSTEM_CERT_DIR=/tmp/empty XDG_DATA_HOME=/tmp/firstrun cargo run -p adguard-gui
```

`mkdir` first: an earlier version of this recipe created the empty *bundle* and not the empty *directory*, which was worth the correction it prompted. Both variables now answer **nothing** when they name nothing rather than falling back to the machine's own locations, so a missing directory fails the way it should — visibly, with the anchor reported as absent — instead of quietly answering from `/usr/local/share/ca-certificates` and reporting the real certificate as installed.

`$SYSTEM_CERT_DIR` is AdGuard's own variable, honoured by `install_cert.sh`; `$ADGUARD_CA_BUNDLE` is ours and exists only so those branches are reachable without removing a certificate from the machine's real trust store. `$ADGUARD_CERT_INSTALLER` does the same for the "installer is missing" branch.

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

**Unset `DISPLAY` before any of this, and know what it looks like when you forget.** A GNOME session exports `DISPLAY=:0`, and a harness that starts `Xvfb :99` without exporting `DISPLAY=:99` into the app's own environment hands the app the *real* display instead — so the window opens on the desktop through Xwayland while `ffmpeg -i :99` grabs an empty screen. The frame comes back black with nothing in it but the X cursor, which reads exactly like a window that failed to open. The AT-SPI walk is no help in spotting it: the accessibility bus is on the session bus and does not care which X server drew anything, so every probe still passes. `env -u DISPLAY -u WAYLAND_DISPLAY` on the way in, and `export DISPLAY=:99` inside.

### Screenshots for the README

`docs/screenshots/` was captured this way on 1 August 2026, one frame per page, and the recipe is worth keeping because two of its four steps are not obvious.

Select the page over AT-SPI as above, then grab with `-draw_mouse 0` — without it the X root cursor lands in the middle of the frame, which on a 1000×1400 screen is somewhere in the middle of the page.

**The window is 880×720 and no window manager is present to resize it**, so a page taller than that is cut off and there is nothing to scroll with. This is the `xdotool` trap again in its third shape: resizing does not need a window manager either, it needs `XMoveResizeWindow`, and with no WM present nothing arbitrates the request. The same twenty lines as `xfocus`, with one call changed:

```c
/* xresize.c — cc -O1 -o xresize xresize.c -lX11 */
#include <X11/Xlib.h>
#include <stdlib.h>
#include <stdio.h>

int main(int argc, char **argv) {
    if (argc < 4) { fprintf(stderr, "usage: xresize <window-id> <w> <h>\n"); return 2; }
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "xresize: cannot open display\n"); return 1; }
    XMoveResizeWindow(d, (Window) strtoul(argv[1], NULL, 0), 0, 0,
                      (unsigned) atoi(argv[2]), (unsigned) atoi(argv[3]));
    XSync(d, False);
    XCloseDisplay(d);
    return 0;
}
```

Resize once after the window maps, re-read the geometry from `xwininfo` afterwards rather than assuming the request was honoured, and crop each frame to it.

**Three values in those frames belong to this machine and none of them may be committed**: the licence owner's e-mail and the four unmasked characters of the key, both on the Status page, and the Stealth page's custom `X-Forwarded-For` address. `handoff.md` §4's redactor covers a *terminal* dump and does nothing for a PNG. Repaint them instead — Noto Sans at 11 px for a row subtitle and 13 px for an entry-row value is what the app rendered with here, since that is what Cantarell resolves to on this machine, so a placeholder drawn that way is indistinguishable from a real row.

The unmet certificate, root-helper and browser-integration groups are invisible on this machine, so a screenshot of them needs the overrides above — but check what the *command* row ends up saying before shipping the frame. Pointing `$ADGUARD_ROOT_HELPER` at `/bin/true` renders `sudo /bin/true -s`, which is a real rendering of a fake install and reads as a real instruction.

### Taking focus away and giving it back

The three checks that live outside `proxy.yaml` — the root helper, the certificate, and browser integration — all re-read themselves from one `connect_is_active_notify` handler in `main.rs` (`architecture.md` §6). For a long time the handler was the one line in this application nothing had ever exercised, because the note in this section said focus needed `xdotool`, and there is no `xdotool` here, no `wmctrl`, and no window manager on the Xvfb display at all.

**None of that is needed. `XSetInputFocus` is one call, and with no window manager present there is nothing to argue with it.** Twenty lines of C against `libX11`, which is installed:

```c
/* xfocus.c — cc -O1 -o xfocus xfocus.c -lX11 */
#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "xfocus: cannot open display\n"); return 1; }
    if (argc < 2) { fprintf(stderr, "usage: xfocus show|none|<window-id>\n"); return 2; }
    if (strcmp(argv[1], "show") == 0) {
        Window w; int revert;
        XGetInputFocus(d, &w, &revert);
        printf("focus=0x%lx revert=%d\n", (unsigned long) w, revert);
    } else if (strcmp(argv[1], "none") == 0) {
        XSetInputFocus(d, None, RevertToNone, CurrentTime);
    } else {
        XSetInputFocus(d, (Window) strtoul(argv[1], NULL, 0), RevertToParent, CurrentTime);
    }
    XSync(d, False);
    XCloseDisplay(d);
    return 0;
}
```

The window id comes from `xwininfo`, which *is* installed, and the title is the one `main.rs` sets:

```bash
xwininfo -root -children | grep '"AdGuard UI"' | awk '{print $1}'
```

Then `./xfocus none`, a second's pause, `./xfocus 0x400005`. GTK4 takes the resulting `FocusOut`/`FocusIn` without a window manager anywhere in the picture, `is-active` moves, and the handler runs. Measured: the window holds the focus from the moment it maps (`focus=0x400005 revert=2`, not `PointerRoot`), so the round trip has somewhere to come back from.

**Give the run a phase that changes nothing, and put it before the focus round trip.** The check's input is a file, and a harness that writes the file and immediately takes focus proves only that the rows are capable of changing — a 2 s poll would pass it just as well. Write the file, walk the page, and assert the walk is **identical**; then take focus, walk again, and assert it is not. It is the same discipline as hashing `proxy.yaml` either side of an edit (`handoff.md` §4), pointed the other way: there, silence had to be shown to mean something; here, a change has to be shown to have a cause.

`$ADGUARD_BROWSER_HOME` makes the browser check the cheapest of the three to drive this way, because its whole input is files under one directory that a test may create and delete — no environment variable has to change mid-process, which is impossible anyway, and no browser profile of the user's is touched.

---

## 4. Local install

Matching the pattern already used for other tools on this machine (`~/.local/bin` + a desktop entry). This is an *alternative* to the `.deb` in §5, not a companion to it: `~/.local/bin` comes before `/usr/bin` on a stock `$PATH`, so an install here wins over an installed package and keeps winning until it is removed. `make uninstall-local` removes exactly the files below, and `make install` runs it.

```bash
cargo build --release && install -Dm755 target/release/adguard-ui ~/.local/bin/adguard-ui
```

```bash
install -Dm644 data/*.desktop ~/.local/share/applications/
```

```bash
install -Dm644 -t ~/.local/share/icons/hicolor/scalable/apps data/icons/hicolor/scalable/apps/*.svg
```

```bash
install -Dm644 -t ~/.local/share/icons/hicolor/symbolic/apps data/icons/hicolor/symbolic/apps/*.svg
```

```bash
for d in data/icons/hicolor/*x*/apps; do install -Dm644 -t ~/.local/share/icons/hicolor/"$(basename "$(dirname "$d")")"/apps "$d"/*.png; done
```

```bash
gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor && update-desktop-database ~/.local/share/applications
```

The nine PNGs are pre-rendered sizes of the same drawing, 16 through 256. They are not strictly needed — GTK renders the scalable SVG at any size through librsvg — but the icon theme spec prefers an exact-size raster where one exists, and shipping them means the dock and the app grid never pay for a rasterise. If you would rather carry only the SVGs, delete `data/icons/hicolor/*x*/` and drop the loop above; nothing else refers to them.

The `-t` on the two `install` lines is not decoration: `install -D` only creates leading directories when the destination is a *file*, so the plain form above silently fails against `symbolic/apps/`, which no other application on a stock system creates. The `-t` on `gtk-update-icon-cache` is a different flag entirely — `--ignore-theme-index`, needed because `~/.local/share/icons/hicolor` has no `index.theme` and never will; only `/usr/share/icons/hicolor` ships one.

The second file is the monochrome form, for notifications and anywhere the app is drawn at label size. **The tray is not one of those places** — its icon has to say whether the proxy is running, which one static glyph cannot, so it stays on the stock `security-high-symbolic`/`security-low-symbolic` pair and nothing in this section changes it.

**Installing the icon without the desktop entry does nothing.** Nothing points a *window* at an icon file. The shell resolves the window's application ID to a desktop entry and that entry's `Icon=` to a name in the theme, so both halves have to be in place — which is why a `cargo run` build shows the generic cog until §4 has been done once. It does not need re-doing after that: the entry names the icon, not the binary's build.

The autostart entry is a separate file in a separate directory, and the glob above deliberately does not reach it — installed among the launchers it would show up as a second, windowless entry in the app grid:

```bash
install -Dm644 data/autostart/*.desktop ~/.config/autostart/
```

**The desktop file, the GTK application ID, `StartupWMClass`, and the icon filename must all be the same reverse-DNS string.** If the first three diverge, GNOME shows a second, unbranded icon below the dock separator instead of grouping the window with its launcher. If the icon is the one that drifts, the grouping still works and the icon is simply the generic cog — the quieter failure of the two, and the one to suspect first when the artwork is right but nothing shows it.

**Nothing here needs root, and nothing should.** This section used to end with a `sudo install` of `data/*.policy` into `/usr/share/polkit-1/actions/`. Do not run it, and there is nothing left to run it against: **the `.policy` file was deleted with the auto-mode work.** The application performs no privileged operation and ships no privileged component (`architecture.md` §6): AdGuard's own root helper — which `auto` mode needs, and which its HTTP proxy turns out to need in every mode (contract §8) — is set up by the user with AdGuard's own `sudo` command, which the app shows and never runs.

If an older checkout of this repo installed that file, remove it — it is a root-owned file authorising a helper binary that does not exist and never will:

```bash
sudo rm -f /usr/share/polkit-1/actions/io.github.dominik-najberg.AdGuardUI.policy
```

---

## 5. Packaging

Both recommended routes are built. Neither script needs root, and neither installs anything — building a package and installing one are separate steps here, and only the second of them is privileged:

```bash
make package
```

That leaves `target/package/adguard-ui_0.1.0_amd64.deb` (2.4 MB) and `adguard-ui-0.1.0-x86_64.tar.gz` (3.0 MB — gzip against the `.deb`'s zstd, and it carries the whole `data/` tree). `make deb` and `make tarball` build one each; the work is in `packaging/deb.sh` and `packaging/tarball.sh`, which carry the reasoning per step.

To put the `.deb` on this machine rather than just build it:

```bash
make install
```

That is `make deb`, one `sudo apt-get install` of the file it wrote, and the cleanup described below it; it is the only target in the Makefile that asks for a password — the *build* stays unprivileged, exactly as above, and only the install step is handed to `sudo`. It is `apt-get install ./file.deb` and not `dpkg -i` because apt resolves the `Depends:` line `deb.sh` derived; `dpkg` would leave the package unpacked-but-unconfigured with a "dependency problems" error and expect you to run `apt-get -f install` yourself. Uninstall is `sudo apt-get remove adguard-ui`, which has no `make` target because it needs nothing built and nothing worked out.

**The two routes shadow each other, and `make install` now resolves that rather than leaving it to be discovered.** A per-user install from the tarball puts `adguard-ui` in `~/.local/bin`, which is ahead of `/usr/bin` on a stock Ubuntu `$PATH`; both `.desktop` files run a bare `Exec=adguard-ui`; so with both installed, the package is on disk and the older per-user binary is what opens — from the terminal and from the app grid alike. Nothing about it reads as a failure. `apt` reports the package unpacked, `/usr/bin/adguard-ui` really is the new build, and the window that appears is weeks old. Version numbers cannot help here: these are not two versions of a package, they are two files with the same name, and only one of them is a package at all.

So after the `apt` step, `install` removes the `~/.local` copy — naming every path as it goes, and only paths that belong to this application — and then checks that `$PATH` actually resolves `adguard-ui` to the file the package installed, failing if anything else still wins. `make uninstall-local` does the removal on its own, for undoing a per-user install without installing the `.deb` over it. To go the other way, re-run the tarball's `install.sh`; `sudo apt-get remove adguard-ui` is still the way to remove the package.

The tarball's route is the unprivileged counterpart: extract it and run its `install.sh`, which writes under `~/.local` and never asks for anything.

The routes were assessed for this machine before either was written:

| Route | Status here | Notes |
| --- | --- | --- |
| **Tarball + `.desktop`** | **Built** — `packaging/tarball.sh` | Payload plus an `install.sh` that puts it under `~/.local`, which is §4 as a script. `PREFIX=` moves it, `--autostart` adds the login entry, `--list` names the files it would write without writing any. |
| **`.deb`** | **Built** — `packaging/deb.sh` | `dpkg-deb -b` on a hand-assembled tree; `dpkg-dev` 1.23.7 supplies `dpkg-shlibdeps`, and `debhelper` is still not installed and still not needed. |
| **Flatpak** | Not installed at all | Needs `flatpak` + `flatpak-builder` + the `org.gnome.Platform//50` runtime/SDK (~1–2 GB download). The most "correct" GNOME distribution route, but greenfield here — and see the confinement note below. |
| **Snap** | `snapd` 2.76.1 running; `snapcraft` **not** installed | Strict confinement would fight reaching `~/.local/bin/adguard-cli` and its data directory — which is the whole application. Not recommended. |
| **AppImage** | No tooling | Possible; nothing in the app resists it. |

The constraint is not privilege — there is none to preserve — but reach: this GUI is a front-end to a binary and a data directory under `$HOME`, and confinement is what breaks that. It is now a little more than a data directory, too: the certificate check reads `/usr/local/share/ca-certificates` and `/etc/ssl/certs` (§6 of `architecture.md`), which a sandboxed build would also have to be granted or would silently report every install as untrusted.

Six things the two scripts do that are worth knowing before changing them:

- **`Depends:` is derived, never written down.** `dpkg-shlibdeps` reads the binary's nine `DT_NEEDED` entries and each providing package's `.symbols` file, which gives the symbol-level minimum: `libc6 (>= 2.39)`, where copying this machine's *installed* version would have said 2.43 and refused to install on two years of perfectly capable systems — glibc 2.39 through 2.42, which is Ubuntu 24.04 through 25.10. It wants a `debian/control` relative to the working directory and reads only the package name out of it, so it gets a two-line stub in a scratch directory; `-O` makes it print to stdout instead of writing a `debian/substvars` nobody asked for.

  Two things about the fallback beneath it, both of which it got wrong first time. The substitution ends in `|| true`, because `set -e` aborts on a failed command substitution — so a `dpkg-shlibdeps` that *errors* (a library whose package ships no dependency data, which is what a locally built libadwaita looks like) would have killed the build instead of reaching the fallback written for that case. And the fallback carries the derived version predicates rather than bare package names: `libc6` with no predicate installs cheerfully on a system too old to run the binary and fails at exec with `version 'GLIBC_2.39' not found`, which is the one direction a dependency must never be wrong in.
- **No `libsqlite3-0`.** `rusqlite` is built with `bundled`, so SQLite is compiled in — confirmed from the `ldd` output, which names no sqlite at all.
- **Neither `sudo` nor `fakeroot`.** `dpkg-deb` records each file's uid and gid verbatim, so a tree built as you would install every path in `/usr` owned by you — which is what the conventional `fakeroot chown -R root:root` exists to fix. It is not needed: `--root-owner-group` (dpkg 1.19+) forces `0/0` into the archive on its own. Measured both ways, `dpkg-deb -c` shows identical `root/root` on every path, so carrying `fakeroot` would be a hard build dependency for a step with no effect — and one that is not in `build-essential`.
- **The binary is stripped in the packaging step, not by a `[profile.release]`.** 9.3 MB to 7.0 MB, all of it Rust symbol names — worth removing from a package and worth keeping in a `cargo build`, where a backtrace is the point.
- **No maintainer scripts.** dpkg's own file triggers already refresh both caches this package touches, `hicolor-icon-theme` on `/usr/share/icons/hicolor` and `desktop-file-utils` on `/usr/share/applications`. A `postinst` calling `gtk-update-icon-cache` would be duplicating dpkg's work. The `.deb` therefore has no `preinst`/`postinst`/`prerm`/`postrm` at all.
- **The autostart entry ships as an example, not a launcher.** `/usr/share/doc/adguard-ui/examples/autostart/`. Installed among the applications it would appear in the app grid as a second, windowless entry (§4); installed into `/etc/xdg/autostart` it would start the tray at login for *every* user of the machine, which is a decision for whoever runs the package and not for whoever built it.

**There is no `Depends: adguard-cli`, and there cannot be.** No such apt package exists — AdGuard CLI is a third-party install under `$HOME`, and `dpkg` resolves dependencies only against installed packages, so naming it would make the `.deb` uninstallable on every machine. The requirement is declared where a user will actually meet it: in the package description, in the tarball's README, and at runtime, where `paths::cli_binary` returns `None` and `main.rs`'s `missing_cli_view` renders an explanation instead of crashing — which is what "fail with a clear message" meant, and it was already true before there was anything to package.

**The two packages carry the licence differently, and that is deliberate.** The repository has held `LICENSE` — the verbatim GPLv3, byte-identical to `/usr/share/common-licenses/GPL-3` — since 1 August 2026. The `.deb` still ships no copy of it: its `copyright` file points at that system path, which is what Debian policy asks for and what every package on the machine already does. The tarball ships the file itself, because it is the one route by which this build reaches a machine whose `/usr/share/common-licenses` may not exist, and GPL-3.0-or-later §4 wants a copy conveyed with the program rather than a reference to one.

One thing the packaging still does **not** do, on purpose: it writes no `changelog.Debian.gz`. Debian policy wants one for an archive upload, and this package is not built for an archive.

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
| [`handoff.md`](handoff.md) | **Start here.** §0 is the entry point for a new session — ground truth to check, what state this machine is in, what to read, what is next and what nothing may do. Then current state, the open gaps, and the traps |
| `building.md` | This file — prerequisites, running, tests, install, packaging |
| `overnight-plan.md` | **Archived.** Its night is over and every item in it is done. Kept for §3, the standing stop list, and §4, the verification discipline — both of which apply to any session, not just an unattended one |

The scripts under `packaging/` are the seventh piece of documentation, and are written to be read: each step says why it is that shape, and §5 above says what the two of them are for.

Keep all six in `docs/`, versioned with the code, so a change to CLI-handling behaviour and its documentation land in the same commit. When bumping the supported `adguard-cli` version, re-verify `cli-contract.md` — every claim in it is a measurement that a new release could invalidate.
