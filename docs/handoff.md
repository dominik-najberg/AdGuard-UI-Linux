# Handoff

The entry point for a new session on this project. **§0 is what to run and read before touching anything**; §1 is what exists, §2 what was decided, §3 what is still open, §4 what will bite you. Everything here is current as of **2 August 2026**, against **`v1.0.0`**, and measured on the machine §0 describes.

---

## 0. Starting a new thread

### The state of play, in three sentences

**v1 is complete** (`architecture.md` §7), and so are the three checks added after it closed — certificate trust, the root helper, and browser integration — plus packaging. **v1.0.0 is released**: tagged, with both packages and their checksums attached to the GitHub release by [`.github/workflows/release.yml`](../.github/workflows/release.yml), and `CHANGELOG.md` is what its notes are read from — `building.md` §5, *Cutting a release*, is the procedure. **218 tests pass and 44 are `#[ignore]`d**, and every page can be opened and read without a display, so "it renders" is provable here rather than assertable — within the limits §4 sets out, the sharpest being that an `AdwSpinRow` is absent from the accessibility tree entirely, so any page with a number row is only half-read by a walk. **v2 is open and now scoped**, both by the owner's decision on 2 August 2026: [`v2-plan.md`](v2-plan.md) is live and is the first plan file to be so since `overnight-plan.md` was archived, and its first task — the scope decision `architecture.md` §7 had never carried — was taken the same day. **§7 is retitled *Scope* and has a *v2 — open* half: in are HAR capture, full advanced-settings parity and import/export; out are live stats (its own milestone, behind a spike), userscripts (re-checked 2 August 2026, unchanged) and the `speed` UI (unmeasured).** **No v2 code exists**, and §7 is scope rather than a queue — the next session picks an item and starts it the way every v1 feature was started, measurements into the contract first.

**The repository is public**, as of 2 August 2026 and by the owner's decision — 1.0.0 was tagged the day before, while it was still private. Both halves of that mattered and one of them is now a thing to remember rather than a thing to check: the release is downloadable by anyone, verified by fetching the `.deb` and its `SHA256SUMS` with no credentials at all and checking one against the other; and the three GitHub URLs in the AppStream file now resolve, which took `appstreamcli validate` from three `url-not-reachable` warnings to none. The single warning left is `cid-rdns-contains-hyphen`, which is the component ID itself — it is also the GApplication ID and the stem of every installed icon, desktop and metainfo path, so it is a rename of fourteen files and not a release note.

**What that changes for a session, and it is the only thing:** everything written here is now public, including this file. It has always been written as though it were — the measurements, the mistakes and the traps are the useful part of it — and the one category that was ever sensitive is already handled: no licence key or owner address appears anywhere in the tree or in the history (every occurrence is a fixture, `ABCDEFGH12345678` and `someone@example.com`), and the screenshots carry `user@example.com`, a masked key and a TEST-NET-3 address in place of this machine's own. Checked against the whole history before the visibility was flipped, not assumed. Keep it that way: §4's redaction rule was about pasting a walk into a terminal, and it now covers pasting one into a commit.

### Ground truth before writing a line

```bash
git log --oneline -1        # the v1.0.0 tag or later
git describe --tags         # v1.0.0, or v1.0.0-<n>-g<sha> once work has landed on top
git status --porcelain      # must be empty
cargo test --workspace      # 218 pass, 44 ignored
sha256sum ~/.local/share/adguard-cli/proxy.yaml
```

The config hash is `7b419727afde68a8e09cdc90382915d14daff4159ae2a0c85aa0b300d38af3f5`, and that file is 220 lines with no backup and no regeneration path short of `configure`. **A mismatch is a stop — and then a diff, not a conclusion.** The last one was a single line, `proxy_mode: 'manual'` → `'auto'`, changed by the owner on purpose through the feature that exists for it. Every `adguard-cli` invocation rewrites the file, and the running proxy moves its mtime without moving a byte, so neither a fresh timestamp nor a moved hash means what it looks like until it has been diffed against a known copy.

