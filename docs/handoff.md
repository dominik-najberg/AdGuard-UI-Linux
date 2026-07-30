# Handoff

Working state as of the commit that added `--background` and the autostart entry. Read [`cli-contract.md`](cli-contract.md) and [`architecture.md`](architecture.md) first — the contract doc records measured CLI behaviour and the code depends on it. §5 of the contract is the part that matters for anything touching config; §4 of architecture.md is the part that matters for anything touching the tray or the way the process starts.

---

## 1. Where things stand

**90 tests pass by default** and 23 more are `#[ignore]`d.

| Page | State |
| --- | --- |
| Status | Done. Runtime state, start/stop/restart, 2 s poll (10 s when only the tray shows). |
| Protection | Done. Six switches, `proxy.yaml` → `config set`. |
| Filters (HTTP) | Done. SQLite-backed catalogue with localised names. |
| Advanced | Done. Ports, listen address, auth, outbound proxy, worker threads, log level. |
| Tray | Done. Start/stop plus the six Protection toggles, in the GUI process. |
| Config monitor | Done. External edits to `proxy.yaml` reconcile Protection and Advanced live, without churning on our own CLI traffic. |
| Autostart | Done. `--background` starts windowless; `data/autostart/` installs into `~/.config/autostart/`. |
| DNS | Not started. |
| Userscripts | Not started. |
| First-run assistant | Not started. |

Run it:

```bash
cargo run -p adguard-gui
```

One binary, `adguard-ui`, serves the window and the tray. `adguard-tray` is a library. Seeing the icon needs a real session plus an AppIndicator extension — see `building.md`.

Three things about startup are worth knowing before touching `main.rs`. The UI is built by the **first** activation and kept, so a later one presents that window instead of building a rival with its own poll timer and tray. The application takes `HANDLES_COMMAND_LINE`, so `--background` reaches the instance that acts on it rather than being parsed and dropped by the launching process. And under `--background` a tray that will not register is **fatal** — the inverse of the rule everywhere else, because there is no window to fall back to. `architecture.md` §4 has the reasoning.

---

## 2. Next step: the config file monitor

It unblocks a class of staleness rather than a feature. All four pages are refresh-button-only, so an edit made in a terminal — which the CLI itself suggests — never reaches the UI.

**The trap, measured an earlier cycle:** every `adguard-cli` invocation rewrites `proxy.yaml` and touches its mtime, `--version` included, even when no byte changes (contract §5). Since the app polls `status` every 2 s, a `gio::FileMonitor` would fire continuously against changes we caused ourselves, repainting pages under the user's pointer. **It must compare content, not trust the event** — keep a hash of the last-read file and ignore events where it has not moved. Debouncing does not help; the churn never stops.

Autostart made this slightly worse and slightly better at once: a background session polls at the 10 s rate, so the self-inflicted churn is a fifth of what a windowed one produces — but it now runs from login, so the monitor has to see through it for the whole session rather than for as long as a window is open.

Two things already in place to build on:

- The pages have observer hooks (`StatusPage::connect_status`, `ProtectionPage::connect_config`) that the tray uses. A monitor can drive the same reconcile paths.
- `advanced.rs` already leaves a row alone when its setting has not moved in the file since the row was last painted, so an external-change repaint will not disturb a part-typed entry.

Also from the same measurement: a key *deleted* from the file is silently restored with its default by the next invocation, while a wrongly *typed* value is not. So "unavailable" for a missing key is transient and self-healing; for a bad type it is permanent.

---

## 3. Known gaps, in the order I would fix them

1. **Licence activation.** The error mapping is done — a lapsed licence now surfaces as `Error::Unlicensed` carrying the CLI's own sentence and the command that fixes it, rather than "adguard-cli rejected `status`". What remains is the flow itself (`architecture.md` §5: open the URL with `gtk::UriLauncher`, poll `license` until `APP_ACTIVE`), and two measured complications make it unsafe to code blind — `license` is itself licence-gated, so while unlicensed the poll condition is "stops refusing" rather than "returns `APP_ACTIVE`"; and the CLI's own no-TTY message says to run `activate` *again* to complete, so polling `license` alone may never flip.
2. **`cli.rs`'s timeout TODO.** Still outstanding, still blocking any network command. Nothing network-touching has been wired, so it has not bitten — but `filters update` and `check-update` cannot land before it does.
3. **DNS user-rules toggle needs `config list-add`/`list-remove`**, not `dns filters enable` (contract §6). `Config::list_at` is the read half and already exists.
4. **Stealth mode has ~20 sub-settings** plus a nested `anti_dpi` section; it wants its own sub-page rather than a group on Protection.
5. **`https_filtering.encrypted_client_hello` and `filter_secure_dns_mode` are documented as requiring `dns_filtering`**, and neither the CLI nor the GUI enforces it. Same class as the `dns_filtering.listen_port` caveat that Protection already shows.
6. **Custom filter install by URL** (`filters install`).
7. **Auto mode via polkit** — `architecture.md` §6. Nothing exists to reuse: `adguard_root_helper` is not setuid and the package ships no polkit policy.

---

## 4. Things that will bite you if you do not know them

**Config writes.** `Config has been updated` is necessary but not sufficient — it prints for a no-op *and* for a change the CLI silently declined. Always re-read `proxy.yaml`. Only ever write lowercase `true`/`false`. Pass `--` before any user-supplied key or value, or a value starting with `-` is eaten as an option. `config set` type-checks and never range-checks, so bounds are ours. Nothing enforces dependencies between settings; the GUI owns them.

**Testing writes.** The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`, so `Cli::with_xdg_data_home` gives the real binary a throwaway config. Put anything dangerous in `tests/config_sandbox.rs`, which never touches the machine and asserts as much:

```bash
cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
```

A sandbox is unlicensed, so only the `config` family and `--version` work there. `config_mutate.rs` still drives the real config and is deliberately kept to one boolean round-trip behind a restoring `Drop` guard.

The same trick works on the app, which is the only practical way to see how a page renders against a config you would not create on purpose:

```bash
XDG_DATA_HOME=/tmp/fake cargo run -p adguard-gui
```

**Formatting.** The tree is hand-formatted and `cargo fmt --check` has been dirty since the first commit. That is deliberate — the measured-behaviour tables in `config.rs`, `cli.rs` and `model.rs` do not survive rustfmt. Do not reformat.

**Screenshots.** GNOME denies D-Bus screenshots, and `x11grab` on `:0` captures nothing under Wayland because Xwayland windows are not drawn into the X root window. Use Xvfb; the recipe is in `building.md`. There is no `xdotool`, so the virtual screen has to be taller than the window to get a whole page in one frame.

**Subagents.** If you run a review workflow, check `git status` afterwards — one previously wrote a scratch test file into the tree. And do not apply fixes while a verify phase is still running; verifiers ended up reading already-corrected code and citing the new tests as proof the findings were wrong.
