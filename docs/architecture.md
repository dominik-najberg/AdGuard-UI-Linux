# Architecture

The recommended design for **AdGuard UI for Linux** — a GTK4/libadwaita desktop front-end for `adguard-cli` on Ubuntu.

Read [`cli-contract.md`](cli-contract.md) first. It records the measured behaviour of the CLI, and several decisions below exist purely because of what it found.

---

## 1. Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Language | **Rust 1.97** | Already installed via rustup; you ship a GTK4 GUI in Rust in `LenovoLegionToolLinux`. |
| Toolkit | **GTK4 4.22 + libadwaita 1.9** | Dev headers already present (`libgtk-4-dev`, `libadwaita-1-dev`). Native GNOME 50 look on Ubuntu 26.04. Zero apt installs needed to start. |
| Crates | `gtk4` 0.11, `libadwaita` 0.9 (feature `v1_7`), `zbus` 5, `ksni`, `rusqlite`, `yaml-rust2`, `strip-ansi-escapes`, `tokio` (process + time) | Mirrors the versions already proven in `crates/legion-gui`. |
| YAML | **`yaml-rust2`**, read into a generic value tree — not `serde_yaml` and not a `derive(Deserialize)` struct | `serde_yaml` is archived upstream (its last release is literally `0.9.34+deprecated`). More decisive: `config set <bool key> 1` is accepted and writes a literal `enabled: 1`, so a strict deserialise would fail the *whole document* on one type-punned key and blank every switch on the page. Reading scalars by dotted path keeps the blast radius at one row — and the dotted path is the same string `config set` takes, so one constant drives both directions. |
| Tray | **StatusNotifierItem via `ksni`**, as a library inside the GUI process | `org.kde.StatusNotifierWatcher` is live on the session bus and `ubuntu-appindicators@ubuntu.com` is ACTIVE. `ksni` speaks SNI over D-Bus with **no C headers** — relevant because `libayatana-appindicator3-dev` is not installed. One process, not two: see §4. |
| Process model | **No daemon of our own** | The AdGuard proxy is already the daemon. CLI calls cost 10–30 ms, so there is nothing to amortise. |
| Build system | **Cargo** | Keep it plain. Meson only if flatpak packaging later demands it. |
| v1 scope | Tray + core controls | See §7. |
| Privileged ops | **None of our own** | AdGuard already ships the root helper `auto` mode needs; we detect and instruct. See §6. |

**The no-daemon point is worth stating explicitly**, because it differs from `LenovoLegionToolLinux`. That project needs `legiond` because hardware registers require sustained root. Here, root is needed only for one occasional action — setting up AdGuard's own root helper so `auto` mode can work — and AdGuard already provides the command for it, so the GUI stays a plain user-session app that never escalates at all. A persistent root daemon would be a larger attack surface for no benefit; so, it turns out, would a one-shot helper of ours. See §6.

---

## 2. Crate layout

A Cargo workspace, so the CLI-wrapping logic stays testable without a display server:

```
adguard-ui/
├── Cargo.toml                  # workspace
├── crates/
│   ├── adguard-core/           # no GTK dependency — pure logic, unit-testable
│   │   ├── cli.rs              # process wrapper: spawn, ANSI strip, error mapping, timeouts
│   │   ├── config.rs           # read proxy.yaml (yaml-rust2); writes delegate to cli.rs
│   │   ├── filters.rs          # read-only rusqlite over agflm_*.db
│   │   ├── model.rs            # ProxyStatus, Filter, FilterGroup, License, Toggles, settings tables
│   │   └── paths.rs            # locate binary + data dir, XDG-aware
│   ├── adguard-gui/            # GTK4 + libadwaita application
│   │   └── style.rs            # the app's one stylesheet: layout + theme-derived tints, §5
│   └── adguard-tray/           # ksni StatusNotifierItem — a library, not a binary
├── data/
│   ├── com.github.<you>.AdGuardUI.desktop
│   ├── com.github.<you>.AdGuardUI.metainfo.xml
│   ├── autostart/                             # the --background entry, §4
│   └── icons/
└── docs/
```

Keeping `adguard-core` GTK-free matters: the riskiest code is the output parsing, and it must be testable headlessly in CI against recorded fixtures.

---

## 3. How the GUI talks to AdGuard

Three channels, each with a single direction and purpose:

```
                    ┌─────────────────────────────────────┐
   READ  state ─────┤ proxy.yaml         (serde_yaml)      │  authoritative values
                    │ agflm_*.db         (rusqlite, RO)    │  filter catalogue + state
                    │ adguard-cli status (parse text)      │  runtime up/down + ports
                    └─────────────────────────────────────┘

   WRITE all ───────► adguard-cli <subcommand>              only mutation path

   PRIVILEGED ──────► nothing. AdGuard's own root helper,    auto mode; §6
                      set up by the user, out of process
```

