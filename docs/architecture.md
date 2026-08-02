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
| Privileged ops | **None of our own** | AdGuard already ships the root helper it needs — for `auto` mode, and measurably for its HTTP proxy in any mode; we detect and instruct. See §6. The one thing the app does perform — clearing a proxy process the CLI has lost track of — needs no privilege, because the process is the user's own. |

**The no-daemon point is worth stating explicitly**, because it differs from `LenovoLegionToolLinux`. That project needs `legiond` because hardware registers require sustained root. Here, root is needed only for one occasional action — setting up AdGuard's own root helper, without which auto mode does nothing and the HTTP proxy serves nothing — and AdGuard already provides the command for it, so the GUI stays a plain user-session app that never escalates at all. A persistent root daemon would be a larger attack surface for no benefit; so, it turns out, would a one-shot helper of ours. See §6.

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

   PRIVILEGED ──────► nothing. AdGuard's own root helper,    §6
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
| **Status** | Hero panel: protection on/off, one primary action, restart; three at-a-glance figures; HTTP + SOCKS5 endpoints; what is being filtered; licence state. Every figure and row but the licence links to the page that owns the setting behind it, and the HTTP endpoint carries the root-helper caveat of §6 | `status`, `license`, plus `proxy.yaml` and `agflm_*.db` for the figures, and one `stat` of the root helper |
| **Protection** | `AdwSwitchRow`s: ad blocking, HTTPS filtering, stealth mode, DNS filtering, Safe Browsing, CRLite; the certificate-trust check under them; the statistics-consent row last | `proxy.yaml` → `config set`, plus the system trust store (§6) |
| **Filters** | `AdwPreferencesGroup` per `filter_group`, switch per filter, custom-filter add | `agflm_standard.db` → `filters …` |
| **DNS** | DNS filter list, user rules, upstream/fallback/bootstrap servers, listen port, ECH blocking | `agflm_dns.db`, `dns_filtering.*` |
| **Advanced** | Proxy mode, HTTPS filtering, secure DNS filtering, ports, listen address, auth, outbound proxy, worker threads, log level | `proxy.yaml` → `config set` |
| **Stealth** | The 26 settings behind the one `stealthmode.enabled` switch Protection shows: cookies, tracking, identity, browser APIs, anti-DPI | `proxy.yaml` → `config set` |

Notes that shape the widgets:

- **Status is the one page that is not a settings list**, and it is built differently on purpose: it answers *am I protected?*, which a row reading `Status: Running` answers in the same visual weight as the eleven rows around it. The answer is lifted into a tinted `.card` panel carrying the state, one sentence, and the single lifecycle action that applies — Start *or* Stop, never both — with the rows below it as the detail. `status.rs` opens with the reasoning.
- **The three figures on Status never come from `adguard-cli`.** They are read from `proxy.yaml` (modules on, out of six) and the two catalogues (`Catalogue::enabled_count`), which is what lets them be refreshed on a page switch: `status` is on a 2 s timer, and a figure that needed the CLI could not be recounted freely without risking the concurrent-invocation failure in contract §3.
- **Everything on Status that reports a setting is a way in to that setting.** The page reads and does not write, which is what makes it readable and what also made it a dead end: "4 of 6" is a question about the other two, "Disabled" is a question about how to change it, and the answer to both used to be for the user to already know which of the five other pages to open. So the three figures are buttons, the endpoint and filtering rows are activatable with a `go-next-symbolic` arrow, and each names a `Destination` — a page, and where on it. `status.rs` picks the destination and knows nothing more; `main.rs` resolves it by **selecting the sidebar row**, so the highlight, the header title, the recount of the figures and the narrow-window transition all follow from the one thing they already hang off. **No link writes anything**: a shortcut on Status that flipped `proxy_mode` would be a second writer for a key the Advanced page owns, which is the arrangement §4 exists to prevent.
- **Arriving is not the same as switching pages**, so a destination that is one group deep says which. `crate::reveal` scrolls it to the top of the view and tints it for a moment — the tint is not decoration: a page that has jumped to the middle of itself is indistinguishable from one that opened somewhere arbitrary until something says which group was meant. `crate::scroll_to` is the same without the tint, for the filter counts, where the answer is every group from there down rather than the first one. Both wait for a frame before measuring: a `GtkStack` does not allocate the children that are not showing, so at the instant the page is switched to there is no position to scroll to, and asking for one yields zero.
- **One stylesheet, `gui/style.rs`**, at `APPLICATION` priority — above the platform stylesheet, below the user's `gtk.css`. It holds layout and tints only, and every colour in it is `alpha(@success_color, …)` or similar rather than a literal, so dark mode, the user's accent colour and high contrast all still apply. Prefer libadwaita's own classes (`.card`, `.title-2`, `.dim-label`, `.numeric`, `.success`) over adding to it.
- Use the **localised** filter names from `filter_localisation` (3828 rows, keyed by `lang`) rather than the English `filter.title`, matching the system locale. The tags are POSIX-style (`pt_BR`, not `pt-BR`) — see contract §6.
- **Filter text is data, not markup.** `AdwPreferencesRow:use-markup` and `AdwToast:use-markup` both default to *true*, and filter 216 is literally titled "Official Polish filters for AdBlock, uBlock Origin & AdGuard". Left on, Pango fails to parse the `&`, GTK warns, and the label renders mangled. Every row and toast carrying AdGuard's text — or the CLI's — must turn markup off, and must do so **before** the title is assigned: the label is rendered as the property is set, so passing a title to the builder warns regardless of what happens afterwards. (`AdwPreferencesGroup` has no such property; its heading is a plain `GtkLabel`, where markup is off by default.)
- Reconcile a switch **per row**, not by rebuilding the page: a 54-filter group like "Language-specific" makes losing the scroll position on every toggle obvious. The row keeps the last database-confirmed state, so `action_for` always decides from observed reality, and a programmatic write is flagged so it is not mistaken for a click.
- **The catalogue is searched, not scrolled.** 86 rows in the HTTP catalogue and 65 in the DNS one, 54 of the first in "Language-specific" alone — finding a list by eye is the wrong tool for that, so a `GtkSearchEntry` sits above the page and hides what does not match. It matches on the name, the description, the source URL and the group name **together**: "cookie", "tracking" and "annoyance" name no list in the catalogue and describe a dozen, and a custom list with no `! Title:` header *is* its URL (contract §6). The field is above the scrolled page rather than inside it, since what it acts on is precisely what has scrolled off, and not in the window's header bar, which is shared with five pages that have nothing to search. Terms are ANDed; a group that keeps no row hides with its heading; a search matching nothing swaps in a status page rather than leaving an empty one, which would read as a catalogue that had failed to load. The query survives the rebuild an install or a removal triggers — the same search that found a list is usually about to find the next one. On the DNS page the host's settings groups step aside for the length of a search: the field searches filter lists, and a settings group cannot answer.
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

  Three consequences shape the pages. **Activation comes first**, because `configure` is licence-gated — so the welcome page reads the licence before offering to do anything, and sends an unlicensed user to the app proper where the Status page already carries activation. **The questions are pre-filled from the seeded file**, not from defaults copied into our source, so what the user sees is AdGuard's answer rather than our record of it; only settings they actually move are written, and moving nothing issues no calls. **What it does not ask, it says it is not asking**: `listen_address` is left to Advanced because the seed always leaves `listen_auth` off with empty credentials, which makes any move beyond loopback a measured silent no-op; `proxy_mode` is left alone because `auto` needs the root helper of §6, which that screen now carries rows for rather than merely mentioning; filter lists belong to the Filters page. `model::SETUP` carries the four that remain and the table's own comment carries those four reasons.
