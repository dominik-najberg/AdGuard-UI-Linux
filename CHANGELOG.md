# Changelog

Notable changes per release. Versions are [semantic](https://semver.org/); the
public surface this promises against is the application's own behaviour and its
packaging, not any Rust API — the three crates are internal and are not
published to crates.io.

There is no `changelog.Debian.gz` in the `.deb`, on purpose: that file is for an
archive upload and this package is not built for one (`docs/building.md` §5).
This file is the one changelog.

---

## 1.2.0 — 4 August 2026

### Start at login, as a switch

Asked for from the outside: an option to start with the session, writing a
`.desktop` file into `~/.config/autostart`, with a flag that keeps the main
window closed.

The flag was already there. `adguard-ui --background` has registered the tray
and presented no window since 1.0, and it is what the entry in
`data/autostart/` — installed by `packaging/tarball.sh --autostart` — has been
running at login all along. What was missing was a way to install that entry
without a terminal, so that is what this adds:

- **Start at login**, at the foot of the Advanced page. Switching it on writes
  `~/.config/autostart/io.github.dominik-najberg.AdGuardUI.desktop`; switching
  it off deletes it. The name is the one the packaging already installs, so the
  switch, the shipped entry and whatever a startup-applications editor lists are
  all one file — disable it out there and the switch reads off, and the row
  re-reads itself every time the window is focused.
- **The entry runs this binary's own path**, not the bare `adguard-ui` that the
  shipped example resolves against `$PATH`. A session manager's `$PATH` need not
  include `~/.local/bin`, where the per-user install puts things, and an entry
  it cannot resolve fails at login with nothing on screen to say so.
- **The row says when a background start would have nowhere to appear.** With no
  tray icon — GNOME without an AppIndicator extension — `--background` leaves
  the application unreachable and exits instead, which is the one place a tray
  that will not register is fatal. The window knows whether the tray registered,
  so the switch says so beside itself rather than leaving it for the journal.

- **It is reported on the Status page too**, as a read-only row at the foot that
  leads to the switch. That page answers *am I protected?* and owns no settings,
  so it reports and links rather than offering a second control — and the group
  it sits in says plainly that this is about the window and the tray icon, since
  a row reading "Start at login — No" on that page invites exactly one wrong
  conclusion.

No new flag: `--silent` and `--quiet` would have been synonyms for
`--background`, and one behaviour with three names is one more thing to keep in
step. This switch neither starts nor stops AdGuard's protection: what runs the
proxy at login is AdGuard's own arrangement, not this.

### The Annoyances group could not be switched on at all

Reported from the outside: the five `AdGuard …` annoyance filters could not be
enabled from the application, which showed *"Please read carefully before
enabling Annoyance filters"* and then never showed anything to read. The
workaround was to enable them in a terminal and accept there.

AdGuard's CLI gates that group behind an agreement typed at a prompt, and this
application runs it with stdin closed so that every prompt takes its default
([`docs/cli-contract.md`](docs/cli-contract.md) §7). That is right for every
other prompt the CLI has and wrong for this one, which does not take a default —
it refuses the work. Three things were wrong and all three are fixed:

- **The agreement is now shown and answered.** Switching on a list from the
  Annoyances group opens a dialog carrying AdGuard's own wording verbatim, and
  agreeing sends the answer through to the CLI. Declining leaves the switch off
  and nothing is run — the question comes *before* the command, because
  `filters add` subscribes to a list before refusing to enable it, so asking
  afterwards would have left the subscription behind.
- **A silent half-success is now reported.** `filters add` prints
  `Filter […] added` before it refuses, which the wrapper's success check
  accepted — so the first click on one of these lists really did subscribe to
  it, left it switched off, and said only *"Could not enable …"*.
- **The gate is eleven lists, not five.** Measuring the whole catalogue found
  `Fanboy's Annoyances`, `Web Annoyances Ultralist`, `Adblock Warning Removal
  List`, `EasyList Cookie List` and two more gated identically — while
  `CJX's Annoyances List`, which has the word in its title but sits in
  Language-specific, is not gated at all. The check is by catalogue group, and
  it is per-set: group 4 of the DNS catalogue is *Security*, and testing the
  number bare would have put a dialog about violating websites' terms of use in
  front of the DNS malware lists.

---

## 1.1.0 — 2 August 2026

Settings that existed in `proxy.yaml` and on no page, traffic capture, and
backup and restore. Ten slices of work, none of which changes anything 1.0.0
already did — which is why this is a minor release. "v2" in this project's
documentation is a scope word and not a version number
([`docs/architecture.md`](docs/architecture.md) §7).

### The settings that were in the file and not on a page

An enumeration taken on 2 August 2026 walked every leaf key of `proxy.yaml`
against every key the pages can actually reach — mechanically, so the two sides
could not drift the way a retyped list does. It found **80 keys in the file, 58
rendered somewhere, 22 not**. Seven of the 22 should stay unrendered and two
belonged to the traffic-capture work below, which left eleven rows worth
building; all eleven are here. **At this release 71 of the 80 keys are
rendered**, and `docs/architecture.md` §5 carries the enumeration and the reason
for each of the nine that are not.

- **HTTPS filtering** (Advanced) — five switches: EV-certificate sites, TLS 1.3,
  OCSP, Certificate Transparency and HTTP/3. One group description carries their
  shared dependency on HTTPS filtering being switched on, rather than five
  separate markings saying the same thing five times.
- **Browser compatibility** (DNS) — blocking Encrypted Client Hello, last on the
  page because `proxy.yaml`'s own comment says the common case is to leave it
  alone. Its subtitle covers three states, including that it does nothing at all
  while DNS filtering is off — a combination the CLI will happily create.
- **Privacy** (Protection) — the anonymous-statistics consent. **The first row
  this application ships that it cannot describe**: `proxy.yaml`'s comments, the
  CLI's help and the binary's own strings document nothing about what the key
  sends, so every state of the row says so rather than inventing a payload.
- **Diagnostics: response tagging** (Advanced) — the `X-Adguard-Filtered` and
  `X-Adguard-Rule` headers. They are added to **responses**, so the sites you
  visit never receive them; the row says which direction it is, because the
  intuitive reading is the wrong one.
- **Filtered ports** (Advanced) — the port list, directly above *Manual proxy
  ports* and worded to mirror it. The one row that checks the value before
  writing instead of letting the CLI word its own refusal, because this key's
  refusal recommends the very form it rejects.
- **Outgoing connections** (Advanced) — the outbound interface, in its own group
  after *Listen address* so the page reads incoming and then outgoing. It binds
  every outgoing connection rather than only outbound-proxy traffic, which is
  why it is not filed under *Outbound proxy*. Empty is a real setting here and
  means the system decides.
- **Automatic language filters** (Filters) — on the Filters page, above the
  catalogue it writes to, because a user who finds a filter switched on that
  they never switched on is looking at that page. Its subtitle is a measurement:
  the automatic add keys on whether a list is installed, so a list you switched
  off stays off, while a list you removed comes back switched on.

### Two things 1.0.0 listed as out of scope, now built

- **Traffic capture** (Advanced) — the HAR switch and its folder, in a group
  whose description is the feature: capture records response bodies, the files
  are world-readable, and a measured run produced 114 MB in six minutes, one
  file per run, with nothing pruning them. It ships switched off, which is what
  answers the "too heavy for an always-on UI" objection that kept it out of
  1.0.0. A leading `~` in the folder is expanded before the write; the CLI
  stores it literally.
- **Backup and restore**, in two halves. *Export settings* and *Restore
  settings* sit in their own group at the foot of Advanced, and *Export logs*
  sits beside the log level that decides what goes into it. Three of the strings
  exist to forbid an obvious assumption: a round trip **loses DNS filter choices
  and DNS user rules**, because only `proxy.yaml` is exported; a restore leaves
  the **licence and the certificate untouched**; and the logs bundle **includes
  your configuration and not your browsing record**. A chosen archive is
  identified before anything is offered, so a logs bundle is refused with an
  explanation rather than handed to the importer — which would accept it at exit
  0 and half-replace the install. Export asks for a folder rather than a
  filename, so there is no ambiguity about an existing path.
- **Restore from a backup at first run** is the second half, and it is offered
  in **every** branch of the assistant including the two that otherwise refuse
  to set anything up: importing settings is not licence-gated where seeding a
  configuration is, so a restore is reachable by exactly the user that screen
  turns away. It does not hand over the window silently the way a completed
  setup does — it ends on a screen naming what a backup cannot carry: the
  licence, the certificate while the restored configuration says HTTPS filtering
  is on, and the DNS selections.

### Known limits at 1.1.0

- **The activation success leg is still unmeasured**, unchanged from 1.0.0 and
  for the same reason: watching it needs a real account and spends a device slot
  ([`docs/handoff.md`](docs/handoff.md) §3 item 6).
- **A headless GUI run under a sandboxed data directory can stop a running
  proxy.** Measured on 2 August 2026 from the daemon's own log rather than
  inferred — three clean shutdowns, each within about a second of such a start.
  Six launches against the real data directory did not reproduce it, so this
  looks like a property of the test sandbox rather than of ordinary use, but
  which part of the GUI does it is still not established; recovery is
  `adguard-cli start`. This predates the release: it is the cause of outages two
  earlier sessions recorded as unexplained, and what is new is the measurement,
  not the behaviour ([`docs/handoff.md`](docs/handoff.md) §3 item 11).
- **Userscripts, live blocked-request stats and the `speed` benchmark remain out
  of scope**, each for a recorded reason (`docs/architecture.md` §7). HAR capture
  and import/export were on that list at 1.0.0 and have come off it.
- **A tray icon still needs an AppIndicator extension** — GNOME has no native
  tray. Without one the application prints a line to stderr and runs windowed.

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