**Reads never go through the CLI where a file will do.** Two reasons, both from the contract doc: the `filters list` table is unparseable for long titles (column overflow), and `config show` masks secrets and folds sections. Files give exact values; the CLI gives a presentation layer.

**Writes never touch files.** `proxy.yaml` is half explanatory comments, and `serde_yaml` cannot round-trip them, so serialising over it would strip the documentation the user relies on. `config set` preserves the file.

### State refresh

There is no push/event mechanism anywhere in the CLI, so:

- **Runtime status** — poll `adguard-cli status` on a ~2 s timer while a window is open; slow to ~10 s when only the tray is visible. At 10 ms per call this is negligible. Implemented in `status.rs` as one tick in five while the window is hidden, which is only possible because the tray shares this process (§4).
- **Config** — watched with `gio::FileMonitor`, in `adguard-gui/src/watch.rs`. External edits (the user is expected to hand-edit; the CLI even suggests it) appear live in the UI.
- **Filters** — watch the `.db` files with the same mechanism, debounced; the daemon rewrites them on update.

**A file monitor on `proxy.yaml` cannot trust its events.** Measured (contract §5): *every* `adguard-cli` invocation rewrites the file and touches its mtime, even `--version`, and even when no byte changes. Combined with the 2 s `status` poll above, a naive monitor would fire continuously against changes we caused ourselves — and each reload would repaint the page under the user's pointer.

So the monitor compares content, not notification: `config::Watch` holds the text behind the last reading and answers whether anything actually moved, and that answer — not the event — drives the repaint. Debouncing alone would not help, because the churn never stops. Measured with the app running: 40 s of idling moves the mtime and produces no reconcile at all; an edit produces exactly one; a bare `touch` produces none. The same measurement has a small silver lining — a key deleted from the file is silently restored with its default by the next invocation, so a missing setting is self-healing.

A repaint driven from outside goes to `reconcile`, never `reload`: reload swaps in a spinner and rebuilds every widget, which would discard the Advanced page's per-row `painted` guard and with it any half-typed entry. The one case that does rebuild is a page showing a spinner or an error, which has no rows to patch — so a config that was unreadable and becomes readable heals itself.

**The monitor cannot tell an outside edit from one of our own**, and it must not pretend otherwise. `Watch::prime` runs once at install and nothing re-primes after the app's own `config set`, so a user flipping a switch in the UI produces a genuine content change and therefore a reconcile — a harmless no-op repaint, since the rows already match. Re-priming after each write is not the fix: our write and the re-prime are not atomic, and losing that race means either announcing a change that was ours or missing one that was not.

So the *signal* is "the file moved"; the *fact worth reporting* is "a row you can see moved". `reconcile` returns how many displayed rows actually differed, and only a non-zero count raises an `AdwToast`. Self-inflicted writes then suppress themselves for free, and an edit to a key no page displays stays silent — which is right, because nothing the user is looking at changed. The stderr diagnostic follows the same rule and must not claim the change came from outside the app; it cannot know that. It says what it did instead: the file moved, and this many rows moved with it.

**"For free" means the per-row `pending` flag, and one page has none.** The suppression is not a property of counting; it is a property of the page that issued the write still holding the row while the write is in flight, so that row is skipped and cannot count. The Status page's *Protection modules* figure is the exception, and measurement found it the hard way: flipping ad blocking in the app left Protection's own row correctly silent and the figure reporting a change, which toasted the user's own click back at them. It is **repainted but never counted**. Nothing is lost — that figure is derived from exactly the six keys the Protection page displays, so it cannot move without a Protection row moving too.

The count gates the toast and does not appear in it. One key can legitimately move rows on two pages — `dns_filtering.enabled` is a Protection switch and is also read by the DNS page's mode row — so a number shown to the user would be arithmetic about widgets rather than about settings.

**A row's snapshot has to cover everything the row displays, not just the key it writes.** Both table-driven pages skip a row whose snapshot is unchanged, which is also how they answer whether it moved. Two settings on the Advanced page render a caveat that depends on `dns_filtering.enabled` (`Setting::requires`), and keying their snapshot on their own value alone left that caveat stale when the dependency was the thing that moved. The same applies to Protection's inert-DNS caveat and to the DNS page's mode subtitle. A snapshot narrower than the rendering is a stale row today and a wrong count tomorrow.

### Verify, don't trust

Because semantic failures exit 0 (contract §3), every mutation follows **act → re-read → reconcile**. Set a toggle, then re-read `proxy.yaml` and render from that. Never optimistically flip a switch and assume it stuck; the UI state must always be a projection of observed reality.

---

## 4. Threading, and why the tray is not its own process

