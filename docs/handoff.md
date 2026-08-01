# Handoff

> **Where the session of 1 August 2026 stopped.** Both items the previous note
> called ready are **done, committed, and verified**: **certificate-trust
> detection** (§3 gap 4) and **packaging** — a `.deb` and a tarball, behind
> `make package`. The real `proxy.yaml` still hashes to `c4b58ce8…`, unchanged
> across both commits, and nothing was installed into the system trust store or
> anywhere else.
>
> **One thing is left open, and it is still not an agent's to take:** the
> activation success leg (§3 gap 5). It needs a real account and spends a
> device slot.
>
> Five things from this session worth carrying forward:
>
> - **`configure` does not generate a CA when `adguard.conf` is present — it
>   reproduces the existing one, byte for byte** (contract §8). So that one file
>   carries the CA's private key as well as the licence key, and the sandbox
>   recipe in `building.md` §3 copies rather more than it used to admit. It also
>   means a first-run sandbox on this machine inherits a certificate the system
>   already trusts, which is why the assistant's certificate rows are correctly
>   *invisible* there — that read as a bug for a while and was not one.
> - **The system trust bundle carries no names at all.** `grep AdGuard
>   /etc/ssl/certs/ca-certificates.crt` returns nothing on a machine where the
>   certificate *is* trusted. Membership is decided on the certificate's own
>   base64 body, and anything that looks up a trust store by name is wrong.
> - **AdGuard's installer checks the anchor path, not its contents**, so a
>   regenerated CA leaves a file of the right name holding the wrong certificate
>   that re-running the installer reports success over. That state is detected
>   and named separately, and it is why the check compares bytes.
> - **A command this app shows is a command it vouches for.** The certificate
>   path comes from a setting, so `trust::quotable` refuses any path that would
>   break out of AdGuard's quoting rather than handing the user a line that does
>   something other than what the row says (`architecture.md` §6).
> - **`$DISPLAY` leaks into a headless harness and nothing tells you.** The
>   window opens on the real desktop through Xwayland, the AT-SPI walk passes
>   because the accessibility bus does not care which X server drew anything,
>   and only the frame grab — black, with an X cursor — gives it away.
>   `building.md` §3 now says so.
>
> Three from the night before, still true:
>
> - The Status page's module figure is **repainted but not counted** by the
>   reconcile, because it is derived from the keys Protection owns and so moves
>   for the app's own writes (`architecture.md` §3). The certificate rows are
>   now the second thing painted under that rule, for a different reason: they
>   do not come from `proxy.yaml` at all.
> - `config set proxy_mode auto` **succeeds with AdGuard's root helper unmet**
>   (contract §8), which is why the gate lives in this app and why the unmet
>   state is rendered rather than merely prevented.
> - Custom filter ids are **never reused**, so a removal cannot be undone by id
>   — which is why removal is confirmed up front rather than offered as an undo
>   afterwards (contract §6).

Working state as of 31 July 2026. The overnight run closed the config monitor, the CLI timeout, the lapsed-licence mapping, the Stealth page and the `dns_filtering` dependency caveat; the session after it built **licence activation**, the one after that the **DNS page**, the one after that the **first-run assistant** — the first page here that exists for a machine where AdGuard has never been configured at all — and the one after that **custom filter install by URL**. Read [`cli-contract.md`](cli-contract.md) and [`architecture.md`](architecture.md) first — the contract doc records measured CLI behaviour and the code depends on it. §5 of the contract is the part that matters for anything touching config; §4 of architecture.md is the part that matters for anything touching the tray or the way the process starts.

**If you are picking up where the custom filters left off**, the thing to know is that contract §6 gained a subsection, and the fact in it that changes decisions is this: **AdGuard checks only whether what it downloaded *begins* with HTML.** That catches a link answering 200 with an error page, and nothing else. JSON, prose, the wrong plain-text file and an empty response all install as filter lists holding no rules, report success, and leave a switch reading *on* over something that filters nothing. The Filters page says so in the group description because no other part of this UI ever could.

