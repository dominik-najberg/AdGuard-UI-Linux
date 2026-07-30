# Handoff

Working state as of 30 July 2026. The overnight run closed the config monitor, the CLI timeout, the lapsed-licence mapping, the Stealth page and the `dns_filtering` dependency caveat; the session after it built **licence activation**, and the one after that the **DNS page** — which cures the inert-DNS caveat Protection had been showing with no remedy anywhere in the app. Read [`cli-contract.md`](cli-contract.md) and [`architecture.md`](architecture.md) first — the contract doc records measured CLI behaviour and the code depends on it. §5 of the contract is the part that matters for anything touching config; §4 of architecture.md is the part that matters for anything touching the tray or the way the process starts.

---

## 1. Where things stand

**142 tests pass by default** and 32 more are `#[ignore]`d.

| Page | State |
| --- | --- |
| Status | Done. Runtime state, start/stop/restart, 2 s poll (10 s when only the tray shows), and the licence. |
| Protection | Done. Six switches, `proxy.yaml` → `config set`. |
| Filters (HTTP) | Done. SQLite-backed catalogue with localised names. |
| Advanced | Done. Ports, listen address, auth, outbound proxy, worker threads, log level, secure DNS filtering. |
| Stealth | Done. The 26 settings behind Protection's stealth switch, including the nested `anti_dpi` section. |
| Tray | Done. Start/stop plus the six Protection toggles, in the GUI process. |
| Config monitor | Done. External edits to `proxy.yaml` reconcile the table-driven pages live, without churning on our own CLI traffic. |
| Autostart | Done. `--background` starts windowless; `data/autostart/` installs into `~/.config/autostart/`. |
| Icon | Done. Colour and symbolic SVGs in `data/icons/`, installed by `building.md` §4. Until that install has been done once, a `cargo run` window gets the generic cog — the artwork is reached through the desktop entry, not through the binary. |
| DNS | Done. The `agflm_dns.db` catalogue, the user-rules toggle, the three server settings, and the tri-state `listen_port`. Its settings sit above the catalogue as a `filters::Host` prelude so both halves share one scroll. |
| Licence activation | Done, bar the success leg. Owner and masked key when licensed; `activate` → open the link → *finish activation* when not. Never polled. |
| Auto mode | Not started. No privileged component of ours — detection and instruction only, `architecture.md` §6. |
| Reconcile toast | Not started. `architecture.md` §3. |
| First-run assistant | Not started. |

Userscripts are **out of v1** — `architecture.md` §7 has the reasoning.

Run it:

```bash
cargo run -p adguard-gui
```

One binary, `adguard-ui`, serves the window and the tray. `adguard-tray` is a library. Seeing the icon needs a real session plus an AppIndicator extension — see `building.md`.

Three things about startup are worth knowing before touching `main.rs`. The UI is built by the **first** activation and kept, so a later one presents that window instead of building a rival with its own poll timer and tray. The application takes `HANDLES_COMMAND_LINE`, so `--background` reaches the instance that acts on it rather than being parsed and dropped by the launching process. And under `--background` a tray that will not register is **fatal** — the inverse of the rule everywhere else, because there is no window to fall back to. `architecture.md` §4 has the reasoning.

---

## 2. Decisions taken, 30 July 2026

The five open questions in [`overnight-plan.md`](overnight-plan.md) §5 were settled by the project owner. Each is now written into the doc that owns it; this list is the index, not the reasoning.

| Question | Decision | Written up in |
| --- | --- | --- |
| Userscripts page in v1? | **Out of v1.** One script exists and AdGuard says only that one is supported | `architecture.md` §7 |
| Licence activation in v1? | **In**, but user-driven: open the URL, then a *finish activation* button that re-runs `activate`. Never polled | `architecture.md` §5, contract §7 |
| Auto mode via a helper of ours? | **No.** AdGuard ships its own setup path (`sudo … adguard_root_helper -s`). We detect the three properties it checks and show its command. No polkit action, no privileged binary, no `pkexec` | `architecture.md` §6, contract §8 |
| May the DNS page write `listen_port`? | **Yes** — disabled / automatic / fixed, the three states the config documents. Measure the bind address before shipping the row | `architecture.md` §5 |
| Silent reconcile, or a toast? | **Toast, but only when a row the user can see actually moved.** `reconcile` returns a count; zero stays silent | `architecture.md` §3 |

