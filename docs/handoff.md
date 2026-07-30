# Handoff

Working state as of the overnight run of 30 July 2026, which closed the config monitor, the CLI timeout, the lapsed-licence mapping, the Stealth page and the `dns_filtering` dependency caveat. Read [`cli-contract.md`](cli-contract.md) and [`architecture.md`](architecture.md) first — the contract doc records measured CLI behaviour and the code depends on it. §5 of the contract is the part that matters for anything touching config; §4 of architecture.md is the part that matters for anything touching the tray or the way the process starts.

---

## 1. Where things stand

**108 tests pass by default** and 23 more are `#[ignore]`d.

| Page | State |
| --- | --- |
| Status | Done. Runtime state, start/stop/restart, 2 s poll (10 s when only the tray shows). |
| Protection | Done. Six switches, `proxy.yaml` → `config set`. |
| Filters (HTTP) | Done. SQLite-backed catalogue with localised names. |
| Advanced | Done. Ports, listen address, auth, outbound proxy, worker threads, log level, secure DNS filtering. |
| Stealth | Done. The 26 settings behind Protection's stealth switch, including the nested `anti_dpi` section. |
| Tray | Done. Start/stop plus the six Protection toggles, in the GUI process. |
| Config monitor | Done. External edits to `proxy.yaml` reconcile the table-driven pages live, without churning on our own CLI traffic. |
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

## 2. Next step: a decision, not code

Everything that could be done without one has been. The three questions below are in [`overnight-plan.md`](overnight-plan.md) §5 with the evidence; the short forms:

1. **Is a Userscripts page in v1?** §1 here and `architecture.md` §5 both list it; §7's "In" list does not. Whichever way it goes, fix the contradiction in the docs rather than settling it by writing code.
2. **Is licence activation in v1?** See gap 1 — the design in `architecture.md` §5 rests on two assumptions that measurement contradicts.
3. **Is auto mode worth a setuid-adjacent helper?** There is a smaller version worth weighing first: ship no helper, show AdGuard's own recommended privileged command with an explanation, detect the resulting state, then do the unprivileged mode switch. Most of the value, no new root attack surface, and verifiable headlessly.

---

## 3. Known gaps, in the order I would fix them

1. **Licence activation.** The error mapping is done — a lapsed licence now surfaces as `Error::Unlicensed` carrying the CLI's own sentence and the command that fixes it, rather than "adguard-cli rejected `status`". What remains is the flow itself (`architecture.md` §5: open the URL with `gtk::UriLauncher`, poll `license` until `APP_ACTIVE`), and two measured complications make it unsafe to code blind — `license` is itself licence-gated, so while unlicensed the poll condition is "stops refusing" rather than "returns `APP_ACTIVE`"; and the CLI's own no-TTY message says to run `activate` *again* to complete, so polling `license` alone may never flip.
2. **DNS page.** `FilterSet::Dns` already carries the CLI prefix, the database path and the user-rules path, and `FiltersPage` is parameterised by it, so the catalogue half is close to free. The rest is not: the upstream/fallback/bootstrap lists need `config list-add`/`list-remove`, which `cli.rs` does not have (`Config::list_at` is only the read half), and the user-rules toggle cannot go through `dns filters enable` (contract §6).
3. **First-run assistant** — `architecture.md` §5. Discrete `config set` calls through a path that is fully built; the care needed is in not tripping the silent no-ops in contract §5.
4. **Custom filter install by URL** (`filters install`). Network-touching, so it wants `NETWORK_TIMEOUT` and a visible progress state, both of which now exist in `cli.rs`.
5. **Auto mode via polkit** — `architecture.md` §6, and see §2 above before starting. `data/` already holds a commented `.policy` scaffold for three actions; what is missing is the helper binary. AdGuard's own `adguard_root_helper` is not setuid and its package ships no policy, so there is nothing upstream to reuse.

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

**Driving the UI headlessly.** Any page can now be opened and read without a display, which is what makes "the page renders" provable rather than assertable. Start the app under `xvfb-run` on a private bus, launch `at-spi-bus-launcher`, then find the node with role **`list`** — not `list box`, which is what the sidebar is *not* — and call `get_selection_iface().select_child(n)`. Walking names afterwards gives every row and subtitle of the page that is now visible. Only the visible `GtkStack` child appears, so select first and read second.

**A measurement taken from one line of output is not a measurement.** Twice in one night: the unlicensed error looked like a single sentence until the real binary showed twenty lines of usage after it, and the `anti_dpi` write looked unverified because a `grep -A 7` window fell short in a file that is half comments. Print the whole thing before building on it.