Worth knowing before anything else surprises you — the proxy is very likely running, and not because a session started it:

```bash
ps -eo pid,lstart,cmd | grep -E "adguard-cli start|adguard_root_helper|adguard_cli_nm" | grep -v grep
```

### The machine this was all measured on

| Fact | State here | What that costs a test |
| --- | --- | --- |
| `proxy_mode` | `auto`, root helper installed and running | The helper's **unmet** rendering is unreachable locally; `$ADGUARD_ROOT_HELPER` is the only route to it |
| Certificate | Trusted, and `adguard.conf` reproduces the same CA | Unmet branches need `$ADGUARD_CA_BUNDLE`, `$SYSTEM_CERT_DIR`, `$ADGUARD_CERT_INSTALLER` |
| Browser integration | Installed for five browsers; no Firefox profile, so Firefox is not reported | Unmet branches need `$ADGUARD_BROWSER_HOME` |
| Licence | Active | A Status-page walk carries the owner's e-mail **and** key. Redact at the harness, on both patterns — §4 |

The pattern is worth naming, because it is the reason every one of those paths is a parameter rather than a constant: **every check this application renders is in its met state on this machine**, so each one's interesting branch is reachable only through an override. A test that hard-codes a path is a test that can only ever see the boring answer.

### Read in this order

1. [`cli-contract.md`](cli-contract.md) — **measured** CLI behaviour, which the code depends on. §5 for anything touching config, §6 for filters, §7 for what needs a TTY, §8 for privilege, §12 for browser integration.
2. [`architecture.md`](architecture.md) — §3 for refresh and reconcile, §4 for the process, tray and startup, §5 for the pages, §6 for detect-and-instruct, §7 for scope.
3. [`building.md`](building.md) §3 — every verification recipe, including the headless ones and the focus round trip.
4. **§4 of this file.** It is the longest section here and the one that saves the most time; most of it was paid for once already.

### The three ways to run it

```bash
cargo run -p adguard-gui                                    # the real config, read-write
XDG_DATA_HOME=/tmp/fake cargo run -p adguard-gui            # a sandbox config; writes land there
env -u DISPLAY -u WAYLAND_DISPLAY xvfb-run -n 99 -s "-screen 0 1000x1400x24" \
  dbus-run-session -- env GDK_BACKEND=x11 ./target/debug/adguard-ui   # headless, and driveable
```

One binary, `adguard-ui`, serves the window and the tray; `adguard-tray` is a library. The second line is how a page is seen against a configuration nobody would create on purpose, and the third is how anything gets *proved* — `building.md` §3 has the AT-SPI walk, the frame grab and the focus round trip that go with it. The overrides in the table above compose with all three.

**The third line is for looking, not for driving.** `xvfb-run` keeps its `MIT-MAGIC-COOKIE` in a directory only its own child can read, so an `ffmpeg -i :99` or an AT-SPI probe started from outside it finds nothing and fails as though the window never opened. A run you intend to *drive* starts `Xvfb :99` directly and puts the app and the probe inside one `dbus-run-session` — §4, and `building.md` §3.

### What to do next

v2 is scoped, so this is now a pickup rather than a decision — but **the scope is not a queue**, and no v2 code exists. In the order I would consider them:

- **Check the issues**, which is the one genuinely new category of work since 1.0.0: reports from people whose machines are not this one. Every measurement in `cli-contract.md` was taken against `adguard-cli` 1.4.13 on Ubuntu 26.04 with GNOME 50 on Wayland, and the first bug report from anywhere else is worth more than any of the v2 backlog — it is the only way to find out which of this project's constants were facts about the CLI and which were facts about this machine. `v2-plan.md` §5. **Checked 2 August 2026: none.** No issues, no open PRs, no stars — the category is real and it is empty, so there is nothing to weigh the backlog against yet. It is two commands (`gh issue list --state all`, `gh pr list --state all`) and it goes first every session, because a report arriving is the one thing that reorders everything below it.
- **Pick a v2 item**, from `architecture.md` §7's *v2 — open* half. Cheapest first, which is also the order §7 reasons in: **HAR capture** is an Advanced-page group over two keys and its one real design question is the subtitle that names what leaving it on costs; **advanced parity** begins with an enumeration nobody has written down, walking `proxy.yaml`'s keys against what Advanced and Stealth render, and that enumeration is not code; **import/export** is the largest, because `import-settings` is the only thing besides `configure` that can create `proxy.yaml` and therefore collides head-on with the first-run assistant — that interaction gets designed before either half is built. `v2-plan.md` §3 is the working behind all three.
- **§3 item 6, the activation success leg** — the only functional gap left, and **the owner's call, not an agent's**: it needs a real account and spends a device slot. Opening v2 does not open it, and **the owner was asked again on 2 August 2026, alongside the v2 scope decision, and left it open.** So it is not an unanswered question any more; it is an answered one, and the answer is *not yet*. Do not re-ask it every session — raise it only if something changes what it costs.
- **Stop** is still a legitimate outcome. v1 and the three post-v1 checks are done, verified and released; a session that adds nothing to a project in this state has not failed at anything.
- **Both of the things nobody had decided were settled on 1 August 2026**, by the project owner, and neither is open any more.

  **`README.md` is written**, with seven screenshots in `docs/screenshots/` captured by the recipe now in `building.md` §3. Two things to know before regenerating any of them: the Status and Stealth frames carry **placeholder** values repainted over this machine's own, and the unmet-prerequisite frame is a real rendering against sandbox paths, which the README says plainly rather than leaving a reader to assume this machine is misconfigured. `LICENSE` is in too — the verbatim GPLv3, byte-identical to `/usr/share/common-licenses/GPL-3`, which the tarball now ships and the `.deb` still points at rather than duplicating (`building.md` §5).

  **CI is `.github/workflows/ci.yml`**: build and the default test suite, on push to `main` and on pull requests. No `fmt`, no clippy, never `--ignored` — the file explains each omission so none of them reads as an oversight. It runs in an `ubuntu:26.04` container because `ubuntu-latest` ships libadwaita 1.5 and this workspace needs 1.7.

  **It found two failures on its first clean run, which is the whole argument for it.** A container runs as root, so a file a test writes is root-owned — and `helper.rs` has two cases whose entire premise is a file that is *not*. On a developer's machine that premise is true by construction, so nothing local could have caught it. Both now skip at `geteuid() == 0`, the same way their neighbours skip when `/bin/ls` is absent. The suite is still 218 passing here; in CI, two of them assert nothing and say so.

### What no session does without being asked

The full list is `overnight-plan.md` §3, which is archived but still in force, and §4 there is the verification discipline. The three that would do real damage:

- **`adguard-cli configure` against a directory that already has a `proxy.yaml`.** It resets the user's whole configuration and there is no prompt to decline at with stdin closed. `Cli::configure` guards this; do not add a second call site around the guard.
- **`sudo`, `pkexec`, or a suid bit on anything.** This application ships no privileged component and there is a section explaining why (`architecture.md` §6).
- **`cargo test --test config_mutate` / `--test filters_mutate`, or a filter installed into the real catalogue.** They drive the user's real install. Sandbox everything: `Cli::with_xdg_data_home`, and §4 below.

---

**What the last sessions found, and it is still true.** Kept because each of these cost something to learn and none of it is recoverable from the code:

> **The `connect_is_active_notify` trigger is no longer unverified.** Two
> entries in §3 used to end "what is untested is the trigger, not the
> re-check", on the grounds that taking focus from an Xvfb window needs
> `xdotool` and there is none here. That was wrong, and it stood for three
> features and about a week. There is no window manager on the Xvfb display, so
> there is nothing to negotiate with: `XSetInputFocus` is one call, `libX11` is
> installed, and twenty lines of C do it. The recipe is `building.md` §3, and
> the whole of §3's "not verified" wording is gone rather than softened.
>
> Two things from that verification worth carrying forward, both about the
> shape of the proof rather than the result:
>
> - **A phase that must change nothing has to come first, or the run proves
>   nothing.** Write the manifest, walk the page, assert the walk is
>   *identical*; only then take focus and assert it is not. Without that middle
>   phase a passing run is equally consistent with a 2 s poll having noticed —
>   and this application has three of those. It is §4's rule about hashing
>   `proxy.yaml` either side of an edit, inverted: there silence had to be shown
>   to mean something, here a change had to be shown to have a cause.
> - **Drive it in both directions.** Met → unmet is the direction the feature
>   exists for and the one a single run is tempting to stop at. The reverse —
>   a browser appearing on disk mid-session, the group coming *back* naming it
>   — is the ordering trap contract §12 describes, and it is the case with no
>   diagnostic anywhere else in AdGuard's tooling.
>
> Five things from the session that built the certificate check, still true:
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
> Three from the overnight run of 31 July, still true:
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

**How it got here**, for anyone reading the commit log and wondering what the shape of a session on this project is. The overnight run of 31 July closed the config monitor, the CLI timeout, the lapsed-licence mapping, the Stealth page and the `dns_filtering` dependency caveat; the session after it built **licence activation**, then the **DNS page**, then the **first-run assistant** — the first page here that exists for a machine where AdGuard has never been configured at all — then **custom filter install by URL**, then **certificate trust** and **packaging**, then **browser integration**, which is the only check in this application whose answer can be invalidated by something that has nothing to do with AdGuard, and last the **focus-trigger verification** in §3 item 5. One feature per session, each with its measurements written into the contract before the code that depends on them.

**If you are touching the filter pages**, the fact in contract §6 most likely to change a decision is this: **AdGuard checks only whether what it downloaded *begins* with HTML.** That catches a link answering 200 with an error page, and nothing else. JSON, prose, the wrong plain-text file and an empty response all install as filter lists holding no rules, report success, and leave a switch reading *on* over something that filters nothing. The Filters page says so in the group description because no other part of this UI ever could.

That subsection also carries the correction worth reading before trusting anything else in it: an earlier revision of it said content was *never* validated, generalised from a single probe file that happened to open with a line of prose before its HTML. The reasoning was fine and the fixture was not — the same lesson §3 already records about measuring one line and one stream, arriving a third time as one sample. A test caught it, which is the argument for `filters_sandbox.rs` pinning both sides of the boundary.

---

## 1. Where things stand

**218 tests pass by default** and 44 more are `#[ignore]`d.

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
| Root helper | Done. `gui/root_helper.rs`, one widget behind two screens — the Advanced page under the mode row it gates, and the first-run assistant, because every install this app completes ends unmet. The Status page carries the symptom instead, as one line under the HTTP endpoint, re-read on its existing 2 s poll rather than on focus. `architecture.md` §6. |
| Browser integration | Done. Six manifest locations, four states — ready, missing, stale, unreadable — on the Protection page below the certificate group, with `install-browser-integration` and a copy button. Browsers that are not installed are not reported; the command is withheld when `adguard_cli_nm` is absent. `architecture.md` §6, contract §12. |
| Packaging | Done. `make deb`, `make tarball`, `make package`. Neither needs root; `Depends:` is derived by `dpkg-shlibdeps` rather than written down. `building.md` §5. |
| Release | Done, at **1.0.0**. A `v*` tag builds both packages in an `ubuntu:26.04` container, checksums them and attaches them to a GitHub release whose notes are the matching `CHANGELOG.md` section; the workflow refuses a tag that disagrees with `Cargo.toml`. `workflow_dispatch` runs the build and stops before publishing, which is how it is exercised without tagging. `building.md` §5. |