All CLI invocations and SQLite reads happen off the main thread. Use the pattern already proven in `legion-gui`: a worker task plus `async-channel`, results delivered to the UI via `glib::spawn_future_local`.

### One process owns everything

`adguard-tray` began as a second binary, because `ksni` needs a tokio runtime and the GUI runs a glib main loop. That is a real constraint — the two cannot share a thread — but it was the wrong thing to organise the process model around.

The problem with two processes was not duplicated code. `adguard-core` is synchronous and GTK-free, so both already shared the act and re-read halves; and the tray's reconcile is far simpler than the GUI's, because a `ksni` menu is rebuilt from state on every update and so needs none of the pending/painted machinery that exists to stop GTK widgets drifting.

The problem was **two independent writers to `proxy.yaml`, with neither observing the other**. Toggle ad blocking from the tray with a window open and the Protection page went on showing the old value until the user pressed refresh. It also made the refresh policy in §3 inexpressible: a separate tray binary cannot know whether a window is open, and it doubled the `status` polling — which, given that every invocation touches the config's mtime, doubles the churn a file monitor has to see through.

So the GUI binary owns the process, and `adguard-tray` is a library holding the ksni layer:

- The tray thread runs a `current_thread` tokio runtime that serves D-Bus and waits on one channel. **No timer, no `Cli`, no config reads.**
- Menu activations become a `Command` on an unbounded channel, drained on the GTK main loop and dispatched to the same page methods a click uses — so a tray toggle and the switch on the page cannot disagree. This is also what `ksni`'s own documentation asks for: callbacks must not block, or the menu freezes.
- State flows the other way from the polls that already exist — the Status page's `status` read and the Protection page's config read — into `Tray::set_state`, which drops an unchanged state so an idle session generates no D-Bus traffic.
- Registration failure is a **normal outcome**, not an error. GNOME has no native tray, so a missing or disabled AppIndicator extension is expected; the application carries on windowed.
- With a tray, closing the window hides it and the process is held alive; without one, closing quits. Getting that backwards would leave a hidden app with no way to reach or quit it.

The single-instance behaviour that `adw::Application` gives us for free matters more now: launching `adguard-ui` twice activates the running one rather than starting a rival writer.

### Starting without a window

`adguard-ui --background` registers the tray and presents nothing; the autostart entry in `data/autostart/` runs it, so the tray is there from login. Three things follow from that, and none of them are free:

- **The UI is built on the first activation and kept.** Activation used to build a window unconditionally, which was invisible while the only way in was clicking a launcher for a process that was not running yet. With `--background` a later activation is routine, and each one would otherwise raise a rival window with its own poll timer and its own tray registration — inside the single process the whole model rests on.
- **The flag has to reach the instance that acts on it**, so the application takes `HANDLES_COMMAND_LINE` rather than parsing options locally and discarding them. Otherwise a second `adguard-ui --background` — autostart racing a manual launch, or a session restoring both — arrives at the running process as a bare `activate` and pulls the window on screen, which is the one thing the flag asks us not to do. It is also the only place GApplication offers to set an exit status.
- **A tray that will not register is fatal here, and only here.** The rule above is that the application carries on windowed; with `--background` there is no window to carry on with, so the process would be left with nothing on screen and no way to be reached or quit. It reports why and exits 1. Being the inverse of the surrounding rule, it is stated in the code rather than left to be inferred.

The Status page is told the window is hidden before the first poll rather than after the first close, so a background session polls at the 10 s rate from the start.

Fast reads (`status`, `config get`) can be `tokio::process::Command` awaits. Network commands (`check-update`, `filters update`, `update`) need a visible progress state and a generous timeout — a real `HttpClientNetworkError` reaching `filters.adtidy.org` is already in this machine's logs, so failure is a normal path, not an edge case.

---

## 5. UI structure (libadwaita)

An `AdwApplicationWindow` with `AdwNavigationSplitView`, plus `AdwToastOverlay` for command results:

| View | Contents | Backing |
| --- | --- | --- |
| **Status** | Hero panel: protection on/off, one primary action, restart; three at-a-glance figures; HTTP + SOCKS5 endpoints; licence state | `status`, `license`, plus `proxy.yaml` and `agflm_*.db` for the figures |
| **Protection** | `AdwSwitchRow`s: ad blocking, HTTPS filtering, stealth mode, DNS filtering, Safe Browsing, CRLite; the certificate-trust check under them | `proxy.yaml` → `config set`, plus the system trust store (§6) |
| **Filters** | `AdwPreferencesGroup` per `filter_group`, switch per filter, custom-filter add | `agflm_standard.db` → `filters …` |
| **DNS** | DNS filter list, user rules, upstream/fallback/bootstrap servers, listen port | `agflm_dns.db`, `dns_filtering.*` |
| **Advanced** | Ports, listen address, auth, outbound proxy, worker threads, log level | `proxy.yaml` → `config set` |