That subsection also carries the correction worth reading before trusting anything else in it: an earlier revision of it said content was *never* validated, generalised from a single probe file that happened to open with a line of prose before its HTML. The reasoning was fine and the fixture was not — the same lesson §3 already records about measuring one line and one stream, arriving a third time as one sample. A test caught it, which is the argument for `filters_sandbox.rs` pinning both sides of the boundary.

---

## 1. Where things stand

**176 tests pass by default** and 44 more are `#[ignore]`d.

| Page | State |
| --- | --- |
| Status | Done. Runtime state, start/stop/restart, 2 s poll (10 s when only the tray shows), and the licence. |
| Protection | Done. Six switches, `proxy.yaml` → `config set`. |
| Filters (HTTP) | Done. SQLite-backed catalogue with localised names, plus custom lists installed by URL. |
| Custom filters | Done. A URL entry row above the catalogue on both filter pages; `filters install` behind `NETWORK_TIMEOUT`, verified by the row that appears rather than by what was printed. |
| Advanced | Done. Ports, listen address, auth, outbound proxy, worker threads, log level, secure DNS filtering. |
| Stealth | Done. The 26 settings behind Protection's stealth switch, including the nested `anti_dpi` section. |
| Tray | Done. Start/stop plus the six Protection toggles, in the GUI process. |
| Config monitor | Done. External edits to `proxy.yaml` reconcile the table-driven pages live, without churning on our own CLI traffic, and raise one toast when a row the user can see actually moved. |
| Autostart | Done. `--background` starts windowless; `data/autostart/` installs into `~/.config/autostart/`. |
| Icon | Done. Colour and symbolic SVGs plus nine pre-rendered PNG sizes in `data/icons/`, all installed by `building.md` §4. Until that install has been done once, a `cargo run` window gets the generic cog — the artwork is reached through the desktop entry, not through the binary. |
| DNS | Done. The `agflm_dns.db` catalogue, the user-rules toggle, the three server settings, and the tri-state `listen_port`. Its settings sit above the catalogue as a `filters::Host` prelude so both halves share one scroll. |
| Licence activation | Done, bar the success leg. Owner and masked key when licensed; `activate` → open the link → *finish activation* when not. Never polled. |
| First-run assistant | Done. Shown when there is no `proxy.yaml`: licence check → one guarded `configure` to seed → four questions pre-filled from the seeded file → writes the deltas and reports what landed → hands the window to the pages. Driven end to end headlessly. |
| Custom filter removal | Done. A trash button on custom rows only, behind an `AdwAlertDialog` that names the URL and offers switching off instead; verified by the row being gone from the database. `architecture.md` §5. |
| Auto mode | Done. A `proxy_mode` row on Advanced, AdGuard's three-property helper check beside it, its `sudo` command with a copy button, and a re-check on window focus. No privileged component of ours, `architecture.md` §6. |
| Reconcile toast | Done. One toast per reading, gated on `reconcile`'s count of rows that actually moved; the Status module figure is repainted but not counted, `architecture.md` §3. |
| Certificate trust | Done. Three facts read from three files, under the Protection switches and in the first-run assistant, with AdGuard's own `install_cert.sh` command and a copy button. Four unmet states, all four rendered headlessly; hidden when trusted or when HTTPS filtering is off. `architecture.md` §6, contract §8. |
| Packaging | Done. `make deb`, `make tarball`, `make package`. Neither needs root; `Depends:` is derived by `dpkg-shlibdeps` rather than written down. `building.md` §5. |

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
| May the DNS page write `listen_port`? | **Yes** — disabled / automatic / fixed, the three states the config documents. The bind address the decision made conditional has since been measured: loopback, and not movable | `architecture.md` §5, contract §5 |
| Silent reconcile, or a toast? | **Toast, but only when a row the user can see actually moved.** `reconcile` returns a count; zero stays silent | `architecture.md` §3 |

Two of these overturned a recommendation in `overnight-plan.md` §5, both because reading the `adguard-cli` binary's strings contradicted what had been inferred from file modes and from a design written before measurement. Contract §8 in particular said no upstream escalation path existed. One did.

---

## 3. Known gaps, in the order I would fix them