Userscripts are **out of v1 and out of v2** — `architecture.md` §7 has the reasoning for both, and the v2 half carries the date of the re-check that confirmed it (2 August 2026: still one script, `adguard-extra`, still pre-enabled).

The three ways to run it are in §0. Seeing the **tray icon** needs a real session plus an AppIndicator extension, which no headless recipe supplies — `building.md` §2 has that one.

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

   The re-check is verified by repointing the helper mid-session and provoking a repaint, which takes the group off the page. The `connect_is_active_notify` trigger above it is verified too, as of 1 August — item 5. Note this machine has since run the `sudo`, so the *unmet* rendering is now the one nothing local reaches; `$ADGUARD_ROOT_HELPER` is the only route to it.
3. ~~**Reconcile toast**~~ — done, `architecture.md` §3. Left here for the one thing it found: the suppression of our own writes comes from the per-row `pending` flag, not from counting, so any figure rendered *outside* the page that writes it has no such flag and will announce the user's own click back at them. The Status module count is that figure, and it is now repainted without being counted. The stderr line no longer claims the change came from "outside the app"; it reports the count instead, which is a fact it has.
4. ~~**The certificate is seeded but not trusted.**~~ — done, `architecture.md` §6. Detect, then show AdGuard's own `install_cert.sh`; nothing here installs anything. Four things it turned up are worth keeping:

   - **The trusted bundle has no names in it.** A `grep` for the certificate's name returns nothing whether or not it is trusted, so membership is decided on the base64 body. Measured against a machine where it *is* installed.
   - **The installer's idempotence check is on the path, not the contents** (`[ ! -f "${SYSTEM_CERT_PATH}" ]`), so a regenerated CA leaves the old one in place and re-running reports success. That is a state its own tooling will not repair, and the reason the check compares bytes rather than asking whether a file exists.
   - **`configure` reproduces the CA from `adguard.conf` rather than generating one** — byte-identical, weeks-old dates. That file carries the private key of a CA this system trusts, which makes `building.md` §3's "delete the sandbox afterwards" rather more than housekeeping.
   - **The met branch is this machine's real state**, the mirror of the root helper, so every *unmet* branch needed the paths to be parameters. `$SYSTEM_CERT_DIR` is AdGuard's own variable; `$ADGUARD_CA_BUNDLE` and `$ADGUARD_CERT_INSTALLER` are ours and exist only so those branches can be reached without touching the real trust store.

5. ~~**The focus trigger has never been exercised.**~~ — done, 1 August, and it had been open since the root helper landed. All three checks above re-read themselves from one `connect_is_active_notify` closure in `main.rs`, and every entry here used to end by excusing it: no `xdotool`, no `wmctrl`, no way to take focus from an Xvfb window and give it back. **The excuse was wrong.** There is no window manager on that display, so there is nothing to negotiate with — `XSetInputFocus` is one call and `libX11` is installed. Twenty lines of C, in `building.md` §3. Three things it settled:

   - **The trigger fires.** Driven through the browser check, whose entire input is files under `$ADGUARD_BROWSER_HOME`: a sandbox `$HOME` holding `.config/chromium` and no manifest renders the group naming the file it looked for; writing a valid manifest changes nothing; `xfocus none` then `xfocus <window-id>` takes the group off the page. Since the closure calls all three re-checks unconditionally, this is the trigger for the certificate and the helper as well.
   - **The middle phase is the proof.** A run that writes the file and immediately takes focus demonstrates only that the rows *can* change — a 2 s poll would pass it identically, and this app has three of those. The walk taken after the write and before the focus round trip must be **byte-identical** to the one before it, and it was. Same discipline as hashing `proxy.yaml` either side of an edit (§4), pointed the other way: there silence had to be shown to mean something, here a change had to be shown to have a cause.
   - **Both directions, because the second one is the feature.** Reversed — one browser set up, group hidden, then `.config/vivaldi` created mid-session — the group comes *back*, naming Vivaldi and the manifest it looked for. That is contract §12's ordering trap driven end to end: a browser installed after `install-browser-integration` last ran, which the installer's own success message hides and which nothing else in AdGuard's tooling reports.

   What is still **not** verified anywhere is the window regaining focus by the route a user would take, since that needs a window manager. What the focus round trip proves is that GTK's `is-active` moving runs the handler, which is the line that was in question.