Notes that shape the widgets:

- **Status is the one page that is not a settings list**, and it is built differently on purpose: it answers *am I protected?*, which a row reading `Status: Running` answers in the same visual weight as the eleven rows around it. The answer is lifted into a tinted `.card` panel carrying the state, one sentence, and the single lifecycle action that applies — Start *or* Stop, never both — with the rows below it as the detail. `status.rs` opens with the reasoning.
- **The three figures on Status never come from `adguard-cli`.** They are read from `proxy.yaml` (modules on, out of six) and the two catalogues (`Catalogue::enabled_count`), which is what lets them be refreshed on a page switch: `status` is on a 2 s timer, and a figure that needed the CLI could not be recounted freely without risking the concurrent-invocation failure in contract §3.
- **One stylesheet, `gui/style.rs`**, at `APPLICATION` priority — above the platform stylesheet, below the user's `gtk.css`. It holds layout and tints only, and every colour in it is `alpha(@success_color, …)` or similar rather than a literal, so dark mode, the user's accent colour and high contrast all still apply. Prefer libadwaita's own classes (`.card`, `.title-2`, `.dim-label`, `.numeric`, `.success`) over adding to it.
- Use the **localised** filter names from `filter_localisation` (3828 rows, keyed by `lang`) rather than the English `filter.title`, matching the system locale. The tags are POSIX-style (`pt_BR`, not `pt-BR`) — see contract §6.
- **Filter text is data, not markup.** `AdwPreferencesRow:use-markup` and `AdwToast:use-markup` both default to *true*, and filter 216 is literally titled "Official Polish filters for AdBlock, uBlock Origin & AdGuard". Left on, Pango fails to parse the `&`, GTK warns, and the label renders mangled. Every row and toast carrying AdGuard's text — or the CLI's — must turn markup off, and must do so **before** the title is assigned: the label is rendered as the property is set, so passing a title to the builder warns regardless of what happens afterwards. (`AdwPreferencesGroup` has no such property; its heading is a plain `GtkLabel`, where markup is off by default.)
- Reconcile a switch **per row**, not by rebuilding the page: a 54-filter group like "Language-specific" makes losing the scroll position on every toggle obvious. The row keeps the last database-confirmed state, so `action_for` always decides from observed reality, and a programmatic write is flagged so it is not mistaken for a click.
- `listen_auth` must be forced on when `listen_address` leaves loopback — the config comment says authentication is required, and the GUI should enforce rather than merely warn. This is a **precondition, not a fix-up**: with auth off, `config set listen_address 0.0.0.0` prompts for a username, finds no TTY, and silently keeps the old address while still printing `Config has been updated` (contract §5). `config::listen_address_plan` returns the two calls in the only order that works.
- Enabling authentication is **not sufficient**: the same silent no-op happens when `listen_auth.username` *or* `listen_auth.password` is empty (contract §5). The plan cannot fix that by reordering, and must not invent a credential the user could never log in past — so it refuses and names what is missing, and the Advanced page states the requirement in the group description before the user meets it. Conversely, a *retreat* to loopback always succeeds from any state, so it is never gated: a user exposed with unusable credentials must always be able to come back.
- **The Advanced page enforces the invariant from both directions.** Authentication cannot be switched off while the listen address is beyond loopback, and moving beyond loopback asks for confirmation first — exposing a proxy to the network is not something to do on a mistyped keystroke. The row also carries a warning while it *is* exposed, for the same reason the DNS filtering row carries one while it is inert.
- **Numeric settings are the GUI's responsibility to bound.** `config set` type-checks and nothing more: it accepts port `99999`, `worker_threads 0`, and `3.5` — which writes a float that every later integer read then fails on (contract §5). `Setting::permits_number` holds the ranges. A file value outside them renders read-only with the real number shown, never clamped, since clamping the display would invite the user to write the clamped value back by accident.
- A setting that reads "on" is not necessarily doing anything. `dns_filtering.enabled` has no effect in `manual` proxy mode unless `dns_filtering.listen_port` names a real port, and the CLI enforces no such dependency — so the Protection page marks the row rather than letting the switch imply protection the user does not have. **The DNS page carries the cure**, and Protection's caveat links to it: a row offering the three states the config file documents — disabled (`-1`), automatic port (`0`), or a fixed port (`N`, bounded 1..65535 through `Setting::permits_number` like every other number). Nothing is written until the user picks one, so no listener ever appears unbidden. That row's bind address has now been **measured** (contract §5): the listener is pinned to `127.0.0.1` on UDP and TCP and does not follow `listen_address`, so it needs no confirmation dialog and no standing warning — it cannot expose anything even on a machine bound to `0.0.0.0`, and the row's description says so as fact. The measurement also found the dependency runs both ways: a port with the switch off brings up no listener either, so the row states what the other half is doing rather than implying the port alone is the cure.
- **A custom filter is added by URL, and the URL is the only thing that identifies it.** The Filters and DNS pages carry an `AdwEntryRow` in a "Custom filters" group, which is rendered even while empty — it holds the row that installs the first list, and AdGuard's own `display_number = 0` puts the group above *Ad blocking* either way. Three measured facts shape it (contract §6). The install cannot be verified the way every other mutation here is, because the new row's id is assigned by AdGuard and unknowable in advance — so the custom rows are read before and after and a row that was not there before is the evidence, never the confirmation line and never the URL, which comes back normalised for a local path. A list with no `! Title:` header stores an **empty** title and has no localisation rows, so `Filter::display_name` falls back to the URL rather than letting the row render nameless. And the group's description warns that a bad link is added rather than refused, because AdGuard checks only whether the response begins with HTML: everything else installs holding no rules, with a switch reading on.
- **An install in flight is not fenced off, and the consequence is understood rather than fixed.** A slow fetch holds AdGuard's config-path lock for up to its 60 s deadline, so a filter switch flipped meanwhile queues behind it (contract §3) — the row greys out, waits, and then settles correctly, because `toggle` already makes a switch insensitive until the database answers. Nothing is corrupted and nothing is lost; the page simply feels stuck. Locking the whole page for the duration would be the alternative and is worse: it would take the catalogue away from a user whose only mistake was pasting a URL that is slow to answer.
- **The one filter action that destroys something is confirmed, and only custom rows have it.** `filters remove` on a custom filter deletes the row outright, where the same command against a catalogue filter merely clears `is_installed` — measured from both sides, in a sandbox, before any of this was built (contract §6). That asymmetry is why turning a switch off is `disable` everywhere in this app, and it decides the affordance three times over.

  A **suffix button, not a gesture.** The row is an `AdwSwitchRow` whose activatable widget is the switch, so anything subtler — a swipe, a long press, a row click — would be reached by the same motion that toggles the list. "Off" and "gone" are precisely the two outcomes that must not be confusable here.

  A **confirmation, not an undo.** Custom ids are never reused: a list removed and re-fetched comes back as a *new* row, so anything holding an id across a removal holds a dangling reference. There is nothing to offer an undo against, which is what makes the dialog the only honest place to put the decision. It names the URL rather than only the title, because a list installed without a `! Title:` header has no name of its own and the URL is the sole thing that can bring it back — and it points at the switch, since "I wanted it off, not gone" is the mistake being guarded against.

  **Verified from the database, never from the confirmation.** `Filter [ID: …] removed` prints at exit 0 whatever happened. The check is the mirror of install's: there, a row that was not there before; here, a row that is not there after. An unreadable catalogue is reported as neither outcome — claiming a deletion we cannot see would be the worst direction to guess in.