1. ~~**Removing a custom filter.**~~ — done, `architecture.md` §5. A trash button on custom rows only, behind an `AdwAlertDialog` naming the URL, verified by the row being gone from the database rather than by `Filter [ID: …] removed`. Two facts it turned up are worth keeping: custom ids are **never reused**, so a removed-and-re-fetched list comes back as a *new* row and nothing may hold an id across a removal; and `remove` on a **catalogue** filter clears `is_enabled` as well as `is_installed`, which contract §6's original table did not say.
2. ~~**Auto mode**~~ — done, `architecture.md` §6. The `.policy` file is deleted. Three things it found are worth carrying:

   - **`config set proxy_mode auto` succeeds with the helper unmet.** Exit 0, `Config has been updated`, and the file really holds `auto` (contract §8). AdGuard does not check its own helper at write time, so the GUI's gate is the only one there is — and the unmet state has to be *rendered* as well as prevented, because a terminal reaches it in one command.
   - **The helper is a sibling of the *resolved* binary.** `$PATH` finds `~/.local/bin/adguard-cli`, which is a symlink; the helper is next to the real file. `paths::root_helper` canonicalises, and `RootHelper::inspect` uses `fs::metadata` so symlinks are followed — `symlink_metadata` would report the link's `lrwxrwxrwx` and read as not-root-owned whatever it pointed at.
   - **The met branch needs no privilege to test.** `/usr/bin/passwd` is already `-rwsr-xr-x root root`, so pointing `$ADGUARD_ROOT_HELPER` at it exercises the whole met path without this project ever setting a suid bit on anything.

   What is **not** verified: the `connect_is_active_notify` line itself. There is no `xdotool` or `wmctrl` here to take focus from an Xvfb window and give it back. The half underneath it is verified — repointing the helper mid-session and provoking a repaint takes the group off the page — so what is untested is the trigger, not the re-check.
3. ~~**Reconcile toast**~~ — done, `architecture.md` §3. Left here for the one thing it found: the suppression of our own writes comes from the per-row `pending` flag, not from counting, so any figure rendered *outside* the page that writes it has no such flag and will announce the user's own click back at them. The Status module count is that figure, and it is now repainted without being counted. The stderr line no longer claims the change came from "outside the app"; it reports the count instead, which is a fact it has.
4. ~~**The certificate is seeded but not trusted.**~~ — done, `architecture.md` §6. Detect, then show AdGuard's own `install_cert.sh`; nothing here installs anything. Four things it turned up are worth keeping:

   - **The trusted bundle has no names in it.** A `grep` for the certificate's name returns nothing whether or not it is trusted, so membership is decided on the base64 body. Measured against a machine where it *is* installed.
   - **The installer's idempotence check is on the path, not the contents** (`[ ! -f "${SYSTEM_CERT_PATH}" ]`), so a regenerated CA leaves the old one in place and re-running reports success. That is a state its own tooling will not repair, and the reason the check compares bytes rather than asking whether a file exists.
   - **`configure` reproduces the CA from `adguard.conf` rather than generating one** — byte-identical, weeks-old dates. That file carries the private key of a CA this system trusts, which makes `building.md` §3's "delete the sandbox afterwards" rather more than housekeeping.
   - **The met branch is this machine's real state**, the mirror of the root helper, so every *unmet* branch needed the paths to be parameters. `$SYSTEM_CERT_DIR` is AdGuard's own variable; `$ADGUARD_CA_BUNDLE` and `$ADGUARD_CERT_INSTALLER` are ours and exist only so those branches can be reached without touching the real trust store.

   What is **not** verified, exactly as with the helper: the `connect_is_active_notify` trigger itself. There is still no `xdotool` here to take focus from an Xvfb window and give it back. The re-check underneath it is verified — the rows repaint from a check pointed elsewhere — so what is untested is the trigger, not the re-check.