- **Licence activation is user-driven, not polled.** Run `activate`, take the URL out of its no-TTY message, open it with `gtk::UriLauncher`, and show a *finish activation* button that re-runs `activate` once and then reads `license`. The obvious design — poll `license` until `APP_ACTIVE` — cannot work: while unlicensed, `license` is itself refused, so there is no status to poll; and the CLI says the flow completes by running `activate` again, so waiting alone may never succeed (contract §7). The button is not a lesser version of the poll, it is the only shape the CLI supports. What makes that shape sound rather than merely available is measured: the `appid` in the link belongs to the data directory, so running `activate` again asks after the same pending activation rather than starting a rival one.
- **The link is shown as well as opened.** `UriLauncher` can fail — no browser, a portal that refuses — and a flow whose only exit is a browser that did not open is a dead end. The row carries the link with a copy button, and the launcher's failure becomes a toast rather than the end of the road.
- **Activation is offered only from a licence that is readably inactive.** Not from one we merely failed to read: what `activate` does to a working licence is not measured, and the app should not find out on a user's machine. "The licence is not active" and "the licence could not be read" are different facts and the page keeps them apart, exactly as it distinguishes "off" from "unknown" everywhere else.
- **A successful `license` read is sensitive output.** It carries the owner's e-mail and the licence key in full. `License::masked_key` shows the key's last four characters and nothing else, and `License`'s `Debug` is hand-written to mask both fields, so a stray `{:?}` in a log or an error cannot leak them. Note that the crate's older scrubber is no help here: `redact_error` replaces a secret the *caller* already knows, which is why its only caller is `Cli::set_secret`. A licence key is what came back, so there is nothing to hand it — `Cli::license` redacts by shape instead, with `redact_values` (contract §3).

### What the pages do not render — the advanced-parity enumeration

§7 makes this enumeration the **first task** of the advanced-parity item and says plainly that it is not code. It was taken on 2 August 2026. The walk is mechanical rather than by eye: every leaf path of `proxy.yaml` on one side, every `key:` literal reachable from `ADVANCED`, `STEALTH`, `SETUP` and `Toggle::key` on the other, resolved through `config::key` so the table cannot drift from the source the way a retyped one would.

**The count when it was taken: 80 leaf keys in the file, 58 rendered somewhere, 22 not.** And §7's prediction holds — the gap is smaller than "parity" sounds, because seven of the 22 should stay unrendered and two belong to a different item.

**Since then eight have been built** — the *HTTPS filtering* group, `dns_filtering.block_ech`, `safebrowsing.send_anonymous_statistics` and `adguard_headers_enabled`, all below — so the live figures are **66 rendered, 14 not**. The enumeration's own counts are left as they were taken rather than edited in place: it is a measurement with a date on it, and rewriting the numbers would make the later work invisible.

**One measured caveat on the 58.** `send_crash_reports` is the only key rendered *exclusively* by the first-run assistant. It is reachable on the one screen a user sees once and can never return to, so a user who changes their mind about crash telemetry has no page to change it on. That is a gap of a different shape from the 22 and it is listed with them below.

Everything in the *Key*, *Type* and *Stock* columns is read from the file; every key marked addressable answered `config get` with `key = value` at exit 0, measured against the live install with `proxy.yaml`'s hash taken either side and unmoved (contract §5 for the three that refuse). **The *Verdict* column is a proposal, not a measurement** — it is the reasoning this enumeration exists to produce, and §7 remains the authority over whether any of it becomes a row.

#### Should become rows — 11 proposed, **9 built**

**Nine are done**, as of 2 August 2026: the *HTTPS filtering* group on the Advanced page, between *Proxy mode* and *Secure DNS filtering*, `dns_filtering.block_ech` as DNS ▸ *Browser compatibility*, `safebrowsing.send_anonymous_statistics` as Protection ▸ *Privacy*, `adguard_headers_enabled` in Advanced ▸ *Diagnostics*, and `filtered_ports` as Advanced ▸ *Filtered ports* — the first of the nine that is not a switch, and the only row on that page which does not let the CLI word its own refusal. Every one was measured writable through `config set` before the code was written — surgical, one line each, the file's 220 unchanged — and then verified rendering headlessly, in each state the row can reach. **So the gap is now 13 unrendered keys, not 22**, and this table is the record of why each of the rest is or is not next.

| Key | Type | Stock | Where it belongs, and what it depends on |
| --- | --- | --- | --- |
| ~~`https_filtering.filter_ev_certificates`~~ | bool | `false` | **Built** — Advanced ▸ *HTTPS filtering* |
| ~~`https_filtering.enable_tls13`~~ | bool | `true` | **Built** — same group |
| ~~`https_filtering.ocsp_check_enabled`~~ | bool | `true` | **Built** — same group |
| ~~`https_filtering.enforce_certificate_transparency`~~ | bool | `true` | **Built** — same group |
| ~~`https_filtering.http3_filtering_enabled`~~ | bool | `true` | **Built** — same group, and the row carries the file's *experimental* |
| ~~`dns_filtering.block_ech`~~ | bool | `false` | **Built** — DNS ▸ *Browser compatibility*, its own group and the page's last |
| ~~`safebrowsing.send_anonymous_statistics`~~ | bool | `false` | **Built** — Protection ▸ *Privacy*, its own group and the page's last |
| `auto_enable_language_filters` | bool | `true` | **Filters page**, and **blocked on one measurement** — see below. Both halves of this row's original reasoning were wrong |
| ~~`adguard_headers_enabled`~~ | bool | `false` | **Built** — Advanced ▸ *Diagnostics*, beside `log_level` |
| ~~`filtered_ports`~~ | str | `'80:5221,5300:49151'` | **Built** — Advanced ▸ *Filtered ports*, its own group directly above *Manual proxy ports*. ~~Its compound range syntax is ours to validate, since `config set` type-checks strings not at all~~ — **refuted 2 August 2026**: the CLI validates it, and what was ours turned out to be the wording, see below |
| `outbound_interface` | null | `null` | Advanced *Outbound proxy*. ~~Needs a design for the null case: how an empty text field writes back `null` is unmeasured~~ — **measured 2 August 2026, and the write half was never the problem.** The read half is: this row cannot be a `Kind::Text`, see below |

Five of the eleven were one coherent block — the `https_filtering.*` group — which is what made it the obvious first slice, and it is the one that got built.

**Its dependency is stated in the group description, not on each row, and that is deliberate.** All five are inert unless `https_filtering.enabled` is on, but that switch is the *section they live in* rather than another section — which is not what `Setting::requires()` models. `requires()` exists for the cross-section dependencies `proxy.yaml` states in words and the CLI does not enforce, and `only_the_documented_settings_declare_a_dependency` asserts there are exactly two of those. The right precedent is Stealth: its twenty-six settings all depend on `stealthmode.enabled`, and every group there says so in its description rather than marking twenty-six rows. Extending that is what §7 asked for; adding a sixth and seventh `requires()` would have been the *dependency the GUI invents* that same test exists to catch.

**`dns_filtering.block_ech` was called an inconsistency, and reading the file settles it the other way.** The claim was that it and `https_filtering.encrypted_client_hello` are two halves of ECH handling landing on two pages, and that whichever way it went it should go deliberately. It went deliberately, and it went *apart*, because `proxy.yaml` describes them as different features rather than two halves of one:

> `encrypted_client_hello` — *"Encrypted Client Hello (ECH) support - enables ECH for better privacy. Requires dns_filtering to be enabled"*
>
> `block_ech` — *"Block ECH by removing 'ech' parameter from SVCB/HTTPS DNS records. Most browsers auto-detect HTTPS filtering and disable ECH themselves. Enable this only for problematic browsers that don't auto-detect"*

One is a privacy feature AdGuard offers; the other is a workaround for a browser that fails to notice HTTPS filtering, and turning it on **costs** the privacy the first one buys. Filing them together under *Secure DNS filtering* would put a switch that strips ECH directly beneath one that enables it, reading as a matched pair of preferences when they are nothing of the kind. So `encrypted_client_hello` stays where v1 shipped it and `block_ech` went to the DNS page — in **its own group, *Browser compatibility*, placed last**, because the file's own comment says the common case is to leave it alone.

