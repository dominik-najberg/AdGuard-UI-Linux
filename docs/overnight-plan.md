# Overnight plan

An operational document for an autonomous run with nobody watching. Written against `a9a03ff`, from a ten-agent scoping pass over every remaining v1 item plus two adversarial review lenses (unattended safety, and verifiability).

Read [`handoff.md`](handoff.md) for state and [`building.md`](building.md) §3 for the verification recipes. This file says only what to do tonight, in what order, and what not to touch.

---

## 1. Ground truth before writing a line

```bash
git log --oneline -1        # a9a03ff or later
git status --porcelain      # must be empty
cargo test --workspace      # 95 pass, 23 ignored
sha256sum ~/.local/share/adguard-cli/proxy.yaml
```

The config hash is `c4b58ce8ced6598fa94a5c48faae7bd4ac9695a64a924b3f27263ee7cbce19e3`. Re-check it after every commit. **A mismatch is a hard stop.** That file is 220 lines, roughly half of them upstream explanatory comments, with no backup and no regeneration path short of `configure`.

Two facts a planning pass got wrong and that are worth stating, because they were asserted confidently before being checked:

- `data/` already contains `io.github.dominik-najberg.AdGuardUI.policy` and `…metainfo.xml`. The polkit action file exists as commented scaffolding for three actions; only the helper binary is missing.
- The pipe-inheritance hazard behind `a9a03ff` is real and measured, not theoretical: `sh -c "sleep 10 & echo done"` exits at once and a reader waiting for EOF sits for the full ten seconds.

---

## 2. Ordered work

Done already, this session:

| | |
| --- | --- |
| `5ebe8d7` | Every CLI invocation is bounded in time — closes the `cli.rs` TODO and handoff §3 gap 2 |
| `a9a03ff` | That bound can no longer hang on a descendant holding the pipe |
| *next* | The stealth sub-page — 26 settings, keys proven against the real file |
| `02857ad` | The lapsed-licence error mapping — proven against an unlicensed sandbox |
| `3e52fc7` | The `proxy.yaml` config monitor — acceptance test met: 40 s idle with the mtime moving produces zero reconciles, an edit produces exactly one, a bare `touch` produces none |

Remaining, in order. Each lands as its own commit with its own proof.

**1. The `dns_filtering` dependency for `encrypted_client_hello` and `filter_secure_dns_mode`** — gap 5, ~40 lines. Same shape as the caveat Protection already renders.

Anything past here will not be reached, and that is the correct outcome rather than a shortfall.

---

## 3. Stop list

| Do not | Because |
| --- | --- |
| `sudo`, `pkexec`, or installing the `.policy` | Needs root; the helper it names does not exist, so installing grants nothing |
| Write `adguard-ui-helper` | The one component where a mistake is a local privilege-escalation bug. Not to be written and merged unreviewed |
| `cargo test --test config_mutate` / `--test filters_mutate` | They mutate the real AdGuard install |
| `adguard-cli configure` | Interactive, and it rewrites the config wholesale |
| Any write to a real `listen_address`, or anything that exposes the proxy | Reachable in one call, and nobody is awake |
| `pkill adguard-ui` | Nothing is resident tonight, so it can only ever kill something unexpected. A private bus makes it unnecessary |
| Kill a child's process **group** on timeout | `adguard-cli start` deliberately leaves the daemon behind; `killpg` would take down the proxy the user just started |
| Lower `LOCAL_TIMEOUT` to speed a test up | Every invocation rewrites `proxy.yaml`, and whether that rewrite is atomic is unmeasured. The generous margin over a 10–30 ms operation *is* the mitigation against a `SIGKILL` truncating the file |
| Resolve a documented contradiction by writing code | Fix the doc, or leave the decision. See §5 |

`cargo test --test config_sandbox -- --ignored` **is** permitted: `building.md` certifies it and the suite itself asserts the machine's `proxy.yaml` is byte-identical afterwards.

---

## 4. Verification discipline

- **`cargo build` is not verification.** Compilation is a precondition, never evidence.
- Every claim names a command and pastes its output. A claim with no output is reported as **not done** — never as done-but-untested.
- Timing assertions are upper-bound only, with generous margin. Never assert a command took *at least* n ms; a loaded machine breaks it, and it can pass for the wrong reason.
- Fixed headless recipe, which makes the new process unavoidably primary so no single-instance handover can quietly leave you screenshotting a resident copy:
  ```bash
  xvfb-run -n 99 -s "-screen 0 1000x1400x24" dbus-run-session -- env GDK_BACKEND=x11 ./target/debug/adguard-ui
  ```
- **Prefer D-Bus oracles to pixels.** The tray's dbusmenu returns the six Protection toggles with machine-readable state (`GetLayout 0 3 '[]'` — depth `3`, not `-1`, because `gdbus` eats a leading dash). A screenshot is bounded by the viewport and cannot be diffed.
- **Know what is invisible and say so.** Only the visible `GtkStack` child appears in the accessibility tree, there is no window manager under Xvfb and no `xdotool`, so no page but Status can be brought on screen. Until that changes, "the page renders" is a claim, not a proof — say which it is.
- Measured, refining the above: the AT-SPI tree *is* reachable (133 nodes, every visible `AdwActionRow` named). Two mechanisms were tried and both failed — `do_action` finds only a label's `clipboard.copy`, and the sidebar exposes no selection interface. The cause is that the five navigation rows have **no accessible name** (handoff §3 gap 4). Fix that and page-by-page verification opens up.
- Verify config changes by re-reading the file, never from the CLI's confirmation: `Config has been updated` prints for a no-op *and* for a change it silently declined.
- `git status --porcelain` must be free of unintended paths at the end. Stage named paths. This repo has had a subagent leave a scratch test file in the tree before.

---

## 5. Decisions still open

Not to be invented by an agent. Each blocks the item beside it.

1. **Is a Userscripts page in v1?** `handoff.md` §1 and `architecture.md` §5 both list it; §7's "In" list does not. Recommendation: **out** — §7 is the scope authority, and §7 already pushes userscript *installation* to v2, which leaves a read-only list of thin value.
2. **Is licence activation in v1?** §5 specifies it and gap 1 depends on it; §7's "In" list does not name it. Recommendation: **split** — the error mapping is in (item 2 above), activation itself is out. Two measured complications make the §5 design unsafe to code blind: `license` is itself licence-gated, so while unlicensed the poll condition is "stops returning unlicensed" rather than "returns `APP_ACTIVE`"; and the CLI's own no-TTY message says to run `activate` *again* to complete, which suggests polling `license` alone may never flip.
3. **Auto mode — worth a setuid-adjacent helper at all?** There is a smaller version: ship no helper, have the GUI display AdGuard's own recommended privileged command with an explanation, detect the resulting state, then perform the unprivileged mode switch. Most of the value, no new root attack surface, and fully verifiable headlessly.
4. **May the DNS page write `dns_filtering.listen_port`?** Writing a port starts a listener nobody asked for; leaving it read-only means the inert-DNS caveat Protection shows has no cure anywhere in the app.
5. **Silent reconcile, or a toast?** A user who edited in a terminal is otherwise unsure the UI noticed. Recommendation: silent.

---

## 6. What one night actually closes

Items 1 and 2 above, plausibly 3 and 4. Roughly 500 lines of verifiable progress, no root, no network, no licence-gated commands, no writes to the real configuration.

The DNS page, the first-run assistant, custom filter install and auto mode will all still be open in the morning. Four of those five cannot be honestly verified without a human at a real session, so attempting them overnight buys unverified code rather than progress.