5. **The activation success leg is a claim, not a measurement.** Everything up to the browser log-in is proven, including against a real unlicensed install: `activate` hands back a link, the page shows it, *finish activation* re-runs `activate`, reads `license`, and says "not activated yet" without pretending otherwise. What nobody has watched is the leg after a genuine log-in — it needs a real account, and completing an activation spends a device slot. **This is the owner's call, not an agent's.** Two things go with it: what `activate` prints against an install that is *already* licensed is unmeasured for the same reason, and the "AdGuard is activated" wording has never been seen on screen.

---

## 4. Things that will bite you if you do not know them

**Config writes.** `Config has been updated` is necessary but not sufficient — it prints for a no-op *and* for a change the CLI silently declined. Always re-read `proxy.yaml`. Only ever write lowercase `true`/`false`. Pass `--` before any user-supplied key or value, or a value starting with `-` is eaten as an option. `config set` type-checks and never range-checks, so bounds are ours. Nothing enforces dependencies between settings; the GUI owns them.

**Testing writes.** The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`, so `Cli::with_xdg_data_home` gives the real binary a throwaway config. Put anything dangerous in `tests/config_sandbox.rs`, which never touches the machine and asserts as much:

```bash
cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
```

A sandbox is unlicensed, so only the `config` family, `--version` and `activate` work there — `activate` because it is the command that exists to fix an unlicensed install, which makes a sandbox the only honest place to exercise it. `config_mutate.rs` still drives the real config and is deliberately kept to one boolean round-trip behind a restoring `Drop` guard.

The same trick works on the app, which is the only practical way to see how a page renders against a config you would not create on purpose:

```bash
XDG_DATA_HOME=/tmp/fake cargo run -p adguard-gui
```

**Two `adguard-cli` invocations at once against such a directory will bite you, and it is AdGuard's fault, not ours.** Whichever loses the race with the other's initialisation exits **1** with `Filter manager initialization failed` on **stdout** and stderr empty — eight runs in twelve. It is why `Error::Refused` is no longer exclusive to exit 0 (contract §3), and why `StatusPage::reload` reads `status` and the licence sequentially on one worker rather than firing both. Once anything has initialised the directory it never recurs, so a second run always looks fine — which is what makes it a trap rather than a bug you meet once.

**Formatting.** The tree is hand-formatted and `cargo fmt --check` has been dirty since the first commit. That is deliberate — the measured-behaviour tables in `config.rs`, `cli.rs` and `model.rs` do not survive rustfmt. Do not reformat.

**Screenshots.** GNOME denies D-Bus screenshots, and `x11grab` on `:0` captures nothing under Wayland because Xwayland windows are not drawn into the X root window. Use Xvfb; the recipe is in `building.md`. There is no `xdotool`, so the virtual screen has to be taller than the window to get a whole page in one frame.

**Subagents.** If you run a review workflow, check `git status` afterwards — one previously wrote a scratch test file into the tree. And do not apply fixes while a verify phase is still running; verifiers ended up reading already-corrected code and citing the new tests as proof the findings were wrong.

**Driving the UI headlessly.** Any page can now be opened and read without a display, which is what makes "the page renders" provable rather than assertable. Start the app under `xvfb-run` on a private bus, launch `at-spi-bus-launcher`, then find the node with role **`list`** — not `list box`, which is what the sidebar is *not* — and call `get_selection_iface().select_child(n)`. Walking names afterwards gives every row and subtitle of the page that is now visible. Only the visible `GtkStack` child appears, so select first and read second. The sidebar is `PAGES` in `main.rs` order — Status 0, Protection 1, Filters 2, DNS 3, Stealth 4, Advanced 5 — and the index is positional, so inserting a page without matching the `stack.add_named` order sends the selection to the wrong one with no error.

**`AdwEntryRow`'s apply button is not in the tree either**, so the third widget in a row is unreachable this way. The row itself appears — as a `list item` holding two `label`s and one `text` — and its editable interface works: `get_editable_text_iface().set_text_contents(url)` types into it and reads back correctly. What is absent is anything to press. There is no `button` under the row at any depth, `grab_focus` fails with the same bare `atspi_error` it fails with everywhere else under GTK4, and a synthetic click needs extents that come back pointing at the sidebar. So an entry row's *write* leg joins `AdwComboRow`'s: rendering is provable, the commit is not. The custom-filter install is covered at the CLI layer by `filters_sandbox.rs` instead, and its rendering by a frame.

**Take the frame, and look at it.** The pattern is now three deep — `AdwSpinRow` missing entirely, `AdwComboRow` with no way to select, `AdwEntryRow` with no way to apply — and the fallback `building.md` §3 offers is the one that keeps working. A 1000×1400 Xvfb screen fits the whole Filters page in one grab.

**That dump now contains the licence owner's e-mail address**, in full and by design — it is the Status page, and the Owner row shows it whenever the machine is licensed. Everything else in this codebase is careful about that address; this recipe is the one route that hands it straight to a terminal, and from there to a commit message or a bug report. Redact it before pasting a Status-page walk anywhere.

**The "use a sandbox" escape hatch no longer holds on its own.** It used to end "…or take the walk against a sandbox `$XDG_DATA_HOME`, which has no licence and therefore no owner". Since the licence has been found to live in `adguard.conf` and to travel with a copy of that one file (contract §5), a sandbox is exactly where a licence-gated flow gets driven — the assistant's `configure` cannot be exercised any other way — and such a sandbox has an owner row like any other install. A sandbox is also where output feels safest to paste. **Redact at the harness**, over every line before it is printed; do not rely on the directory being unlicensed.

**An e-mail regex on its own is not the harness this section wants**, and that sentence used to say it was. `license` prints three lines, and the key is on the second — so a harness filtering only addresses hands the more sensitive of the two fields straight to the terminal. Measured the hard way: a probe run while sizing up `filters install` printed `License key: <sixteen characters>` in full, from a sandbox, through a redactor written from this paragraph. Filter both:

```bash
sed -E -e 's/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/<redacted@e-mail>/g' \
       -e 's/(License key:).*/\1 <redacted>/'
