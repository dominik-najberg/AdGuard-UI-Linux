# Overnight plan — closed

> **ARCHIVED, 1 August 2026. Do not execute this file.** Every item in §2 is
> done and committed: the reconcile toast (§2.1), auto mode (§2.2) and custom
> filter removal (§2.3). What §6 predicted would still be open in the morning is
> the one thing that is — the activation success leg — and the certificate work
> it hoped for landed the same day, along with packaging, browser integration
> and the focus-trigger verification. [`handoff.md`](handoff.md) is the current
> state; this file is kept for the two sections that outlived the night.
>
> **§3 and §4 are still in force**, and they are the reason this document was
> not deleted. §3's stop list is the standing set of things nothing here may run
> — `configure` against a populated directory, the mutate suites, `sudo`, an
> exposed `listen_address`, a filter installed into the real catalogue — and §4
> is the verification discipline the project is held to. Both are written as
> rules rather than as tonight's instructions, and they apply to any session.
>
> **§1's hash is stale, and the way it went stale is worth more than the hash
> was.** `proxy.yaml` now reads
> `7b419727afde68a8e09cdc90382915d14daff4159ae2a0c85aa0b300d38af3f5`. The whole
> difference is one line — `proxy_mode: 'manual'` → `'auto'` — and it is the
> **owner's own change**, made through the feature §2.2 built: the root helper
> is installed and running on this machine, which `architecture.md` §6 already
> records. So the discipline held; what failed is pinning a mutable file by
> hash in a document nobody re-pins. **A mismatch is still a hard stop for a
> session that did not expect one**, but diff before concluding anything — the
> file is rewritten by every `adguard-cli` invocation, and the running proxy
> touches its mtime without touching its contents.
>
> **§5 was already history before this banner.** It is the evidence the five
> scope decisions were taken from, and two of its recommendations were
> overturned by measurement. `handoff.md` §2 indexes what was actually decided.

An operational document for an autonomous run with nobody watching. Written for the night of **31 July 2026**, against `5744205`; the previous edition, written against `a9a03ff`, closed everything it planned and was superseded except for §5. This edition did the same, and is superseded by the banner above.

Read [`handoff.md`](handoff.md) for state and [`building.md`](building.md) §3 for the verification recipes.

---

## 1. Ground truth before writing a line — superseded

```bash
git log --oneline -1        # 5744205 or later
git status --porcelain      # must be empty
cargo test --workspace      # 150 pass, 42 ignored
sha256sum ~/.local/share/adguard-cli/proxy.yaml
```

The counts are three sessions out of date — it is **218 passing and 44 ignored** now — and so is the hash below. The *shape* of the check is the part to keep: know what the working tree and the user's real configuration look like before touching either, and re-check after every commit.

The config hash was `c4b58ce8ced6598fa94a5c48faae7bd4ac9695a64a924b3f27263ee7cbce19e3` and is now `7b419727afde68a8e09cdc90382915d14daff4159ae2a0c85aa0b300d38af3f5`, for the one deliberate reason in the banner. **A mismatch is a hard stop** — but a hard stop means stop and diff, not stop and assume the worst. That file is 220 lines, roughly half of them upstream explanatory comments, with no backup and no regeneration path short of `configure`.

The hash held unchanged across four feature sessions before the owner moved it, which is the evidence the sandbox discipline in §3 actually holds rather than merely being written down.

---

## 2. Ordered work — all three done

**Closed on the night, in this order, one commit each: `1310f19` the toast, `57be45b` auto mode, `9391fcb` custom filter removal.** What each of them turned up is in `handoff.md` §3, which is where to read it; this section is left as written so the plan can be compared against what the night actually produced.

Three items, smallest first. Each is a separate commit, pushed before the next is started, so a night that ends early still ends somewhere clean.

### 2.1 The reconcile toast — `architecture.md` §3

The smallest of the three and the last easy v1 item. `reconcile` returns how many *displayed* rows actually differed; a non-zero count raises an `AdwToast`, zero stays silent. The point is not the toast — it is that the count is what stops the app announcing its own writes as somebody else's.

Fix the stderr diagnostic in the same change. It currently claims a change came from "outside the app", which it has no way to know: `Watch::prime` runs once at install and nothing re-primes after our own `config set`, so a user flipping a switch in the UI produces a genuine content change. Re-priming after each write is **not** the fix — our write and the re-prime are not atomic, and losing that race either announces a change that was ours or misses one that was not.

Verifiable headlessly: edit a key the Protection page shows and expect exactly one toast; edit a key no page displays and expect none.

### 2.2 Auto mode — `architecture.md` §6, contract §8

The largest remaining v1 item, and it needs **no privileged component of ours**. AdGuard ships the escalation path and names it itself.

`stat` the helper for the three properties `adguard-cli` checks, and report the check rather than a guess — three separate facts, so a helper that is root-owned but not suid says so. Measured on this machine right now:

```text
~/.local/opt/adguard-cli/adguard_root_helper
-rwxr-xr-x 1 potworny potworny        ->  owned_by_root=false, has_suid=false, is_executable=true
```

So the **unmet** branch is this machine's real state and renders for free; the **met** branch is provable by pointing the check at a fake path, which is why the path must be a parameter and not a constant buried in the function. When unmet, show AdGuard's own command (`sudo <path>/adguard_root_helper -s`) with a copy button and an explanation of what the suid bit grants. Re-check on window focus, so a user who runs it in a terminal sees the row change without hunting for a refresh. When met, `config set proxy_mode auto` — an ordinary unprivileged write through the path every other setting uses.

**Delete `data/io.github.dominik-najberg.AdGuardUI.policy` with this work**, and do not install it. It is scaffolding for a helper that will never exist, and §1 of the previous edition of this file was wrong to describe it as anything else.

Note the first-run assistant deliberately does not offer `proxy_mode`, and `model::SETUP`'s comment says why. This is the change that revisits that decision — revisit it explicitly, in the comment, rather than silently leaving it stale.

### 2.3 Custom filter removal — `architecture.md` §5

Install landed tonight without it, which makes the feature a one-way door: a list added by URL can be switched off but never taken out of the page.

The design question is the whole of it. `filters remove <id>` on a **custom** filter deletes the row outright; the same command against a catalogue filter merely clears `is_installed` and the row stays. That asymmetry is why every switch in this app turns off with `disable`, and why removal wants a confirmation of its own rather than a quiet suffix button. There is no undo but re-fetching the URL.

Verify from the database, as ever — the row is gone or it is not, and `Filter [ID: …] removed` proves nothing.

---

## 3. Stop list

| Do not | Because |
| --- | --- |
| **The activation success leg** (`handoff.md` §3 gap 5) | Needs a real account and **spends a device slot**. Explicitly the owner's call, not an agent's. Do not run `activate` against the licensed install to see what happens either — that is the same decision wearing a different hat |
| `sudo`, `pkexec`, or setting the suid bit on anything | The helper lives in a user-writable directory, so suid-root on it makes anyone who can write that file root. AdGuard's design, the user's decision, taken at a prompt and not behind a button |
| Write `adguard-ui-helper`, or install the `.policy` | §2.2 deletes that file. The one component where a mistake is a local privilege-escalation bug |
| `cargo test --test config_mutate` / `--test filters_mutate` | They mutate the real AdGuard install |
| `adguard-cli configure` against a directory that has a `proxy.yaml` | It resets the user's whole configuration and there is no prompt to decline at with stdin closed. `Cli::configure` guards this; do not add a second call site around the guard |
| Any write to a real `listen_address`, or anything that exposes the proxy | Reachable in one call, and nobody is awake |
| Install a custom filter into the **real** catalogue | New this edition: `filters install` now has a caller. It writes to `agflm_standard.db`, and removal deletes the row outright, so a stray probe is not a no-op. Sandbox it |
| Kill a child's process **group** on timeout | `adguard-cli start` deliberately leaves the daemon behind; `killpg` would take down the proxy the user just started |
| Lower `LOCAL_TIMEOUT` to speed a test up | Every invocation rewrites `proxy.yaml`, and whether that rewrite is atomic is unmeasured. The generous margin over a 10–30 ms operation *is* the mitigation against a `SIGKILL` truncating the file |
| Reformat anything | `cargo fmt --check` has been dirty since the first commit, deliberately — the measured-behaviour tables do not survive rustfmt |
| Resolve a documented contradiction by writing code | Fix the doc, or leave the decision |

Both sandbox suites **are** permitted, and both assert they left the machine alone:

```bash
cargo test -p adguard-core --test config_sandbox -- --ignored --nocapture
```

```bash
cargo test -p adguard-core --test filters_sandbox -- --ignored --nocapture
```

---

## 4. Verification discipline

- **`cargo build` is not verification.** Compilation is a precondition, never evidence.
- Every claim names a command and pastes its output. A claim with no output is reported as **not done** — never as done-but-untested.
- **Measure before coding.** Anything not already in `cli-contract.md` gets sandboxed first, and the measurement goes into the contract *before* the code that depends on it.
- **One sample is not a measurement.** This has now cost three cycles in three different shapes: one line of a twenty-line output, one stream of two, and — tonight — one fixture. The `filters install` content check was written up as "never validates" from a single probe file that happened to open with a line of prose before its HTML. Vary the input, and pin the boundary from both sides.
- Timing assertions are upper-bound only, with generous margin. Never assert a command took *at least* n ms; a loaded machine breaks it, and it can pass for the wrong reason.
- Fixed headless recipe, which makes the new process unavoidably primary so no single-instance handover can quietly leave you screenshotting a resident copy:
  ```bash
  xvfb-run -n 99 -s "-screen 0 1000x1400x24" dbus-run-session -- env GDK_BACKEND=x11 ./target/debug/adguard-ui
  ```