**Its dependency is on the row, not the group, which is the opposite of the `https_filtering` five and for a reason.** Same shape — a key depending on the `enabled` of the section it lives in, so no `requires()` either way — but the group here holds one row, and a description carrying a caveat for a single switch is a caveat the user reads before knowing whether it applies. The DNS page already puts this kind of thing in the subtitle and moves it with the file: the mode row's subtitle reads `dns_filtering.enabled` as well as its own key. The ECH row does the same, and its paint snapshot keys on both settings for the same reason that one does.

**Four renderings, four walks.** Off-and-live, on-and-live, on-with-DNS-filtering-off, and a `block_ech` no reader can take as a boolean — the last only reachable by hand-editing the file, because `config set` refuses `notabool`, and accepts `1`/`0` as values `Config::bool_at` still reads. The third is the one worth having: `config set dns_filtering.block_ech true` succeeds and prints `Config has been updated` with `dns_filtering.enabled = false`, so the subtitle is the only thing between the user and a switch that is stored and not applied. The fourth greys the row out, which the walk now proves rather than asserts — `building.md` §3.

**`safebrowsing.send_anonymous_statistics` is the first row this project has built that it cannot describe**, and that is the whole finding. Measured 2 August 2026: `proxy.yaml` gives the `safebrowsing:` block one comment — *"Browsing security settings"* — and nothing for this key; `config --help` and `--help-all` never mention it; the binary's string table holds the key's name and no description. Three sources, no answer.

That is a problem because the row is a **consent** control, where a confident-sounding description would be the most damaging possible invention. Three options were open: describe it from general knowledge, which is invention and is what `overnight-v2.md` §4 forbids; leave it unrendered, which means a user cannot confirm their own telemetry state without reading YAML; or render it and say the description is missing. The third is what shipped, in all three reachable states — *"Off. What it would send is not documented in proxy.yaml or the CLI"*, its *On* counterpart, and the inert case. `consent_never_claims_to_know_what_is_sent` asserts every state carries that admission, so a future edit cannot quietly add a payload description nobody measured.

**It is not a seventh [`Toggle`], and the reason generalises.** That enum is the six switches that change what AdGuard does to *traffic*; this changes what AdGuard is *told*. More concretely, `Toggle::description` is documented as taking its wording from `proxy.yaml`'s own comments so that the GUI and a user reading the file are told the same thing — a rule this key cannot satisfy, having no comment to take. A seventh variant would have broken that silently, so the row lives in its own group with its own write path, and two tests hold the line: one that no `Toggle` ever adopts the key, one that no `Toggle` description ever contains *"not documented"*.

**Its group is Protection ▸ *Privacy*, placed last**, below the certificate and browser-integration checks. Those two are problems to fix; this is a preference, and nothing is wrong when it is off — which is how it ships. That group is also the obvious home for `send_crash_reports`, the caveat noted above about the only key rendered exclusively by the first-run assistant: a user who changes their mind about crash telemetry still has no page to change it on, and now there is one to put it on. **That is a proposal, not a decision** — `send_crash_reports` was never among the eleven, and §7 owns whether it becomes a row.

**`auto_enable_language_filters` had both halves of its verdict wrong, and correcting them changes what the row is for.** The original read *"it decides what that page's catalogue turns on, and `locale.rs` already holds the matching logic"*. Measured 2 August 2026:

- **It does not decide what the page turns on — it is a writer the page cannot see.** `proxy.yaml`'s own comment is *"Enables filters based on the query language and system locale"*, and the shipped binary carries the runtime string ``Language filter `{}` has been added automatically``. So this is a daemon-side automatic **add**, and the Filters page is its subject rather than its owner: rows can turn themselves on with no user action. The word *query* is the half our own `adguard-cli.md` gloss had dropped, and it is the half that runs continuously rather than at install.
- **`locale.rs` is not the matching logic and cannot be reused as it stands.** It serves `filter_localisation` — translated filter *names* — as its own module doc says. Language *targeting* lives in `filter_locale`, whose vocabulary is incompatible: **39 rows, 38 distinct tags, every one exactly two characters, none containing an underscore**, so `Locale::primary()` returning `en_US` can never match a row there. Neither `en` nor `en_US` is in the table at all, so anyone testing this on the reference machine would get an empty result and conclude the feature was broken. **Nothing in `crates/` reads `filter_locale` or the `lang:*` tags.**

**It should still become a row, and §5's own `filters` reasoning does not excuse skipping it.** The `filters` list stays unrendered because a row there would be *a second, contradictory way* to manage filter lists — a redundant writer whose every effect is already reachable from a switch, so dropping it costs the user nothing. This key is the opposite shape: leaving it unrendered does not remove the second writer, it removes the user's only brake on one that runs whether the GUI renders it or not. The mechanical reason is absent too — `filters` refuses `config get`; this key answers it.

**What blocks it is one measurement, and the row must not ship without it: does the automatic add respect a filter the user turned off?** The database cannot answer. `pragma table_info(filter)` has `is_user_title` and `is_user_description` — AdGuard records user provenance for a filter's *name* — and **nothing of the kind for `is_enabled` or `is_installed`**, so no consumer can distinguish "off because I chose off" from "off because it was never on". The two possible behaviours need opposite subtitles: if the add path keys on installation then `disable` survives (it leaves `is_installed = 1`) and only a `remove` is undone, which would invert the mental model the removal dialog is built around; if it enables unconditionally, the setting re-flips the user's switch. **Reported from static analysis of the stripped binary and not reproduced here:** a rolling 300-second window needing three or more matching samples, announced by a desktop notification rather than a log line. Treat the constants as a lead for whoever runs the measurement, not as contract.

That measurement needs a sandbox proxy with driven traffic, which is a second daemon on a machine already redirecting system traffic through the first — the same wall §3 item 9 hit, and sharper now that launching a second process is measured to stop the running one. `handoff.md` §3 item 12 carries it. **The `send_anonymous_statistics` precedent does not apply**: that row shipped saying "not documented" because three sources genuinely had no answer, whereas this one is measurable and merely costs something. Shipping a hedge here would borrow that row's honesty without earning it.

**`adguard_headers_enabled` was placed for the right reason by accident, and the row nearly said the opposite of the truth.** §5's verdict was *"Advanced Diagnostics, the natural neighbour of HAR capture"*. The placement is right; the reason is not, and it is worth replacing rather than quietly keeping. Diagnostics **already exists** — it is `ADVANCED`'s last group and it shipped inside `v1.0.0` holding `worker_threads` and `log_level` — so nothing had to be created, and a rationale resting on a feature that is unbuilt and blocked (§3 item 9) would read as blocked itself. The neighbour that actually ships is `log_level`, and the property `proxy.yaml` itself asserts is the one that justifies the group: *"for debugging purposes"*.

**The row's content is directionality, and the intuitive answer is wrong.** A switch that adds `X-Adguard-Filtered` and `X-Adguard-Rule` invites the reading that every site the user visits learns they run AdGuard and which rule fired — a fingerprinting vector a row would have to warn about in the plainest words available. Measured on 1.4.13, it is the other way round. Both names are referenced exactly once each, from one ~650-byte region around `0x6bc3d0`, and every immediate beside them is that header name's own length — `0x12` = 18 for `X-Adguard-Filtered`, `0xe` = 14 for `X-Adguard-Rule`, `0x1b` = 27 for `Access-Control-Allow-Origin` — so these are `(name, len)` pairs into header calls rather than incidental strings. Both `X-Adguard-*` names are **written** through `0x7a36c0` into the collection held in `%r13`, and `Access-Control-Allow-Origin`, which exists only on responses, is operated on through that **same** `%r13`; the CORS *request* header `access-control-request-method` is read from a different object (`%r14`) through a different function. So the collection is the response headers, the sites never receive these, and **a privacy warning here would have been measured false**. `the_header_row_does_not_imply_the_site_sees_them` holds that line.