- **A switch that reads on and cannot work is marked, wherever the reason lives.** DNS filtering without a listen port is one; HTTPS filtering with an untrusted certificate is the other, and it is worse, because the seeded state of *every* install is exactly that (contract §7). The Protection page carries the certificate check as a group directly under the switches, and the first-run assistant carries the same group under its own HTTPS question — the assistant because that screen is the moment the state is created, and the page because it is where a user goes back to. The rows go away entirely once the certificate is trusted, and never appear while HTTPS filtering is off. §6 has the design; the point here is that the two cases are treated the same way and neither is allowed to be silent.
- **Distinguish "off" from "unknown".** A key that is absent, or holds something that is not a boolean, renders as an insensitive row reading *unavailable* — never as off. Claiming ad blocking is disabled when we simply could not read the setting is the more dangerous of the two errors.
- **First run is two movements, not one: seed, then set.** This bullet used to read "an `AdwNavigationView` assistant issuing discrete `config set` calls", which measurement retired — until `proxy.yaml` exists, `config set` refuses every real key, and nothing but `configure` creates that file (contract §5). So the assistant runs `configure` **once**, on an explicit press, and only ever when the file is absent; everything after that is the ordinary write path. The guard lives in `Cli::configure` beside the spawn rather than beside the button, because the branch it prevents resets the user's whole configuration and a guard next to a call site is one someone can add a second call site around.

  Three consequences shape the pages. **Activation comes first**, because `configure` is licence-gated — so the welcome page reads the licence before offering to do anything, and sends an unlicensed user to the app proper where the Status page already carries activation. **The questions are pre-filled from the seeded file**, not from defaults copied into our source, so what the user sees is AdGuard's answer rather than our record of it; only settings they actually move are written, and moving nothing issues no calls. **What it does not ask, it says it is not asking**: `listen_address` is left to Advanced because the seed always leaves `listen_auth` off with empty credentials, which makes any move beyond loopback a measured silent no-op; `proxy_mode` is left alone because `auto` needs the root helper of §6; filter lists belong to the Filters page. `model::SETUP` carries the four that remain and the table's own comment carries those four reasons.
