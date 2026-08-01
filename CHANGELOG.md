# Changelog

Notable changes per release. Versions are [semantic](https://semver.org/); the
public surface this promises against is the application's own behaviour and its
packaging, not any Rust API — the three crates are internal and are not
published to crates.io.

There is no `changelog.Debian.gz` in the `.deb`, on purpose: that file is for an
archive upload and this package is not built for one (`docs/building.md` §5).
This file is the one changelog.

---

## 1.0.0 — 1 August 2026

First release. Everything below has been in the tree for some time; what is new
on this date is that it is tagged, packaged and downloadable.

### The pages

- **Status** — runtime state, start / stop / restart, the proxy endpoints and the
  licence. Polled every 2 s while the window is up, every 10 s when only the tray
  is showing.
- **Protection** — the six protection modules, each one switch over one key in
  `proxy.yaml`.
- **Filters** — AdGuard's own catalogue read from its SQLite databases with
  localised names, plus custom lists installed by URL and removed behind a
  confirmation that names the list.
- **DNS** — the DNS filter catalogue, your own DNS rules, the three server
  settings, and the local DNS proxy's listen port as disabled / automatic /
  fixed.
- **Stealth** — the 26 tracking-protection settings behind Protection's stealth
  switch, including the nested anti-DPI section.
- **Advanced** — proxy mode, ports, listen address and authentication, outbound
  proxy, worker threads, log level and secure DNS filtering. A setting whose
  effect depends on another setting says so rather than appearing to work.
- **First-run assistant** — for a machine with no `proxy.yaml` at all: the licence
  check, one guarded `configure` to seed a configuration, four questions, and
  then the pages above.

### Around the pages

- **A tray icon** carrying start / stop and the six protection toggles, in the
  GUI process rather than a second executable — so a tray toggle and the switch
  on the Protection page are the same write and cannot disagree.
- **Licence activation**, user-driven: `activate` hands back a link, and a
  *finish activation* button re-runs it. Never polled.
- **External edits reconcile live.** A monitor on `proxy.yaml` repaints the
  table-driven pages when the file moves, without churning on the application's
  own CLI traffic, and raises a toast only when a row you can actually see moved.
- **`--background`** registers the tray and presents no window, which is what the
  autostart entry runs at login. A second launch activates the running copy
  rather than starting a rival writer.

### The three prerequisites it detects and will not perform

The certificate that HTTPS filtering signs with, AdGuard's root helper, and the
browser-integration manifests are each detected, named, and paired with
**AdGuard's own command** and a copy button. All three re-read themselves when
the window regains focus. This application ships no privileged component: no
`sudo`, no `pkexec`, no setuid bit set on anything (`docs/architecture.md` §6).

### Packaging and distribution

- `make package` builds a `.deb` and a tarball for `~/.local`; neither build step
  needs root, and only `make install` asks for a password.
- The `.deb`'s `Depends:` is derived by `dpkg-shlibdeps` rather than written
  down, which is what keeps it installable on Ubuntu 24.04 through 26.04 rather
  than only on the machine that built it.
- Tagging `v1.0.0` builds both packages in an `ubuntu:26.04` container and
  attaches them, with checksums, to the GitHub release
  ([`.github/workflows/release.yml`](.github/workflows/release.yml)).

### Known limits at 1.0.0

- **The activation success leg is unmeasured.** Everything up to the browser
  log-in is proven, including against a real unlicensed install; what nobody has
  watched is the leg after a genuine log-in, because it needs a real account and
  spends a device slot (`docs/handoff.md` §3 item 6).
- **Userscripts, live blocked-request stats, HAR capture, the `speed` benchmark
  and import/export are out of scope**, each for a recorded reason
  (`docs/architecture.md` §7).
- **A tray icon needs an AppIndicator extension** — GNOME has no native tray.
  Without one the application prints a line to stderr and runs windowed.