What the row does disclose is smaller and is what it says: the matched rule and filter-list id arrive in the browser, where same-origin script can read response headers. **Unmeasured and therefore unclaimed:** that the boolean gating that code is this key rather than another (the chain is `proxy.yaml`'s comment naming exactly these two headers for exactly this key, plus there being exactly one emission site — strong, but two facts rather than a trace); whether the headers appear on every response or only filtered ones, since the function is reached through a vtable; and whether they are emitted at all with HTTPS filtering off, which would need traffic through a second proxy. None of those is asserted in the row.

**The last two rows were held back for "a design decision first". Measuring them dissolved one of the decisions and moved the other.** Both verdicts above are struck rather than edited, because in each case the sentence this table asserted was wrong in a way worth keeping visible. Contract §5 has the thirty-four writes; what they mean for the rows is here.

**`filtered_ports` is validated by the CLI, so the syntax was never ours to own — but its refusal text is, because the CLI's is wrong.** This table said the compound range syntax was ours to validate *"since `config set` type-checks strings not at all"*. Twenty-nine sandboxed writes say otherwise: `9000:80` is refused for descending, `65536` and `80:65536` for the ceiling, `80:`, `80:90:100`, `-1`, `http` and the empty string all refused, while `0`, `65535`, `80:80` and `80:90,443` are accepted. That is a real grammar, enforced, and richer than `listen_address`'s.

What it leaves us is narrower and more interesting than validation. **The refusal names *space-separated* as the valid form, and space-separated is exactly what it rejects** — `80 443` and `80:90 443` are both refused by the message recommending them. Every other refusal in contract §5 can be shown to a user verbatim, and `log_level`'s even enumerates its options correctly; this is the first that would actively mislead. So the row's design content is one sentence: **it must state the grammar in the file's words, not the binary's.** The file has them, and has had them all along — *"format: `80:5221,5300:49151` or `80,443,8080`"*.

**Built 2 August 2026, and the wrong wording is what the tests guard.** `AdvancedPage::settle` toasts a CLI refusal verbatim, with a comment saying the CLI's wording beats ours — true everywhere else on the page, so this row is the single documented exception rather than a new rule: `entered` checks `config::is_port_list` first and toasts `PORT_LIST_ADVICE`, which is `proxy.yaml`'s grammar. Three tests hold the line, two on the row's own description and the toast (**neither may contain "space-separated", both must name commas**) and one asserting that **every example the advice offers is a value `is_port_list` accepts** — so wording and validator cannot drift into advising something refused.

**The validator is deliberately no stricter than the CLI**, which is the harder half. Refusing a value `config set` would have written is the same class of error as `choice_at` rendering a CLI-written value as unavailable, and far likelier here, so all thirty-nine measured outcomes are encoded as a transcript in `port_list_tests` — including the junk. Two of them cost a correction during the build and are the reason it is a transcript rather than an opinion: `9:80` is a legal ascending range that a **string** comparison refuses, since `"9"` sorts after `"80"`; and `80: 90` and `80 :90` are accepted, so whitespace around the colon had to be tolerated. Both were caught by measuring shapes the first implementation had merely assumed.

**Its own group, mirroring *Manual proxy ports*, and that settles the dependency question the other way from `block_ech`.** The dependency is on `proxy_mode`, which is a **choice**, so `Setting::requires()` — which models a dependency on a boolean — could not carry it even if it should. The page already had the answer: *Manual proxy ports* is a group whose description names the mode its rows apply in. *Filtered ports* is the same shape for the opposite mode, placed directly above it so the two read as a pair, and a test asserts the group holds exactly one row, since a second would silently inherit a mode caveat nobody wrote for it.

**Three renderings, three walks**, against a sandbox: the stock value, a CLI-written `80, 443,` that renders **verbatim** rather than normalised, and a hand-edited `filtered_ports: 80` — an integer where a string belongs — which greys the row out. That last is the honest "unavailable" state and is reachable only by hand, the CLI having refused every non-string offered to it.

**What the walk could not prove, and it is the refusal path itself.** Driving the entry needs focus, and `Atspi.Component.grab_focus` errors here (`atspi_error (1)`); a programmatic `EditableText.set_text_contents` does not make an `AdwEntryRow` reveal the apply button, so `connect_apply` never fires and nothing commits. So the toast was verified by test and by reading, **not by watching it appear**. That is a harness limit worth knowing rather than a gap in the row — `handoff.md` §4 — and it is the same family as the `AdwSpinRow` being absent from the tree entirely.

Two smaller consequences. The CLI accepts and writes back verbatim several shapes the file's comment does not cover — `80,`, `80,,443`, `80 `, `00080:00090` — so the row renders whatever is there rather than normalising it, for the same reason `choice_at` reads case-insensitively. **Still unmeasured, and not blocking:** whether the running proxy agrees with the CLI about `80,,443`. That needs a second proxy, the same wall as §3 item 9.

**`outbound_interface`'s null was a read problem, not a write problem, and it is the one thing here that cannot be solved by wording.** The open question was how an empty text field writes back `null`. It answers easily — writing the literal word `null` restores the stock line **byte-identically**, where writing `""` leaves an empty scalar that every YAML reader calls null and the CLI itself reads back as an empty string. Two readers disagreeing about one line is a state to avoid rather than choose, so the clear action writes `null`. That is the whole write half.

The read half stops the row. `Config::str_at` returns `Some` only for `Yaml::String`, so a null reads as `None` — and `None` is what the Advanced page renders as **unavailable**, the state reserved for a key we could not read at all. `outbound_interface` is the **only null-valued scalar in the whole 220-line file**, which is why nothing in the reader has ever needed to tell "legitimately empty" from "unreadable". Adding it as a `Kind::Text` was tried rather than reasoned about, and `every_advanced_setting_resolves_with_the_right_type` fails on a stock install:

```
PROBE (outbound_interface) did not resolve as Text { secret: false }
  in /home/potworny/.local/share/adguard-cli/proxy.yaml
```

So this row costs a change to `Kind::Text` — a shared type behind nine existing rows — to carry a notion of *absent* distinct from *unreadable*, plus the read helper to match. **That is the design, and it is a bigger one than the table implied**; the failing assertion is exactly the guard that should stop a row shipping with a hole in it, and it did.

#### Should stay unrendered (7)

| Key | Type | Why not |
| --- | --- | --- |
| `show_hints` | bool | Configures the **CLI's own terminal output** — the hint text contract §5 notes landing between the echo and the confirmation. A GUI switch for it changes nothing the GUI user can see |
| `access_log_file` | str | Renaming the log file moves an artifact `export-logs` bundles **by name**. The useful feature is reading or exporting the log, not renaming it |
| `filters` | list | The plumbing behind the Filters page, which manages the catalogue through `adguard-cli filters`. A row here would be a second, contradictory way to manage filter lists. Refuses `config get` anyway |
| `userscripts` | list | Out by §7's own decision, re-checked 2 August 2026 and unchanged. Refuses `config get` |
| `apps` | list | Per-app filter actions: three different entry shapes and an **ordering rule** (*"Wildcard should be last"*) that no generic row can express. This is a feature with a design, not a parity gap. Refuses `config get` |
| `https_filtering.exclusions` | str | Names the file `https_exclusions.txt`, 72,563 B of it. The feature a user wants is editing the **list**; renaming the file is not that, and the list is a `--list-file` job of its own |
| `https_filtering.certificates_cache` | str | A cache directory the trust check of §6 already reasons about by its real path. Letting a user point it elsewhere invites the same failure `root_certificate_name` was found to cause: a check aimed at a path nothing will create |

#### Cannot be classified without a measurement (2)

| Key | Type | Stock | What is missing |
| --- | --- | --- | --- |
| `update_channel` | str | `'release'` | `adguard-cli` really does have `update` and `check-update` subcommands (measured from `--help` on 1.4.13), so the key has a consumer. What is **not** measured is whether either command is safe to expose from a GUI on a machine where `adguard-cli` was installed by a package manager, or what the three channels do to a working install |
| `show_notifications` | bool | `false` | The file's own comment says *"show protection status notification"* and says nothing about who shows it. `adguard-cli.md` glosses it as "desktop notifications" — **that gloss is this project's writing and is unmeasured**. If it is desktop notifications, it collides head-on with this app's tray, and the row is a design question rather than a switch; if it is terminal output it belongs with `show_hints` above. One measurement decides which table it goes in |

#### Belongs to a different item (2)

`har_writer.enabled` and `har_writer.location` are §7's own HAR-capture item and are not counted as a parity gap. Contract §9 now carries what is measured about the second of them, which is that its `'.'` default still cannot be predicted.

#### And one that is rendered, but only once

`send_crash_reports` — see the caveat above. Whether it gets a permanent home is a smaller decision than the eleven, and it is the only one on this page that costs nothing to reason about: the assistant's own table already argues the key is worth asking about, and that argument does not expire when the assistant closes.

### Import and export, and the first-run collision

§7 puts import/export in v2 and requires **the first-run collision to be designed before either half is built**. This is that design. It is written against contract §13, which was measured first, and it decides layout and behaviour only — **whether to build it is §7's call and the owner's**, and the three forks that are genuinely scope rather than design are marked as such at the end.

#### Where the three commands live

**Not a new page.** Three buttons is the case §7 already reasoned about for userscripts — *a sidebar page for that is navigation without content*. They go on Advanced, in two groups, and the split is by purpose rather than by which binary subcommand they call:

- **Backup and restore** — `export-settings` and `import-settings`.
- **Diagnostics** — `export-logs`, beside `log_level`, which is the setting that decides what ends up in it. That group already exists and the parity enumeration proposes `adguard_headers_enabled` and the HAR pair for it too, which makes it the coherent home for *give me something to send to support*.

One caveat for whoever builds it: if parity's eleven rows and the HAR group all land, Advanced becomes long enough that a separate page is arguable again. That is a reversible decision and should not be pre-empted now.

#### The collision, and why the assistant has to own it

The assistant's entire trigger is `proxy.yaml`'s absence. **`import-settings` creates that file** (contract §13), so an import is a second path through first run whether or not the assistant acknowledges one. Two further measurements decide the shape:

- **`import-settings` is not licence-gated; `configure` is.** So a restore is reachable by exactly the user the assistant currently turns away — someone rebuilding a machine who has their backup but has not yet activated. Ignoring the import would leave the app refusing to help a user the CLI would have helped.
- **The install an import leaves is unlicensed and has no certificate**, while the `proxy.yaml` it writes says `https_filtering.enabled: true`.

So: **the welcome screen offers a second, secondary action — *Restore from a backup* — beside *Set up AdGuard***, and it is offered *before* the licence check gates the primary one, because it does not need a licence.

**A restore does not then hand the window silently to the pages, the way a completed `configure` does.** It ends on a screen naming the two things the backup could not carry, because both are states this application already renders and neither is the user's mistake:

- **No licence** — the activation affordance the Status page already owns.
- **No certificate, with HTTPS filtering reading on** — the certificate-trust group the assistant already carries under its own HTTPS question, and Protection carries permanently.

That is §6's detect-and-instruct pattern pointed at a state *this app just created* rather than one it found. It is also the same rule as everywhere else here: **a switch that reads on and cannot work is marked**, and an import produces that state by the supported route.

#### The wrong-zip guard, which is not optional

Contract §13 measured that the two exports **share one filename** (`adguard-cli_<date>_<time>.zip`) and that `import-settings` accepts a *logs* zip at exit 0, with wording identical to the correct case, leaving a partial install. There is no exit code, no message and no filename that separates the two.

**So a file picker is never handed straight to `import-settings`.** The GUI reads the zip's central directory first and classifies it:

| Manifest contains | Verdict |
| --- | --- |
| `filters.yaml` **and** `agflm_standard.db` | a settings backup — proceed |
| `app.log` | a **logs** export — refuse, and say which button made it |
| neither | not an AdGuard export — refuse |

Reading a listing is not speaking a protocol, which is the same line §1 draws for browser integration and §6 draws for `filters list`: **read the manifest, never the wire format.** The check costs four filenames and removes the only failure mode in this feature that is silent.

#### The confirmation, and what it may not say

Import onto a *configured* install replaces everything, so it takes the discipline custom-filter removal got (§5 above): an `AdwAlertDialog` naming what is about to be replaced — settings, filter selections, custom filters, userscripts and HTTPS exclusions.

Two things make it a better dialog than the removal one, and both come from measurement:

- **There is a real undo here, and the dialog offers it.** Custom-filter removal had none, which is what made its confirmation the only honest place for the decision. Here, *Export current settings first* is a complete escape, and the dialog offers it as an action rather than as advice.
- **It must not warn about the licence or the certificate.** Contract §13 measured that an import leaves `adguard.conf` and the CA untouched and the licence still active. A dialog that said otherwise would be frightening the user with something false — and this project's rule is that a warning is a measurement, not a mood.

It **does** have to say what a backup silently does not restore: **the DNS filter selections and the DNS user rules**, neither of which is in the bundle, while every `dns_filtering.*` setting in `proxy.yaml` is. A user restoring a machine gets their DNS servers back and not their DNS filters, and nothing else in the flow would tell them.

#### What the two export buttons say

The wording rule is §6's — say what the command will do, in the CLI's own terms where it has them:

- **Export settings** carries no licence and no certificate (`adguard.conf` is not in the bundle), so it is safe to keep or hand to someone else. It is also large — 51 of its 52 MB are the filter catalogue, which is redownloadable — so the button should not imply the wait is about the user's settings.
- **Export logs** contains the **configuration** as well as the logs, and does **not** contain the browsing access log. That is the inverse of what this project assumed before measuring it (`overnight-v2.md` §2.3, struck), and it is the sentence the button exists to get right.

#### Three forks that are the owner's, not this document's

1. **Whether restore-at-first-run ships at all.** The alternative is import only after setup, which is simpler and never produces the configured-but-unlicensed state — at the cost of turning away the user with a backup and no licence, which is the case the feature is most for.
2. **How a zip gets read.** The `zip` crate (a dependency), shelling out to `unzip` (a runtime dependency the project does not currently have), or hand-rolling the central-directory read (~100 lines, none). This project's habit is to read files itself rather than take a dependency or parse another program's output, which points at the third — but a dependency decision is not a design decision.
3. **Whether any of it is v2 at all.** §7 says it is; this section changes nothing about that and adds no scope.

### GNOME dock icon grouping

Set the GTK application ID, the `.desktop` filename, and `StartupWMClass` to the **same** reverse-DNS string. On GNOME a mismatch makes the running window fail to group with its pinned launcher — a duplicate icon appears below the favourites separator. This is a known recurring annoyance on this machine, so get it right from the first commit.

---

## 6. Privileged operations

Two things this application touches need root, and it takes neither: AdGuard's root helper, and installing AdGuard's certificate into the system trust store. **This application performs no privileged operation and ships no privileged component** — no helper binary, no polkit action, no `pkexec` call, no setuid bit.

That is possible because AdGuard already ships the escalation path for both — a fact an earlier revision of contract §8 missed for the first of them, and which turned out to hold for the second as well.

The rest of this section is the helper; the certificate has a subsection of its own further down, because the two are the same design applied twice — and browser integration has one after it, which is that design a third time with the privilege taken out of it. Root is what the section is named for, but detect-and-instruct is what it is actually about, and the two subsections that need no root at all (clearing a wedged proxy, and browser integration) are here to mark where the boundary really runs. `adguard-cli` gates auto mode on `adguard_root_helper` being `owned_by_root`, `has_suid` and `is_executable`, and when the check fails it names the fix itself: `sudo <path>/adguard_root_helper -s`. Once that has run, switching mode is an ordinary unprivileged `config set proxy_mode auto`.

**This section used to open by calling auto mode "the one thing in this application that needs root", and that was wrong.** It was taken from AdGuard's own strings, every one of which names automatic mode — but with the helper in its shipped state, `manual` mode's HTTP proxy answers **every** request with 502 and never opens an upstream connection, while the SOCKS5 listener beside it works normally (contract §8). The helper ships unmet, so **every install starts with an HTTP proxy that cannot serve a request**, which is exactly what this section already says about the certificate and makes the helper the second least hypothetical case here rather than a corner of the Advanced page. Nothing in the CLI connects the two for the user: `status` reports the port listening, because it is, and the browser reports a connection reset by the far end.

Design:

1. **Detect.** `stat` the helper for the same three properties `adguard-cli` checks. Report the check, not a guess — three separate facts, so a helper that is root-owned but not suid says so. `helper::RootHelper` takes the path as a **parameter**, not a constant, and which branch that buys has since flipped: the helper ships unmet, so the *met* branch was once the unreachable one — and this machine has now run the `sudo`, so the unmet rendering is what nothing local reaches. `$ADGUARD_ROOT_HELPER` overrides the path either way, and the two ends are proven against binaries the system already ships (`/usr/bin/passwd` setuid-root, `/bin/ls` root-owned without the bit).
2. **Instruct.** When unmet, AdGuard's own command with a copy button, an explanation of what the suid bit grants, and no way to run it from the app. Three screens carry it, and the reason each does is the reason it is not one screen: the **Advanced page**, under the mode row it also gates; the **first-run assistant**, because every install this app completes ends in the unmet state; and the **Status page**, which is where the symptom is — one line under the HTTP endpoint saying requests through it fail, since that group's whole purpose is to advertise an address the user is about to point a browser at. Only the first two carry the command, and `gui/root_helper.rs` is the single widget behind both. Re-check when the window regains focus, so a user who runs it in a terminal sees the row change without hunting for a refresh; Status re-reads on its existing 2 s poll instead, which costs one `stat` and clears the caveat without needing a focus event at all. The check is re-read every time rather than cached — a cache would be wrong at precisely the moment the re-check exists for.
3. **Switch.** When met, `config set proxy_mode auto` — a plain write through the path every other setting uses.

**The gate is load-bearing, and that is a measurement rather than an assumption.** `config set proxy_mode auto` **succeeds with all three properties unmet**: exit 0, `Config has been updated`, and `proxy.yaml` really holds `auto` afterwards (contract §8). AdGuard does not consult its helper at config-write time. So nothing but this application stands between the user and a mode that quietly does nothing, which is the `dns_filtering` mistake §5 already refuses to repeat.

It also means the unmet state has to be **rendered**, not merely prevented. A terminal or a text editor reaches `proxy_mode: 'auto'` with an unmet helper in one step, and the mode row marks that rather than correcting it — the same judgement Protection makes about DNS filtering that is switched on but inert. Writing `manual` back over the user's setting would be a change nobody asked for.

**Where AdGuard's check does fire is not measured**, and the honest place to say so is here. Reaching it needs `start`: a sandbox is unlicensed so `start` never gets that far, and starting the real proxy in `auto` is a system-wide change that is the owner's call. The design does not depend on the answer — the GUI checks before writing either way — but nobody should read the code as though the moment of enforcement were known.

**Why the app does not run that `sudo` for the user, even via `pkexec`.** The helper lives in a user-writable directory, so setting suid-root on it makes anyone who can write that file root. AdGuard chose that design and the user accepted it by installing AdGuard; conferring it from behind a GUI button is a different act from typing `sudo` at a prompt, and the deliberateness is the only safeguard the arrangement has.

The corollary is worth stating for anyone tempted to add one later: a root-invoked helper of ours would have to be explicit about which user's config it edits — `adguard-cli` and its data live under `~/.local`, so it would need the target `$HOME`/UID passed explicitly and would have to refuse any path outside that user's data dir. Getting that wrong is a local privilege-escalation bug. Not writing the helper is how this project avoids owning that problem.

`data/io.github.dominik-najberg.AdGuardUI.policy` was deleted with the auto-mode work. It declared three polkit actions against `/usr/libexec/adguard-ui-helper`, a binary that was never written and now never will be, and its own header still asserted that AdGuard "ships no polkit policy … so there is nothing to reuse" — the conclusion contract §8 retracted. Nothing installed it; `building.md` §4 says so and now says how to remove it if an older checkout did.

### Clearing a wedged proxy process is not one of these

One recovery the app *does* perform itself, and the boundary is worth stating precisely so this section is not read as forbidding it.

An install can be left holding a proxy process the CLI has lost track of: alive, still bound to the proxy ports, while `status` reports the proxy stopped. In that state `stop` is a no-op and `start` fails after 60 s, so the CLI has no route out and the user's Start button does nothing (contract §11). A `SIGTERM` to that one pid cures it.

**That is not a privileged operation.** The process belongs to the user running this application; `kill(2)` against one's own process needs no root, no helper, no polkit action, and confers nothing on anybody. The rule this section states is that the app never *escalates* — and there is nothing here to escalate. Detecting and instructing would mean showing a `kill` command for a pid the user cannot see, to fix a state they did not cause, in place of an action the app can take correctly.

What it borrows from this section instead is the deliberateness. It runs on exactly two occasions — at start-up, and after a start that did not take — never on the poll; it signals only processes that existed *before* the attempt, so it cannot kill a daemon a start has just forked; it re-checks the process identity against `/proc` immediately before signalling, so a recycled pid is never touched; and it names the pids it ended in the toast afterwards. `killall adguard-cli`, the recovery users arrive at unaided, has none of those properties and takes a healthy proxy with it.

See `orphan.rs`, which is the only place in the codebase that sends a signal.

### The certificate is the same shape of problem

HTTPS filtering signs every connection it inspects with a CA generated on this machine, and until that CA is in the system trust store the filtering it enables breaks the first HTTPS site the user opens. `configure` generates it and then skips its own install prompt in silence, because that step needs a password and there is no TTY (contract §7) — so **every install this application sets up ends in the unmet state**, which makes it the least hypothetical case in this section.

It resolves exactly like auto mode, and for the same reason: AdGuard ships the installer. `install_cert.sh`, beside the resolved binary, elevates itself with `sudo`, copies the certificate into the system's anchor directory, rebuilds the trust store, and adds the certificate to Firefox and Chrome with `certutil` — the system's if one is installed, otherwise the copy shipped beside it, which is the branch this machine takes because `libnss3-tools` is not installed here. So:

1. **Detect.** `trust::CaTrust` reads three files and reports three facts — the certificate exists, a byte-identical copy is anchored, and the bundle carries it — in the order the machine applies them, so a user who ran the installer and still has broken HTTPS learns which step did not take. Every path is a parameter, with `$SYSTEM_CERT_DIR` (AdGuard's own variable) and `$ADGUARD_CA_BUNDLE` (ours) overriding the search. That is not symmetry for its own sake: on the reference machine the certificate **is** trusted, so here it is the *unmet* branches that would otherwise be unreachable. That was written as the mirror image of the root helper, whose met branch was then the unreachable one; the helper has since been set up on this machine and the two now point the same way. Which is the argument for the parameter, not against it — the reachable branch is a property of the machine on the day, and neither check should be written as though its own machine were the specification.
2. **Instruct.** The Protection page carries the rows, directly below the switches, and the first-run assistant carries them too — immediately after its own HTTPS question, because that screen is where the state is created. AdGuard's own command, a copy button, no way to run it from the app, and a re-check when the window regains focus. `AdwPreferencesPage` has no insert-at-index, so a group's position is its `add` order and "under the switch" means under the group that holds it.
3. **There is no third step.** Unlike auto mode there is nothing for the app to write afterwards: the trust store is the whole of it, and once the user has run the command the rows disappear.

Four unmet states rather than one, because the fixes differ: no certificate at all (`adguard-cli cert`), not installed (the installer), installed but the trust store not rebuilt (`sudo update-ca-certificates`), and a **different** certificate already occupying the name. That last one is the reason the check compares bytes instead of asking whether a path exists: AdGuard's installer tests for the path and stops, reporting success, so a regenerated CA leaves a state its own tooling will not repair and a name-only check would call trusted (contract §8).

**What the check cannot see, the wording admits.** Firefox and Chrome keep their own NSS databases and read nothing from the system store; the installer covers them and this check does not. So the rows say the machine trusts the certificate, never that every browser on it does.

**A command this app will not run is still a command this app vouches for**, and that turns out to be the sharper end of showing rather than doing. The certificate's path is not a constant — it is named by `https_filtering.root_certificate_name`, an ordinary setting `config set` will write any string to — so a name carrying a `"`, a backtick, a `$` or a newline would close AdGuard's own quoting and leave the rest of it running as a second command, in a line the user has been told is AdGuard's and may well paste behind a `sudo`. `trust::quotable` refuses those paths and the row shows the state with no command at all, saying which of the two reasons applies. Re-quoting them with `'…'` was the alternative and is worse: the command would no longer be the one upstream documents, which is the entire basis for showing it.

### Browser integration needs no root, and is still not ours to run

The third application of this section's pattern, and the first where the reason for not running the command has nothing to do with privilege. It is filed here anyway, because the shape is the one this section describes — a step AdGuard ships a command for, which no install performs, and whose unmet state has to be rendered rather than prevented — and because keeping it beside the other two is what stops the section being read as a rule about `sudo` rather than about deliberateness.

**The symptom points at the wrong thing, which is the whole reason for the check.** AdGuard's browser extension does not look for `adguard-cli` on `$PATH`; it asks the browser for a native-messaging host, and the browser resolves that name out of a manifest on disk. Without one, the extension reports that it cannot detect `adguard-cli` in the system — sending the user to inspect their AdGuard install, their `$PATH` and their proxy, when all three may be fine and the missing thing is a 500-byte JSON file the extension never names. `install-browser-integration` writes those manifests and is not part of unpacking the CLI, so **every stock install is in this state** (contract §12).

1. **Detect.** `browser::BrowserIntegration` reads the manifest for each of the six browsers AdGuard knows about and reports four states, not a bool: ready, missing, *stale* — a manifest naming a host that is not the one beside this machine's `adguard-cli` — and unreadable. The stale state is why the check compares the manifest's `path` against the resolved binary instead of testing that some file exists, and it is the same judgement the certificate's byte-comparison makes for the same reason: an AdGuard reinstalled under another prefix leaves a manifest its own tooling will not correct, naming a host that still exists. `$ADGUARD_BROWSER_HOME` overrides `$HOME` — one variable rather than six per-path ones, so the manifests cannot be made to disagree — and it is what keeps the unmet rendering reachable on a machine that has run the command, exactly as `$ADGUARD_ROOT_HELPER` and `$ADGUARD_CA_BUNDLE` do above.
2. **Instruct.** The Protection page carries the rows, below the certificate group: the subject is a filtering surface rather than the daemon, which is the same reason the certificate rows are there and not on Status. AdGuard's own command, a copy button, no way to run it from the app. Browsers that are **not** installed are not reported — there is nothing to integrate with, and six rows of absent browsers on every machine would bury the one that matters.
3. **There is no third step.** As with the certificate, the manifests are the whole of it; once the user has run the command the rows disappear.

**A browser installed later invalidates the answer, and nothing else in this app has that property.** The installer writes only where it already sees a browser, and says `installed successfully` whether it wrote six manifests or none (contract §12). So a browser installed after the command was last run is silently unintegrated, with no diagnostic anywhere in AdGuard's tooling and an extension that blames the CLI. The focus re-check the other two prerequisites use for a *user's* action in a terminal therefore covers a second case here — a change with nothing to do with AdGuard at all — and it is the only route this application can offer.

**Why it is not run from the app, given no password is involved.** The command writes into five other applications' configuration directories. Which browsers on this machine should be handed a native-messaging host — one that lets a page-level extension talk to a local binary — is the user's call, and it is not made better by a GUI making it silently on their behalf. The deliberateness §6 asks of the `sudo` commands is the same thing being asked for here; only the hazard differs.

The command is guarded the same way the certificate's is: `trust::quotable` refuses a CLI path that cannot be written into a shell line safely, and the row then shows the state with no command. One further branch has no counterpart above — if `adguard_cli_nm` is missing from beside the binary, the command is withheld even though it would succeed. Running it then would write six manifests pointing at a program that is not there, replacing a browser that cannot find AdGuard with one that launches nothing: a worse state, harder to diagnose, and one that looks like the fix worked.

### The re-check the three of them share

All three read themselves again from **one** `connect_is_active_notify` closure in `main.rs`, on `is-active` rather than on a widget focus event: the check is about the window as a whole, and the row the user needs to see is rarely the one holding the keyboard focus. The closure guards on regaining focus, so losing it costs nothing, and the whole re-read is one `stat`, three small file reads and at most six more — cheap enough for the main loop, which is why none of it is cached. A cache would be wrong at exactly the moment the re-check exists for.

**That line is verified, as of 1 August 2026,** and for a while it was the one thing here nothing had ever exercised — the notes excusing it said focus needs `xdotool`, which is not installed. It does not: there is no window manager on an Xvfb display for `xdotool` to talk to, and `XSetInputFocus` is a single X call (`building.md` §3). Driven through the browser check, whose input is entirely files under `$ADGUARD_BROWSER_HOME`, the group leaves the page when a manifest appears and comes back when a browser does — with an intervening phase, after the write and before the focus round trip, whose page walk must be byte-identical to the one before it. Without that phase a passing run would be equally consistent with one of this application's three polls having noticed.

---

## 7. Scope

This section is the scope authority for both milestones, and every other document defers to it. It was titled *v1 scope* from a time when there was nothing else; the v1 half below is closed, and the v2 half was decided by the project owner on **2 August 2026** — the day the repository went public, and the day after 1.0.0 was tagged.

### v1 — closed

**In:** status + lifecycle control, protection toggles, filter enable/disable with the SQLite-backed catalogue, custom filter install by URL, tray icon with quick toggles, first-run assistant, licence activation, the DNS page including its listen port, and the auto-mode switch — the last as detection and instruction, never as an escalation of our own, alongside the root-helper check it shares that treatment with (§6).

**Out:** live blocked-request stats (needs log tailing; format undocumented and unstable — contract §9), **userscripts entirely**, HAR capture, `speed` benchmark UI, import/export, full advanced-settings parity. Those six were carried as *Out (v2)* for as long as v2 was a label rather than a milestone; where each of them actually landed is below.

Userscripts are out because there is only one. `userscripts list` returns a single entry, `adguard-extra`, and `proxy.yaml` says in AdGuard's own words that only AdGuard Extra is supported; with installation deferred, the feature is one switch for one script that ships pre-enabled. A sidebar page for that is navigation without content. This section is the scope authority — §5 and `handoff.md` no longer list a Userscripts view, and if the upstream ever supports more, this is the decision to revisit.

Ship the tray + core controls first; it is the part that replaces day-to-day terminal use.

**Added after v1 closed:** the certificate-trust check (§6). It is not a scope change so much as the other half of a v1 feature — HTTPS filtering was in from the start, and shipping it without saying whether its certificate is trusted left every install in a state the app could see and would not mention.

**Also added after v1 closed:** the browser-integration check (§6). The same argument, arrived at from the opposite direction — not a state this app creates, but one it is uniquely placed to explain, because the extension's own report of it names `adguard-cli` and sends the user looking at everything except the missing file. Nothing else in the toolchain says it: the installer reports success without writing anything, and a browser installed afterwards is silently left out (contract §12).

Status: Status, Protection, Filters (HTTP), DNS, Advanced and Stealth are done; both filter pages install custom lists by URL (§5); licence activation lives on the Status page; the first-run assistant seeds an unconfigured install and hands the window to the pages when it is finished (§5); the tray carries start/stop plus the six Protection toggles as quick toggles (§4); the config monitor reports an external edit with a toast, gated on a row the user can see having moved (§3); the Advanced page carries the proxy mode with AdGuard's root-helper check beside it (§6); and a custom list can be removed, behind a confirmation, on both filter pages. **v1 is complete.**

### v2 — open

**In:** HAR capture, full advanced-settings parity, import/export.

**Out:** live blocked-request stats — **its own milestone, behind a spike**; userscripts, re-checked 2 August 2026 and unchanged; the `speed` benchmark UI, unmeasured.

Decided by the project owner on 2 August 2026. Three of v1's six *Out* items move in, three do not, and each of the three that stay out has a reason below rather than an inheritance from the line above. Nothing is queued: this is scope, and [`v2-plan.md`](v2-plan.md) §1 still governs how a session starts.

The three chosen share a property worth naming, and it is about *knowing* rather than about writing. **None introduces a new way of knowing things** — every one of them is verified by re-reading a file, the way everything else here is verified. Live stats is the one that is not, which is the argument for separating it rather than a complaint about it.

**Two of the three are also ordinary `config set` writes; import/export is not, and that distinction matters more than the shared one.** HAR capture and advanced parity go through `config set` like every other switch in the application. `import-settings`, `export-settings` and `export-logs` are their own top-level subcommands over zip artifacts — measured against 1.4.13 — so `import-settings` is a write outside the rule contract §5 states the project's writes in terms of (`config set|reset|list-add|list-remove`). It is the **second** such path, not the first: `configure` is already one, which is why the first-run assistant exists (§5). What makes `import-settings` new is that it is the first to operate on an install that is *already* configured. Its verification is still an ordinary file read — re-read `proxy.yaml` after an import, look for the artifact after an export — which is why it is in scope at all.

**HAR capture overturns a reason that is still in the contract, and this is where that is done** rather than in the code that would depend on it. Contract §9 calls full HAR dumps *too heavy for an always-on UI*, which is a correct statement about an always-on capture and not about a switch: `har_writer.enabled` is `false` in a stock `proxy.yaml`, and the feature is off until someone turns it on for a debugging session. What the objection actually requires is that the row say what leaving it on costs — the same voice §6's certificate and helper rows use for what a command will do. Mechanically it is an Advanced-page group and nothing more: two keys, `enabled` (bool) and `location` (string), through the same `config set` path as every other switch. Measured 2 August 2026 against this machine's `proxy.yaml` lines 202–204, where `config show` folds the section to `har_writer: <folded> disabled`.

One measured detail no plan carried: **the stock `location` is `'.'`** — measured 2 August 2026 at `proxy.yaml` line 204. That is the whole of what is measured. **Where the proxy resolves that relative path is not**, and this section will not guess it: neither the contract nor `adguard-cli.md` records a working directory for the proxy process, and no HAR dump has ever been produced here to look for. **That resolution is the first measurement of this item**, into contract §9, before the row is designed — a relative default is very likely to need resolving to an absolute path on the row, since a user who cannot find the dumps has not got the feature, but that is the expected outcome and not yet an established one. Stated separately because an earlier revision of this paragraph asserted the resolution as measured and hung a ship gate on it, which is the measure-first rule this project states in §3 and `v2-plan.md` §4 being broken in the document that states it.

**Full advanced-settings parity is a specification before it is rows, and the specification is the first task.** Nobody has written down what is missing; the enumeration — walk `proxy.yaml`'s keys against what the Advanced and Stealth pages render — *is* the work before any row is added, and it is not code. Expect the gap to be smaller than "parity" sounds and expect part of it to be keys that should stay unrendered. Contract §5 records that nothing enforces dependencies between settings, so a key whose effect depends on another says so on its row; the Advanced page already does this for several, and that pattern is the one to extend rather than a new mechanism.

**Import/export is in, and the collision is the design.** Measured 2 August 2026 on 1.4.13: `import-settings` takes `-i,--input` and it is **REQUIRED**; `export-settings` and `export-logs` take `-o,--output`, optional, "Can be a directory"; all three artifacts are **zip**. Two things make it bigger than that command list. `import-settings` overwrites the whole configuration, with no undo but a prior export — so it takes the confirmation discipline custom-filter removal got (§5), an `AdwAlertDialog` naming what is about to be replaced. And it is **the only thing besides `configure` that can create `proxy.yaml`** (contract §5), which puts it in direct collision with the first-run assistant, whose entire trigger is that file's absence: an unconfigured install offered an import is a second path through first run. **That interaction is designed before either half is built**, not discovered by whoever reaches it second.

`export-logs` bundles `app.log`, `proxy.log` and `access.log`. Those are a record of what the user browsed — contract §9 shows an `access.log` line — so the button says what is in the bundle, in the same voice as the rest of §6.

**Live blocked-request stats is out of v2 and is its own milestone, behind a spike.** The objection is its *kind*, not its cost. Contract §9 records that there is no push or event mechanism, so a live view must tail a format that is undocumented and unstable across versions, whose detail varies with `log_level`, and **which AdGuard rotates under the reader** — measured 2 August 2026, `proxy.log` and `access.log` roll at ~10 MiB with `.1`/`.2` generations kept, by the writing process itself rather than by `logrotate` or cron. A tailer holding an fd loses the stream silently at every roll. (An earlier revision of this line said *nothing rotates*, generalised from contract §9's careful "no rotation policy is configured **by us**" — which is true of this project and says nothing about AdGuard. One `ls` disproved it, and it had inverted the hazard: rotation is the thing a tailer must survive, not something absent.) Every other reading in this application is a fact checked against a file or a database, and *verify, don't trust* (§3) is the rule the whole design rests on; a tailer over that format would be the first feature here whose correctness cannot be checked against anything. **The spike decides whether there is a feature at all**: how the format moves across a version bump, what `log_level` elides, and what a reader sees when the file is rotated or truncated underneath it. Folding it into a mixed v2 would make the milestone hostage to the one item that might not be buildable.

**Userscripts stays out, and now carries the date of its re-check rather than the old reasoning silently.** Re-checked 2 August 2026 against `adguard-cli` 1.4.13: `userscripts list` exits 0 and returns a single entry — id `adguard-extra`, title *AdGuard Extra*, marked `[x]`, already enabled. Unchanged, so the paragraph above stands: one switch for one script that ships pre-enabled is navigation without content. It is one command, so re-check it again when `adguard-cli` moves; if the upstream ever supports more, this is still the decision to revisit.

**The `speed` benchmark UI stays out because it is unmeasured**, which is a statement about the order of work and not about the feature. `--json` would make it the least risky parse in the backlog — this project does not parse human output anywhere it can avoid it, and contract §6 is the standing example — but how long it runs, what it does with no proxy running, and whether it is interruptible are all unknown. **A benchmark that cannot be cancelled is a modal that cannot be closed.** Measure those three into the contract and it becomes a candidate; it does not become one before.

**The activation success leg is not v2's** (`handoff.md` §3 item 6). It needs a real account and completing an activation spends a device slot, and the owner left it open on 2 August 2026, in the same decision that set this scope. Opening v2 did not open it.

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