- **Any page can be opened and read headlessly**, which is what makes "the page renders" provable. Find the node with role `list` (not `list box`), `get_selection_iface().select_child(n)`, then walk names. The sidebar is `PAGES` in `main.rs` order and the index is positional.
- **Know what AT-SPI will not show you, because it fails silently in the worst direction.** `AdwSpinRow` is absent from the tree entirely — row, title and subtitle. `AdwComboRow` exposes neither a selection interface nor an action. `AdwEntryRow` appears and its text can be set, but its apply button is not in the tree, `grab_focus` fails with a bare `atspi_error`, and synthetic clicks need extents that come back wrong. A missing row is indistinguishable from a row that was never added. **When checking that one of those rendered, take a frame instead** — a 1000×1400 screen fits a whole page in one grab:
  ```bash
  ffmpeg -f x11grab -video_size 1000x1400 -i :99 -frames:v 1 -y /tmp/shot.png
  ```
- Select on the action **name** `toggle` when driving a switch, never on the action count: the row's title label passes an `n_actions > 0` filter and carries eight actions that press nothing. Confirm with `get_state_set().contains(Atspi.StateType.CHECKED)` before and after.
- **Redact at the harness, on two patterns not one.** An e-mail regex alone is what the last session's own probe used, and it printed a licence key in full — `license` puts the key on the line after the address:
  ```bash
  sed -E -e 's/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/<redacted@e-mail>/g' \
         -e 's/(License key:).*/\1 <redacted>/'
  ```
- Verify config changes by re-reading the file, never from the CLI's confirmation: `Config has been updated` prints for a no-op *and* for a change it silently declined.
- **One CLI call at a time.** Measured tonight: against an already-initialised directory a second invocation does not fail, it *blocks* — `config get` waited 58 s behind an in-flight `filters install` while `status` was unaffected. A test that fires two config-path commands at once will look like a hang, not a race.
- `git status --porcelain` must be free of unintended paths at the end. Stage named paths. This repo has had a subagent leave a scratch test file in the tree before.

---

## 5. Decisions still open

> **All five were settled by the project owner on 30 July 2026.** [`handoff.md`](handoff.md) §2 indexes the answers and points at the doc that now owns each one. What follows is left unedited as the evidence the decisions were taken from — **two of its recommendations were overturned**, so read it as history, not advice.
>
> Items 2 and 3 were the ones that changed, and for the same reason in both cases: reading the `adguard-cli` binary's strings contradicted what had been inferred without it. Activation is **in**, as a user-driven finish button rather than a poll. Auto mode needs **no helper of ours at all** — AdGuard ships `sudo <path>/adguard_root_helper -s` and checks `owned_by_root`/`has_suid`/`is_executable` itself, which contract §8 had concluded did not exist. Item 5 also went the other way from the recommendation below: a toast, gated on a displayed row actually having moved.

Not to be invented by an agent. Each blocks the item beside it.

1. **Is a Userscripts page in v1?** `handoff.md` §1 and `architecture.md` §5 both list it; §7's "In" list does not. Recommendation: **out** — §7 is the scope authority, and §7 already pushes userscript *installation* to v2, which leaves a read-only list of thin value.
2. **Is licence activation in v1?** §5 specifies it and gap 1 depends on it; §7's "In" list does not name it. Recommendation: **split** — the error mapping is in (item 2 above), activation itself is out. Two measured complications make the §5 design unsafe to code blind: `license` is itself licence-gated, so while unlicensed the poll condition is "stops returning unlicensed" rather than "returns `APP_ACTIVE`"; and the CLI's own no-TTY message says to run `activate` *again* to complete, which suggests polling `license` alone may never flip.
3. **Auto mode — worth a setuid-adjacent helper at all?** There is a smaller version: ship no helper, have the GUI display AdGuard's own recommended privileged command with an explanation, detect the resulting state, then perform the unprivileged mode switch. Most of the value, no new root attack surface, and fully verifiable headlessly.
4. **May the DNS page write `dns_filtering.listen_port`?** Writing a port starts a listener nobody asked for; leaving it read-only means the inert-DNS caveat Protection shows has no cure anywhere in the app.
5. **Silent reconcile, or a toast?** A user who edited in a terminal is otherwise unsure the UI noticed. Recommendation: silent.

---

## 6. What one night actually closes

§2.1 and §2.2 plausibly, which between them would leave **auto mode and the reconcile toast both done and v1 complete but for custom filter removal**. §2.3 is the stretch, and it is the one carrying a design decision rather than a specification, so it is last on purpose: a removal affordance built at 4 a.m. against nobody's opinion is the kind of thing that gets reverted.

What will still be open in the morning regardless: the activation success leg, which §3 forbids and which is the owner's to decide, and the CA trust detection in `handoff.md` §3 gap 4 — the same detect-then-instruct shape as auto mode, and worth doing alongside it if §2.2 lands early.

**Leave a note at the top of `handoff.md` saying where the night stopped and what the next thing is.** A run that ends mid-item and says so is worth more than one that ends tidily and does not.