Two of these overturned a recommendation in `overnight-plan.md` §5, both because reading the `adguard-cli` binary's strings contradicted what had been inferred from file modes and from a design written before measurement. Contract §8 in particular said no upstream escalation path existed. One did.

---

## 3. Known gaps, in the order I would fix them

1. **First-run assistant** — `architecture.md` §5. Discrete `config set` calls through a path that is fully built; the care needed is in not tripping the silent no-ops in contract §5.
2. **Custom filter install by URL** (`filters install`). Network-touching, so it wants `NETWORK_TIMEOUT` and a visible progress state, both of which now exist in `cli.rs` — `Cli::activate` is the first caller of that timeout and shows the shape.
3. **Auto mode** — `architecture.md` §6. No longer a polkit item: AdGuard ships its own setup path, so the work is a `stat` of `~/.local/opt/adguard-cli/adguard_root_helper` for the three properties `adguard-cli` itself checks (`owned_by_root`, `has_suid`, `is_executable`), a row showing AdGuard's `sudo … -s` command when they are unmet, a re-check on window focus, and an ordinary `config set proxy_mode auto` when they are met. Both branches are provable headlessly by pointing the check at a fake path. **`data/io.github.dominik-najberg.AdGuardUI.policy` is now dead scaffolding** — delete it with this work, and do not install it; it names a helper that will never exist.
4. **Reconcile toast** — `architecture.md` §3. The smallest of these: `reconcile` returns how many displayed rows differed, and a non-zero count raises an `AdwToast`. The point is not the toast, it is that the count is what makes the app's own writes stop announcing themselves as somebody else's. Fix the stderr line in the same change — it currently claims "outside the app" about something it has no way to know.
5. **The activation success leg is a claim, not a measurement.** Everything up to the browser log-in is proven, including against a real unlicensed install: `activate` hands back a link, the page shows it, *finish activation* re-runs `activate`, reads `license`, and says "not activated yet" without pretending otherwise. What nobody has watched is the leg after a genuine log-in — it needs a real account, and completing an activation spends a device slot. **This is the owner's call, not an agent's.** Two things go with it: what `activate` prints against an install that is *already* licensed is unmeasured for the same reason, and the "AdGuard is activated" wording has never been seen on screen.

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

**Two `adguard-cli` invocations at once against such a directory will bite you, and it is AdGuard's fault, not ours.** Whichever loses the race with the other's initialisation exits **1** with `Filter manager initialization failed` on **stdout** and stderr empty — eight runs in twelve. It is why `Error::Refused` is no longer exclusive to exit 0 (contract §3), and why `StatusPage::reload` reads `status` and the licence sequentially on one worker rather than firing both. Once anything has initialised the directory it never recurs, so a second run always looks fine — which is what makes it a trap rather than a bug you meet once.

**Formatting.** The tree is hand-formatted and `cargo fmt --check` has been dirty since the first commit. That is deliberate — the measured-behaviour tables in `config.rs`, `cli.rs` and `model.rs` do not survive rustfmt. Do not reformat.

**Screenshots.** GNOME denies D-Bus screenshots, and `x11grab` on `:0` captures nothing under Wayland because Xwayland windows are not drawn into the X root window. Use Xvfb; the recipe is in `building.md`. There is no `xdotool`, so the virtual screen has to be taller than the window to get a whole page in one frame.

**Subagents.** If you run a review workflow, check `git status` afterwards — one previously wrote a scratch test file into the tree. And do not apply fixes while a verify phase is still running; verifiers ended up reading already-corrected code and citing the new tests as proof the findings were wrong.