- **Licence activation is user-driven, not polled.** Run `activate`, take the URL out of its no-TTY message, open it with `gtk::UriLauncher`, and show a *finish activation* button that re-runs `activate` once and then reads `license`. The obvious design — poll `license` until `APP_ACTIVE` — cannot work: while unlicensed, `license` is itself refused, so there is no status to poll; and the CLI says the flow completes by running `activate` again, so waiting alone may never succeed (contract §7). The button is not a lesser version of the poll, it is the only shape the CLI supports. What makes that shape sound rather than merely available is measured: the `appid` in the link belongs to the data directory, so running `activate` again asks after the same pending activation rather than starting a rival one.
- **The link is shown as well as opened.** `UriLauncher` can fail — no browser, a portal that refuses — and a flow whose only exit is a browser that did not open is a dead end. The row carries the link with a copy button, and the launcher's failure becomes a toast rather than the end of the road.
- **Activation is offered only from a licence that is readably inactive.** Not from one we merely failed to read: what `activate` does to a working licence is not measured, and the app should not find out on a user's machine. "The licence is not active" and "the licence could not be read" are different facts and the page keeps them apart, exactly as it distinguishes "off" from "unknown" everywhere else.
- **A successful `license` read is sensitive output.** It carries the owner's e-mail and the licence key in full. `License::masked_key` shows the key's last four characters and nothing else, and `License`'s `Debug` is hand-written to mask both fields, so a stray `{:?}` in a log or an error cannot leak them. Note that the crate's older scrubber is no help here: `redact_error` replaces a secret the *caller* already knows, which is why its only caller is `Cli::set_secret`. A licence key is what came back, so there is nothing to hand it — `Cli::license` redacts by shape instead, with `redact_values` (contract §3).

### GNOME dock icon grouping

Set the GTK application ID, the `.desktop` filename, and `StartupWMClass` to the **same** reverse-DNS string. On GNOME a mismatch makes the running window fail to group with its pinned launcher — a duplicate icon appears below the favourites separator. This is a known recurring annoyance on this machine, so get it right from the first commit.

---

## 6. Privileged operations

Two things this application touches need root, and it takes neither: `auto` proxy mode, and installing AdGuard's certificate into the system trust store. **This application performs no privileged operation and ships no privileged component** — no helper binary, no polkit action, no `pkexec` call, no setuid bit.

That is possible because AdGuard already ships the escalation path for both — a fact an earlier revision of contract §8 missed for the first of them, and which turned out to hold for the second as well.

The rest of this section is auto mode; the certificate has a subsection of its own at the end, because the two are the same design applied twice and the second is the one every install meets. `adguard-cli` gates auto mode on `adguard_root_helper` being `owned_by_root`, `has_suid` and `is_executable`, and when the check fails it names the fix itself: `sudo <path>/adguard_root_helper -s`. Once that has run, switching mode is an ordinary unprivileged `config set proxy_mode auto`.

Design:

1. **Detect.** `stat` the helper for the same three properties `adguard-cli` checks. Report the check, not a guess — three separate facts, so a helper that is root-owned but not suid says so. `helper::RootHelper` takes the path as a **parameter**, not a constant: the helper ships unmet on every machine, so the met branch would otherwise be unreachable without setting a suid bit on something — the one act this whole design exists to avoid. `$ADGUARD_ROOT_HELPER` overrides it, and a file that is already setuid-root (`/usr/bin/passwd`) is what the met branch is proven against.
2. **Instruct.** When unmet, the Advanced page shows AdGuard's own command with a copy button, an explanation of what the suid bit grants, and no way to run it from the app. Re-check when the window regains focus, so a user who runs it in a terminal sees the row change without hunting for a refresh. The check is re-read every time rather than cached — a cache would be wrong at precisely the moment the re-check exists for.
3. **Switch.** When met, `config set proxy_mode auto` — a plain write through the path every other setting uses.

**The gate is load-bearing, and that is a measurement rather than an assumption.** `config set proxy_mode auto` **succeeds with all three properties unmet**: exit 0, `Config has been updated`, and `proxy.yaml` really holds `auto` afterwards (contract §8). AdGuard does not consult its helper at config-write time. So nothing but this application stands between the user and a mode that quietly does nothing, which is the `dns_filtering` mistake §5 already refuses to repeat.

It also means the unmet state has to be **rendered**, not merely prevented. A terminal or a text editor reaches `proxy_mode: 'auto'` with an unmet helper in one step, and the mode row marks that rather than correcting it — the same judgement Protection makes about DNS filtering that is switched on but inert. Writing `manual` back over the user's setting would be a change nobody asked for.

