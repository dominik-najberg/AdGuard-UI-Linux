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
| Tray | **StatusNotifierItem via `ksni`** | `org.kde.StatusNotifierWatcher` is live on the session bus and `ubuntu-appindicators@ubuntu.com` is ACTIVE. `ksni` speaks SNI over D-Bus with **no C headers** — relevant because `libayatana-appindicator3-dev` is not installed. |
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
│   └── adguard-tray/           # ksni StatusNotifierItem
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

- **Runtime status** — poll `adguard-cli status` on a ~2 s timer while a window is open; slow to ~10 s when only the tray is visible. At 10 ms per call this is negligible.
- **Config** — watch `proxy.yaml` with `gio::FileMonitor`. External edits (the user is expected to hand-edit; the CLI even suggests it) then appear live in the UI.
- **Filters** — watch the `.db` files with the same mechanism, debounced; the daemon rewrites them on update.

### Verify, don't trust

Because semantic failures exit 0 (contract §3), every mutation follows **act → re-read → reconcile**. Set a toggle, then re-read `proxy.yaml` and render from that. Never optimistically flip a switch and assume it stuck; the UI state must always be a projection of observed reality.

---

## 4. Threading

All CLI invocations and SQLite reads happen off the main thread. Use the pattern already proven in `legion-gui`: a worker task plus `async-channel`, results delivered to the UI via `glib::spawn_future_local`.

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