**Driving the UI headlessly.** Any page can now be opened and read without a display, which is what makes "the page renders" provable rather than assertable. Start the app under `xvfb-run` on a private bus, launch `at-spi-bus-launcher`, then find the node with role **`list`** — not `list box`, which is what the sidebar is *not* — and call `get_selection_iface().select_child(n)`. Walking names afterwards gives every row and subtitle of the page that is now visible. Only the visible `GtkStack` child appears, so select first and read second.

**That dump now contains the licence owner's e-mail address**, in full and by design — it is the Status page, and the Owner row shows it whenever the machine is licensed. Everything else in this codebase is careful about that address; this recipe is the one route that hands it straight to a terminal, and from there to a commit message or a bug report. Redact it before pasting a Status-page walk anywhere, or take the walk against a sandbox `$XDG_DATA_HOME`, which has no licence and therefore no owner.

**A command's echo is not its effect, even when the echo looks like the file.** `config list-remove` of a list's last element prints `filters:` with nothing after it. The file gets `filters: []`. Those read the same to a human and differently to a YAML parser — null versus an empty sequence — and the difference was written up in contract §5, and into two doc comments and a test name, before anyone looked at the bytes. The rule this project already had for `config set` covers it exactly: **the confirmation is never the evidence, re-read the file.** It just had not occurred to anyone that a command whose output *is* YAML-shaped needed the same suspicion.

**A measurement taken from one line of output is not a measurement.** Twice in one night: the unlicensed error looked like a single sentence until the real binary showed twenty lines of usage after it, and the `anti_dpi` write looked unverified because a `grep -A 7` window fell short in a file that is half comments. Print the whole thing before building on it.

**Nor is a measurement of one stream.** The same mistake in a new shape, a day later: the initialisation failure above was first written up as "the CLI exits 1 having printed nothing at all", from a probe that captured stderr and discarded stdout (`2>&1 >/dev/null`). It had printed a perfectly good sentence — on the other stream — and the fix that would have been written for a silent failure would have been the wrong fix. Capture both, and say which one carried the message.

**Two nodes carry a switch row's name, and only one of them works.** An `AdwSwitchRow` appears in the tree twice under the same name — the row and the `GtkSwitch` inside it. The row reports `n_actions = 0`; the inner one has a single `toggle`. Taking the first match makes `do_action` raise `No action with index 0` (or, if you reach for the row's own interface, nothing happens at all and the state never moves), which reads exactly like the write path being broken. Filter candidates by `Atspi.Action.get_n_actions(iface) > 0` rather than by role or position. Driven properly, the DNS user-rules switch moves in both directions and `proxy.yaml` follows.

**`AdwComboRow` cannot be driven this way at all.** It exposes neither a selection interface nor an action, so a tri-state control's *write* leg is not provable headlessly — only its rendering, by seeding the config and reading the row back. That is what was done for the DNS listen-port row's four states; the write underneath it is covered at the CLI layer instead, by `config_sandbox.rs`.

**AT-SPI can drive buttons, not just read them.** `get_action_iface()` plus `Atspi.Action.do_action(iface, 0)` presses a button by name, which is how the whole activation flow was proven headlessly: press *Activate…*, read the link row that appears, press *Finish activation*, read the refusal. The role name to match on is `button`, not `push button`. `scroll_to` and `grab_focus` both fail with a bare `atspi_error` under GTK4, so anything below the fold of an 820×680 window can be read but not photographed.

**A live log-in link deserves the same care as a write.** Pressing *Activate…* under a test harness will hand a real activation URL to whatever handler the session has. Point the `https` scheme at a no-op `.desktop` inside the sandbox `$XDG_DATA_HOME`, with `GTK_USE_PORTAL=0` so the portal does not consult the machine's own defaults instead — otherwise a browser already signed in to AdGuard could bind a device slot to a throwaway install.