```

The same applies to an AT-SPI walk, where the key can reach a dump through the Status page; matching `\b[A-Z0-9]{16}\b` catches it there, where the label is not on the same line.

**A command's echo is not its effect, even when the echo looks like the file.** `config list-remove` of a list's last element prints `filters:` with nothing after it. The file gets `filters: []`. Those read the same to a human and differently to a YAML parser — null versus an empty sequence — and the difference was written up in contract §5, and into two doc comments and a test name, before anyone looked at the bytes. The rule this project already had for `config set` covers it exactly: **the confirmation is never the evidence, re-read the file.** It just had not occurred to anyone that a command whose output *is* YAML-shaped needed the same suspicion.

**An edit that changed nothing looks exactly like an event that was dropped.** New, and it cost most of an hour on the reconcile toast. The harness proved the *negative* cases — a key no page displays produces no toast — by `sed -i`-ing the sandbox `proxy.yaml` and watching for silence. Three of those seds matched nothing, because a previous run had already set those keys to the values being written, so the file never moved, the monitor was right to say nothing, and the silence read as a broken watch. The file monitor was fine the whole time. **Hash the file either side of the edit and refuse to draw any conclusion from a hash that did not move** — silence only means something once the input is known to have changed. It is the same failure as the one-fixture measurement above, inverted: there the sample was too narrow to support a claim, here it was too stale to support one.

**A leaked `$DISPLAY` fails in the one direction a probe cannot see.** New, and it cost a frame that read as a window which never opened. A GNOME session exports `DISPLAY=:0`; a harness that starts `Xvfb :99` but does not export `DISPLAY=:99` into the app's own environment hands it the *real* display, so the window opens on the desktop through Xwayland while `ffmpeg -i :99` grabs an empty screen. Every AT-SPI assertion still passes, because the accessibility bus is on the session bus and does not care which X server drew anything — so the walk vouches for a run that was never headless at all. `env -u DISPLAY -u WAYLAND_DISPLAY` on the way in, and `export DISPLAY=:99` inside.

**Two things about the headless recipe that the recipe does not say.** `xvfb-run` keeps its `MIT-MAGIC-COOKIE` in a temp directory only its own child can read, so an `ffmpeg -f x11grab -i :99` started from outside fails with `Invalid MIT-MAGIC-COOKIE-1 key` and captures nothing. And the accessibility bus is advertised on the *session* bus, so an AT-SPI probe run outside the `dbus-run-session` never finds the app at all — it fails as "the app never appeared in the tree", which reads like a window that did not open. Start `Xvfb :99` directly and run both the app and the probe inside one `dbus-run-session`; the property the recipe exists for — a private bus, so the new process is unavoidably primary — is preserved either way.

**A measurement taken from one line of output is not a measurement.** Twice in one night: the unlicensed error looked like a single sentence until the real binary showed twenty lines of usage after it, and the `anti_dpi` write looked unverified because a `grep -A 7` window fell short in a file that is half comments. Print the whole thing before building on it.

**Nor is a measurement of one stream.** The same mistake in a new shape, a day later: the initialisation failure above was first written up as "the CLI exits 1 having printed nothing at all", from a probe that captured stderr and discarded stdout (`2>&1 >/dev/null`). It had printed a perfectly good sentence — on the other stream — and the fix that would have been written for a silent failure would have been the wrong fix. Capture both, and say which one carried the message.

**Three nodes carry a switch row's name, and only one of them works.** An `AdwSwitchRow` appears in the tree as the row, its title *label*, and the `GtkSwitch` inside it — all three under the same name. The row reports `n_actions = 0`; the inner switch has a single `toggle`. Taking the first match makes `do_action` raise `No action with index 0` (or, if you reach for the row's own interface, nothing happens at all and the state never moves), which reads exactly like the write path being broken.

**The `n_actions > 0` filter this section used to recommend is not enough**, and it cost a debugging cycle on the assistant's crash-reports switch. The label passes it — measured, it carries eight of them:

```text
candidate: role=label  actions=['clipboard.copy', 'selection.delete', 'clipboard.paste',
                                'link.open', 'clipboard.cut', 'link.copy', 'menu.popup',
                                'selection.select-all']
