# v2 plan

**Live as of 2 August 2026, against `v1.0.0`.** This is the queued work: the
owner opened v2 the day 1.0.0 was released and the repository went public.
[`handoff.md`](handoff.md) is still the state of the project and §0 there is
still the entry point, and this is still the first plan file to be live since
[`overnight-plan.md`](overnight-plan.md) was archived.

**What this file no longer is: the answer to what v2 contains.** It said so
until its own §2 was carried out, later the same day —
[`architecture.md`](architecture.md) §7 now has a *v2 — open* half and is the
scope authority for both milestones. This file is the reasoning behind that
decision and the standing rules for working under it; §7 is what v2 *is*. Where
they disagree, §7 wins.

**It deliberately pins nothing that moves.** No test count, no `proxy.yaml`
hash, no commit sha. The archived plan carried all three and all three went
stale within days; its own banner says the way it went stale was worth more than
the hash was. Ground truth lives in `handoff.md` §0, which is re-read at the
start of every session, and this file points at it rather than copying it.

---

## 1. Before writing a line

`handoff.md` §0 — the tag check, the clean tree, the default test suite, and the
`proxy.yaml` hash. Then §0's reading order. A hash mismatch is a stop and then a
**diff**, not a conclusion.

Nothing in this document changes any of that, and nothing in it licenses a
shortcut through it.

---

## 2. ~~The first task is not code~~ — done, 2 August 2026

**Answered.** [`architecture.md`](architecture.md) §7 now carries a *v2 — open*
half, it is retitled *Scope*, and the decision in it is the project owner's,
taken 2 August 2026. **v2 = HAR capture + full advanced-settings parity +
import/export**; live stats is its own milestone behind a spike, userscripts is
out with the date of its re-check, and the `speed` UI is out until it is
measured. §7 is the authority; §3 below is the input that fed it and is kept for
the reasoning, not as a live list.

The section is left here because the *rule* it states outlives the task, and it
is the one most likely to be skipped by the next session doing everything else
right:

- The project's convention is decision-first, and `handoff.md` §1 describes v1 as
  one feature per session "each with its measurements written into the contract
  before the code that depends on them". `overnight-plan.md` §4 states the same
  rule as a rule. Scope is the same shape of thing one level up.
- **Four of the six items were out for reasons that were still on the page.** A
  session that starts implementing has not reopened those reasons; it has
  skipped them. Read the reason, then either overturn it in writing or leave the
  item out.

One of the three that moved in required exactly that, and it is the worked
example: **HAR capture was out because contract §9 calls full HAR dumps too
heavy for an always-on UI.** That reason is not withdrawn — it is answered, in
§7, on the grounds that `har_writer.enabled` ships `false` and the switch is not
always-on, with the cost owed to the row's subtitle. The other two carried no
stated reason and needed none.

**What is still true here: no v2 code has been written.** §7 is scope, not a
queue. The next session picks an item and starts it the way every v1 feature was
started — measurements into the contract first.

---

## 3. The backlog, with what is already known

Ordered by cost, cheapest first. **This section has been consumed**: it was the
input to §2, that decision is taken, and [`architecture.md`](architecture.md) §7
is now the authority for what is in and out. What is kept here is the working —
the measurements and the trade-offs behind each item — which §7 states more
briefly than it reasons. Where the two ever disagree, §7 wins.

### 3.1 HAR capture

Two keys, `har_writer.enabled` and `location`, written through the same
`config set` path as every other switch in the application. No new machinery at
all; it is an Advanced-page group.

The decision is not how to build it but whether to offer it: contract §9 calls
full HAR dumps too heavy for an always-on UI, and a switch that degrades the
machine when left on is a switch that needs its subtitle to say so. `har_writer`
is already one of the foldable sections `config show` collapses
([`adguard-cli.md`](adguard-cli.md)), so the keys are reachable and typed like
every other pair.

### 3.2 Full advanced-settings parity

More rows on a table-driven page — no new machinery either. **Nobody has written
down what is actually missing**, and that enumeration is the whole of the work
before the rows: walk `proxy.yaml`'s keys against what the Advanced and Stealth
pages render, and the gap is the specification.

Expect the answer to be smaller than "parity" sounds, and expect some of it to
be keys that should stay unrendered. Contract §5 records that nothing enforces
dependencies between settings, so a key whose effect depends on another has to
say so on the row — the Advanced page already does this for several, and that
pattern is the one to extend.

### 3.3 Import / export

`export-settings`, `import-settings` and `export-logs` — the *Backup and
diagnostics* group in `adguard-cli.md`. Both exports take `--output` as either a
file or a directory; `import-settings` **requires** `--input`.

Bigger than it looks, for two measured reasons:

- **`import-settings` overwrites the whole configuration.** It deserves the
  confirmation discipline custom-filter removal got — an `AdwAlertDialog` that
  names what is about to be replaced — and there is no undo but a prior export.
- **It is the only thing besides `configure` that can create `proxy.yaml`**
  (contract §5). So it collides with the first-run assistant, whose entire
  trigger is that file's absence: an unconfigured install offered an import is a
  second path through first-run. That interaction is to be **designed**, not
  discovered by whoever gets there.

`export-logs` bundles `app.log`, `proxy.log` and `access.log` for a support
report. Those are a record of what the user browsed — contract §9 shows an
`access.log` line, and `proxy.log` was already 8 MB unrotated. A UI that
produces that bundle should say what is in it, on the button, in the same voice
the certificate and helper rows use for what a command will do.