6. **The activation success leg is a claim, not a measurement.** Everything up to the browser log-in is proven, including against a real unlicensed install: `activate` hands back a link, the page shows it, *finish activation* re-runs `activate`, reads `license`, and says "not activated yet" without pretending otherwise. What nobody has watched is the leg after a genuine log-in — it needs a real account, and completing an activation spends a device slot. **This is the owner's call, not an agent's.** Two things go with it: what `activate` prints against an install that is *already* licensed is unmeasured for the same reason, and the "AdGuard is activated" wording has never been seen on screen.
7. **Three CLI behaviours the UI depends on are deliberately unmeasured**, and this is the list, because they are scattered across the contract and each looks like an oversight where it sits. None is a gap an agent should close; each would cost more than it settles.

   - **What `adguard-cli cert` does to a CA that already exists** (contract §8). The command installs into the **system** trust store, which is a machine-wide change no test here is entitled to make. The UI therefore only ever *names* it, in AdGuard's own words, and never says what it will do — that wording is load-bearing and should not be "improved" into a description.
   - **`configure`'s second-run branch** (contract §7). Against a directory that already has a `proxy.yaml` it announces that the configuration will be reset, and with stdin closed there is no prompt at which to decline. `Cli::configure` guards the call; the branch behind the guard stays unmeasured on purpose.
   - **Where `proxy_mode auto` is actually validated** (contract §8). Reaching the check needs `start`, and neither route is open: a sandbox is unlicensed so `start` is refused first, and starting the real proxy in `auto` is the owner's call. The contract says validation-at-use is an *inference* from where the strings sit in the binary, and it should keep saying so.

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

**Two commands this app shows and nothing here may run.** `adguard-cli cert` and `install_cert.sh` both install into the **system** trust store, which is a machine-wide change no test is entitled to make — so neither is measured, and contract §8 says so rather than leaving the gap to be discovered. The consequence for anyone extending the certificate work: what `cert` does to a CA that already exists is unknown, and the UI must keep naming it in AdGuard's own words rather than describing what it will do. The same rule as the root helper's `sudo` line, which has never been run here either.

**Formatting.** The tree is hand-formatted and `cargo fmt --check` has been dirty since the first commit. That is deliberate — the measured-behaviour tables in `config.rs`, `cli.rs` and `model.rs` do not survive rustfmt. Do not reformat.

**Screenshots.** GNOME denies D-Bus screenshots, and `x11grab` on `:0` captures nothing under Wayland because Xwayland windows are not drawn into the X root window. Use Xvfb; the recipe is in `building.md`. There is no `xdotool`, so the virtual screen has to be taller than the window to get a whole page in one frame.

**"The tool is not installed" is a statement about the tool, not about the task.** New, and it had been quietly costing this project a verified line since the root helper landed: three separate entries in §3 excused the focus trigger on the grounds that focus needs `xdotool`, and each new feature inherited the excuse from the one before it rather than re-examining it. `xdotool` exists to talk to a *window manager*, and there is no window manager on an Xvfb display — the thing standing in the way was the reason it was thought impossible. `XSetInputFocus` is one X call, `libX11` is installed, and `cc -o xfocus xfocus.c -lX11` is the whole build (`building.md` §3). Before writing that something cannot be verified here, check whether the missing tool was ever the one the job needed; a note like that gets copied forward and stops being questioned.

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
