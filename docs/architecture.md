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
| Privileged ops | polkit action + `pkexec` | Needed for `auto` mode; see §6. |

**The no-daemon point is worth stating explicitly**, because it differs from `LenovoLegionToolLinux`. That project needs `legiond` because hardware registers require sustained root. Here, root is needed only for occasional discrete actions (switching to auto mode, system DNS, system cert install) — so escalate per-action via `pkexec` and keep the GUI a plain user-session app. A persistent root daemon would be a larger attack surface for no benefit.

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
│   │   ├── model.rs            # ProxyStatus, Filter, FilterGroup, Userscript, License, Toggles
│   │   └── paths.rs            # locate binary + data dir, XDG-aware
│   ├── adguard-gui/            # GTK4 + libadwaita application
│   └── adguard-tray/           # ksni StatusNotifierItem — a library, not a binary
├── data/
│   ├── com.github.<you>.AdGuardUI.desktop
│   ├── com.github.<you>.AdGuardUI.metainfo.xml
│   ├── com.github.<you>.AdGuardUI.policy      # polkit action
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

   PRIVILEGED ──────► pkexec adguard-ui-helper <action>     auto mode, system DNS, system cert
```

**Reads never go through the CLI where a file will do.** Two reasons, both from the contract doc: the `filters list` table is unparseable for long titles (column overflow), and `config show` masks secrets and folds sections. Files give exact values; the CLI gives a presentation layer.

**Writes never touch files.** `proxy.yaml` is half explanatory comments, and `serde_yaml` cannot round-trip them, so serialising over it would strip the documentation the user relies on. `config set` preserves the file.

### State refresh

There is no push/event mechanism anywhere in the CLI, so:

- **Runtime status** — poll `adguard-cli status` on a ~2 s timer while a window is open; slow to ~10 s when only the tray is visible. At 10 ms per call this is negligible. Implemented in `status.rs` as one tick in five while the window is hidden, which is only possible because the tray shares this process (§4).
- **Config** — watch `proxy.yaml` with `gio::FileMonitor`. External edits (the user is expected to hand-edit; the CLI even suggests it) then appear live in the UI.
- **Filters** — watch the `.db` files with the same mechanism, debounced; the daemon rewrites them on update.

**A file monitor on `proxy.yaml` cannot trust its events.** Measured (contract §5): *every* `adguard-cli` invocation rewrites the file and touches its mtime, even `--version`, and even when no byte changes. Combined with the 2 s `status` poll above, a naive monitor would fire continuously against changes we caused ourselves — and each reload would repaint the page under the user's pointer.

So the monitor must compare content, not react to notification: keep a hash of the last-read file and ignore events where it has not moved. Debouncing alone does not help, because the churn never stops. The same measurement has a small silver lining — a key deleted from the file is silently restored with its default by the next invocation, so a missing setting is self-healing.

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

Fast reads (`status`, `config get`) can be `tokio::process::Command` awaits. Network commands (`check-update`, `filters update`, `update`) need a visible progress state and a generous timeout — a real `HttpClientNetworkError` reaching `filters.adtidy.org` is already in this machine's logs, so failure is a normal path, not an edge case.

---

## 5. UI structure (libadwaita)

An `AdwApplicationWindow` with `AdwNavigationSplitView`, plus `AdwToastOverlay` for command results:

| View | Contents | Backing |
| --- | --- | --- |
| **Status** | Running/stopped, start/stop/restart, HTTP + SOCKS5 endpoints, licence state | `status`, `license` |
| **Protection** | `AdwSwitchRow`s: ad blocking, HTTPS filtering, stealth mode, DNS filtering, Safe Browsing, CRLite | `proxy.yaml` → `config set` |
| **Filters** | `AdwPreferencesGroup` per `filter_group`, switch per filter, custom-filter add | `agflm_standard.db` → `filters …` |
| **DNS** | DNS filter list, upstream/fallback/bootstrap entries | `agflm_dns.db`, `dns_filtering.*` |
| **Userscripts** | Installed list, enable/disable/remove | `userscripts list` (parseable — small, stable) |
| **Advanced** | Ports, listen address, auth, outbound proxy, worker threads, log level | `proxy.yaml` → `config set` |

Notes that shape the widgets:

- Use the **localised** filter names from `filter_localisation` (3828 rows, keyed by `lang`) rather than the English `filter.title`, matching the system locale. The tags are POSIX-style (`pt_BR`, not `pt-BR`) — see contract §6.
- **Filter text is data, not markup.** `AdwPreferencesRow:use-markup` and `AdwToast:use-markup` both default to *true*, and filter 216 is literally titled "Official Polish filters for AdBlock, uBlock Origin & AdGuard". Left on, Pango fails to parse the `&`, GTK warns, and the label renders mangled. Every row and toast carrying AdGuard's text — or the CLI's — must turn markup off, and must do so **before** the title is assigned: the label is rendered as the property is set, so passing a title to the builder warns regardless of what happens afterwards. (`AdwPreferencesGroup` has no such property; its heading is a plain `GtkLabel`, where markup is off by default.)
- Reconcile a switch **per row**, not by rebuilding the page: a 54-filter group like "Language-specific" makes losing the scroll position on every toggle obvious. The row keeps the last database-confirmed state, so `action_for` always decides from observed reality, and a programmatic write is flagged so it is not mistaken for a click.
- `listen_auth` must be forced on when `listen_address` leaves loopback — the config comment says authentication is required, and the GUI should enforce rather than merely warn. This is a **precondition, not a fix-up**: with auth off, `config set listen_address 0.0.0.0` prompts for a username, finds no TTY, and silently keeps the old address while still printing `Config has been updated` (contract §5). `config::listen_address_plan` returns the two calls in the only order that works.
- Enabling authentication is **not sufficient**: the same silent no-op happens when `listen_auth.username` *or* `listen_auth.password` is empty (contract §5). The plan cannot fix that by reordering, and must not invent a credential the user could never log in past — so it refuses and names what is missing, and the Advanced page states the requirement in the group description before the user meets it. Conversely, a *retreat* to loopback always succeeds from any state, so it is never gated: a user exposed with unusable credentials must always be able to come back.
- **The Advanced page enforces the invariant from both directions.** Authentication cannot be switched off while the listen address is beyond loopback, and moving beyond loopback asks for confirmation first — exposing a proxy to the network is not something to do on a mistyped keystroke. The row also carries a warning while it *is* exposed, for the same reason the DNS filtering row carries one while it is inert.
- **Numeric settings are the GUI's responsibility to bound.** `config set` type-checks and nothing more: it accepts port `99999`, `worker_threads 0`, and `3.5` — which writes a float that every later integer read then fails on (contract §5). `Setting::permits_number` holds the ranges. A file value outside them renders read-only with the real number shown, never clamped, since clamping the display would invite the user to write the clamped value back by accident.
- A setting that reads "on" is not necessarily doing anything. `dns_filtering.enabled` has no effect in `manual` proxy mode unless `dns_filtering.listen_port` names a real port, and the CLI enforces no such dependency — so the Protection page marks the row rather than letting the switch imply protection the user does not have.
- **Distinguish "off" from "unknown".** A key that is absent, or holds something that is not a boolean, renders as an insensitive row reading *unavailable* — never as off. Claiming ad blocking is disabled when we simply could not read the setting is the more dangerous of the two errors.
- First run replaces the TTY-only `configure` wizard with an `AdwNavigationView` assistant issuing discrete `config set` calls.
- Licence activation opens the activation URL with `gtk::UriLauncher`, then polls `license` until `APP_ACTIVE` (contract §7).

### GNOME dock icon grouping

Set the GTK application ID, the `.desktop` filename, and `StartupWMClass` to the **same** reverse-DNS string. On GNOME a mismatch makes the running window fail to group with its pinned launcher — a duplicate icon appears below the favourites separator. This is a known recurring annoyance on this machine, so get it right from the first commit.

---

## 6. Privileged operations

`auto` proxy mode is in scope for v1. There is nothing to reuse: `adguard_root_helper` is not setuid, and the package ships no polkit policy (contract §8).

Design:

1. A small **`adguard-ui-helper`** binary with a closed, enumerated set of actions — `set-proxy-mode auto|manual`, `install-system-cert`, `set-system-dns on|off`. It takes no free-form arguments, no paths, no shell strings.
2. A polkit action file in `data/`, `auth_admin_keep`, so one authentication covers a short burst of changes.
3. The GUI invokes it via `pkexec`; never `sudo`, never a setuid bit of our own.
4. The helper validates every argument against its enum and refuses anything else. It performs the change by calling `adguard-cli config set proxy_mode …` (or the documented system operation) — it does **not** reimplement AdGuard's logic.
5. Toggles that require escalation are visually marked so the authentication prompt is never a surprise.

Because `adguard-cli` and its data live under `~/.local`, a root-invoked helper must be explicit about which user's config it edits — pass the target `$HOME`/UID explicitly rather than relying on the ambient environment, and refuse to operate on a path outside that user's data dir. Getting this wrong is a local privilege-escalation bug.

---

## 7. v1 scope

**In:** status + lifecycle control, protection toggles, filter enable/disable with the SQLite-backed catalogue, tray icon with quick toggles, first-run assistant, auto-mode switch via polkit.

**Out (v2):** live blocked-request stats (needs log tailing; format undocumented and unstable — contract §9), userscript installation from URL, HAR capture, `speed` benchmark UI, import/export, full advanced-settings parity.

Ship the tray + core controls first; it is the part that replaces day-to-day terminal use.

Status: Status, Protection, Filters (HTTP) and Advanced are done, and the tray carries start/stop plus the six Protection toggles as quick toggles (§4). Still open for v1: the first-run assistant, the DNS page, auto-mode via polkit, and autostart so the tray is there at login without launching the window.

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
| Helper misuse | Enumerated actions only, no free-form input, explicit target user. |
| Proxy stopping underneath the UI | Status polling already detects it; surface it as a toast rather than failing silently. |
| A test suite that edits the user's real settings | `Cli::with_xdg_data_home` gives the real binary a throwaway config, so the write path — including the cases that expose the proxy — is covered against a copy (contract §5). Only one boolean round-trip still touches the live install, behind a restoring `Drop` guard. |
| A credential leaking into a log or toast | `config set` echoes the value it was given and our own error type quotes the command line; `Cli::set_secret` scrubs both. The value is still visible in `argv` for the ~20 ms of the call — unavoidable, since the CLI's only other route for a credential is the interactive prompt. |
| A `config set` that hangs instead of failing | The prompting commands only give up because there is no TTY, which is a property of *how the app was launched*. `Cli::run` closes stdin so it is a property of the wrapper instead (contract §7). |