**Where AdGuard's check does fire is not measured**, and the honest place to say so is here. Reaching it needs `start`: a sandbox is unlicensed so `start` never gets that far, and starting the real proxy in `auto` is a system-wide change that is the owner's call. The design does not depend on the answer — the GUI checks before writing either way — but nobody should read the code as though the moment of enforcement were known.

**Why the app does not run that `sudo` for the user, even via `pkexec`.** The helper lives in a user-writable directory, so setting suid-root on it makes anyone who can write that file root. AdGuard chose that design and the user accepted it by installing AdGuard; conferring it from behind a GUI button is a different act from typing `sudo` at a prompt, and the deliberateness is the only safeguard the arrangement has.

The corollary is worth stating for anyone tempted to add one later: a root-invoked helper of ours would have to be explicit about which user's config it edits — `adguard-cli` and its data live under `~/.local`, so it would need the target `$HOME`/UID passed explicitly and would have to refuse any path outside that user's data dir. Getting that wrong is a local privilege-escalation bug. Not writing the helper is how this project avoids owning that problem.

`data/io.github.dominik-najberg.AdGuardUI.policy` was deleted with the auto-mode work. It declared three polkit actions against `/usr/libexec/adguard-ui-helper`, a binary that was never written and now never will be, and its own header still asserted that AdGuard "ships no polkit policy … so there is nothing to reuse" — the conclusion contract §8 retracted. Nothing installed it; `building.md` §4 says so and now says how to remove it if an older checkout did.

### The certificate is the same shape of problem

HTTPS filtering signs every connection it inspects with a CA generated on this machine, and until that CA is in the system trust store the filtering it enables breaks the first HTTPS site the user opens. `configure` generates it and then skips its own install prompt in silence, because that step needs a password and there is no TTY (contract §7) — so **every install this application sets up ends in the unmet state**, which makes it the least hypothetical case in this section.

It resolves exactly like auto mode, and for the same reason: AdGuard ships the installer. `install_cert.sh`, beside the resolved binary, elevates itself with `sudo`, copies the certificate into the system's anchor directory, rebuilds the trust store, and adds the certificate to Firefox and Chrome with `certutil` — the system's if one is installed, otherwise the copy shipped beside it, which is the branch this machine takes because `libnss3-tools` is not installed here. So:

1. **Detect.** `trust::CaTrust` reads three files and reports three facts — the certificate exists, a byte-identical copy is anchored, and the bundle carries it — in the order the machine applies them, so a user who ran the installer and still has broken HTTPS learns which step did not take. Every path is a parameter, with `$SYSTEM_CERT_DIR` (AdGuard's own variable) and `$ADGUARD_CA_BUNDLE` (ours) overriding the search. That is not symmetry for its own sake: on the reference machine the certificate **is** trusted, so here it is the *unmet* branches that would otherwise be unreachable — the mirror image of the root helper, where the met branch was the unreachable one.
2. **Instruct.** The Protection page carries the rows, directly below the switches, and the first-run assistant carries them too — immediately after its own HTTPS question, because that screen is where the state is created. AdGuard's own command, a copy button, no way to run it from the app, and a re-check when the window regains focus. `AdwPreferencesPage` has no insert-at-index, so a group's position is its `add` order and "under the switch" means under the group that holds it.
3. **There is no third step.** Unlike auto mode there is nothing for the app to write afterwards: the trust store is the whole of it, and once the user has run the command the rows disappear.

Four unmet states rather than one, because the fixes differ: no certificate at all (`adguard-cli cert`), not installed (the installer), installed but the trust store not rebuilt (`sudo update-ca-certificates`), and a **different** certificate already occupying the name. That last one is the reason the check compares bytes instead of asking whether a path exists: AdGuard's installer tests for the path and stops, reporting success, so a regenerated CA leaves a state its own tooling will not repair and a name-only check would call trusted (contract §8).

**What the check cannot see, the wording admits.** Firefox and Chrome keep their own NSS databases and read nothing from the system store; the installer covers them and this check does not. So the rows say the machine trusts the certificate, never that every browser on it does.

**A command this app will not run is still a command this app vouches for**, and that turns out to be the sharper end of showing rather than doing. The certificate's path is not a constant — it is named by `https_filtering.root_certificate_name`, an ordinary setting `config set` will write any string to — so a name carrying a `"`, a backtick, a `$` or a newline would close AdGuard's own quoting and leave the rest of it running as a second command, in a line the user has been told is AdGuard's and may well paste behind a `sudo`. `trust::quotable` refuses those paths and the row shows the state with no command at all, saying which of the two reasons applies. Re-quoting them with `'…'` was the alternative and is worse: the command would no longer be the one upstream documents, which is the entire basis for showing it.


---

## 7. v1 scope

**In:** status + lifecycle control, protection toggles, filter enable/disable with the SQLite-backed catalogue, custom filter install by URL, tray icon with quick toggles, first-run assistant, licence activation, the DNS page including its listen port, and the auto-mode switch — the last as detection and instruction, never as an escalation of our own (§6).

**Out (v2):** live blocked-request stats (needs log tailing; format undocumented and unstable — contract §9), **userscripts entirely**, HAR capture, `speed` benchmark UI, import/export, full advanced-settings parity.

Userscripts are out because there is only one. `userscripts list` returns a single entry, `adguard-extra`, and `proxy.yaml` says in AdGuard's own words that only AdGuard Extra is supported; with installation deferred, the feature is one switch for one script that ships pre-enabled. A sidebar page for that is navigation without content. This section is the scope authority — §5 and `handoff.md` no longer list a Userscripts view, and if the upstream ever supports more, this is the decision to revisit.

Ship the tray + core controls first; it is the part that replaces day-to-day terminal use.

**Added after v1 closed:** the certificate-trust check (§6). It is not a scope change so much as the other half of a v1 feature — HTTPS filtering was in from the start, and shipping it without saying whether its certificate is trusted left every install in a state the app could see and would not mention.

Status: Status, Protection, Filters (HTTP), DNS, Advanced and Stealth are done; both filter pages install custom lists by URL (§5); licence activation lives on the Status page; the first-run assistant seeds an unconfigured install and hands the window to the pages when it is finished (§5); the tray carries start/stop plus the six Protection toggles as quick toggles (§4); the config monitor reports an external edit with a toast, gated on a row the user can see having moved (§3); the Advanced page carries the proxy mode with AdGuard's root-helper check beside it (§6); and a custom list can be removed, behind a confirmation, on both filter pages. **v1 is complete.**

---

## 8. Risks

| Risk | Mitigation |
| --- | --- |
| CLI output format changes on update | All parsing confined to `adguard-core::cli`; fixture-based tests; pin the tested CLI version in `docs/` and re-verify on upgrade. |
| Semantic failures exit 0 | act → re-read → reconcile everywhere (§3). |
| A command reports success without acting | Same discipline, and it is not hypothetical: `config set listen_address` does exactly this (contract §5). Never treat a confirmation line as evidence. |
| SQLite schema changes | Treat as read-only cache; degrade to `filters list` parsing (with its known limits) if the schema is unrecognised, rather than crashing. |
| `proxy.yaml` key renamed or retyped upstream | Per-key tolerant reads, so one bad key costs one row; `config_live.rs` asserts every key the UI depends on still resolves in the real file *and* is still recognised by `config get`. |
| `proxy.yaml` comments destroyed | Never write YAML; enforced by keeping write access out of `config.rs`, and asserted by `config_mutate::a_write_disturbs_exactly_one_line`. |
| A privileged helper of ours being misused | Retired as a risk: there is no such helper, and §6 explains why there will not be. `auto` mode uses AdGuard's own, set up by the user. |
| A licence key or owner e-mail leaking into a toast or log | `license` returns both on every successful read. `License::masked_key` is the only sanctioned way to show the key; `License`'s `Debug` masks both fields so a `{:?}` cannot leak them; and `Cli::license` redacts the values out of its own parse-failure message, which would otherwise quote the reading verbatim. `license_live.rs` asserts the first two against the machine's real key; the third is pinned by `cli::tests::license_redacts_what_it_could_not_parse`, which goes through `Cli::license` rather than the helper — deleting the call at its one call site used to leave the whole suite green. |
| A GUI that blames the user's licence — or itself — for the wrong thing | Three ways an invocation can fail are told apart rather than flattened: no licence (`Unlicensed`), the program refusing on stdout (`Refused`), and a command line we built wrong (`BadInvocation`). The middle one was found by driving the licence page against a never-used data directory; see contract §3. |
| Proxy stopping underneath the UI | Status polling already detects it; surface it as a toast rather than failing silently. |
| A test suite that edits the user's real settings | `Cli::with_xdg_data_home` gives the real binary a throwaway config, so the write path — including the cases that expose the proxy — is covered against a copy (contract §5). Only one boolean round-trip still touches the live install, behind a restoring `Drop` guard. |
| A credential leaking into a log or toast | `config set` echoes the value it was given and our own error type quotes the command line; `Cli::set_secret` scrubs both. The value is still visible in `argv` for the ~20 ms of the call — unavoidable, since the CLI's only other route for a credential is the interactive prompt. |
| A `config set` that hangs instead of failing | The prompting commands only give up because there is no TTY, which is a property of *how the app was launched*. `Cli::run` closes stdin so it is a property of the wrapper instead (contract §7). |
