# Overnight v2

**Live, from 2 August 2026.** The plan for an unattended run on v2, written the
night the scope was decided. [`handoff.md`](handoff.md) §0 is still the entry
point and this file does not replace it; [`architecture.md`](architecture.md) §7
is still the scope authority and this file does not extend it. What this file
adds is the one thing neither of those carries: **how a session behaves when
nobody is awake to answer it.**

It pins nothing that moves — no test count, no hash, no sha. That was
[`overnight-plan.md`](overnight-plan.md)'s mistake and its banner says so.

---

## 1. Every iteration starts here

`handoff.md` §0, in full, before anything else: the tag check, the clean tree,
`cargo test --workspace`, and the `proxy.yaml` hash.

**A hash mismatch is a stop and then a diff, not a conclusion.** The owner is
asleep and cannot be asked; if the diff is not obviously your own sandboxed
work, stop the run and write what you saw into §3. Do not repair it.

Then re-read §7's *v2 — open* half, because that is what you are working from.

---

## 2. The order, and why

Not "cheapest to build" — **cheapest to be wrong about**. An unattended session
should spend the early hours on work whose failure mode is a wasted night
rather than a damaged install.

### 2.1 The advanced-parity enumeration — do this first

Pure measurement, zero writes, and §7 names it as that item's first task. Walk
`proxy.yaml`'s keys against what the Advanced and Stealth pages actually render
and write the gap down. Expect it to be smaller than "parity" sounds, and expect
part of it to be keys that *should* stay unrendered — say which and why, because
that half is the more useful half.

Contract §5 records that nothing enforces dependencies between settings, so any
key whose effect depends on another has to say so on its row. The Advanced page
already does this for several; extend that pattern, do not invent one.

Output is a section in the contract or a table in `architecture.md` §5. **It is
not code**, and the item is not started until it exists.

### 2.2 HAR capture — the first thing that writes

Two open points, and the order between them is fixed:

1. **Where `location: '.'` resolves.** Unmeasured. §7 says so explicitly and
   says it is this item's first task. Nothing records the proxy's working
   directory and no HAR dump has ever been produced here. Measure it into
   contract §9 **before designing the row** — the expected answer (the row must
   show an absolute path) is expected, not established, and writing it down as
   established is exactly the error §4 was just amended for.
2. **The subtitle.** Contract §9 calls full HAR dumps too heavy for an always-on
   UI. §7 answers that objection rather than dropping it, on the grounds that
   the switch ships `false`; the price of that answer is a subtitle that says
   what leaving capture on costs, in the voice §6 uses for what a command does.

Then the group itself: `har_writer.enabled` (bool) and `.location` (string),
through `config set` like every other switch. Sandbox it. Verify by re-reading
`proxy.yaml`, never by the confirmation line — §4.

### 2.3 Import/export — design only, and stop at the design

Do **not** build this overnight. §7 requires the first-run collision to be
designed before either half is built, and that design is a decision about what
the user sees on an unconfigured install offered an import. Write the design;
leave it for the owner.

Two measured facts it starts from: `import-settings` takes `-i,--input` and it
is REQUIRED, both exports take optional `-o,--output` which may be a directory,
and all three artifacts are zip. And `import-settings` is the second write path
outside contract §5's rule — `configure` is the first — and the first to operate
on an install that is already configured.

`export-logs` bundles `app.log`, `proxy.log` and `access.log`, which are a record
of what the user browsed. The button says what is in the bundle.

---

## 3. What this run may not do

`overnight-plan.md` §3 is the standing stop list and it is in force unchanged.
The ones an ambitious night is most likely to talk itself into:

- **No `sudo`, `pkexec`, or a suid bit on anything.** This application ships no
  privileged component and §6 explains why.
- **No `adguard-cli configure` against a directory that already holds a
  `proxy.yaml`.** It resets the user's whole configuration and there is no
  prompt to decline at with stdin closed.
- **No `config_mutate` / `filters_mutate`, and no filter into the real
  catalogue.** Sandbox everything: `Cli::with_xdg_data_home`.
- **No reformatting.** `cargo fmt --check` has been deliberately dirty since the
  first commit; the measured-behaviour tables do not survive rustfmt.
- **Nothing from `handoff.md` §3 item 6.** The activation success leg needs a
  real account and spends a device slot. The owner was asked on 2 August 2026
  and left it open. It is answered, not pending.
- **No push.** Commit locally. The repository is public and unreviewed commits
  landing on `main` overnight is a different risk category from local ones.
- **No resolving a documented contradiction by writing code.** Fix the doc, or
  leave the decision.

---

## 4. Verification, and the four traps that have actually fired

`overnight-plan.md` §4 is the discipline: `cargo build` is not verification,
every claim names a command and pastes its output, measurements land in the
contract *before* the code that depends on them, and one sample is not a
measurement.

Four failure modes have really happened on this project, and an unattended run
is where they are cheapest to repeat and most expensive to notice:

- **The confirmation is not the evidence.** `Config has been updated` prints for
  a no-op and for a silently declined write. Re-read the file.
- **Silence only means something once the input is known to have changed.** Hash
  either side of an edit and refuse to conclude anything from a hash that did
  not move.
- **A qualifier is load-bearing.** "No rotation policy is configured *by us*"
  became "nothing rotates" and was false. If a source sentence has a scope word
  in it, carry the word or re-measure. `handoff.md` §4.
- **Do not launder an unmeasured clause into a measured sentence.** Split the
  clause and mark which half was measured. Same §4 entry.

**The repository is public and so is every commit.** Redact before anything
reaches a commit message, a doc, or the terminal — both patterns, e-mail *and*
licence key, per `handoff.md` §4. A Status-page walk carries both.

---

## 5. How an iteration ends

Every iteration, without exception:

1. Verify with a pasted command and its output.
2. Update [`handoff.md`](handoff.md) — §1's table, §3's gaps, §4 if you learned
   a trap. **The test is whether the next thread can start from §0 alone.**
3. Commit locally, with a message that says what was *measured*, not what was
   written.

If you are blocked, write the blocker into `handoff.md` §3 and move to the next
item. **Do not guess, and do not decide anything §7 or the owner owns.**

---

## 6. When to stop

**Stop is a legitimate outcome and it always was.** If what is left needs the
owner — a scope call, a device slot, a machine-wide change, a contradiction only
a human can resolve — say so in `handoff.md` §3 and end the run.

A session that adds nothing to a project in this state has not failed at
anything. A session that invents work to avoid stopping has.