candidate: role=switch actions=['toggle']
```

Pressing the label does nothing, silently, and the run then looks like a switch that will not write. **Select on the action *name* `toggle`**, not on the action count. Confirm with `get_state_set().contains(Atspi.StateType.CHECKED)` before and after — a press that did not move the state is the thing worth failing on, and it is one line.

**`AdwComboRow` cannot be driven this way at all.** It exposes neither a selection interface nor an action, so a tri-state control's *write* leg is not provable headlessly — only its rendering, by seeding the config and reading the row back. That is what was done for the DNS listen-port row's four states; the write underneath it is covered at the CLI layer instead, by `config_sandbox.rs`.

**`AdwSpinRow` is worse: it does not appear in the tree at all.** Not the row, not its title, not its subtitle. Verified against the *shipped* Advanced page as well as the assistant — "Worker threads" and "Manual proxy ports"' two rows are simply absent from a walk that shows every switch around them. So this section's claim that a walk "gives every row and subtitle of the page" is too strong, and has been since the Advanced page landed: any page with a number row has been half-read this whole time. A missing row is indistinguishable from a row that was never added, which is exactly the wrong failure mode — if you are checking that a number row rendered, take a frame instead (`building.md` §3), because AT-SPI will tell you it is not there either way.

**AT-SPI can drive buttons, not just read them.** `get_action_iface()` plus `Atspi.Action.do_action(iface, 0)` presses a button by name, which is how the whole activation flow was proven headlessly: press *Activate…*, read the link row that appears, press *Finish activation*, read the refusal. The role name to match on is `button`, not `push button`. `scroll_to` and `grab_focus` both fail with a bare `atspi_error` under GTK4, so anything below the fold of an 820×680 window can be read but not photographed.

**A live log-in link deserves the same care as a write.** Pressing *Activate…* under a test harness will hand a real activation URL to whatever handler the session has. Point the `https` scheme at a no-op `.desktop` inside the sandbox `$XDG_DATA_HOME`, with `GTK_USE_PORTAL=0` so the portal does not consult the machine's own defaults instead — otherwise a browser already signed in to AdGuard could bind a device slot to a throwaway install.