### 3.4 The `speed` benchmark UI

`adguard-cli speed [--json] [-c|--chunk <bytes>]...`. The `--json` mode makes it
the least risky parse in the backlog — this project does not parse human output
anywhere it can avoid it (contract §6 is the standing example), and here it does
not have to.

Unmeasured: how long it runs, what it does with no proxy running, and whether it
is interruptible. Measure those into the contract before designing a page around
it — a benchmark that cannot be cancelled is a modal that cannot be closed.

### 3.5 Live blocked-request stats — its own milestone

The expensive item, and the one that changes the project's character. Contract
§9 is blunt about `access.log`: the format is undocumented and unstable across
versions, its detail varies with `log_level` (at `info` many messages are elided
to `...`), nothing rotates it — `proxy.log` was already 8 MB — and **there is no
push or event mechanism**, so a live view must tail the file.

That last point is the one to weigh. Every other reading in this application is
a fact checked against a file or a database, and *verify, don't trust*
([`architecture.md`](architecture.md) §3) is the rule the whole design is built
on. A tailer over an undocumented format is the first feature here whose
correctness cannot be checked against anything.

**Recommendation: not in v2.** Put a spike on the log format first — how it
moves across a version bump, what `log_level` costs it, what happens when the
file is rotated or truncated underneath a reader — and let the spike decide
whether there is a feature here. Folding it into a mixed v2 makes the whole
milestone hostage to the one item that might not be buildable.

### 3.6 Userscripts — out unless the upstream moved

Out of v1 because there is one script. `userscripts list` returned a single
entry, `adguard-extra`, and `proxy.yaml` says in AdGuard's own words that only
AdGuard Extra is supported — so the feature was one switch for one script that
ships pre-enabled, and a sidebar page for that is navigation without content.

**Re-checked 2 August 2026 — unchanged.** Against `adguard-cli` 1.4.13,
`userscripts list` exits 0 and returns one entry: id `adguard-extra`, title
*AdGuard Extra*, marked `[x]` and therefore already enabled. So the v1 reasoning
stands, and §7 now carries it with that date attached rather than silently. It
is still one command, so re-check it again when `adguard-cli` moves — the
version bump is the trigger, not the calendar.

### 3.7 The recommendation — taken, 2 August 2026

**v2 = HAR capture + advanced parity + import/export**, with the userscripts
re-check as a one-command precondition rather than an item. Stats behind a spike,
as its own milestone.

The three chosen items share a property worth naming: none introduces a new way
of knowing things. All three are `config set` writes and file reads, verified
the way everything else here is verified. Stats is the one that is not, which is
the argument for separating them rather than a complaint about it.

~~**This is a recommendation and not a decision.**~~ **It was adopted unchanged
by the project owner on 2 August 2026** and is now written into
[`architecture.md`](architecture.md) §7, which is the authority. The precondition
it names has been discharged: §3.6's re-check is done and the answer did not
move. Two things §7 records that this recommendation did not: HAR's *always-on*
objection from contract §9 is answered rather than dropped, and `har_writer`'s
stock `location` is `'.'` — a relative path, which the row has to resolve.

---

## 4. Not v2's job

- **The activation success leg** — `handoff.md` §3 item 6. Needs a real account
  and spends a device slot. The owner's call, not an agent's. Still open, and
  opening v2 does not open it.
- **The three deliberately unmeasured CLI behaviours** — `handoff.md` §3 item 7.
  Each costs more to close than it settles, and each looks like an oversight
  where it sits, which is why they are listed in one place.
- **Everything on `overnight-plan.md` §3's stop list.** Archived, still in force,
  and it is the standing set: no `sudo` and no suid bit on anything, no
  `configure` against a populated directory, neither mutate suite, no exposed
  `listen_address`, no filter installed into the real catalogue, and no
  reformatting — `cargo fmt --check` has been deliberately dirty since the first
  commit because the measured-behaviour tables do not survive rustfmt.
- **Resolving a documented contradiction by writing code.** Fix the doc, or leave
  the decision. Also from that list, and the one most likely to be forgotten by a
  session that is otherwise doing everything right.

§4 of that same file is the verification discipline, and it applies here
unchanged: `cargo build` is not verification, every claim names a command and
pastes its output, measure into the contract *before* the code that depends on
the measurement, and one sample is not a measurement.

---

## 5. The new category, and it outranks this backlog

**The repository is public** as of 2 August 2026. Every measurement in
[`cli-contract.md`](cli-contract.md) was taken against `adguard-cli` 1.4.13 on
Ubuntu 26.04, GNOME 50, Wayland — one machine, one CLI version, one desktop.

The first bug report from a machine that is not that one is worth more than any
item in §3. It is the only way to learn which of this project's constants are
facts about `adguard-cli` and which are facts about this machine, and
`handoff.md` §0's table already names four checks whose *interesting* branch is
unreachable locally. **Check the issues before planning a session.**

Two standing consequences of the flip, both from `handoff.md` §0:

- Every file here and every commit message is public. §4's redaction rule was
  written about pasting an AT-SPI walk into a terminal; it now covers pasting one
  into a commit. A Status-page walk carries the owner's e-mail **and** licence
  key.
- A version bump of `adguard-cli` is now a re-verification of `cli-contract.md`,
  not a footnote — `building.md` §7 says this already, and a public issue tracker
  is what will make it arrive as a bug report rather than as a plan.
