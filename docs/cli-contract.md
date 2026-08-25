# The `adguard-cli` integration contract

Verified behaviour of `adguard-cli` v1.4.13 as an automation target. **Read this before writing any code that shells out to it.** Everything here was measured on this machine, not inferred from documentation.

The GUI treats the CLI as its only *write* API. This file records what that API actually guarantees — and where it will bite.

---

## 1. Rejected integration points

Four other surfaces exist. None should be used.

| Surface | Why not |
| --- | --- |
| `agcli.socket` (unix socket in the data dir) | Undocumented internal control channel between the CLI and its daemon. No stability guarantee; format not published. Disappears entirely when the proxy is stopped. |
| `AGLocalApiServer` — a WebSocket server inside the daemon | Confirmed present (`AGLocalApiServer.cpp`, `AGWebSocketHandler`, `connectToLocalApiServer` in the binary). But it is wired to userscript/content processing (`AGPFProcessingUnit`), not general control. Its port key `local_api_server_port` is not even present in the config (`config get local_api_server_port` → `not found`). Internal. |
| `adguard_cli_nm` (Native Messaging host) | Locked to specific browser extension IDs via the manifests that name it — five `chrome-extension://` origins, two Firefox `allowed_extensions` — and the browser vouches for the caller. Impersonating a browser extension is fragile and rude. **Its absence is worth reporting, and is:** `install-browser-integration` is a separate step that unpacking the CLI does not perform, so on a stock install the extension reports that it cannot detect `adguard-cli` while AdGuard runs and filters perfectly. That check is `crates/adguard-core/src/browser.rs`, which reads the manifests without ever speaking the protocol — see [§12](#12-browser-integration-is-a-separate-step-and-quietly-conditional). |
| Writing `proxy.yaml` directly | See [§5](#5-configuration-writes) — it would destroy the file's comments. |

**Decision: shell out to the `adguard-cli` binary for all writes; read state from `proxy.yaml` and the filter SQLite DBs.**

This is viable mainly because invocation is cheap — see §2.

---

## 2. Invocation cost (measured)

| Command | Wall time |
| --- | --- |
| `status` | 0.01 s |
| `license` | 0.02 s |
| `filters list` | 0.02 s |
| `config show` | 0.02 s |
| `filters list --all` | 0.03 s |
| `start` (success) | 1.1 s |
| `start` (failure) | **60.0 s** |

Process startup is ~10–30 ms. Polling `status` on a 1–2 s timer is entirely affordable; there is no need for a persistent connection or a caching daemon.

**`start` is the one local command whose failure is slow.** A start that cannot bind waits on AdGuard's own internal deadline before admitting it — see [§11](#11-a-proxy-the-cli-has-lost-track-of) — so a wrapper deadline sized for the 1.1 s success case kills the command three quarters of the way through the 60 s failure and replaces the CLI's explanation with a timeout of ours. `Cli`'s `START_TIMEOUT` sits above AdGuard's at 90 s for that reason; every other local command keeps the 15 s one.

Still run every invocation off the GTK main thread — 30 ms of jank is visible, and network-touching commands (`check-update`, `filters update`) take seconds.

---

## 3. Exit codes are only half-trustworthy

Measured directly (not through a pipe — piping makes `$?` report the *last* command in the pipeline, which is a trap):

| Invocation | Exit code | Stream |
| --- | --- | --- |
| `status` | 0 | stdout |
| `config get proxy_mode` | 0 | stdout |
| `config get bogus_key_xyz` → `'bogus_key_xyz' not found` | **0** | stdout |
| `config get filters` → `This field is not a separate setting` | **0** | stdout |
| `config show nonexistent_section` → `not found` | **0** | stdout |
| `bogus-subcommand` | 1 | **stderr** |
| `config` (missing subcommand) | 1 | **stderr** |
| `config get` (missing argument) | 1 | **stderr** |
| `filters list --bogus-flag` | 1 | **stderr** |

**The rule:** exit code 1 means the command never ran, and the message goes to **stderr**. Every *semantic* failure — unknown config key, wrong key type, missing section — prints to **stdout** and exits **0**.

That rule survives as far as the *stream*, and no further: see [exit 1 is usually our bug](#exit-1-is-usually-our-bug-but-not-always) for the two cases where a failure exits 1 anyway, one of them on stdout. **The stream is the reliable discriminator, not the status.**

Consequences for the wrapper layer:

- Real failures must be detected by **matching output text**. This is inherently brittle: pin the patterns in one place, and treat an unrecognised output shape as failure rather than success.
- Never conclude "the operation worked" from exit 0 alone. For state changes, re-read the resulting state and verify.

### Exit 1 is *usually* our bug, but not always

The original reading of the table above was that exit 1 always means CLI11 rejected our command line — a programming error in this codebase, never a user-facing condition. Three later measurements show that is too strong, and `Error::BadInvocation` should not be treated as unreachable:

| Also exits 1 | Stream | Cause | Whose fault |
| --- | --- | --- | --- |
| `config set <key> --anything` | stderr | A positional value beginning with `-` is read as an option: `<value> is required` | ours — fixed by the `--` guard in [§5](#the----guard-is-mandatory) |
| `status`, `license`, `filters list` in an unlicensed install | stderr | `You need to activate an AdGuard license to use this command` | **neither** — a real user state |
| any command, while another is initialising the same **fresh** data directory | **stdout** | `Filter manager initialization failed` | AdGuard's — a race in its own start-up |

The second is the one that matters most. It is not reachable on this machine (`license` reports `APP_ACTIVE`), which is why it went unnoticed, but a lapsed licence made `Cli::status` return "adguard-cli rejected `status`" — describing the user's expired licence as an internal error. `Error::Unlicensed` now carries it instead, matched on the two tokens *licen…* and *activat…* rather than the exact sentence, so a rewording degrades to the old behaviour rather than to a missed case.

**The third breaks the "exit 1 means stderr" half of the rule**, and it was found by driving the licence page against a data directory that had never been used. Measured: run two commands at once against such a directory and one of them exits **1** with `Filter manager initialization failed` on **stdout** and stderr *empty* — eight runs in twelve. Once the directory is initialised it never happens again.

The shape it needs was not exotic: it was this application's own start-up, where `status` and the licence read went out together. `StatusPage::reload` now runs them one after the other on a single worker for exactly this reason — not racing is cheaper than recovering, and the licence read has no poll behind it to try again. The mapping stays anyway, because two invocations can still meet by other routes and because a wrong answer here is one that blames the user's command line for AdGuard's own start-up.

Reported as `BadInvocation` that read as *"adguard-cli rejected `license` (exit 1): "* — a claim that our command line was malformed, with nothing after the colon to support it. So the wrapper reads the **stream**: a non-zero exit whose only text is on stdout is the program refusing, not the parser rejecting, and becomes `Error::Refused` carrying AdGuard's own sentence. `Refused` is therefore no longer exclusive to exit 0.

This is also the one failure a developer meets routinely, because `XDG_DATA_HOME=/tmp/fake` (`building.md` §3) creates exactly that never-used directory.

### Once the directory is initialised, a second invocation *blocks*

"It never happens again" is true of the *failure* and was mistaken for the whole story. Measured while a 60-second `filters install` was in flight against an already-initialised sandbox:

| Run alongside an in-flight `filters install` | Wall time |
| --- | --- |
| `status` | 0.02 s — unaffected |
| `config get log_level` | **58 s** — released the moment the install returned |
| `filters disable <id>` | **52 s** — same |

So the lock covers the config and filter-manager paths and not the status path. Nothing fails and nothing is corrupted; the second command simply waits. That is the better of the two behaviours, but it is not free for a GUI: every one of those waits is a worker thread held for up to a minute ([`worker::run`](../crates/adguard-gui/src/worker.rs) spawns one per call), and any page that reads `proxy.yaml` through the CLI freezes behind a slow install rather than reporting anything.

Two consequences. The 2 s `status` poll is safe to leave running across a long command, which is what makes a progress state on the Filters page workable at all. And a command that can take a minute must be the *only* config-path call in flight — the UI should disable the affordances that would issue another rather than queue them behind it.

One limit of the measurement: the sandbox proxy was **stopped**, so this says nothing about whether `status` still avoids the lock when it has a live daemon to talk to.

**That limit is still open, and a measurement taken on 9 August 2026 did not close it — though it was first written up here as though it had.** Eight `status` calls issued alongside an in-flight `check-update`, on the real install with the proxy up, came back in 0.03 s each ([§14](#cost-and-what-it-does-not-block)). What that establishes is exactly itself: **`status` does not contend with an in-flight `check-update` on a live daemon**, which is all the About page needed to know. It is not evidence about the lock, because **nothing shows `check-update` takes the lock at all** — and this run, unlike the table above, had no positive control. The table's `config get` at 58 s is what proves a lock was being held; with only `status` in flight, eight fast replies look identical whether `status` avoids the lock or the command never took one. `check-update` running happily unlicensed ([§14](#it-runs-unlicensed-and-it-creates-the-data-directory)), where every lock-holding command measured above is licence-gated, is weak evidence for the second reading.

Closing it properly needs a **known** lock-holder — a `filters install` — in flight against a live daemon, with `config get log_level` alongside `status` as the control, exactly as the stopped-proxy table was taken. That is a minute-long write against the owner's own install, so it is theirs to authorise rather than an agent's to run.

**The complaint is not the whole of stderr.** Measured after the mapping was first written against only the opening line: the CLI follows that sentence with its entire usage dump — every subcommand, one per line — and then the one line worth acting on.

```text
You need to activate an AdGuard license to use this command
/home/you/.local/bin/adguard-cli
  CLI for controlling AdGuard
  Options:
    -v,--version                Display program version information and exit
  Commands:
    activate                    Activate an AdGuard license
    …
You can activate your AdGuard license by running `/home/you/.local/bin/adguard-cli activate`
```

Roughly twenty lines, destined for an `AdwActionRow` subtitle. `Cli` keeps the first line and the advice and drops the dump.

Activation is built (`architecture.md` §5): it opens the URL and waits for the user, and does **not** poll — see [§7](#7-commands-that-need-a-tty) for why.

**`license` output is sensitive.** On a licensed machine it returns three lines — owner e-mail, licence key, status — at exit 0, with nothing on stderr and, alone among this CLI's output, **no ANSI escapes**:

```
License owner: someone@example.com
License key: XXXXXXXXXXXXXXXX
License status: APP_ACTIVE
```

The key is sixteen characters, and it is a secret; the e-mail is personal data. Anything that surfaces this — a Status row, a toast, a log line, an error path — masks the key to its last four characters. `License::masked_key` is that mask, and `License`'s `Debug` is hand-written so a `{:?}` cannot leak either field.

The crate's existing scrubber does not reach this. `redact_error` replaces a secret **the caller already holds** — that is why its only caller is `Cli::set_secret`, which knows the password it just passed on the command line. A licence key is what came *back*, so there is nothing to hand it.

Which matters for one path in particular: `Cli::license` must not report a parse failure the way `Cli::status` does. `Error::Unparseable` quotes the output it could not read, and that output is the key — so this one call redacts by shape instead, with `redact_values`, keeping the labels (`License key: <hidden>`) and dropping every value. Enough to recognise a rewording; useless to anyone reading over a shoulder.

`license_live.rs` runs the real command on every `cargo test`, so a rewording upstream shows up as a failing test rather than as a blank row. It skips when AdGuard is absent, and skips again when the install is unlicensed.

Discovered by running the CLI against a sandboxed data directory ([§5](#measuring-writes-without-touching-the-real-config)), where nothing is licensed.

---

## 4. ANSI escapes are unconditional

The CLI emits SGR bold codes **even when stdout is not a TTY**, and honours none of the usual opt-outs:

```
$ adguard-cli filters list | cat -v
^[[1m    |           ID | Title                                   Last update        ^[[0m
^[[1mAd blocking^[[0m
[x] |            2 | AdGuard Base filter                     2026-07-29 20:24:45
```

Verified ineffective: piping (no TTY), `NO_COLOR=1`, `TERM=dumb`.

**Every captured stdout/stderr buffer must be run through an ANSI stripper before parsing or display.** Use the `strip-ansi-escapes` crate; do it once, in the wrapper layer, so no call site can forget.

> Caution when testing this yourself: `status` output contains bold only in the "listening on" lines, which are absent when the proxy is stopped. Testing ANSI behaviour against a stopped proxy gives a false clean result.

---

## 5. Configuration writes

`proxy.yaml` is a **hand-commented file** — 221 lines, of which roughly half are explanatory comments documenting every key ("Supported proxy modes are: manual, auto", "Use -1 to disable SOCKS5 manual proxy", …).

Serialising a parsed YAML document back over it would delete all of that. No YAML serialiser round-trips comments.

**Rule: all writes go through `adguard-cli config set|reset|list-add|list-remove`.** Read the file freely; never write it.

That rule rests on `config set` being surgical, which is measured: a write replaces the **single** line, leaves the line count unchanged, and preserves every comment. `config_mutate::a_write_disturbs_exactly_one_line` asserts exactly this, because if it ever stopped being true the GUI would be quietly shredding the file's documentation on every switch flip.

`config show` is a **rendered view, not the file**:

- Large sections are collapsed to `<folded> enabled` / `<folded> disabled`.
- Secrets are masked — `config show listen_auth` prints `password: <set>` where the file contains `password: 'admin'`.
- Therefore: parse `proxy.yaml` for real values, and use `config show` only when mirroring the CLI's own presentation.

Key syntax facts:

- Dotted paths work for scalars: `config get stealthmode.enabled`, `config get listen_ports.http_proxy`.
- `config show <section>` accepts **top-level** sections only. Nested ones fail: `config show anti_dpi` → `not found`, even though `stealthmode.anti_dpi` exists in the file. Expand the parent instead.
- List-valued keys (`filters`, `userscripts`, `apps`) are not scalars — `config get filters` refuses. Use `list-add`/`list-remove`, or edit an auxiliary file via `--list-file`. **The refusal is at exit 0**, measured 2 August 2026 for all three: `This field is not a separate setting` followed by ``Please run `adguard-cli config show <key>` to see its structure``, byte-identical across the three, exit 0 every time. So a wrapper that distinguishes "not a setting" from "read it" cannot do so on the exit code; it is a semantic refusal like `'--bogus' not found` above. This matters for an enumeration: every other leaf key of `proxy.yaml` answers `config get` with `key = value` at exit 0 too, so **exit status separates nothing here and only the stdout shape does**.
- **`config get` does not mask secrets.** `config get listen_auth.password` prints `listen_auth.password = admin` in full; only `config show` masks, as `password: <set>`. So `config get` is not a safe thing to log.
- `config reset <key>` restores the shipped default and confirms in the same way (`log_level` → `info`). Not used yet; the obvious home for it is a "restore default" affordance per row.

### Before `proxy.yaml` exists, almost nothing works

Everything above assumes the file is there. On a machine where `configure` has never run it is **not**, and that is not a corner case — it is every fresh install.

Measured against a virgin `$XDG_DATA_HOME`:

| Invocation | Exit | stdout | Effect |
| --- | --- | --- | --- |
| any command | 0 | `Created data directory <path>` | the directory, `adguard.conf`, both `agflm_*.db` and `logs/` appear — **but no `proxy.yaml`** |
| `config set <any real key> <value>` | 0 | `No configuration YAML file` + `You can only configure the 'log_level' and 'update_channel'` + advice to run `configure` | nothing |
| `config set log_level debug` | 0 | `log_level = debug`, the same advice, then `Config has been updated` | persists — into `adguard.conf`, **not** into a config file that still does not exist |
| `config get log_level` | 0 | `log_level = debug` | reads back the above |
| `activate` | 0 | the ordinary log-in link | **no `proxy.yaml`** |

Four things follow.

**The absence of `proxy.yaml` is the first-run signal**, and it is a file test rather than a command. Nothing else needs inventing, and `paths::config_file` already names the path.

**`config set` is useless until the file exists.** This is what makes a first-run assistant built purely out of `config set` calls impossible, and it retired the design sentence in `architecture.md` §5 that described one.

**`configure` is the only thing that creates the file.** `config get`, `config set` and `activate` were each run against a virgin directory and none of them produced one. `import-settings <zip>` is the only alternative and it needs a zip nobody has.

**`Config has been updated` reaches a new low here.** For `log_level` it is printed truthfully — the value really is stored — about a file that is not the one any of this application's reads look at. The rule this document keeps repeating holds in its strongest form yet: the confirmation is never the evidence.

One smaller measurement worth keeping, because it makes a test pass for the wrong reason if you do not know it: **the type check runs before the missing-file check**. `config set listen_ports.http_proxy true` answers *"the value of the setting must be an integer"* even with no config at all, so the CLI evidently knows every key's type from a built-in default. Probe this path with type-appropriate values or you will not be measuring what you think.

### Measuring writes without touching the real config

**The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`.** Pointing that at a scratch directory holding a copy of `proxy.yaml` gives a complete, throwaway AdGuard configuration:

```bash
XDG_DATA_HOME=/tmp/sandbox adguard-cli config set listen_address 0.0.0.0
```

This is how everything in this section was measured, and `Cli::with_xdg_data_home` exposes it to `tests/config_sandbox.rs`. It matters because the interesting write behaviours are the ones nobody should provoke on a real machine: exposing the proxy on `0.0.0.0`, blanking the proxy password, setting a listen port to a value that takes the listener down.

Two limits:

- A sandbox is an **unlicensed** install by default. `status`, `license` and `filters list` all fail there with exit 1 (see [§3](#exit-1-is-usually-our-bug-but-not-always)). The `config` family, `--version` and `activate` need no licence and behave exactly as they do for real — `activate` because it is the command that exists to *fix* an unlicensed install, which makes a sandbox the only honest place to exercise it.

  **The licence lives in `adguard.conf`, and it travels.** Copy that one file into a sandbox and `license` answers `APP_ACTIVE` there. An earlier revision of this bullet said the licence "evidently lives elsewhere", inferred from copying `gm.db` and seeing no change — the wrong file, not the wrong directory. This matters more than a correction: it is what makes the licence-gated commands measurable against a throwaway config at all, and `Cli::configure` could not have been covered without it, since the alternative was resetting the author's own install to watch what happened.

  It has a sharp edge. `handoff.md` §4 says a Status-page walk leaks the licence owner's e-mail and offers "take the walk against a sandbox, which has no licence and therefore no owner" as the way out. A **lent** licence defeats that exactly, and a sandbox is where someone is most likely to feel safe pasting output. Redact at the harness instead.
- It says nothing about whether our *reads* point at the file AdGuard really uses. That still needs a test against the live install — which is what `config_live.rs` and the one round-trip left in `config_mutate.rs` are for.

### The `--` guard is mandatory

Both arguments of `config set` are positionals, and CLI11 still tries to read a leading `-` as an option:

```
$ adguard-cli config set listen_auth.password --flag-shaped
<value> is required                      # exit 1, nothing written
$ adguard-cli config set listen_auth.password -abc
<value> is required                      # exit 1, nothing written
$ adguard-cli config set -- listen_auth.password --flag-shaped
listen_auth.password = --flag-shaped     # exit 0, written
```

`-1` survives without the guard, because a negative *number* parses as a positional — which is what made the manual proxy ports (`-1` to disable) look safe and hid this. A password or hostname starting with `-` does not.

The guard changes nothing for ordinary values — verified for `-1`, plain strings and every enum — so `Cli::config_set` applies it unconditionally rather than by a rule someone has to remember. It also improves the failure mode for a bad *key*: `'--bogus' not found` at exit 0, an ordinary semantic refusal, instead of a parse error.

### `config set` type-checks, and nothing else

For an integer setting it verifies only that the value **is** an integer:

| `config set listen_ports.http_proxy …` | Result |
| --- | --- |
| `0`, `65536`, `99999`, `-2` | **accepted**, written verbatim |
| `3.5` | **accepted** — the file gets a *float* where an integer belongs |
| `abc`, empty | refused: `Invalid value type: The value of the setting must be an integer` |

`worker_threads 0` and `worker_threads -1` are accepted too. So **range-checking is the GUI's job**, like the cross-setting dependencies below; `model::Setting`'s `min`/`max` are ours, not the CLI's, and `Setting::permits_number` is the gate. `3.5` is the nastiest of them, because `Config::int_at` then reads nothing at all and the row goes "unavailable" — a value the CLI itself accepted rendering as unreadable.

Enumerated settings *are* checked, and name their options in the refusal:

```
$ adguard-cli config set log_level bogus
Invalid value for key `log_level`. Valid values are: info, debug, trace
$ adguard-cli config set outbound_proxy.mode bogus
Invalid value for key `outbound_proxy.mode`. Valid values are: http, https, socks4, socks5
```

But **the accepted value is written back verbatim, in whatever case it was given.** `config set log_level INFO` leaves `log_level: 'INFO'`, and `outbound_proxy.mode socks5` leaves `mode: 'socks5'` where the default is `'HTTP'`. Reads must therefore be case-insensitive — `Config::choice_at` — or a value the CLI produced would render as unavailable.

`listen_address` is validated, and more narrowly than the file's own comment suggests: it must be **a bare IP address with no port**. `127.0.0.1`, `0.0.0.0`, `::1`, `::` and `192.168.1.10` are accepted; `localhost`, `0.0.0.0:3128`, `1.2.3.4.5` and the empty string are refused with `Value for key 'listen_address' must be a valid IP address without port`. Note `localhost` is *rejected on write* even though it appears in the comment ("if not localhost, authentication is required") — so `config::is_loopback` accepting it is right for reading a hand-edited file, but the UI must never offer it.

### `filtered_ports` is validated too — and its refusal names the wrong separator

**Measured 2 August 2026**, thirty-nine writes against a sandbox seeded from the real file, each one diffed rather than read from the confirmation line. This correction matters because both `architecture.md` §5 and `handoff.md` §0 said the opposite — that this key's *"compound range syntax is ours to validate, since `config set` type-checks strings not at all"*. **It is not ours. The CLI validates it, and more thoroughly than `listen_address`.**

| Written | Result |
| --- | --- |
| `80:5221,5300:49151`, `80,443,8080` | accepted — the two forms the file's comment documents |
| `80`, `0`, `65535`, `80:80`, `0:80`, `0:65535`, `80:65535`, `80:90,443` | accepted |
| `9:80` | accepted — ascending, and a *string* comparison would refuse it |
| `65536`, `80:65536`, `80,65536` | refused — the ceiling is 65535, in every position |
| `9000:80` | refused — **a range must ascend** |
| `80:`, `:90`, `80,:90`, `80,90:` | refused — an empty endpoint, in every position |
| `80:90:100`, `-1`, `http`, `hello world`, `80,abc` | refused |
| *(empty)*, `" "` | refused — so this key has no "clear" state, unlike `outbound_interface` below |
| `80, 443`, `80,`, `80,,443`, `80 `, `080`, `00080:00090` | **accepted, and written verbatim** |
| `80: 90`, `80 :90`, `80:90 ` | accepted — whitespace around the colon is tolerated |
| `,80`, ` 80` | refused — but with the **integer** message, not the range one |

So the grammar the CLI actually enforces is: comma-separated elements, each either `N` or `LO:HI`, with `0 ≤ N ≤ 65535` and `LO ≤ HI`. Three things follow, and the first is the one a row depends on:

- **The refusal text is wrong, and wrong in the most expensive direction.** It reads ``Invalid value for key `filtered_ports`. Valid values are: space-separated list of valid ports or range of port`` — and space-separated is *precisely* the form it rejects (`80 443`, `80:90 443`, both refused). Every other refusal in this section can be shown to the user verbatim; `log_level`'s even enumerates the valid options correctly. **This one cannot.** A row that surfaces the CLI's own words here would instruct the user to do the one thing that cannot work. The file's comment is right where the binary's message is wrong, which is the reverse of the usual direction and is why this was worth thirty-nine writes. `config::is_port_list` is what the GUI checks instead, and its rustdoc carries this table so the two cannot drift.
- **The tolerated junk is written back verbatim and must survive a read.** `80,` and `80,,443` and `80 ` and `00080:00090` all land in `proxy.yaml` exactly as typed. A GUI that re-normalises what it reads would rewrite a file the user has not touched, and one that refuses to render them would call a value unavailable that the CLI itself produced — the same trap `choice_at` exists for.
- **Two refusals, two messages, and the split is positional.** An empty or whitespace-led element is refused with the *integer* message at the **start of the string** (`,80`, ` 80`) and accepted anywhere later (`80,,443`, `80, 443`). Measured on both sides; the mechanism behind it is not, and does not need to be.

Nothing here was measured against a *running* proxy. Whether the daemon agrees with the CLI about `80,,443` is unknown and needs a second proxy to find out — the same wall as §9's HAR `location`, `handoff.md` §3 item 9.

### Writing a null: the empty string and the word `null` are not the same write

`outbound_interface` is the **only null-valued scalar in the whole 220-line file** — every other bare-looking line is a mapping header — so it is the only place this question arises, and it had never been measured. Both routes back to nothing work, and they do not produce the same file:

| Written | The line becomes | `config get` says | A YAML reader says |
| --- | --- | --- | --- |
| `eth0` | `outbound_interface: eth0` | `= eth0` | `"eth0"` |
| *(empty string)* | `outbound_interface: ` — a bare empty scalar | `= ` (empty) | **null** |
| `null` | `outbound_interface: null` — **byte-identical to stock** | `= null` | null |

**So the empty string leaves the CLI and every YAML reader disagreeing about what is in the file**, and the word `null` does not. A row that clears this field must therefore write `null`, not `""` — which restores the stock line exactly and keeps both readers saying the same thing. That is a measurement, not a preference.

The value itself is **not validated in any respect**: `no such iface 0` — spaces and all — is accepted and written unquoted. Range-checking an interface name is the GUI's job in the way §5 means it everywhere else.

### Every invocation rewrites `proxy.yaml`

Measured for `--version`, `config get`, `config show`, `status` and `license`: all of them write the file back and touch its mtime, **even when not a single byte changes**. `--version` is the striking one — it has no reason to open the config at all.

The rewrite is itself surgical: comments are preserved, and a **missing key is restored with its default**, in the right place. An *invalid* value is left alone.

Three consequences:

- **A `gio::FileMonitor` on `proxy.yaml` will churn.** The GUI polls `status` every ~2 s, so the monitor would fire continuously, triggered by nothing but our own polling. It must compare content, not trust the event — see `architecture.md` §3.
- The two "unavailable" states are not equivalent: a **missing** key heals itself the next time anything runs the CLI, while a **wrongly typed** one persists until someone edits the file.
- `config_mutate::a_write_disturbs_exactly_one_line` assumes one changed line. Against a config that is *missing* a key, a write would change two — the target line and the restored one. It would fail loudly and legibly, which is the right outcome, but it is not a bug in the write path.

### Reading `config set`'s answer

Success is again defined **positively**, by the line `Config has been updated`. Every refusal exits 0, prints to stdout, and leaves the file untouched:

| Invocation | stdout | Effect |
| --- | --- | --- |
| `config set stealthmode.enabled true` | `stealthmode.enabled = true` + `Config has been updated` | the one line is rewritten |
| `config set bogus_key true` | `'bogus_key' not found` | nothing |
| `config set stealthmode.enabled bogus` | `Invalid value type: The value of the setting must be an boolean` | nothing |
| `config set https_filtering.filter_secure_dns_mode nope` | ``Invalid value for key `…`. Valid values are: off, transparent, redirect`` | nothing |
| `config set filters something` | `This field is not a separate setting` | nothing |
| `config set anti_dpi.enabled true` | `'anti_dpi.enabled' not found` | nothing — paths must start at the top level |

Two shapes make positional parsing impossible:

- With `show_hints: true` (the default) a hint lands **between** the echo and the confirmation — `config set https_filtering.enabled true` prints the certificate-install advice in the middle.
- Setting a coupled key echoes several lines first: `config set listen_address …` echoes `listen_address`, then `listen_auth`, then `  username`.

So match `Config has been updated` as a whole line **anywhere** in the output.

### `Config has been updated` is not proof the value changed

It is necessary but **not sufficient**. It is printed for a no-op, and — measured — even when the CLI declined to make the requested change:

```
$ adguard-cli config set listen_address 0.0.0.0      # with listen_auth.enabled = false
Enter username for accessing proxy server:
Warning: No TTY for user input. Use `adguard-cli config set listen_auth.username` to set the value.
listen_address = 127.0.0.1        <- the OLD value; proxy.yaml is untouched
listen_auth = false
Config has been updated           <- ...and it still says this
```

`Ok` from the wrapper therefore means only *"the CLI accepted the command"*. **Re-read `proxy.yaml` to learn what actually happened** — act → re-read → reconcile, exactly as for filters.

### Booleans have two spellings, and one of them lies

| Written | Result |
| --- | --- |
| `true` / `false` | accepted; file gets `true` / `false` |
| `1` / `0` | **accepted**; file gets a literal `1` / `0` — an *integer* where a bool belongs |
| `True`, `TRUE`, `yes`, `on`, `false ` (trailing space) | rejected, file unchanged |

**Always write lowercase `true`/`false`; read tolerantly.** A strict struct deserialise would fail the whole document on a single `enabled: 1`, taking every unrelated setting down with it — which is why [`config.rs`](../crates/adguard-core/src/config.rs) walks a generic value tree and coerces per key. One junk value then costs one row instead of the page.

**The refusal is at exit 0, and it is a refusal.** Measured on `dns_filtering.block_ech`, 2 August 2026, with the exit status taken from the command and not from a pipeline: `config set dns_filtering.block_ech notabool` prints `Invalid value type: The value of the setting must be an boolean`, **exits 0**, and leaves the file byte-identical. Same shape as the list-key refusal in §5, as `har_writer.enabled` in §9, and as `safebrowsing.send_anonymous_statistics` — four keys now, so the pattern is the pattern and not a quirk of one. Anything reading exit status to decide whether a boolean write landed will conclude it did.

**Not every key in `proxy.yaml` is documented, by the file or by anything else.** Measured 2 August 2026 on `safebrowsing.send_anonymous_statistics`, which is the sharpest case because it is a consent setting: the `safebrowsing:` block carries a single comment, *"Browsing security settings"*, and nothing for the key; `config --help` and `config --help-all` never name it; and the binary's string table holds the key's name with no description beside it. Most keys *are* commented, which is why this project's convention has been to take row wording from the file — so the exception matters more than its size suggests. **Where the file is silent, a row has to say so rather than fill the gap**; `architecture.md` §5 has how that was resolved and the test that keeps it resolved.

**The type-pun's blast radius is every switch this app renders**, which is worth stating once rather than re-measuring per key: it is not a property of a particular setting but of how `config set` type-checks booleans, confirmed on `har_writer.enabled` (§9) and `dns_filtering.block_ech`. Setting `https_filtering.enable_tls13 1` leaves `enable_tls13: 1` in the file too. `Config::bool_at` coerces `Integer(1)`/`Integer(0)`, so a row painted from it survives; a row painted from a strict read would not.

### List writes: `list-add` and `list-remove`

A handful of keys hold YAML sequences rather than scalars — `filters`, `userscripts`, `apps`, and `dns_filtering.filters`. `config get` refuses them (§5 above), and they are written with `config list-add` / `config list-remove` instead. Measured against a sandbox seeded from the real file:

| Invocation | Exit | stdout | Effect |
| --- | --- | --- | --- |
| `list-add -- dns_filtering.filters extra.txt` | 0 | the whole list, then `Config has been updated` | **one** line added |
| `list-add` of a value **already in the list** | 0 | the same shape, showing the value twice | **a duplicate is appended** |
| `list-remove` of a value **not** in the list | 0 | the unchanged list + `Config has been updated` | nothing |
| `list-remove` of the **last** element | 0 | `filters:` with nothing after it | the file gets `filters: []` |
| `list-add -- <a scalar key> <value>` | 0 | `This field is not a list setting` + advice to use `config set` | nothing |
| `list-add <key> -leading-dash` (no `--`) | **1** | *(empty)* — `<value> is required` on **stderr** | nothing |

Five things follow, and three of them are traps.

**The write is as surgical as `config set`.** One line added, and the comment count does not move — measured at 220 → 221 lines with 105 comments either side. So the no-YAML-writes rule (§5 opening) covers list keys too, and a `list_add_disturbs_exactly_one_line` assertion is worth having for the same reason `a_write_disturbs_exactly_one_line` is.

**`list-add` does not deduplicate.** Adding a value the list already holds appends it a second time and reports success, which makes the confirmation line useless as evidence yet again. Anything driving a *toggle* off a list — the DNS user-rules row is exactly this — must read the list, decide membership itself, and issue the call only when it would change something. Re-issuing on a stale read silently corrupts the list rather than no-opping.

**Emptying the list is safe, and the echo says otherwise.** The file gets a proper `filters: []`, which `Config::list_at` reads as `Some(vec![])` and a row renders as *off*, correctly. What the command *prints* is `filters:` with nothing after it — the echo's rendering of an empty sequence, not the bytes it wrote.

That gap is worth stating because an earlier revision of this section got it wrong in exactly that way: the echo was read as the outcome, and a whole paragraph was written about a null the file never contained. **Read the file, not the confirmation** — the rule this document already gives for `config set` applies just as much to a command whose echo happens to look like YAML.

A bare `filters:` *is* still reachable, by hand edit, and it reads as `Yaml::Null`, which `list_at` — matching `Yaml::Array` only — answers `None` for. Since `None` is this crate's "unreadable" answer, a membership test should read null and absent as *"the list does not contain this"* and reserve `None` for a scalar or a mapping, which genuinely cannot be a list. `Config::lists` does that; it is a smaller point than the retracted one, but it is the right behaviour for a file the CLI invites the user to edit.

**The `--` guard is mandatory here too**, and for the same reason: without it a value beginning with `-` is read as an option and the command exits 1 with `<value> is required` on stderr, writing nothing. `Cli::config_list` applies it unconditionally, exactly as `Cli::config_set` does. Note the usage dump also reveals `list-add` accepts **up to three values** in one call and carries a `--list-file` option. Use one value per call regardless: a three-value call whose middle value is refused cannot be attributed.

**The refusal for a scalar key names the remedy.** `This field is not a list setting` followed by advice to use `config set` — the mirror image of what `config get` says about a list key. Between the two, the key's class is always discoverable at runtime, which is how `dns_filtering.upstream`, `.fallbacks` and `.bootstraps` were settled: all three answer `config get` with a value and refuse `list-add`, so they are **scalars**, and the "space-separated list" their comments describe lives *inside* one string. They are ordinary `config set` writes.

Those three are also **validated**, unlike most string settings — an empty value is refused with ``Invalid value for key `dns_filtering.bootstraps`. Valid values are: 'default' or space-separated list of IP addresses or DNS URLs with resolved IPs (Empty value)``. So the CLI's own sentence is worth surfacing rather than pre-empting with a weaker rule of ours.

### `listen_address` needs authentication *fully configured* first

`architecture.md` §5 requires forcing `listen_auth` on when the listen address leaves loopback. Measurement turns that from a fix-up into a **precondition**: with auth off, the command above prompts for a username, finds no TTY, and silently no-ops. Enabling `listen_auth.enabled` first makes the identical command succeed:

```
$ adguard-cli config set listen_auth.enabled true
$ adguard-cli config set listen_address 0.0.0.0
listen_address = 0.0.0.0
Config has been updated                            <- and this time the file really changed
```

The order is load-bearing. `config::listen_address_plan` returns the calls in it; reversed, the second silently does nothing while reporting success.

**Enabling authentication is necessary but not sufficient.** The prompt appears unless authentication is on *and* both credentials are non-empty:

| `listen_auth.enabled` | `username` | `password` | `config set listen_address 0.0.0.0` |
| --- | --- | --- | --- |
| `false` | `admin` | `admin` | prompts → no-op |
| `true` | `''` | `admin` | prompts → no-op |
| `true` | `admin` | `''` | **prompts → no-op** |
| `true` | `admin` | `admin` | succeeds |

The third row is the trap, and it is why `listen_address_plan` returns a *plan* rather than a list of calls: on a machine with a blank password, "enable auth, then write the address" reports success twice and changes nothing. No ordering fixes it, and inventing a password on the user's behalf would be a security decision made behind their back — one they could never log in past. So the plan refuses, and names the credential that is missing.

Naming it ourselves is necessary because **the CLI's own advice is wrong in that case**: it prompts for a *username* whichever credential is empty, and always suggests `config set listen_auth.username`. Following that would not fix an empty password.

The emptiness test is literal, not a trim — a username of `' '` satisfies it and the write goes through. `Config::credential_set` mirrors that exactly, since its only job is to predict the CLI.

**Retreating to loopback is always allowed.** Measured from every broken starting state — exposed with auth off, with an empty username, with an empty password — writing a loopback address succeeds and never prompts. The trigger is the **new** value, not the old one: already sitting on `0.0.0.0` with auth off, a move to `192.168.1.10` still prompts, while a move to `127.0.0.2` does not.

That asymmetry is load-bearing for the UI. A user who is exposed with unusable credentials can always be brought back to safety, so the retreat must never be gated behind the checks that guard exposure.

Add `config set listen_address <non-loopback>` to the TTY-requiring list in [§7](#7-commands-that-need-a-tty).

### Nothing enforces dependencies between settings

`config set` will happily accept any key in any order, including combinations the file's own comments call invalid. Two that matter:

- `https_filtering.encrypted_client_hello` and `filter_secure_dns_mode` are documented as *"Requires dns_filtering to be enabled"*, but both can be set while `dns_filtering.enabled` is `false`.
- `dns_filtering.enabled: true` does nothing in `manual` proxy mode unless `dns_filtering.listen_port` names a real port — the file says *"N = listen on port N (e.g. 5353) — required for DNS filtering in manual proxy mode"*, and `-1` is the default. The switch reads on and filters nothing.

The GUI has to own these. `Config::dns_filtering_is_inert` drives the caveat on the DNS filtering row.

### The DNS listener binds loopback, and needs both keys

Measured on the real licensed install in `manual` proxy mode, restoring to a byte-identical baseline afterwards. This is the measurement `architecture.md` §5 demanded before the DNS page's listen-port row could ship, and it retires the hedge that went with it.

**The dependency is symmetric.** With `listen_port: 5353` but `enabled: false`, a restart brings up **no listener at all** and `status` reads `Manual DNS proxy is disabled`. So a port without the switch is exactly as inert as the switch without a port — `dns_filtering_is_inert` models only the second direction, and a page offering the port must say what the other half is doing.

**The listener binds `127.0.0.1`, and does not follow `listen_address`.** With both keys set it appears on UDP *and* TCP:

```
udp UNCONN 127.0.0.1:5353   adguard-cli
tcp LISTEN 127.0.0.1:5353   adguard-cli
```

Moving `listen_address` to `127.0.0.2` — still loopback, so no authentication precondition and no exposure — separates the two:

```
tcp LISTEN 127.0.0.2:3129   <- HTTP proxy followed listen_address
tcp LISTEN 127.0.0.2:1081   <- SOCKS5 followed listen_address
udp UNCONN 127.0.0.1:5353   <- the DNS proxy did not
tcp LISTEN 127.0.0.1:5353   <- the DNS proxy did not
```

`status` says the same thing in words: `HTTP proxy is listening on 127.0.0.2:3129` alongside `Manual DNS proxy is listening on 127.0.0.1:5353`.

So the DNS listener is **pinned to loopback** and cannot be moved off it by any setting the UI exposes, `listen_address: 0.0.0.0` included. The listen-port row therefore needs **no confirmation dialog and no standing warning** — it is incapable of exposing anything, which is the opposite of the assumption `architecture.md` §5 was written under.

**`status` is the better evidence than the file for this one row.** It carries a third line whichever state the proxy is in:

```
Manual DNS proxy is disabled
Manual DNS proxy is listening on 127.0.0.1:5353
```

`proxy.yaml` records what was asked for; this records what the daemon did. The two disagree until a restart — the file moves immediately, the listener does not — so a row that re-reads only the file will claim a listener that is not yet there.

Range-checking is ours here as everywhere: `config set dns_filtering.listen_port` accepts `70000` and `3.5`, and the float then makes `Config::int_at` read nothing at all, so a value the CLI itself accepted renders as unavailable.

### `show_notifications` is a desktop notification, and the mechanism is a shell-out to `gdbus`

`proxy.yaml` says only *"show protection status notification"* and names no mechanism, which is why the key sat in `architecture.md` §5's *cannot be classified* table. `handoff.md` §3 item 8 posed the fork: a **desktop** notification, which collides with this app's own tray and makes the key a design question, or **terminal output**, which belongs beside `show_hints` and stays out. **Measured 2 August 2026**, from the shipped binary and this machine's state. It is a desktop notification — but the half worth reading is what is still *not* measured, at the end.

**The binary is statically linked and stripped**, so `ldd` reports nothing and no `libnotify` is involved anywhere:

```
$ file $(readlink -f ~/.local/bin/adguard-cli)
ELF 64-bit LSB executable, x86-64, statically linked, stripped
```

**There is exactly one notification mechanism in it, and it is a subprocess.** Two strings, adjacent in `.rodata`:

```
{}: Send notification failed: environment incomplete (geteuid={}, SUDO_USER={}, DISPLAY={}, DBUS_SESSION_BUS_ADDRESS={})
{}env DBUS_SESSION_BUS_ADDRESS={} DISPLAY={} gdbus call --session --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications --method org.freedesktop.Notifications.Notify 'adguardvpn_cli' {} '' {} {} [] {{}} {}
```

The second is a format string for a **command line**, not a D-Bus binding — AdGuard builds it and runs `gdbus`. Its arguments land exactly on `Notify`'s signature, which is what makes the reading certain rather than suggestive:

| `Notify` parameter | What AdGuard passes |
| --- | --- |
| `app_name` | `'adguardvpn_cli'`, hard-coded — the **VPN** CLI's name, in AdGuard CLI |
| `replaces_id` | `{}`, and it reads the id back out of `gdbus`'s stdout to replace its own previous notification: `Failed to parse notification ID from output: '{}'` |
| `app_icon` | `''` — **empty**, so the notification carries no icon |
| `summary`, `body` | `{}`, `{}` |
| `actions` | `[]` — none, so it is not clickable |
| `hints` | `{{}}` — an empty dict, escaped for the formatter |
| `expire_timeout` | `{}` |

Three consequences a UI has to know. **`gdbus` is an undeclared runtime dependency** — it ships with glib2 and is `/usr/bin/gdbus` here, but nothing in AdGuard's packaging asks for it, and without it notifications fail into `Send notification error`. The notification is attributed to **`adguardvpn_cli`**, which is the name a user sees in their notification centre *and* in their per-application notification settings. And the leading `{}` before `env`, together with the `geteuid`/`SUDO_USER` pair in the failure message, is a de-escalation for `sudo adguard-cli …`: the call needs a *session* bus and root does not have the user's.

**The environment it demands is complete on this machine**, so none of the above is theoretical here:

```
$ command -v gdbus
/usr/bin/gdbus
$ tr '\0' '\n' < /proc/$(pidof -s adguard-cli)/environ | grep -E '^(DISPLAY|DBUS_SESSION_BUS_ADDRESS)='
DISPLAY=:0
DBUS_SESSION_BUS_ADDRESS=<set — masked, it is a socket path>
$ ps -o euid= -p $(pidof -s adguard-cli)
1000
```

**The protection wording exists, and never reaches a log.** `Protection started` and `Protection stopped` sit in the proxy-server region of the binary, among `start_dns_proxy`, `stop_dns_proxy`, `init_proxy_server` and `setup_auto_proxy`. Against the four log files, one of which records three clean `AGProxyServer::stop()` cycles on the day of measurement:

```
$ grep -c "Protection start\|Protection stop" ~/.local/share/adguard-cli/logs/*.log
app_nm.log:0   access.log:0   app.log:0   proxy.log:0
```

Neither string carries the `{}: ` component tag every log format string around them has, which corroborates it — but only corroborates: two further untagged strings, `Protection manager not available` and `Protection reload failed, please start AG CLI again to restore`, are plainly CLI error messages, so an absent tag is not by itself a mark of a notification.

So **there is no terminal or log path carrying protection status to a user at all.** Whatever `show_notifications` gates, the only protection-status message this binary has goes out over D-Bus — which answers the fork without needing the key's wiring, and puts it on the design side.

**The wiring was an inference for about twenty minutes, and is now measured.** The paragraph that stood here said the key gating *that call* could not be concluded from strings, because the mechanism has **more than one caller** — a fourth untagged string, `` Language filter `{}` has been added automatically ``. That caution was right and it is what made the following experiment worth running rather than skipping.

**Measured 2 August 2026**, in the authorised sandbox run, with a shim `gdbus` early on `PATH` capturing the call instead of raising a notification. Two legs, one variable, both on a data directory short enough that the daemon started cleanly:

| `show_notifications` | Daemon | `Notify` calls captured |
| --- | --- | --- |
| `true` | 400-line start, 2 listeners | `Protection started`, `Protection stopped`, **and** `` Language filter `AdGuard French filter` has been added automatically `` |
| `false` | 201-line start, 2 listeners, 0 socket errors | **none — the shim was never invoked at all** |

The captured argument vector, verbatim, which is what the table above was decoded from:

```
adguardvpn_cli · 0 · '' · "AdGuard CLI" · "Protection started" · [] · {} · 5000
```

So `summary` is the constant **`AdGuard CLI`** and `body` carries the event; the timeout is **5 s**; and `replaces_id` is `0` on each call, so the "read the id back and replace" path exists but is not being used for these.

**Two things follow that the key's own comment does not say.** It gates **every** caller of the shared mechanism, not just protection status — the language-filter announcement went silent with it, and `proxy.yaml` describes the key only as *"show protection status notification"*. And a UI that offers this switch is therefore also the only control over `auto_enable_language_filters`' notification, which is the one thing that would otherwise tell a user their filter list had been changed for them.

**One smaller fact** for whoever takes it: `adguard_cli_nm` carries the same mechanism and neither `Protection` string, while `adguard_root_helper` carries neither — so a notification never originates in the privileged process.

### A change may not reach the running proxy

Two strings in the binary — *"To apply changes, you need to restart the proxy server by running `… restart`"* and *"Failed to apply settings to running proxy server"* — mean the daemon could not take the setting live. They appear only while the proxy is running, so they are absent from every capture above. `Applied::restart_required` carries this up to a toast rather than swallowing it.

---

## 6. Do not parse `filters list`

The table is fixed-width with a ~40-character title column, and **long titles overflow and collide with the next field**:

```
    |          247 | Polish Anti-Annoying Special Supplement Filter is not added
    |          216 | Official Polish filters for AdBlock, uBlock Origin & AdGuard Filter is not added
```

Row 216's title is 62 characters. There is no delimiter between title and status, and no way to recover the boundary positionally. The `|` separators only delimit the leading checkbox and ID columns.

**Read filter state from SQLite instead.** `agflm_standard.db` (HTTP filters) and `agflm_dns.db` (DNS filters) are plain SQLite 3 databases in the data directory:

| Table | Rows (standard / dns) | Use |
| --- | --- | --- |
| `filter` | 86 / 65 | The catalogue and its state |
| `filter_group` | 8 / 5 | Categories (`group_id`, `name`, `display_number`) |
| `filter_localisation` | 3828 / 1900 | Translated `name`/`description` per `lang` |
| `filter_group_localisation` | — | Translated category headings per `lang` |
| `filter_tag`, `filter_filter_tag`, `filter_tag_localisation` | 71 / 19 | Tagging |
| `filter_locale`, `filter_includes` | — | Language targeting; filter composition |
| `rules_list`, `diff_updates`, `metadata` | — | Internal |

`filter` columns:

```
filter_id, group_id, version, last_update_time, last_download_time,
display_number, title, description, homepage, license, checksum,
expires, download_url, subscription_url,
is_enabled, is_installed, is_trusted, is_user_title, is_user_description
```

That is everything the filter browser UI needs — including `is_enabled` / `is_installed` / `is_trusted` state, grouping, homepages, and localised names for free.

### Two `i32::MIN` sentinels

The schema uses `-2147483648` (`i32::MIN`) for two *different* special cases. Both were found by an integration test asserting referential integrity, and both will corrupt a naive filter list:

| Sentinel | Meaning | Trap |
| --- | --- | --- |
| `filter_id = -2147483648`, title **"User rules"** | The user's own rules (`user.txt` / `dns_user.txt`) | Its `group_id` is **0**, which does **not** exist in `filter_group`. It also has an empty `download_url`, and in `agflm_dns.db` it is `is_enabled = 1` while `is_installed = 0`. |
| `group_id = -2147483648`, name **"Custom filters"** | Lists the user installed by URL | This group *is* real and present in `filter_group` — do not filter it out. |

Consequences, both encoded in `adguard-core`:

- **`is_enabled` implies `is_installed` for every real filter, but not for the user-rules row.** Verified across both databases: no real filter is enabled-but-not-installed.
- Any join or lookup of `filter.group_id` against `filter_group` must exclude the user-rules row first, or it will fail to resolve group 0.

`Catalogue::filters()` therefore excludes `filter_id = Filter::USER_RULES_ID` and exposes it separately via `Catalogue::user_rules()`, since it belongs in the UI as a "your own rules" toggle rather than as a subscribable list.

### Localisation tags are POSIX, not BCP-47

`filter_localisation.lang` and `filter_group_localisation.lang` use an underscore and an uppercase region — `en`, `pl`, `pt_BR`, `pt_PT`, `es_ES`, `zh_TW` (44 languages in `agflm_standard.db`, 34 in `agflm_dns.db`). A hyphenated `pt-BR` matches **nothing**, and because a missing translation is not an error, the failure is silent: every name quietly falls back to English.

So a locale must be normalised before use, and looked up twice — full tag, then bare language — since region-specific rows are the exception. `agflm_standard.db` also has one `filter` row with no `en` row at all (the user-rules pseudo-filter), so the English `filter.title` column remains the last fallback. This is [`locale::Locale`](../crates/adguard-core/src/locale.rs), and the two lookups are the two `LEFT JOIN`s in `filters.rs`.

### Writing filter state

Measured on v1.4.13. All four commands exit **0**, including the failures.

| Invocation | stdout | Effect |
| --- | --- | --- |
| `filters add 3` (not installed) | `Filter [Title: …] added` + `Filter [Title: …] enabled` | `is_installed=1`, `is_enabled=1` |
| `filters add 2` (already installed) | the same two lines | nothing — the message is not evidence |
| `filters add 99999` | `All specified filters have already been added or do not exist` | nothing |
| `filters enable 3` (not installed) | `Before filters can be enabled, they must be added` | **nothing** |
| `filters enable 3` (installed) | `Filter [Title: …] enabled` | `is_enabled=1` |
| `filters disable 3` | `Filter [Title: …] disabled` | `is_enabled=0`, `is_installed` **stays 1** |
| `filters remove 3` | `Filter [ID: 3, Title: …] removed` | `is_installed=0` |

Consequences for a switch-per-filter UI:

- **Turning a switch on is not always `enable`.** For a filter that was never added it must be `add`, which adds *and* enables in one step. `Filter::action_for` encodes this.
- **Turning a switch off is `disable`, never `remove`** — off should not silently unsubscribe.
- **`add`'s confirmation is unreliable**: it prints the same two lines whether it did anything or not. Since it cannot distinguish a no-op, success must be read from the database, not the message.
- The confirmation shape is `Filter [<something>] <verb>`. Matching that positively — and treating every other shape as failure — is the only way to tell the refusals apart from the successes, since both exit 0.

**These four commands are not the only writer, and this section used to imply they were.** Measured 2 August 2026: `auto_enable_language_filters` — top-level in `proxy.yaml`, shipped `true`, commented *"Enables filters based on the query language and system locale"* — makes AdGuard add filters on its own, and the binary carries the runtime string ``Language filter `{}` has been added automatically``. So filter state can move with no command from us and no action by the user, which a UI that patches rows on toggle and re-reads only on demand will not see. It **only ever adds**; nothing here disables.

Two measured facts for whoever builds against this:

- **The database records no user provenance for filter state.** `pragma table_info(filter)` gives `is_user_title` and `is_user_description` — provenance for a filter's *name* — and nothing equivalent for `is_enabled` or `is_installed`. So *"off because the user chose off"* is indistinguishable from *"off because it was never on"*, to us and to anything else reading the file.
- **The language-targeting tables are not the localisation tables, and nothing in this workspace reads them.** `filter_locale` holds **39 rows, 38 distinct tags, every one exactly two characters and none with an underscore** — so a POSIX tag like `en_US` cannot match, and neither `en` nor `en_US` appears at all. That is a different vocabulary from `filter_localisation`, which is what *Localisation tags are POSIX, not BCP-47* above is about and what `locale.rs` serves. Do not reach for `locale.rs` here.

**What remains unmeasured is whether the automatic add respects a filter the user turned off**, which is the question a UI row for this key has to answer before it can exist. `architecture.md` §5 has the reasoning and `handoff.md` §3 item 12 has the blocker.
- The database is updated **immediately**, and while the proxy is **stopped**. No restart is needed for the UI to observe a change.
- Negative IDs need no `--` guard: `filters enable -2147483648` parses as a positional, not a flag, and resolves to `User rules`. (`filters enable 'User rules'` works too — the argument is `TEXT`, matched against ID *or* title.)

**The DNS user-rules row cannot be enabled this way.** In `agflm_dns.db` it is `is_enabled=1, is_installed=0`, and `dns filters enable -2147483648` is refused with *"Before filters can be enabled, they must be added"*. Its real switch is the presence of `dns_user.txt` in the `dns_filtering.filters` list in `proxy.yaml` (`user.txt` sits in the top-level `filters` list the same way), which means `config list-add` / `list-remove`. The HTTP row does not have this problem — it is `is_installed=1`, so `filters enable|disable` drives it. Solve the DNS case when the DNS page lands.

**Open these read-only** (`file:...?mode=ro`, `rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY`). They are the daemon's live databases; never write to them. All mutations go through `filters add|remove|enable|disable|set-trusted|set-title`.

Opening read-only is also verified not to create `-wal`/`-shm` side-car files next to the daemon's databases.

`sqlite3` CLI is not installed on this machine, but that is irrelevant to us — `rusqlite` bundles its own SQLite. It only matters for manual inspection (use `python3 -c` with the stdlib `sqlite3` module).

### Installing a custom filter

`filters install` subscribes to a list AdGuard's catalogue does not carry. Measured on v1.4.13 against a sandbox with a lent licence, for both sets — `dns filters install` behaves identically, minus `--trusted`, which the DNS subcommand does not offer.

```
Usage: adguard-cli filters install [OPTIONS] <filter-url>
  <filter-url> TEXT REQUIRED  Enter the filter URL or path to a local file to install
  --trusted                   Indicate that the filter is trusted
  --title TEXT                Set title for custom filter
```

| Invocation | Exit | Stream | Output | Effect |
| --- | --- | --- | --- | --- |
| a list with a `! Title:` header | 0 | stdout | `Filter [Title: Claude Probe List] from URL: <url> installed` | a new row, `is_enabled=1`, `is_installed=1` |
| a real `https://` list | 0 | stdout | the same, in 0.66 s | as above |
| a list with **no** `! Title:` header | 0 | stdout | `Filter [Title: <the url>] …` | a new row whose `title` column is **`''`** |
| `--title X` | 0 | stdout | echoes `X` | `title = 'X'`, overriding the header |
| `--trusted` | 0 | stdout | unchanged | `is_trusted = 1` |
| the **same URL** again | 0 | stdout | `Filter with the specified URL already exists:` + a `filters list` table | nothing |
| content starting `<html` or `<!DOCTYPE` | 0 | stdout | `Failed to install the filter from URL: <arg>` | nothing — the one content check there is |
| JSON, prose, an empty file, comments only | 0 | stdout | `… installed` | **installed**, holding no rules |
| a path that does not exist | 0 | stdout | `Failed to install the filter from URL: <arg>` | nothing |
| HTTP 404 | 0 | stdout | the same sentence | nothing |
| connection refused | 0 | stdout | the same sentence | nothing |
| an unresolvable host | 0 | stdout | the same sentence | nothing |
| `just some words` | 0 | stdout | the same sentence | nothing |
| a server that accepts and never replies | 0 | stdout | the same sentence, **after 60 s** | nothing |
| a value beginning with `-`, no `--` | **1** | **stderr** | `<filter-url> is required` | nothing |
| unlicensed | **1** | **stderr** | the usual complaint and usage dump | nothing |

Nine things follow, and four of them are traps.

**Success keeps the house shape, so the existing matcher already covers it.** The confirmation is `Filter [<something>] installed`, which is exactly the `Filter [` … `<verb>` form [§6 above](#writing-filter-state) defines — `cli::confirms(&stdout, "installed")` needs no special case. Neither refusal can be mistaken for it: the duplicate begins `Filter with the specified URL`, not `Filter [`, and the failure sentence begins `Failed`. A URL that happens to end in the word *installed* is safe for the same reason.

**Every failure is the same sentence.** A 404, a refused connection, a DNS failure, a missing file and a string that was never a URL are indistinguishable in the output — `Failed to install the filter from URL: <what you passed>`, at exit 0, on stdout, echoing the raw argument rather than the normalised one. So the UI cannot explain *why* an install failed, and must not pretend to. Say that the list could not be fetched and show the CLI's sentence.

**It has a 60-second deadline of its own.** Measured against a socket that accepts the connection and then never answers: the command returns the ordinary failure sentence at exit 0 after 60 s. That is *inside* [`NETWORK_TIMEOUT`](#10-wrapper-layer-checklist)'s 120 s, so the wrapper's deadline is a backstop that should never fire, and the normal worst case is a clean refusal a minute later — which is still far too long to leave a button looking idle.

**The only thing checked about the content is whether it starts with HTML.** Measured across nine bodies: `<html…` and `<!DOCTYPE html>` are refused, leading whitespace and all, while *the same HTML placed after one line of ordinary text is accepted*. Everything that is not an HTML document installs — JSON, prose, a file of blank lines, a file of nothing at all — as a filter list holding no rules, reporting success.

So this is a sniff for "did I get an error page instead of a list", not validation of filter syntax, and it is worth knowing in both directions. It catches the single likeliest accident, a link that 200s with a friendly HTML error. It catches nothing else: a link to a JSON API or the wrong plain-text file leaves the user subscribed to a filter that filters nothing, with no error anywhere and a switch reading *on*.

An earlier revision of this section said content was *never* validated, generalised from one probe file that happened to open with a line of prose before its HTML. The sample was the thing that was wrong, not the reasoning — which is the same lesson [§3](#exit-1-is-usually-our-bug-but-not-always) already records twice about single-line and single-stream measurements, arriving this time as a single *fixture*. `filters_sandbox::html_is_the_one_thing_rejected` pins the boundary from both sides so the next revision has to argue with a test.

**Deduplication is by URL string, not by content.** The same list installed once as `file://…` and once as `http://…` yields two enabled rows. Conversely the *second* install of one URL is **refused**, which is the opposite of `config list-add`'s silent duplicate ([§5](#list-writes-list-add-and-list-remove)) — so this one command may be issued speculatively.

**The echo says a title the database does not have.** A list with no `! Title:` header is confirmed as `Filter [Title: file:///…/untitled.txt]` while the `title` column is set to the empty string. The localised name in [§6](#6-do-not-parse-filters-list) then falls back through `COALESCE` to that same empty string — custom filters have no `filter_localisation` rows at all — so the row renders with **no name whatsoever** unless the UI supplies its own fallback. This is the section's own rule landing in a new place: the confirmation is not the effect, even when the confirmation is the friendlier of the two.

**Custom filters get negative IDs from `-10001` downwards, and they are never reused.** Distinct from the user-rules sentinel `i32::MIN`, and negative here needs no `--` guard either — `filters disable -10001` parses as a positional, exactly as `-2147483648` does. Every custom row has `display_number = 0`, so their order within the group is whatever SQLite returns; a stable list needs a secondary sort of the UI's own. The group itself (`group_id = i32::MIN`, *"Custom filters"*) is real, present in `filter_group` in both databases, and carries `display_number = 0` — so it sorts **above** *Ad blocking*, and an installed custom list appears at the top of the page.

**`filters remove` on a custom filter deletes the row outright.** For a catalogue filter, `remove` only clears `is_installed` and the row stays (§6 above). For a custom one the row is gone from `filter` entirely — `Filter [ID: -10004, Title: …] removed` — which makes it the one genuinely destructive filter operation and not a mirror of `install`. There is no undo but re-fetching the URL.

### Removal, measured from both sides

Pinned by `filters_sandbox::removing_a_custom_filter_deletes_the_row` and `…::removing_a_catalogue_filter_only_uninstalls_it`, because this asymmetry is what a confirmation dialog is built on and it had only ever been measured in one direction:

| Case | Result |
| --- | --- |
| `remove` a custom row that is **enabled** | the row is gone from `filter` |
| `remove` a custom row that is **disabled** | also gone — "off" does not already mean removed |
| two custom rows, one removed | only that one goes; the other keeps its id and URL |
| `remove` a **catalogue** filter | the row **survives**, with `is_installed=0` *and* `is_enabled=0` |
| `remove -99999` (never existed) | refused: `Failed to remove filter with ID: -99999: Filter not found`, and nothing else is touched |
| install the same URL again after removal | succeeds, with a **fresh id** |

Three things worth taking from that table.

The catalogue leg clears **both** flags. §6's own table above says `is_installed=0` and is silent about `is_enabled`; measured, a filter that was enabled comes back `enabled: false, installed: false`. Nothing in this UI depends on the difference — a row that is not installed renders off either way — but the table was incomplete rather than wrong, and an incomplete measurement is how the last three corrections in this document started.

The absent-id refusal is the one a UI will actually hit: two windows open, or a stale page, and the user presses remove on a filter that is already gone. It arrives as `Error::Refused`, which the wrapper already maps, so the failure path needs no new handling — only wording that does not claim the filter is still there.

**Ids are never reused, so removal is not undoable by id.** Re-installing the URL brings the list back as a *new* row: `-10001` removed and re-fetched came back as `-10003`. Anything holding an id across a removal — a pending write, a row widget, an undo affordance — is holding a dangling reference, which is the concrete reason this action is confirmed up front rather than offered as an undo afterwards.

**And custom rows sort newest-first.** `Catalogue::custom_filters` orders by `filter_id` ascending while custom ids *descend* from `-10001`, so index 0 is the most recently installed list. That is stable, unlike the `display_number = 0` ordering warned about above, but it is the opposite of the order they were added in — worth knowing before indexing into that list in a test or a UI.

**`proxy.yaml` is not touched.** Its `filters` list still reads `['flm://', 'user.txt']` after four installs; custom lists live only in the database, behind that `flm://` entry. So no `config list-add` is involved and nothing here needs the write path of [§5](#5-configuration-writes).

### Marking a custom filter trusted

`filters set-trusted` is the after-the-fact half of `install --trusted`: it lets a list already subscribed to use privileged rule types — scriptlets and `$$`/HTML filtering — which is to say it lets that list run script in the pages the user visits. Measured on v1.4.13, 6 August 2026, in a sandbox with a lent licence, for the UI control in [issue #2](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/2).

```
Usage: adguard-cli filters set-trusted [OPTIONS] <filter-id> <filter-trusted>
  <filter-id>      TEXT REQUIRED
  <filter-trusted> BOOLEAN REQUIRED
```

| Invocation | Exit | Stream | Output | Effect |
| --- | --- | --- | --- | --- |
| `set-trusted -10001 true` on a custom row | 0 | stdout | `Filter with ID: -10001 successfully updated trust` | `is_trusted = 1` |
| the same again | 0 | stdout | the same line | nothing — the message is not evidence |
| `set-trusted -10001 false` | 0 | stdout | the same line | `is_trusted = 0` |
| `1` / `0` in place of `true` / `false` | 0 | stdout | the same line | the same |
| on a custom row that is switched **off** | 0 | stdout | the same line | `is_trusted` moves, `is_enabled` stays 0 |
| `set-trusted 2 true` — a **catalogue** filter | 0 | stdout | `Failed to update trust filter with ID: 2: Filter not custom` | nothing |
| `set-trusted -99999 true` — never existed | 0 | stdout | `Failed to update trust filter with ID: -99999: Filter not found` | nothing |
| **`set-trusted -2147483648 false` — the user-rules sentinel** | 0 | stdout | the **success** line | **`is_trusted` really moves** |
| a value that is not a boolean | **1** | **stderr** | `Could not convert: <filter-trusted> = bogus` | nothing |
| the value omitted | **1** | **stderr** | `<filter-trusted> is required` | nothing |
| unlicensed | **1** | **stderr** | the usual complaint and usage dump | nothing |
| `dns filters set-trusted …` | **1** | **stderr** | `A subcommand is required` | **the subcommand does not exist** |

Six things follow, and the fourth is the trap.

**The confirmation is a shape of its own, and the house matcher is blind to it.** Every other filter command answers `Filter [<something>] <verb>` — the form [*Writing filter state*](#writing-filter-state) defines and `cli::confirms` matches. This one answers `Filter with ID: <id> successfully updated trust`. `Filter with ID:` is not `Filter [`, so `confirms` returns **false for a command that worked**, and a wrapper reusing it would report every successful change as a refusal. `cli::confirms_trust` exists for this one command and is anchored at both ends, since the refusals carry the same id in the same place and differ only in how the line opens and closes.

**Trust is orthogonal to the switch, in both directions.** A switched-off list can be trusted; trusting one does not switch it on; and `disable` then `enable` leaves the flag where it was. So a row's `is_trusted` cannot be inferred from anything the switch did, and `Catalogue::state` re-reads all three flags on every reconcile rather than patching the one that was written.

**AdGuard enforces "custom only" itself, for the case it can see.** `Filter not custom` is the CLI refusing a catalogue filter, which is the same rule `Filter::supports_trust` applies before offering a control. That is a rare piece of luck in this contract — two of the three cases need no guard of ours.

**The third case has no guard at all, and it writes.** `filter_id = -2147483648` is the user-rules pseudo-filter, and `set-trusted` treats it as custom: it ships `is_trusted = 1`, and setting it to `false` **moved the flag**, reporting the ordinary success line while doing it. What that turns off is the scriptlet and HTML rules in the user's own `user.txt`. Nothing downstream can catch it — not the confirmation, and not the re-read, which would faithfully report the flag that had just been cleared. So `Cli::filters_set_trusted` refuses this id **before spawning**, as `Cli::configure` refuses an existing `proxy.yaml`, and for the same stated reason: a guard beside a call site is one somebody can add a second call site around. `filters_sandbox::trust_is_refused_for_everything_that_is_not_a_custom_list` asserts the sentinel's flag is unmoved either side, so the guard leaking is a failing test rather than a silent one.

**The DNS set cannot do this at all.** Not "should not" — `adguard-cli dns filters` has no `set-trusted` in its help and asking for it exits 1 at the argument parser. DNS lists are hostname-only and the privileged rule types do not reach them, which is why `Cli::filters_set_trusted` takes no `FilterSet` argument to be wrong about.

**And it is licence-gated — like every other filter command, which is not what this section first said.** The claim here was originally *"licence-gated, where the switch commands are not"*, reasoned from the fact that `install` is gated and never measured. It is wrong. Measured 6 August 2026 in a sandbox whose `adguard.conf` was moved aside: `add`, `enable`, `disable`, `remove`, `set-title` and `set-trusted` **all** exit **1** with `You need to activate an AdGuard license to use this command` on stderr, and none of them writes. So there is no state where a list can be switched but not trusted, and no asymmetry for the UI to handle — it arrives as `Error::Unlicensed`, which the wrapper maps generically, and the page shows the CLI's own sentence.

That correction is [§3](#3-exit-codes-are-only-half-trustworthy)'s lesson landing in a new place, and it is worth naming because of *where* the wrong version was: not in a table of measurements, but in a sentence of prose next to one. The table beside it was right. **A measured document grows unmeasured claims at its edges**, in the connective sentences that explain what the measurements mean, and those are exactly the sentences nobody re-measures.

### What the trust flag actually gates, and when it takes effect

Measured 6 August 2026, authorised, in a scratch `$XDG_DATA_HOME` in **manual** proxy mode on port 3199, against `http://example.com/` over plain HTTP so no certificate is involved. Four rounds; the first two were wrong and are the reason the rest exist.

**Two questions, and the answers are not the ones the section above assumed.**

The probe is the difference between rule classes. A list carrying an element-hiding rule (`##`, not privileged) and a privileged rule is installed, and the page is fetched through the proxy in each trust state, live and after a restart. `##` is the control: it must fire in every state, or the probe is broken rather than the flag.

| round | probe rule | list source | result |
| --- | --- | --- | --- |
| 2 | `example.com$$h1` | `file://` | fired in **all four** states |
| 3 | `example.com$$h1` | `http://` | fired in **all four** states |
| 4 | `example.com#%#//scriptlet(…)` | `http://` | gated — see below |

**1. `$$` HTML-filtering rules are not gated by `is_trusted` at all.** Sixteen fetches across two rounds: the `<h1>` was stripped from the response with `is_trusted = 0`, including from a proxy started fresh with the flag at 0, and from a list fetched over `http://` as well as one installed from a local file. The origin was re-fetched every round, so this is against what `example.com` was serving at the time and not a remembered copy. This contradicts what AdGuard's own documentation implies, and it is what the measurement says on v1.4.13.

**2. Scriptlets *are* gated, and the flag is read only at start.** With the `##` control present in every row — so the probe is known good — the scriptlet appears in the injected content-script payload only in the states reached by a restart:

| trust state | proxy | scriptlet in payload | payload |
| --- | --- | --- | --- |
| untrusted | started with the flag at 0 | absent | 384 518 B |
| **trusted** | **left running** | **absent** | 384 518 B |
| trusted | restarted | **present** | 386 915 B |
| **untrusted** | **left running** | **present** | 386 915 B |
| untrusted | restarted | absent | 384 518 B |

The payload is byte-identical within each state, which is the shape of a filter set compiled once at start. Waiting did not help: 5 s, 15 s and 30 s after the change all read the same.

**So the flag is read when the proxy starts and not again, in both directions — and the withdrawing direction is the one that matters.** Granting trust to a running proxy is inert until it restarts, which is merely surprising. **Taking trust back from a running proxy is also inert**, which means a user who has just decided a list should no longer run script in their pages still has that list's scriptlets running in them. Nothing in the CLI's output says so: unlike `config set`, which reports `restart_required` for itself, `set-trusted` prints its ordinary success line and nothing else.

That is why the Filters page raises *"Restart the proxy to apply this change"* — the same sentence the Protection and Advanced pages use for the same class of fact — and why the confirmation dialog says when the grant takes effect instead of promising it can be withdrawn at any time, which is what it said before this was measured.

**A trap that cost two rounds, and it is not about AdGuard.** A sandbox `$XDG_DATA_HOME` more than about 70 characters deep **cannot start a proxy at all**: `agcli.socket` lands past `sockaddr_un`'s 107-byte `sun_path` limit and the daemon fails with

```text
CONTROL_SOCKET create_sockaddr_un: Socket name length 144 exceeds maximum allowed length 107. The name will be truncated
SERVICE_FACADE start_internal: Failed to init control socket: Create listener error
```

surfacing as the generic `Failed to start proxy server: An unknown error has occurred`, which is the same sentence [§11](#11-a-proxy-the-cli-has-lost-track-of) documents for a wedged process and has nothing to do with one. `filters_sandbox.rs` is unaffected — it builds its root under `std::env::temp_dir()` — but any harness that puts a sandbox under a long path will meet this, and the error names neither the path nor the length as the cause.

**And a probe needs its control checked first.** Round four's initial run reported the scriptlet absent in every state, which reads exactly like a flag that gates nothing — but the `##` control was absent too, and that is what gave it away: a stray `python3 -m http.server` from the previous round still held the port, so the list under test was the *previous* round's. The finding would have been the opposite of the truth. The control is what caught it, and it caught it in the first row.

### `auto_enable_language_filters` keys on *installation*, so a `disable` survives it and a `remove` does not

The question `handoff.md` §3 item 12 opened and could not answer from the database: **does the automatic add respect a filter the user turned off?** It cannot be read off the schema, because there is no column in which *"off because I chose off"* could be written — `filter` has `is_user_title` and `is_user_description` and nothing equivalent for `is_enabled` or `is_installed` (confirmed against the schema, 19 columns).

**Answered 2 August 2026** in the authorised sandbox run, with both asymmetries under test at once so one traffic run settles both. Setup, on a licensed scratch install seeded by copy:

```console
$ adguard-cli filters add 6   && adguard-cli filters disable 6    # German — disabled, still installed
$ adguard-cli filters add 16  && adguard-cli filters remove 16    # French  — removed
```

Then 64 requests to German and French pages through the sandbox proxy over ~6 minutes, in 8 rounds, and the `filter` table read directly either side:

| `filter_id` | Title | Before (`is_enabled`/`is_installed`) | After | Verdict |
| --- | --- | --- | --- | --- |
| 6 | AdGuard German filter | `0` / `1` — **disabled** | `0` / `1` | **untouched** |
| 16 | AdGuard French filter | `0` / `0` — **removed** | `1` / `1` | **re-added *and* enabled** |
| 224 | AdGuard Chinese filter | `0` / `1` | `0` / `1` | control, no matching traffic |

Stable across all eight rounds, and **independently corroborated by a second channel**: with `show_notifications: true`, the run raised `` Language filter `AdGuard French filter` has been added automatically `` over D-Bus and named no other filter (§5).

**So the add path keys on `is_installed`, not on `is_enabled`** — and the inversion item 12 predicted is real: **removing a list is less durable than disabling it.** A `disable` leaves `is_installed = 1`, the heuristic sees the list as present and leaves the switch alone; a `remove` clears it, and the heuristic puts the list back *and turns it on*. That is the opposite of the mental model the removal dialog is built around, where removal is the stronger of the two.

Two consequences for the UI, and the first is not about this row:

- **The removal dialog is now describing the weaker action.** It names the URL and asks for confirmation, which reads as the more serious choice; for a *language* filter with this setting on, it is the one that does not stick.
- **The row's subtitle has to carry the asymmetry**, because nothing else in the flow would: turning this on can restore lists you removed, and will not re-enable lists you disabled. That is one sentence and it is the whole reason the row needed a measurement before it could ship.

---

## 7. Commands that need a TTY

`configure` and `activate` are interactive and cannot be driven headlessly. So, conditionally, is one `config set`.

- **`configure`** — the wizard. This entry used to read *"the GUI reimplements it as a first-run assistant calling `config set`. Never invoke it."* Both halves were wrong, and for the same reason: until `configure` has run there is no `proxy.yaml`, and without one `config set` refuses every real key ([§5](#before-proxyyaml-exists-almost-nothing-works)). There is nothing to reimplement it *with*.

  So it is the **second** exception to the never-invoke rule, on the same grounds as `activate`: with stdin closed its no-TTY branch is the only branch, and that branch is defined, non-interactive and fast. Measured against a **licensed** directory with no `proxy.yaml` — exit **0**, everything on **stdout**, 0.10 s:

  ```text
  Warning: No TTY available. Using default values for configuration.
  Please enter the new value of the HTTP proxy listen port [default: 3129]:
  Warning: No TTY for user input. Using default value (3129). Use `adguard-cli config set listen_ports.http_proxy` to change.
  …
  Select filter list groups to enable (can be changed later):
  Warning: No TTY for user input. Skipping filter selection. Use `adguard-cli filters` to configure filters.
  The proxy server is ready to start. You can start it by running `adguard-cli start`
  ```

  It takes every default and **names the `config set` key for each one itself**, which is where the assistant's question list came from rather than from guesswork:

  | Prompt | Key | Default |
  | --- | --- | --- |
  | HTTP proxy listen port | `listen_ports.http_proxy` | `3129` |
  | SOCKS5 proxy listen port | `listen_ports.socks5_proxy` | `1081` |
  | proxy listen address | `listen_address` | `127.0.0.1` |
  | proxy server mode | `proxy_mode` | `manual` |
  | crash reports | `send_crash_reports` | `no` |
  | HTTPS filtering | `https_filtering.enabled` | `yes` |
  | certificate name | `https_filtering.root_certificate_name` | `AdGuard CLI CA` |
  | filter list groups | the `filters` list | skipped |

  Note the SOCKS key is `socks5_proxy`, not `socks_proxy`.

  It leaves a complete 220-line `proxy.yaml` with all 105 of its upstream comments — the same shape as a real install's — plus `user.txt`, `dns_user.txt`, `https_exclusions.txt`, `browsers.yaml` and the CA certificate. Ordinary `config set` works immediately afterwards and is as surgical as ever.

  **One prompt is skipped in silence.** *"Do you want to install the certificate on the system?. You will need to enter your password to confirm"* is the only one with no no-TTY warning and no key — it is a privileged step, and it simply does not happen. So the seeded state is HTTPS filtering **on** with its CA outside the system trust store. That is a fact for the UI to surface, not to paper over; §8 rules out installing it for the user, and now also says what the UI shows instead.

  And the CA it leaves is not necessarily a new one: against a directory holding a copied `adguard.conf` it comes back byte-identical to the source machine's, which is measured at the end of §8.

  **It is licence-gated.** Unlicensed it exits **1** with the usual complaint and usage dump on stderr — and seeds the file *anyway*, before reaching the gate, but without the CA. Activate first.

  **The second run is the dangerous one, and it is deliberately unmeasured.** Against a directory that already has a `proxy.yaml` the wizard takes another branch entirely; its own strings are *"The initial configuration has already been completed. The running proxy server will be stopped, and the configuration will be reset. Do you want to continue?"* and *"No TTY available. Proceeding with reconfiguration using default values."* With stdin closed there is no prompt at which to decline, so that branch would proceed and take the user's whole configuration with it. The only licensed install available to confirm it on is the author's own, and the strings are clear enough that confirming costs more than it settles.

  `Cli::configure` therefore checks for the file immediately before spawning and refuses with `Error::AlreadyConfigured` if it is there, and it is the only place in the codebase that names the subcommand. Success is decided by the file existing afterwards, not by anything printed.
- **`activate`** — browser-based licence flow, absent from `--help-all` but a real command. Measured against an unlicensed sandbox, with stdin closed: exit **0**, both lines on **stdout**, no ANSI.

  ```text
  How do you want to activate AdGuard CLI?
  Warning: No TTY for user input. Please visit https://link.adtidy.org/forward.html?action=activate&app=cli&appid=<id> to log in, then run `adguard-cli activate` again to complete activation.
  ```

  The first line is a menu prompt that never got asked; the URL sits mid-sentence in the second, so it is found by its `https://` scheme rather than by position — and **only** `https://`, because that string is handed to `gtk::UriLauncher` and thence to the desktop's handler for whatever scheme it names.

  The GUI opens that URL and then waits for the user to say they are done — **it does not poll.** Two measured facts rule polling out: `license` is itself licence-gated, so while unlicensed it refuses rather than reporting a status to poll for; and the CLI's own sentence says the flow is completed by running `activate` *again*, not by waiting. A poll would therefore have no readable exit condition and might never see one. The finish button re-runs `activate` once, then reads `license` — and `license` is what decides, not anything `activate` printed.

  **The `appid` belongs to the data directory, not to the invocation.** Measured: three runs against one sandbox produced the identical link, a second sandbox produced a different one. That is what makes a finish button work rather than merely sound plausible — running `activate` again asks after the same pending activation the user was sent to log into, instead of starting a race with it.

  Timing: 0.14 s the first time in a fresh data directory, which it seeds, and 0.01–0.02 s afterwards. The link is computed locally; it is the *completion* leg that reaches AdGuard, which is why `Cli::activate` takes `NETWORK_TIMEOUT` rather than the local one.

  What `activate` does against an **already licensed** install is deliberately **not measured**: the only install available to try it on is the author's own, and pointing an activation command at a working licence to see what happens is not a measurement worth its risk. The UI therefore offers activation only while `license` says the licence is not active, and never from a reading that failed for some other reason.
- **`config set listen_address <non-loopback>`**, but only while `listen_auth` is not fully configured — it prompts for a username. This one is the nastiest of the three because it does not *look* interactive and it reports success anyway; see [§5](#listen_address-needs-authentication-fully-configured-first). Configure `listen_auth` completely and it needs no TTY at all.
- **`filters add` / `filters enable`, for group 4 of either catalogue** — an agreement that will not take a default. This is the one prompt on the list that a closed stdin does not merely no-op past: it refuses the work outright, so the whole group is unreachable until something answers it. It has a subsection of its own below.

### The wrapper closes stdin, so "no TTY" is the only path

Everything measured about that prompt was measured without a TTY, where the CLI gives up immediately and warns. But a child process inherits its parent's stdin, and **a GUI started from a terminal has a real one** — so the same call that no-ops in every test would sit there indefinitely waiting for a username to be typed into a terminal the user has stopped looking at, holding a worker thread and leaving the control that triggered it spinning.

`Cli::run` therefore spawns with `Stdio::null()`. It makes the no-TTY behaviour deterministic however the app was launched, and nothing here has anything to say on stdin anyway. It is not a substitute for the precondition check — a silent no-op is still a silent no-op — but it removes the hang.

### The annoyance-filter agreement, the one prompt with no usable default

Every other prompt in this section takes a default and carries on. This one refuses. Measured on v1.4.13 with stdin closed, at exit **0**, all on **stdout**:

```text
$ adguard-cli filters add 18
Filter [Title: AdGuard Cookie Notices filter] added

Please read carefully before enabling Annoyance filters

You are about to enable one or more annoyance filters. […]

Enable these filters? (yes/no):
Annoyance filters won't be enabled due to user's choice
```

Four separate traps, in the order they bite:

1. **A closed stdin is a "no", and "no" means the work does not happen.** So `Stdio::null()` — correct everywhere else — makes the entire Annoyances group permanently unswitchable from this application. It was: the defect this section was written for is a user reporting that the five `AdGuard …` annoyance lists could not be enabled from the GUI at all, with the terminal as the only workaround.

2. **`add` prints its success line *before* it refuses.** `Filter [Title: …] added` is line one; the refusal is line six. `confirms(…, "added")` is satisfied by line one, so the obvious reading reports success for a command that subscribed to the list and left it **switched off** — a state change the user did not ask for, reported as the one they did. `Cli::filter_action` therefore looks for `Annoyance filters won't be enabled due to user's choice` *first*, and only then for the confirmation.

   A `filters enable` on an already-added list has no such decoy: it prints the agreement and nothing else, so `first_line` served the user *"Please read carefully before enabling Annoyance filters"* — one line off a twelve-line block, an instruction to read something that was never shown. That was the reported symptom.

3. **The gate is the group, not the name and not a range of ids.** Measured across the whole HTTP catalogue: all **eleven** members of group 4 are gated — 18–22 (`AdGuard Cookie Notices`, `Popups`, `Mobile App Banners`, `Other Annoyances`, `Widgets`), plus `Fanboy's Annoyances` (122), `Web Annoyances Ultralist` (201), `Adblock Warning Removal List` (207), `EasyList Cookie List` (241), `Dandelion Sprout's Annoyances List` (250) and `Stevo's AI Blocklist` (260). Meanwhile `CJX's Annoyances List` (220) has the word in its title, sits in *Language-specific*, and is **not** gated. A control from another group — `Phishing URL Blocklist` (255), Security — adds and enables in one step with no prompt at all.

4. **The gate is the group *number*, and it is not the same category in the two databases.** `agflm_standard.db` group 4 is `Annoyances`; `agflm_dns.db` group 4 is `Security`. **Both are gated.** So `dns filters add 18` reads out a disclaimer about violating websites' terms of use before declining to enable `Phishing Army`, and `dns filters enable 8` prints the agreement, the refusal, and then a `Failed to update filters` line.

   Measured 25 August 2026 by sweeping all 62 un-added lists of `agflm_dns.db` on v1.4.13, one `add` each with stdin closed and a `remove` behind it: **exactly the 17 members of group 4 are gated** — 8, 9, 10, 11, 12, 18, 30, 31, 42, 44, 50, 52, 54, 55, 56, 68, 71 — and the 45 lists of General, Other and Regional add and enable in one step with no prompt at all. The DNS catalogue has no Annoyances group; the number is gated regardless of what the number means.

   **This paragraph used to say the opposite**, on the strength of the group names alone and with no `dns filters add` measured against a Security list: it recorded that the DNS catalogue *never raises the prompt*, and `FilterSet::annoyances_group` returned `None` for it, on the reasoning that a bare `group_id == 4` would put an annoyance dialog in front of the DNS malware lists. It does put one there — AdGuard's — and the DNS Security group was as unswitchable from this application as the Annoyances group had been, for the same reason and for a whole release ([issue #13](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/13)). The rule the correction leaves behind: **a group's name says what to tell the user, never what the binary will do.** `FilterSet::consent_group` now returns group 4 for both sets, and stays per-set and `Option`-shaped so that AdGuard fixing its half is a one-line change here.

**What answers it.** `yes` followed by a newline, written to stdin, which `Cli::run_answering` does before closing the pipe behind it. The newline is the answer — an unterminated line leaves the CLI still waiting when the pipe closes. Closing immediately preserves the guarantee above: the first prompt gets the line, every later one meets EOF and takes its default, so nothing can wait for a second answer that is not coming. `y` was not measured and is not guessed at.

**Who is allowed to say yes.** Not the wrapper. `Consent::Granted` is a value the caller passes, and §8's rule against answering for the user applies with particular force to a prompt whose whole content is a disclaimer about who is liable. The GUI shows AdGuard's text verbatim in an `AdwAlertDialog` — verbatim because a paraphrase would be this application deciding how much of someone else's disclaimer a user needs to see — and asks *before* running anything, since asking afterwards would mean a declined dialog leaving behind the subscription `add` had already made.

**On the DNS page the verbatim text is about something else entirely**, and the dialog says so first: a sentence of ours, marked as ours, that the list is not an annoyance filter and AdGuard asks about one anyway, then AdGuard's wording unchanged. Shown bare over `Stalkerware Indicators List`, that disclaimer is a non-sequitur, and the honest reading of a non-sequitur in a dialog is that the application has gone wrong.

---

## 8. Privileged operations

`adguard_root_helper` is **not setuid** as shipped (`-rwxr-xr-x potworny potworny`, in `~/.local/opt/adguard-cli/`) and the package ships **no polkit policy** — a search of `/usr/share/polkit-1/actions/` and `/etc/polkit-1/` for "adguard" returns nothing.

> **This machine no longer matches that sentence, and the sentence stays.** `sudo … -s` has since been run here, so the helper reads `-rwsr-xr-x root root`. What is recorded above is the **shipped** state, which is what every fresh install starts from and therefore what the GUI has to render. The consequence for testing is that the branch which is unreachable locally has swapped — it used to be the met one — and `$ADGUARD_ROOT_HELPER` is what makes either reachable. What prompted the `sudo` is the subsection below.

**But AdGuard ships its own escalation path, and it is the one to use.** Measured from the binary's strings, `adguard-cli` checks the helper three ways and tells the user exactly how to satisfy the check:

```
Root helper check: owned_by_root={}, has_suid={}, is_executable={}
Automatic mode requires root helper to have suid bit set
Automatic mode requires root helper to be set up, do you want to set it up?
Please run `sudo {} -s` to set it
```

So `sudo ~/.local/opt/adguard-cli/adguard_root_helper -s` is AdGuard's own documented setup, and once it has run, `config set proxy_mode auto` needs no privilege from us at all. An `adguard-ui-helper` of our own would duplicate a root capability that already exists and would shell out to the same unprivileged `config set` in the end. **We author no polkit action and no privileged binary** — the GUI stats the helper for the same three properties and, when they are unmet, shows AdGuard's command with an explanation. See `architecture.md` §6.

The reason to leave that `sudo` to the user rather than run it for them: the helper lives in a user-writable directory, so the suid bit makes anyone who can write that file root. That is AdGuard's design decision, and the user opted into it by installing AdGuard — but it is not something to confer from behind a button.

An earlier revision of this section concluded there was "no existing escalation path to reuse". That was wrong; it was inferred from the file mode without reading the binary.

### The helper is not only about automatic mode: without it the HTTP proxy serves nothing

**Measured, and it is the reason any of this is user-visible.** Every string AdGuard prints about the helper names automatic mode, and this document and the app both read them as meaning auto mode was the only thing that needed it. With `proxy_mode: 'manual'`, the proxy running, and the helper in its shipped state, **every request through the HTTP proxy fails**:

```console
$ curl -sS -o /dev/null -w '%{http_code}\n' -x http://127.0.0.1:3129 http://wp.pl/
502
$ curl -sS -o /dev/null -w '%{http_code}\n' -x http://127.0.0.1:3129 https://wp.pl/
000                                         # CONNECT tunnel failed, response 502
$ curl -sS -o /dev/null -w '%{http_code}\n' --socks5-hostname 127.0.0.1:1081 http://wp.pl/
301                                         # the SOCKS5 listener is unaffected
```

The body of the 502 is AdGuard's own `blocking-pages` error page. **It never opens an upstream connection at all** — pointed at a local `python3 -m http.server`, the HTTP proxy still returns 502 and that server logs no request, while the same fetch over SOCKS5 arrives normally. The daemon logs exactly two lines per attempt and nothing else:

```text
ERROR RootHelperClient send_command: Sequencer is not initialized
WARN  AGStandaloneServerSocketFactory prepareFd: Failed to protect socket: Failed to send command to root helper
```

`ps` says why: the daemon spawns its helper and the helper dies immediately, leaving `[adguard_root_he] <defunct>` parented to it. Socket protection then fails for every outbound socket the HTTP listener creates, and the connection is abandoned before `connect(2)`.

**`restart` was run as a control first, and it does not help** — the 502 came back and the zombie respawned with it, which is what rules out a merely wedged daemon (§11) and leaves the suid bit as the only variable. After `sudo … -s` and a restart, the helper runs as `root`, and:

```console
$ curl -sS -o /dev/null -w '%{http_code}\n' -x http://127.0.0.1:3129 http://wp.pl/
301
$ curl -sS -o /dev/null -w '%{http_code}\n' -x http://127.0.0.1:3129 https://wp.pl/
301
```

A request through the proxy now writes **nothing** to `proxy.log`.

**What the user sees instead of any of this.** The error page reports the failure against the *upstream* host with a `strerror` that varies per attempt — `Error connecting to wp.pl:80. Error: 104(Connection reset by peer)`, and `115(Operation in progress)` and `11(Resource temporarily unavailable)` on other attempts. It reads as a fault at the far end, in a browser, with nothing naming the helper; `adguard-cli status` meanwhile reports the HTTP proxy listening, because it is. **Bound is not the same as working**, and `status` only answers the first. That is why the GUI reports the check on the Status page beside the endpoint and in the first-run assistant, and no longer files it under automatic mode (`architecture.md` §6).

Two limits on the above, stated rather than glossed. The mechanism inside AdGuard is inferred from its own log lines — what is measured is that the failure and the fix track the suid bit exactly. And this is one machine and one version (helper and CLI dated 27 May 2026); it is enough to stop the app claiming the helper matters only for auto mode, which was measurably false, and not enough to claim every version behaves this way.

### And it can die hours later, with the suid bit set — where a restart *is* the cure

**The subsection above and this one share every symptom and differ in the only thing that matters.** There, the helper never starts, because the shipped binary has no suid bit; a `restart` was run as a control and did not help. Here the helper is correctly `-rwsr-xr-x root root`, starts with the daemon, works for hours, and *then* dies. The daemon carries on serving, and a `restart` fixes it every time.

The two are worth telling apart because the log lines are identical, so `Sequencer is not initialized` on its own does not say which one you are looking at. **What separates them is whether a restart helps** — and, before that, whether the helper was running at all since the daemon started.

Measured on this machine on **2026-08-25**, `proxy_mode: 'auto'`, v1.4.13. The daemon started at 10:32 the previous day and the helper with it. At 03:01:39 the IPC between them desynchronised and never recovered:

```text
03:01:39 ERROR RootHelperClient on_packets_received: Failed to parse response
03:01:39 INFO  RootHelperClient disconnect: Finished
03:01:39 ERROR RootHelperClient send_command: Response not found for request ID 15938 after predicate returned true
03:01:39 ERROR RootHelperClient send_command: Sequencer is not initialized
03:01:39 WARN  StandaloneProxyServer onUdpListenerNewConnection: Failed to protect socket
```

Every root-helper command after that line failed for the remaining seventeen hours of the run. `ps` showed the same corpse the subsection above describes, `[adguard_root_he] <defunct>`, parented to a healthy-looking daemon — and `adguard-cli status` went on reporting a running proxy with system-wide filtering enabled throughout.

**In `auto` mode the consequence is silence, which is what makes it dangerous.** The redirect stops, so nothing is filtered and nothing fails either: pages load normally and simply arrive with their ads. Contrast the unmet-suid case above, where the HTTP proxy 502s every request and the user cannot browse at all. Same corpse, opposite user experience.

#### The daemon's own requests are the tell

`access.log` records AdGuard's roughly-hourly internal requests as `"internal_proxy_client"`. They go through its own HTTP proxy, so they fail exactly when socket protection does. Over twelve days on this machine, counting real traffic as every line that is *not* one of those:

| Day | Real traffic | internal 200 | internal 502 |
| --- | ---: | ---: | ---: |
| Fri 14.08 | **0** | **0** | 24 |
| Sat 15.08 | 13,856 | 64 | 4 |
| Sun 16.08 | 11,161 | 34 | 17 |
| Mon 17.08 | **12** | **0** | 17 |
| Tue 18.08 | 14,587 | 60 | 23 |
| Wed 19.08 | **2** | **0** | 16 |
| Thu 20.08 | 32,220 | 73 | 18 |
| Fri 21.08 | **5** | **0** | 14 |
| Sat 22.08 | 0 | 0 | 0 |
| Sun 23.08 | 2,689 | 32 | 0 |
| Mon 24.08 | 40,780 | 232 | 0 |
| Tue 25.08 | 8,058 | 41 | 27 |

Zero internal 200s coincides with zero real traffic on every one of the twelve days, without exception. **The discriminator is the absence of successes, not the presence of failures** — 16.08 and 18.08 each carry a score of 502s alongside a fully filtered day, so "recent requests are failing" is not a usable rule and "nothing has succeeded this run" is.

Three states are distinguishable, which is what makes the signal usable rather than merely suggestive: a powered-off machine logs no internal entries at all (22.08), a running and filtering one logs some 200s, and a bypassed one logs entries of which none is a 200. The requests are AdGuard's own, so none of this confounds with an idle user.

**Five of the twelve days were spent unprotected** — 14, 17, 19, 21 and 25 August. `Sequencer is not initialized` appears in `proxy.log` on fifteen separate days. This is frequent, not exotic.

#### It is upstream's, it is fixed, and the fix is not on the release channel

[`AdguardTeam/AdGuardCLI#136`](https://github.com/AdguardTeam/AdGuardCLI/issues/136) is the same failure reported on v1.4.11, with the same `[adguard_root_he] <defunct>` in the same `ps` output, closed *Resolution: Done* on 2026-08-01 with a fix bound for the nightly channel. Asked to narrow it down, AdGuard's engineer requested exactly one thing from the reporter: the "presence/absence of running `adguard_root_helper` process".

v1.4.13 was published **2026-05-28**, three months before that fix, and it is the newest build on the release channel. So an install tracking `update_channel: 'release'` still has this, which is what justifies the wrapper carrying a check for it at all. See `architecture.md` §3.

### `config set proxy_mode auto` does not check anything

The measurement the auto-mode design actually rests on, and it is the opposite of what the three strings above suggest. With the helper in its shipped state — `owned_by_root=false, has_suid=false, is_executable=true` — the write **succeeds**:

```console
$ adguard-cli config set -- proxy_mode auto      # exit 0, stderr empty
proxy_mode = auto
Config has been updated
$ adguard-cli config get proxy_mode              # re-read, not the confirmation
proxy_mode = auto
```

`proxy.yaml` really does hold `proxy_mode: 'auto'` afterwards. No warning, nothing on stderr, no mention of the root helper. So the helper check does **not** run at config-write time, and `config set` will happily leave the user in a mode that cannot work.

That makes the GUI's check load-bearing rather than decorative, and it has a second consequence: `proxy_mode: 'auto'` with an unmet helper is a state the app must be able to *render*, not merely to prevent. A user can reach it from a terminal or by hand-editing, and it is the same shape as `dns_filtering.enabled` with no listen port — a setting that reads on and does nothing (`architecture.md` §5).

**Where the check does fire is not measured.** Reaching it needs `start`, and neither route is available: a sandbox is unlicensed so `start` is refused long before any helper is consulted, and starting the real proxy in `auto` mode is a system-wide change that is the owner's call, not an agent's. The three strings are in the same block as the `listen_address` authorisation messages, which suggests validation at use rather than at write, but that is an inference and is written here as one.

For contrast, a value that is not a mode at all *is* refused at write time, and the CLI names the valid values itself:

```console
$ adguard-cli config set -- proxy_mode banana    # exit 0, stderr empty
Invalid value for key `proxy_mode`. Valid values are: manual, auto
```

Note the absent `Config has been updated`: this is the shape §5 describes, where the confirmation line's *absence* is the signal and the file is the evidence. `proxy_mode` was unchanged afterwards.

### The helper is a sibling of the *resolved* binary

`paths::cli_binary` finds `~/.local/bin/adguard-cli` first, because `$PATH` is searched before the known install sites — and on this machine that is a **symlink** into `~/.local/opt/adguard-cli/`. The helper lives beside the real binary, not beside the link:

```console
$ command -v adguard-cli
/home/potworny/.local/bin/adguard-cli
$ readlink -f "$(command -v adguard-cli)"
/home/potworny/.local/opt/adguard-cli/adguard-cli
$ ls ~/.local/bin/adguard_root_helper          # does not exist
$ ls ~/.local/opt/adguard-cli/adguard_root_helper
-rwxr-xr-x 1 potworny potworny 14063808 May 27 15:21 …
```

So the helper path must be taken from the **canonicalised** binary path. Joining `cli_binary().parent()` finds nothing here, and "nothing" is indistinguishable from "AdGuard is not installed" unless the code is careful to say which.

The same applies to reading the three properties: follow symlinks. `stat` without `-L` reports the *link's* mode — `lrwxrwxrwx`, uid whatever — so a helper reached through a symlink would read as world-writable and not root-owned regardless of what it points at. Rust's `fs::metadata` follows; `fs::symlink_metadata` does not.

### The certificate: AdGuard ships the installer too

The second privileged step, and it resolves the same way as the first. §7 records that `configure` generates the CA and then skips its own *"Do you want to install the certificate on the system?"* prompt in silence — so every install this app sets up ends with HTTPS filtering on and the CA untrusted, and something has to say so.

**AdGuard's own manual-install route is a script beside the binary**, named in the strings next to the symbol that builds the command:

```text
get_manual_install_script
install_cert.sh
 -f "{}"
"{}" -c "{}"{}
Cert installer not exist
```

So the command to show is `"<path>/install_cert.sh" -c "<path>/AdGuard CLI CA.pem"`, quotes included — AdGuard's own format string, and the quoting is load-bearing because the certificate is named after `https_filtering.root_certificate_name`, whose seeded default has two spaces in it. `-f "<profile>"` adds a Firefox profile and the GUI does not use it.

The script is a sibling of the *resolved* binary, exactly like the root helper, and it is shipped `-rwxr-x---` — owner-only, which is enough, since the owner is who runs it:

```console
$ ls -la ~/.local/opt/adguard-cli/{install_cert.sh,certutil}
-rwxr-x--- 1 potworny potworny    7154 May 27 15:21 install_cert.sh
-rwxr-xr-x 1 potworny potworny 4865856 May 27 15:21 certutil
```

**It elevates itself**, which is why nothing here needs to: `sudo_command='sudo'` when `$EUID` is not 0, and every privileged line runs through that variable. It also does the browser stores with `certutil` — Firefox profiles from `profiles.ini`, Chrome and Chromium from `~/.pki/nssdb` — which the GUI's check cannot see and must therefore not imply. Note *which* `certutil`: `CERTUTIL=$(command -v certutil || true)` first, and only `$SCRIPT_DIR/certutil` when that finds nothing. The copy shipped beside the binary is the fallback, not the default, and it is the branch this machine takes because `libnss3-tools` is not installed here.

Four facts from the script and from `update-ca-certificates` decide the shape of the check:

| Measured | Consequence |
| --- | --- |
| The anchor directory is the first of `/usr/local/share/ca-certificates`, `/usr/share/pki/trust/anchors`, `/etc/pki/ca-trust/source/anchors`, `/etc/ca-certificates/trust-source/anchors` that exists, and `$SYSTEM_CERT_DIR` overrides the search | The check reads the same list in the same order and honours the same variable, or it reports on a different place from the one the command writes to. Only the first exists on Ubuntu |
| The installed copy is `<certificate name>.crt` — `CERT_NAME=$(basename "${CERT_PATH}" .pem)`, then `${SYSTEM_CERT_DIR}/${CERT_NAME}.crt` | Look for `.crt`, not the `.pem` the file is called everywhere else. `update-ca-certificates` scans `find -L "$LOCALCERTSDIR" -type f -name '*.crt'` and ignores anything else in silence |
| The script's idempotence check is `if [ ! -f "${SYSTEM_CERT_PATH}" ]`, else `echo "Certificate already exists in system trust store."` | It tests the **path**, not the contents. A CA that was regenerated after being installed leaves a file of the right name holding the wrong certificate, and re-running the installer reports success without replacing it. The check compares bytes and reports that state separately, because it is the one AdGuard's own tooling will not repair |
| `update-ca-certificates` appends each anchor **file** to `/etc/ssl/certs/ca-certificates.crt` — `sed -e '$a\' "$CERT" >> "$TEMPBUNDLE"`, every byte of it — and symlinks `$(basename … .crt \| sed -e 's/ /_/g' -e 's/[()]/=/g' -e 's/,/_/g').pem` beside it | The bundle carries **no names at all**, because the files that went into it carry none: `grep -c AdGuard /etc/ssl/certs/ca-certificates.crt` returns `0` on a machine where the certificate *is* trusted — measured here. It is a property of the anchors rather than of the bundler, so an anchor written by `openssl x509 -text` would put a subject line in there; membership therefore has to be decided on the certificate's own base64 body either way |

The reading this machine gives, which is the fully-installed one:

```console
$ ls -la '/usr/local/share/ca-certificates/AdGuard CLI CA.crt'
-rw-r--r-- 1 root root 1143 Jul 12 08:47 '/usr/local/share/ca-certificates/AdGuard CLI CA.crt'
$ ls -la /etc/ssl/certs/AdGuard_CLI_CA.pem
lrwxrwxrwx 1 root root 51 Jul 12 08:47 /etc/ssl/certs/AdGuard_CLI_CA.pem -> '/usr/local/share/ca-certificates/AdGuard CLI CA.crt'
$ grep -c AdGuard /etc/ssl/certs/ca-certificates.crt
0
```

(The anchor directory holds other certificates too — this machine has a second, unrelated one — so the commands above name the one file rather than listing the directory.)

**Generation is not measured, deliberately.** `adguard-cli cert` — *"Generate a certificate for HTTPS filtering"* — is the command for a data directory with no CA, and the GUI shows it, but nobody has run it: it offers to install into the **system** trust store, which is a machine-wide change no test here is entitled to make, and it is unmeasured whether it reuses or replaces an existing CA. The UI therefore only ever *names* it, in AdGuard's own words, and never claims what it will do to a certificate that already exists.

### The CA travels in `adguard.conf`, and so does the licence

New, and found by driving the first-run assistant against the sandbox `building.md` §3 describes — a fresh directory holding nothing but a copied `adguard.conf`. After `configure`, the CA that appeared was **not a new one**:

```console
$ cmp /tmp/firstrun/adguard-cli/'AdGuard CLI CA.pem' ~/.local/share/adguard-cli/'AdGuard CLI CA.pem' && echo identical
identical
$ openssl x509 -in /tmp/firstrun/adguard-cli/'AdGuard CLI CA.pem' -noout -dates
notBefore=Jul 11 06:47:06 2026 GMT
```

Byte-identical, same SHA-256 fingerprint, and dated three weeks before the run that produced it. `adguard.conf` is a single 3096-byte line of opaque ASCII with no PEM markers in it, so nothing here says *how* — only that the certificate is reproduced from that file rather than generated, since it was the only file in the directory.

Two consequences, and the second is the one that matters:

- §7's *"leaves … the CA certificate"* is true but reads as though the wizard makes one. On a machine that has ever had a licence in that directory, it restores one.
- **Copying `adguard.conf` into a sandbox carries the certificate as well as the licence key** — and HTTPS filtering in that sandbox works against a CA the system already trusts, which means the file necessarily carries the CA's *private key* too. `building.md` §3's instruction to delete the directory afterwards was written about a licence key. It is now about rather more than that.

---

## 9. Live activity data

`access.log` lines are space-delimited with a quoted client field:

```
29.07.2026 21:45:12.996545 "internal_proxy_client" HTTP1 CONNECT - - 502 any NONE 0 - - 171828b 81ms --
```

Fields observed: date, time, `"client"`, protocol, method, (two unused), status, `any`, `NONE`, count, (two unused), bytes, duration, trailing marker.

Caveats before building stats on this:

- The format is undocumented and unstable across versions.
- Detail depends on `log_level`; at `info` many app-log messages are elided to `...`.
- No rotation policy is configured by us — but **AdGuard rotates these itself**, and a reader must survive it. Measured 2 August 2026: `~/.local/share/adguard-cli/logs/` held `proxy.log.1` at 10,485,626 B and `access.log.1`/`.2` at 10,485,776 / 10,485,648 B — a ~10 MiB threshold with at least two generations kept. It is the writing process's own roll, not `logrotate` and not cron: there is no `/etc/logrotate.d` entry and no cron entry, and the seam is continuous — `proxy.log.1` ends `30.07.2026 22:21:07.275314 WARN [2394586]` and `proxy.log` begins `30.07.2026 22:21:07.276439 WARN [2394586]`, 1.1 ms later under the same PID. **A tailer holding an fd loses the stream silently every ~10 MiB.**
- There is **no push or event mechanism**. A live view must tail the file.

`har_writer` (`enabled`, `location`) is the richer alternative for debugging but writes full HAR dumps — too heavy for an always-on *capture*, which `architecture.md` §7 distinguishes from a switch that ships `false`.

### Where a relative path resolves — measured, and the answer is "it depends on the key"

`architecture.md` §7 makes *where `har_writer.location: '.'` resolves* the first task of the HAR item, on the grounds that nothing records the proxy's working directory. **The working directory is now recorded, and it does not settle the question — it sharpens it.** Measured 2 August 2026:

```
$ pid=$(cat ~/.local/share/adguard-cli/adguard.pid); ps -o cmd= -p $pid; readlink /proc/$pid/cwd
adguard-cli start --no-fork --log-to-file
/home/potworny
```

Three relative paths in `proxy.yaml`, and **no single base directory explains them**:

| Key | Value in the file | Where the artifact actually is | Base that would explain it |
| --- | --- | --- | --- |
| `access_log_file` | `'access.log'` | `<data>/logs/access.log` | `<data>/logs/` |
| `https_filtering.certificates_cache` | `'.'` | `<data>/SSL/` holds `cert.db` — **inferred**, the key was not traced to that path | `<data>/SSL/` |
| `har_writer.location` | `'.'` | **no dump has ever been produced here** | unknown |
| the proxy process | — | cwd `/home/potworny` | the launcher's cwd |

Two things follow, and the second is the load-bearing one.

**`adguard-cli.md`'s gloss on `access_log_file` — "relative to the data dir" — is wrong**, or at best a word short: the file is one directory further down, in `logs/`. That gloss is this project's own writing, not AdGuard's, and it went in unmeasured.

**The proxy's cwd is inherited, not chosen, so a cwd-relative default would be a path the user cannot predict.** `/home/potworny` is not written anywhere in the data directory; it is where the process happened to be started from. The supporting case is on the same machine: `adguard_cli_nm`, launched by Chrome rather than by a shell, has cwd `/home/potworny/.local/opt/adguard-cli`. (That cwd is inherited from the launcher is POSIX, not a measurement of AdGuard — but two AdGuard processes with two different cwds, each matching its own launcher, is what makes it the explanation here rather than a guess.)

So **`.` still cannot be predicted for `har_writer.location`**, and the tempting shortcut — read one of the other two relative keys and generalise — is exactly the wrong move: `access_log_file` is not cwd-relative, and if `certificates_cache` resolves to `<data>/SSL/` it is not cwd-relative either, which means AdGuard resolves these against per-key base directories and not against one rule. **The measurement §7 asks for still requires producing an actual HAR dump.** What this entry establishes is only that the answer cannot be inferred, and that the expected outcome — the row must show an absolute path — now has a *reason* (an unpredictable base) rather than only an expectation.

**Why that dump has not been produced here.** It needs a proxy that is running with `har_writer.enabled`, and starting a second one on this machine is not a sandbox operation. §8 measured that the daemon spawns its root helper in **manual** mode too — that is the whole point of that section — so a sandbox proxy would spawn a second helper against the same suid binary while this machine's real proxy is in `auto` mode redirecting system traffic through the first. §11 is what that risks: a proxy the CLI has lost track of, out of which neither `stop` nor `start` recovers and whose actual cure is `killall adguard-cli` — which would take the real proxy down with it. **This is a machine-wide change and therefore the owner's call**, not an unattended one. `handoff.md` §3 item 9 carries what an authorised run would need to do.

### The HAR keys' write path, measured without a proxy

The *resolution* needs a running proxy; the *write* does not, and it is half of what the row needs. Measured 2 August 2026 in an unlicensed sandbox (`XDG_DATA_HOME` at a scratch copy of `proxy.yaml`, no `adguard.conf` copied in, so no licence and no CA private key), verified by re-reading the file rather than by the confirmation line, with the line count checked at 220 after every write:

| `config set har_writer.location …` | Exit | File afterwards |
| --- | --- | --- |
| `/tmp/har-dumps` | 0 | `'/tmp/har-dumps'` |
| `~/har-dumps` | 0 | `'~/har-dumps'` — **the tilde is stored literally, not expanded** |
| `har` | 0 | `'har'` |
| `/no/such/dir/at/all` | 0 | `'/no/such/dir/at/all'` |
| `''` (empty) | 0 | `''` |
| `/tmp/har-dumps/` | 0 | `'/tmp/har-dumps/'` |
| `/tmp/har dumps` | 0 | `'/tmp/har dumps'` |

**`har_writer.location` is not validated in any respect.** Not existence, not absoluteness, not emptiness — and unlike `listen_address` above, which *is* validated and narrowly, this is a path-valued string and nothing checks it at all. Every value round-trips verbatim into single quotes, exactly as the stock `'.'` is quoted.

**The tilde is the sharp one.** `~/har-dumps` is the single most likely thing a user types into a directory field, it is accepted at exit 0 with a cheerful `har_writer.location = ~/har-dumps`, and what lands in the file is a literal tilde. Nothing in the CLI expands it and nothing warns. Whatever the row ends up doing about `'.'`, **it has to resolve `~` itself or refuse it** — and the same applies to every other path-valued string key this file has (`access_log_file`, `https_filtering.exclusions`, `https_filtering.certificates_cache`), which the parity enumeration in `architecture.md` §5 lists.

Two smaller results from the same run:

- **`config reset` works on this key, and this is the first measurement of `reset` on anything but the contract's `log_level` example.** `config reset har_writer.location` answers `har_writer.location = .` then `Config has been updated`, and the file really goes back to `'.'`. So the "restore default" affordance §5 calls the obvious home for `reset` is available per row, at least here.
- **The boolean type-pun applies to `har_writer.enabled` as well**, which is a confirmation rather than a discovery — *Booleans have two spellings* above already has it, and `Config::bool_at` already coerces `Integer(1)`. Recorded only because the HAR row is a `Switch`: `config set har_writer.enabled 1` is accepted and leaves `enabled: 1` in the file, while `yes`, `TRUE` and `notabool` are all refused with `Invalid value type: The value of the setting must be an boolean` (AdGuard's grammar, not ours).

**Live stats is its own milestone, behind a spike on this format** — `architecture.md` §7 is the scope authority and put it there on 2 August 2026; this section is the input to that spike, not a scope claim of its own. Nothing in the CLI provides a counter or stats endpoint.

### `har_writer.location: '.'` resolves against the **data directory** — measured with a proxy

The question `architecture.md` §7 makes the HAR item's first task. **Answered 2 August 2026**, by the authorised sandbox run the owner cleared: a licensed scratch `XDG_DATA_HOME`, `proxy_mode: manual`, both listen ports moved off 3129/1081, `har_writer.enabled: true`, `location` left at its shipped `'.'`.

**The experiment is the cwd.** The daemon was started from a directory that is neither the data directory nor `$HOME` — `<sandbox>/cwdprobe` — so `'.'` could only resolve to one of three places and each is distinguishable:

```console
$ readlink -f /proc/<daemon>/cwd
<sandbox>/cwdprobe
$ ls -la <sandbox>/cwdprobe                      # after 64 filtered requests
total 0                                          # empty
$ for f in /proc/<daemon>/fd/*; do readlink $f; done | grep har
<sandbox>/adguard-cli/adguard.har
```

**So `'.'` is the data directory, not the working directory** — the same conclusion `access_log_file` reached, and the opposite of what a reader of the file would assume. The subsection above says three relative paths have no single base; this is the third of them, and it lands with `access_log_file` rather than with the cwd. The filename `adguard.har` is fixed: `location` names a **directory**, and nothing in the file names the file.

**What the dump contains, and it decides the subtitle.** Valid HAR 1.2, complete and parseable *while the proxy is running* rather than only at shutdown:

```console
$ python3 -c "import json;d=json.load(open('adguard.har'));print(d['log']['creator'],len(d['log']['entries']))"
{'name': 'AGProxy', 'version': '1.0'} 2
```

Full URLs, request headers, and **response bodies** — the page contents, not a summary of them. `-rw-rw-r--`, so it is **group- and world-readable**, unlike the licence in `adguard.conf`.

**And it is heavier than "too heavy" suggested.** §9 above called full HAR dumps too heavy for an always-on capture on reasoning rather than on a number. The number:

| After | Size |
| --- | --- |
| 2 requests | 6,137 B |
| 64 requests over ~6 minutes of scripted browsing | **114,084,761 B** |

That is ~1.8 MB per request of ordinary page loads, and **the files accumulate**: teardown found `adguard.har` alongside `adguard-1785668116.har` and `adguard-1785668569.har`, one per proxy run, with nothing pruning them. (Observed while deleting the sandbox rather than by a designed measurement — the rotation *rule* is not measured, only that more than one file survives more than one run.)

So the row cannot be worded as a debugging convenience. It writes every page the user visits, in full, to a world-readable file in a directory the UI must name absolutely — because `'.'` tells the user nothing — and it does not stop growing.

---

## 10. Wrapper-layer checklist

Anything in the `adguard-cli` wrapper crate must:

1. Strip ANSI from every captured stream.
2. Treat exit 1 as *our* bug — with the three exceptions in [§3](#exit-1-is-usually-our-bug-but-not-always) — and detect user-facing failure by matching stdout text. A failure whose only text is on stdout is never our command line, whatever the exit status.
3. Verify state changes by re-reading state, not by trusting exit 0.
4. Never write `proxy.yaml`, and never write the `.db` files.
5. Never invoke a command that expects a TTY to be useful. There are **two** exceptions, both because closing stdin makes the no-TTY branch the only branch: `activate`, which prints a log-in link and returns; and `configure`, which is the sole way a first run can produce a `proxy.yaml` at all — guarded so it can only run when that file is absent, since its other branch resets the user's configuration ([§7](#7-commands-that-need-a-tty)).
6. Apply a timeout — network commands (`check-update`, `filters update`, `update`) can hang; a filter update failure was observed in the logs (`HttpClientNetworkError` reaching `filters.adtidy.org`).
7. Run off the GTK main thread.
8. Spawn with **stdin closed**, so a command that would prompt cannot hang ([§7](#the-wrapper-closes-stdin-so-no-tty-is-the-only-path)).
9. Pass `--` before any user-supplied key or value ([§5](#the----guard-is-mandatory)).
10. Range-check numbers itself — `config set` only type-checks ([§5](#config-set-type-checks-and-nothing-else)).
11. Keep secrets out of error text. `config set` echoes the value it was given, and our own `BadInvocation` quotes the whole command line, so a refused password write would otherwise leak into any toast that shows it. `Cli::set_secret` scrubs every variant that carries our arguments.
12. Size the `start` deadline above AdGuard's own. A failing start takes 60 s and a successful one 1.1 s, so the deadline that fits the success case truncates the failure and loses its explanation ([§11](#11-a-proxy-the-cli-has-lost-track-of)). Recognise that failure by its stdout line and define *failure* positively there, not success — an unrecognised confirmation must stay a success and leave the verdict to the status re-read.

---

## 11. A proxy the CLI has lost track of

Measured on 2026-08-01, v1.4.13, after the state arose on its own during ordinary use.

An install can end up with the previous proxy process still alive and still holding the ports, while `adguard-cli` reports the proxy stopped:

```
$ ps -eo pid,ppid,etime,stat,cmd
   6925    2968  01:11:56 Sl  …/adguard-cli start --no-fork --log-to-file
   6932    6925  01:11:56 Z   [adguard_root_he] <defunct>

$ ss -lntp
LISTEN 127.0.0.1:3129  users:(("adguard-cli",pid=6925,fd=62))
LISTEN 127.0.0.1:1081  users:(("adguard-cli",pid=6925,fd=63))

$ adguard-cli status                                    # exit 0
The AdGuard proxy server is not running
```

The daemon has been reparented to `systemd --user` and never reaped its root helper. `status` reports it gone; the kernel says otherwise.

### Neither lifecycle command gets out of it

`stop` is a **no-op** — 0.1 s, exit 0, and the process is still there afterwards:

```
Failed to stop the AdGuard proxy server
Failed to stop proxy server, it is not running
```

`start` cannot bind what is already bound, and takes 60.0 s to say so. From `logs/app.log`:

```
10:37:21.870  AdGuardCli start_command: ...
10:38:21.871  CSM response_from_listener: Client wait data from listener timeout
10:38:21.881  SERVICE_FACADE start_internal: Failed to stop process manager
```

then, on stdout at **exit 0**:

```
Failed to start proxy server: An unknown error has occurred
```

So the CLI has no route out of this state. The user is left with a proxy that is down, a UI that agrees it is down, and a Start button that does nothing for a minute — and `stop && killall adguard-cli` is the recovery people arrive at, of which only the `killall` does anything.

### The symptom reproduces on demand — and the first two explanations for it were both wrong

This section opens *"after the state arose on its own during ordinary use"*, which is why nothing here has ever been a controlled experiment. **On 2 August 2026 the symptom appeared on demand, twice, in the authorised sandbox run**, with stdout byte-identical to the block above and the daemon provably alive:

```console
$ adguard-cli stop                       # exit 0
Failed to stop the AdGuard proxy server
Failed to stop proxy server, it is not running
$ ps -p <daemon> -o pid=,cmd=
1861384 /home/potworny/.local/bin/adguard-cli start --no-fork
```

**First explanation, refuted by its own control.** The sandbox's `XDG_DATA_HOME` was 119 characters, and the daemon had warned at startup: `Socket name length 144 exceeds maximum allowed length 107. The name will be truncated` — a truncated `sun_path`, which is exactly the shape of a client that cannot find its server. Re-running on a 10-character path removed the warning entirely (`grep -c "Socket name length"` → `0`) and **`stop` failed identically**. So the truncation is real, and it is not the cause of this.

**Second explanation, and it is a lead rather than an answer.** The one structural difference from a healthy install is that the sandbox data directory held `agcli.socket` and **no `adguard.pid`**, where the real one holds both. These runs were launched as `start --no-fork` directly; the real proxy is a `start` **parent** with a `start --no-fork` child, and `§11`'s own opening `ps` shows that parent/child pair. So the plausible reading is that the pid file is written by the wrapper and not by the child, and that `stop` consults it — which would mean **this run produced a look-alike rather than the organic bug**: same stdout, same exit 0, same live process, possibly a different cause. It is recorded that way deliberately. Reaching the organic state still needs it to arise on its own.

**What is settled either way**, and both are useful:

- **`SIGTERM` by PID worked every time** — four for four across this run — and takes the root helper with it, leaving no orphan. That is the cure this section already recommends, now exercised deliberately rather than observed once.
- **The truncation is a separate, real trap.** A data directory whose path is long enough pushes the control-socket name past 107 bytes; the daemon says so in one WARN line at startup and then runs anyway. Any harness that sandboxes via a deep `XDG_DATA_HOME` — which is every harness in this project — should keep the path short, and a second start against the same directory then fails with `Failed to init control socket: Socket busy` rather than anything that names the real problem.

### The command line is not the signature; the contradiction is

A **healthy** daemon is also `adguard-cli start --no-fork --log-to-file` — measured immediately after recovery, on the working process that replaced this one. Killing on that alone kills a running proxy.

What identifies the leftover is that such a process exists *and* `status` says nothing is running. Both halves are required, and `orphan.rs` supplies only the first.

### `SIGTERM` is the whole cure

A `SIGTERM` to that one pid ended it in under 0.5 s and released both ports; `SIGKILL` was never needed. The process belongs to the user running this application, so no privilege is involved and [§8](#8-privileged-operations) does not apply — this is the one recovery the app performs itself rather than printing for the user to run.

Two guards make that safe, both in `orphan.rs`:

- **Signal nothing newer than the attempt.** A start forks a daemon that looks identical to the wedged one, so the caller lists daemons *before* running `start` and only ever signals from that list.
- **Signal nothing that has been recycled.** A pid is unique only among live processes, and the two reads are separated by a command that can take a minute, so the start time from `/proc/<pid>/stat` field 22 is carried alongside the pid and re-checked. A zombie counts as gone: it keeps an unchanged start time, and waiting for one to exit again would wait forever.

### Why it gets reparented, measured: there is a systemd user unit, and it has lost the daemon

This section opened by observing that the stray daemon *"has been reparented to `systemd --user`"* without saying why. Measured 2 August 2026: **this machine has `~/.config/systemd/user/adguard-cli.service`**, enabled, and it explains both the reparenting and a hazard the section did not name.

```
$ systemctl --user status adguard-cli.service
   Active: active (exited) since Sat 2026-08-01 09:24:57 CEST; 22h ago
  Process: 6901 ExecStart=…/adguard-cli start (code=exited, status=0/SUCCESS)
 Main PID: 6925 (code=exited, status=0/SUCCESS)
```

The unit is `Type=forking` with `RemainAfterExit=yes` and `Restart=on-failure`. `adguard-cli start` forks a daemon and returns, systemd reaps the launcher, and — because the daemon is not the pid systemd tracked — **`MainPID` becomes 0 and systemd stops following it entirely**. The service reads `active (exited)` forever afterwards, whatever the daemon does.

Three consequences, and the third is the one that bites:

- **The reparenting is ordinary.** The daemon's real parent exits immediately, so PID 1 for the user session — `systemd --user`, pid 2968 here — adopts it. A `PPID` of `systemd --user` is therefore *not* evidence that systemd started or manages that process.
- **`Restart=on-failure` cannot fire.** It never saw a failure, because as far as it is concerned the service succeeded on 1 August. Measured: the daemon died twice during this session and `journalctl --user -u adguard-cli.service` recorded **nothing at all** on 2 August. A unit that looks like a supervisor and supervises nothing is worse than no unit, because it invites the assumption that something is watching.
- **`WorkingDirectory` in the unit is not the daemon's working directory.** The unit sets `%h/.local/opt/adguard-cli`; the running daemon's `/proc/<pid>/cwd` is `/home/potworny` — measured on two independent daemons fifteen hours apart. §9's conclusion that a relative `har_writer.location` resolves against something unpredictable therefore survives contact with the unit, and is strengthened by it: **even an explicit `WorkingDirectory` does not reach the daemon a user ends up with**, because that daemon was not launched from it.

Nothing here changes `orphan.rs`, whose two guards already reason from pids and start times rather than from parentage. What it changes is the diagnosis: **`systemctl --user restart adguard-cli` is not a recovery for §11's wedged state** — it would run `ExecStop` (`adguard-cli stop`, the measured no-op above) against a daemon systemd is not tracking, then `ExecStart` into ports still held.

---

## 12. Browser integration is a separate step, and quietly conditional

Measured on 2026-08-01, CLI v1.4.13, against AdGuard Browser Assistant 1.4.8 (`fbohpolgemkbfphodcfgnpjcmedcjhpn`).

The extension does **not** look for `adguard-cli` on `$PATH`, or for the proxy on its ports. It asks the browser for a native-messaging host, and the browser resolves that name out of a manifest on disk. From the extension's `background.js`:

```js
const HOST_TYPES = { browserExtensionHost: 'com.adguard.browser_extension_host.nm' };
this.port = browser_polyfill_default().runtime.connectNative(HOST_TYPES.browserExtensionHost);
```

With no manifest, `connectNative` fails at once and the extension reports that it **cannot detect `adguard-cli` in the system** — on a machine where AdGuard may be installed, running and filtering perfectly. The message names the wrong thing, which is what makes this worth a check of our own rather than a note in a README.

`adguard-cli install-browser-integration` writes those manifests; `-u`/`--uninstall` removes them. **Unpacking the CLI does not run it**, so a stock install is in the unmet state.

### The six locations are fixed, and in the binary

They are the only native-messaging paths in the CLI's strings:

```
.config/BraveSoftware/Brave-Browser/NativeMessagingHosts
.config/chromium/NativeMessagingHosts
.config/google-chrome/NativeMessagingHosts
.config/microsoft-edge/NativeMessagingHosts
.config/vivaldi/NativeMessagingHosts
.mozilla/native-messaging-hosts
```

Relative to `$HOME`, and `.config` is hard-coded — `$XDG_CONFIG_HOME` is not consulted. A check that honoured XDG would be looking somewhere the manifest never lands.

Each manifest names the host binary and the extensions allowed to reach it — `allowed_origins` for the Chromium family, `allowed_extensions` for Firefox:

```json
{
  "allowed_origins": [ "chrome-extension://fbohpolgemkbfphodcfgnpjcmedcjhpn/", … ],
  "name": "com.adguard.browser_extension_host.nm",
  "path": "/home/you/.local/opt/adguard-cli/adguard_cli_nm",
  "type": "stdio"
}
```

### It reports success even when it writes nothing

This is the measurement that matters, and it is not visible from the command's output. Against a sandbox `$HOME`:

| `$HOME` contains | Manifests written | Exit | stdout |
| --- | --- | --- | --- |
| nothing | **none** | 0 | `Native Messaging manifests installed successfully. You can now use AdGuard Browser Assistant extension` |
| `.config/chromium` | chromium only | 0 | *same* |
| `.mozilla` alone | **none** | 0 | *same* |
| `.mozilla/firefox` | Firefox | 0 | *same* |

So the installer writes only where it already sees a browser, and says the same reassuring sentence either way. Firefox is gated on its **profile** directory, not on `.mozilla`.

**The consequence is an ordering trap with no diagnostic anywhere.** Install a browser *after* running the command and it gets no manifest; the command has already reported success, the CLI will never mention it again, and the extension blames `adguard-cli`. Nothing in AdGuard's tooling closes that loop, which is why the app's check is re-read on window focus rather than performed once (`architecture.md` §6).

### What we do with it

Read the manifests; never speak the protocol. `adguard_cli_nm` is a stdio native-messaging host whose manifests name the extension IDs permitted to reach it, so the browser vouches for the caller — impersonating one of those extensions is both fragile and rude ([§1](#1-rejected-integration-points)).

`browser.rs` compares each manifest's `path` against the `adguard_cli_nm` beside the **resolved** `adguard-cli` binary, rather than merely testing that the file exists. A manifest left by an AdGuard reinstalled under a different prefix can name a host that still exists — the old one — and an existence check would call that healthy while the extension talked to a stale binary.

---

## 13. Import and export

Measured 2 August 2026 against 1.4.13. `architecture.md` §7 puts import/export in v2 and requires the first-run collision to be designed before either half is built; this section is the input to that design and makes no scope claim of its own. Everything below was taken against the real install for the reads and a scratch `XDG_DATA_HOME` for every write, with `proxy.yaml`'s hash checked either side and unmoved.

Flags, confirming §7: `import-settings` takes `-i,--input` and it is **REQUIRED**; `export-settings` and `export-logs` take `-o,--output`, optional, *"Can be a directory"*. All three artifacts are zip.

### The two exports are not distinguishable by name

Both write `adguard-cli_<YYYY-MM-DD>_<HH-MM-SS>.zip` into the directory given to `-o`. **Nothing in the filename says which command produced it.** Two exports taken a minute apart sit in a downloads folder as siblings, and the only way to tell them apart is to open them.

That matters because of what the next subsection measures.

### `export-settings` — nine files, and what is *not* in them

| In the bundle | Bytes |
| --- | --- |
| `proxy.yaml` | 8,950 |
| `https_exclusions.txt` | 72,563 |
| `user.txt` | 14 |
| `browsers.yaml` | 1,219 |
| `userscripts/adguard-extra.meta.json` | 25,661 |
| `userscripts/adguard-extra.user.js` | 531,969 |
| `agflm_standard.db` | 51,138,560 |
| `filters.yaml` | 543 |
| `config.txt` | 28 |

51.8 MB raw, 14.9 MB zipped, and **51.1 MB of that is `agflm_standard.db`** — the HTTP filter catalogue, which is redownloadable and is not the user's settings. An "export settings" that is 99 % filter database is worth knowing about before a progress spinner is designed for it.

`filters.yaml` is the enabled HTTP filters as `internal_filters`, each with `title`, `id` and `is_enabled`. `config.txt` is one line: `Application version: 1.4.13`.

**Three things are absent, and each one is load-bearing:**

- **`adguard.conf` is not in the bundle.** So the export carries **no licence and no CA private key** — §8's *"the CA travels in `adguard.conf`, and so does the licence"* cuts the other way here, and the good way. An exported settings zip is safe to hand to someone else or to a backup. This is the single most useful fact in this section.
- **`agflm_dns.db` and `dns_user.txt` are not in the bundle.** The DNS catalogue and the DNS user rules are **not exported**, while `proxy.yaml` — which *is* exported — carries every `dns_filtering.*` setting. So a round trip preserves DNS *settings* and loses DNS *filter selections* and *user rules*. The DNS page is a shipped v1 feature and half its content does not survive an export.

### `export-logs` — and it does not contain what this project said it did

| In the bundle | Bytes |
| --- | --- |
| `proxy.yaml` | 8,950 |
| `app.log` | 9,200,646 |
| `proxy.log` | 4,053,618 |
| `proxy.log.1` | 10,485,626 |
| `app_nm.log` | 218 |
| `config.txt` | 506 |

**`access.log` is not in it.** Measured on two separate runs, against an install that holds `access.log`, `access.log.1` and `access.log.2` — none of the three is bundled, while `proxy.log`'s rotated `.1` generation *is*. The omission is deliberate and reproducible.

This corrects a sentence this project had already written down. `overnight-v2.md` §2.3 stated that `export-logs` bundles *"`app.log`, `proxy.log` and `access.log`, which are a record of what the user browsed"*, and hung a UI requirement on it. Two thirds of that was right. **The claim was never measured**, and it is the §4 pattern again — a plausible list, asserted, in a document whose own §4 forbids exactly that.

**What it *does* contain that nobody would guess: `proxy.yaml`.** An "export logs" is also a full settings disclosure. `config.txt` here is richer than the settings bundle's — 506 bytes carrying the User-Agent (application version, kernel release, architecture), the install's **Application ID** (the same identifier §7 describes the activation link carrying), and the installed filter list. No e-mail and no licence key appear in it.

So the honest summary for a UI: **the logs bundle is less sensitive than assumed about browsing and more sensitive than assumed about configuration.**

### `-o` decides between "a file" and "a folder" by whether the path exists

Measured 2 August 2026, both exports, against a sandbox:

```console
$ adguard-cli export-settings -o /tmp/exp/          # /tmp/exp exists
Settings successfully exported to zip: /tmp/exp/adguard-cli_2026-08-02_14-40-36.zip
$ adguard-cli export-logs -o /tmp/exp2              # /tmp/exp2 does not exist
Logs successfully exported to zip: /tmp/exp2
$ file /tmp/exp2
Zip archive data
```

So an **existing directory** gets a generated `adguard-cli_<date>_<time>.zip`
inside it, and **anything else** becomes the archive itself — at that exact
path, with **no `.zip` appended**. A save dialog therefore cannot hand the CLI
whatever the user typed and then look for the name it chose: which of the two
happened depends on the filesystem, and the only reliable answer is the path on
the confirmation line, which both forms print.

### The `--` guard is *wrong* here, and the failure line looks like the success line

Two measurements from wiring the wrappers, 2 August 2026, and each one broke a
rule this contract states elsewhere.

**`--` must not be passed to `-o` or `-i`.** §5 makes `--` mandatory for every
`config` call. It is fatal for these:

```console
$ adguard-cli export-settings -o -- /tmp/exp      # exit 1
The following argument was not expected: /tmp/exp
```

`--` ends option parsing, and these subcommands have **no positional** to catch
the path afterwards. §5's guard is about a *value that looks like an option*
being read as one; it does not generalise to an option's own argument. All four
plain forms work and are equivalent — `-o <path>`, `-o<path>`, `--output <path>`
and `--output=<path>` — measured one per directory, for the reason below.

**Two exports into one directory within the same second collide, at exit 0.**
The generated name is `adguard-cli_<date>_<time>.zip`, one-second resolution,
and the CLI does not overwrite:

```console
$ adguard-cli export-logs -o /tmp/d      # exit 0
Logs successfully exported to zip: /tmp/d/adguard-cli_2026-08-02_15-10-06.zip
$ adguard-cli export-logs -o /tmp/d      # exit 0, immediately after
Failed to export logs to zip: /tmp/d/adguard-cli_2026-08-02_15-10-06.zip
$ ls /tmp/d | wc -l
1
```

**The failure line carries the same `zip: ` token and the same path as the
success line.** So a parser that reads the path out of `zip: ` returns the
archive the CLI just *failed* to write — which is not a hypothetical: the first
version of `Cli::exported` did exactly that, and this is reachable in one click
by a user pressing Export twice. Match the **success** prefix, `successfully
exported to zip: `, and treat everything else as a refusal.

It also invalidates any measurement that exports repeatedly into one directory.
A first attempt here compared four `-o` spellings that way and read three of
them as syntax failures; they were collisions. One directory per invocation, or
a second between them.

### `import-settings` creates `proxy.yaml`, and leaves an install that cannot filter HTTPS

Run against a **virgin** `XDG_DATA_HOME` with a settings zip:

```
Created data directory <dir>/adguard-cli
Settings successfully imported from zip: <zip>          # exit 0
```

This confirms §5's aside that `import-settings` is the only alternative to `configure` for creating `proxy.yaml`, and it is the whole of the first-run collision: **an unconfigured install offered an import is a second path through first run.** What that path produces, measured immediately afterwards:

- `proxy.yaml`, `agflm_standard.db`, `browsers.yaml`, `https_exclusions.txt`, `user.txt`, `userscripts/`, `logs/`, and an 88-byte `adguard.conf`.
- **The install is unlicensed.** `license` answers `You need to activate an AdGuard license to use this command`. The zip carried no licence and the import invented none.
- **There is no certificate.** No `AdGuard CLI CA.pem` and no `SSL/` directory, while the imported `proxy.yaml` holds `https_filtering: enabled: true`. That is precisely the *switch that reads on and cannot work* state `architecture.md` §5 requires to be marked — arrived at, this time, by the supported route rather than by a hand edit.
- `agflm_dns.db` appears **anyway**, dated with the shipped defaults rather than the export. It is seeded by the CLI, not restored from the bundle — consistent with it being absent from the zip.
- `dns_user.txt` is **absent immediately after the import** and appears after the next invocation of any command. A transient dangling reference: `proxy.yaml` names a file that does not exist yet. It self-heals, so it is a curiosity rather than a defect — recorded because a check run in that window would see a broken install.

### Feeding it the wrong zip succeeds

Because the two exports share a filename, this is not a hypothetical. `import-settings -i <a **logs** zip>` against a virgin directory:

```
Created data directory <dir>/adguard-cli
Settings successfully imported from zip: <zip>          # exit 0, identical wording
```

`proxy.yaml` is created — it was in the logs bundle — so configuration really is restored. But `app.log`, `proxy.log`, `proxy.log.1` and `app_nm.log` are unpacked **into the data directory root** rather than into `logs/`, and `browsers.yaml`, `https_exclusions.txt`, `user.txt` and `userscripts/` never arrive at all, because they were never in that zip.

**The result is a partial install reported as a complete success, in wording indistinguishable from the correct case.** This is *the confirmation is not the evidence* in its sharpest form yet: there is no exit code, no message and no filename that separates the right artifact from the wrong one. **A file picker handed straight to `import-settings` is not a safe design** — the manifest is the only discriminator (`filters.yaml` and `agflm_standard.db` for settings; `app.log` for logs), and reading it is a zip listing, not a protocol.

### What an import does *not* destroy

The confirmation dialog `architecture.md` §7 requires has to name what an import replaces, which means knowing what it leaves alone. Measured against a **configured, licensed** sandbox — a scratch `XDG_DATA_HOME` holding copies of `proxy.yaml`, `adguard.conf`, `AdGuard CLI CA.pem` and `SSL/`, with the sandbox first driven *away* from the zip's contents so that a restored value proves the import really wrote:

```
config set worker_threads 9      →  worker_threads: 9
import-settings -i <settings zip> →  worker_threads: 4      # the zip's value; the write is real
```

With the write thereby demonstrated rather than assumed:

| | Before | After |
| --- | --- | --- |
| `proxy.yaml` | diverged | replaced by the zip's |
| `adguard.conf` | `715f09b5…` | `715f09b5…` — **unchanged** |
| `AdGuard CLI CA.pem` | `65d7b3db…` | `65d7b3db…` — **unchanged** |
| `license` | owner shown | owner shown — **still active** |

**So an import is not a licence risk and not a certificate risk.** It replaces configuration and leaves credentials alone. A dialog that warned the user they were about to lose their licence would be saying something false, and this is the measurement that forbids it.

`config set` was checked in the same run and also leaves `adguard.conf` untouched, so neither write path disturbs it.

**`import-settings` is not licence-gated, where `configure` is.** It ran to completion on a virgin directory with no `adguard.conf` at all (above), which is the state §7 records `configure` refusing. That asymmetry is what makes a restore reachable by a user the first-run assistant would otherwise turn away.

### `adguard.conf`'s hash is not a stable fingerprint

Worth knowing before anyone builds a change-detector on it, in the way `proxy.yaml`'s hash is used throughout this project. **The real install's `adguard.conf` moved on its own during this session** — `a8678688…` when copied at 05:02, `fc8b693b…` when read again at 05:05 — across nothing but ordinary invocations and a running proxy. Its *size* stayed 3,116 B throughout.

**What causes it was not isolated, and is not guessed at here.** `config get`, `config set`, `license` and `import-settings` were each checked against a settled sandbox copy and none of them moved it; something else does. The usable conclusion is the negative one: **do not hash `adguard.conf` to detect anything**, and do not treat a moved hash there as evidence of a licence change. §4's rule about `proxy.yaml` — a moved hash means nothing until it has been diffed — applies here with no way to take the diff, since the file must not be read at all (`handoff.md` §4).

---

## 14. Checking for updates

`check-update` is the command behind [issue #4](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/4), and until this section existed the only thing recorded about it anywhere was that it touches the network and can hang. Measured 9 August 2026 on v1.4.13: **fourteen runs**, all inside half an hour — seven against this licensed install and seven against throwaway `$XDG_DATA_HOME` sandboxes, five of those the first run in a directory that did not exist yet.

The last three of the fourteen are `update_sandbox.rs` itself, on its first run once it existed. That is worth stating rather than folding in: this section was written after eleven, and the suite it specifies immediately produced three more — one of them a failure under the *filters* header, where the first eleven had only ever seen one there once. The section had already been written as though the split were settled.

**Fourteen is a count of runs somebody read**, not of times the command has been invoked in this tree. Every later `cargo test -- --ignored` runs it three more times and none of those are measurements: nobody reads the output, so nothing is learned and nothing here changes. A number that had to be incremented by a test suite would be wrong within a day of anyone using it.

**The name is wrong, and the wording of every string built on it has to survive that.** `check-update` does not check: it *performs* the content updates — filters, DNS filters, userscripts, Safe Browsing and CRLite are all refreshed — and only the application is checked rather than changed. `filters update` is documented upstream as the same operation.

### The output is header/verdict pairs, and the verdict does not name its component

```
Checking filters updates...
Up to date
Checking DNS filters updates...
Failed to update filters
Checking userscripts updates...
Up to date
Checking SafebrowsingV2 updates...
Updated
Checking CRLite updates...
Updated
Checking app updates...
Up to date
```

Six components, in that fixed order, each announced by a `Checking <name> updates...` line and answered on the next. Every one of the fourteen runs exited **0** with **empty stderr**, including the five that carried a failure.

| Verdict seen | After which header | Meaning |
| --- | --- | --- |
| `Up to date` | all six | nothing to do |
| `Updated` | SafebrowsingV2, CRLite | — but see below, it says this every time |
| `1 filter(s) updated` | filters | a count, and the noun is the component's |
| `1 DNS filter(s) updated` | DNS filters | as above |
| `1 userscript(s) updated` | userscripts | measured 15 August 2026 by ageing an installed script's metadata to `0.0.1` and watching it come back at `1.1.36`. The same `…updated` ending, so it needs no special case — see §15 |
| `Failed to update filters` | **filters *and* DNS filters** | the trap in this section |

**The failure sentence is the same for two different components, and it names neither.** Across the five failures `Failed to update filters` was printed under `Checking DNS filters updates...` three times and under `Checking filters updates...` twice — the identical string either way. So the *header* is the only thing that says which component failed, and a parser or a UI that keeps the verdict lines alone will report a DNS failure as an HTTP-filter failure, or vice versa. Pair each verdict with the header above it and never let the two travel separately. `UpdateReport` does that pairing, and `check_update_pairs_each_verdict_with_its_header` pins it against the two real captures that differ in nothing else.

**A failed component exits 0.** [§3](#3-exit-codes-are-only-half-trustworthy) again, in a new place: five of fourteen runs failed a component and all five reported success to the shell, with nothing on stderr. The exit status carries no information about the outcome here at all — it only says the command ran. `Cli::check_update` therefore derives every verdict from the text and lets the exit status decide nothing.

**And a failure is ordinary, not exceptional.** Five of fourteen, spread across the real install and three separate sandboxes, and in every case the *next* run of the same component succeeded — the run that reported `Failed to update filters` for DNS filters was followed by one reporting `1 DNS filter(s) updated`. So the UI treats a failed component as a normal outcome inviting a retry, not as an error state, and `update_sandbox.rs` deliberately does not assert their absence. **One caveat on the rate**: all fourteen runs fell inside half an hour, which is nothing like ordinary use, so 5/14 may say more about repeated requests to `filters.adtidy.org` than about what a user will see.

### `Updated` from Safe Browsing and CRLite is not news

Tallied over the eleven runs whose full output was captured — the other three printed only the lines their assertions named:

| install | SafebrowsingV2 | CRLite |
| --- | --- | --- |
| this licensed install, 7 runs | `Updated` — **7 of 7** | `Updated` — **7 of 7** |
| sandboxes, 4 runs | `Up to date` — 4 of 4 | `Up to date` — 4 of 4 |

Whatever the mechanism, on a working install those two answer `Updated` every single time, minutes apart, while a virgin one answers `Up to date`. **This forbids a summary line that counts components.** "2 of 6 updated" would render identically forever and would be measuring the CLI's habits rather than the user's install. The About page reports the six verdicts and does not add them up.

**File mtimes do not settle it either, in either direction.** Taken across one run: `agflm_standard.db`'s mtime moved while its component reported `Up to date` and its size was unchanged; `crlitedb`'s did **not** move while its component reported `Updated`; `agflm_dns.db` stayed put for a DNS failure; and `proxy.yaml` moved, as it does for every invocation including `--version` ([§5](#every-invocation-rewrites-proxyyaml)). The text is the only signal. Nothing downstream may infer a change from a timestamp.

### It runs unlicensed, and it creates the data directory

Two things that separate it from most of this CLI:

- **No licence needed.** `status`, `license` and `filters list` all exit 1 on an unlicensed install ([§3](#exit-1-is-usually-our-bug-but-not-always)), and every `filters` write subcommand does too ([§6](#marking-a-custom-filter-trusted)). `check-update` ran to completion in a virgin sandbox and updated filters there. So the control needs no licence caveat and works on an install the first-run assistant has never touched.
- **A first run prints a line that is not part of a pair**, before them all:

  ```
  Created data directory <dir>/adguard-cli
  Checking filters updates...
  ```

  It creates the directory as a side effect, exactly as `import-settings` does ([§13](#13-import-and-export)). A parser that assumes line 1 is a header reads the whole first run as unparseable, so everything before the first `Checking` is skipped. `check_update_skips_the_created_directory_line` covers it — and it is the run *every new install performs*, so getting it wrong would be wrong exactly where it is least recoverable.

### Cost, and what it does not block

Eight timed runs: **1.8 s to 7.3 s**, no outliers beyond that and no run anywhere near a hang. `NETWORK_TIMEOUT`'s 120 s stays the right ceiling — a hang is what it is there for, and `filters.adtidy.org` failing slowly is already in this machine's logs — but the ordinary case is a couple of seconds, so the UI needs a busy state rather than a progress bar.

**`status` is unaffected by an in-flight `check-update`, measured against a live daemon.** Eight `status` calls issued back-to-back while a `check-update` ran, on the real install with the proxy up:

| `status` during `check-update` | Wall time |
| --- | --- |
| first call | 0.396 s |
| calls 2–8 | 0.029 – 0.033 s |

The first is process start, not contention; the rest are the ordinary 0.03 s from [§2](#2-invocation-cost-measured). **So the 2 s poll does not need to pause for a content update** — which is the opposite of what the activation path does, and correctly so: `Cli::activate` stands down the poll because it runs for up to 120 s *and* changes licensing state under a page whose buttons the poll re-renders. Neither is true here. This is the second half of [§3](#once-the-directory-is-initialised-a-second-invocation-blocks)'s consequence — the poll is safe, the *affordance* is not — so the button desensitises itself and nothing else is held.

**It does not close §3's open limit, and the first draft of this section claimed it did.** That limit asks whether `status` avoids the config and filter-manager *lock* when a daemon is live. This run cannot answer it: it carried no positive control, so it cannot tell "`status` avoids the lock" from "`check-update` never took one" — and nothing here establishes that `check-update` takes it. The claim was struck within the hour, and it is worth leaving the correction visible, because it is [§6](#marking-a-custom-filter-trusted)'s lesson arriving in the newest section in the file: **the measurement was right and the sentence next to it was not.** What the About page needed was the narrow fact, and the narrow fact is what was measured.

### The daemon updates on its own, and the database records when

Measured 9 August 2026, after the About page was built and before a *check on launch* was considered for it. The question was whether such a check would be buying anything, and it is not: **AdGuard refreshes filters by itself, on a several-hour cadence, with no `adguard-cli` invocation involved.**

`FilterManagerImpl` update activity in `proxy.log`, on days when nothing here was running commands:

```text
07.08.2026 22:07:16   08.08.2026 01:07:15   08.08.2026 08:07:16
08.08.2026 14:18:31   08.08.2026 20:18:30   08.08.2026 22:18:31
09.08.2026 00:18:30   09.08.2026 05:18:31   09.08.2026 08:18:31   09.08.2026 12:18:31
```

Gaps of two to seven hours, every entry landing on the same second-offset within its run (`:07:15`, then `:18:30` after a daemon restart) — the shape of a timer started with the daemon. `adguard-cli`'s own outbound connections on 7 and 8 August were **10 and 2** in total, so none of this was provoked from here.

**So a check on launch was not built.** It would re-do, at every launch, work done at most a few hours earlier — and under `--background` it would fire at login with no window to report into. What the About page shows instead is the record the daemon leaves behind.

**`filter.last_download_time` moves only when data actually arrives.** Measured across three consecutive runs:

| run | filters verdict | `MAX(last_download_time)` |
| --- | --- | --- |
| 1 | `1 filter(s) updated` | 1786287992 — **moved** |
| 2 | `Up to date` | 1786287992 — unchanged |
| 3 | `Up to date` | 1786287992 — unchanged |

**This is the trap in the column, and it is the opposite of what its use invites.** A UI reaching for "when was this last updated" wants *checked*, and what the database offers is *changed*. A list nobody has revised in a week reads as a week old while AdGuard has been checking it hourly, so a row presenting it as staleness would send the user to press a button that answers `Up to date`. `Catalogue::last_downloaded` is named for what it measures and the row beside it says so in words.

**The newest, not the oldest, and they are far apart.** On this install the freshest enabled list was minutes old and the stalest eight days, because lists are revised on their own schedules. The oldest would report the least active list's author rather than anything about this machine.

**`last_update_time` is always the earlier of the two**, on every row inspected here — consistent with it being the list's own publication time rather than anything local. That is an inference from five rows and is written here as one; nothing renders it.

### Every invocation logs an app-update check, and it is not one

A trap for anyone reading `app.log` to find out how often this application touches the network. `check_app_update` appears **24,616 times** in the logs on this machine, which reads like a process checking constantly.

It is one line per invocation, whatever the invocation:

| command | new `check_app_update` lines |
| --- | --- |
| `--version` | 1 |
| `status` | 1 |
| `config get log_level` | 1 |

So the 2 s `status` poll produces one every two seconds while the window is open. **It does not mean a network check happened**: 301 lines were logged on 9 August against 128 outbound connections from `adguard-cli` in the whole day, and those connections cluster around the `check-update` runs. The message itself is empty — the log line ends `check_app_update: ...`, with the ellipsis being AdGuard's, not a truncation here.

Nothing in this project reads these logs. It is recorded because the count is alarming and wrong, and because the next person to go looking for evidence of network activity will find it first.

### The application half is deliberately half-measured

The app line has printed `Up to date` in all fourteen runs, because 1.4.13 is current. **What it says when an update exists has never been seen, and this project will not manufacture the condition to find out.** So nothing parses that line beyond asking whether it is exactly `Up to date`; anything else is shown to the user verbatim, as AdGuard's own sentence, alongside the `adguard-cli update` command to run.

**One shape is excluded from that pass-through, and it is not the unmeasured one.** A `Failed …` verdict on the app line is a failed *check*, not news of a release, so `UpdateReport::app_notice` returns `None` for it and the line is reported among the failures where it belongs. Without that the same event would be shown twice, once as a failure and once as an update notice offering a command to run — and the second of those would be advice derived from a check that did not complete. It costs nothing to be right about a shape nobody has seen: a failure of the app check has never been observed either, since all five measured failures were filter components.

**`adguard-cli update` is not measured and is never invoked**, which are the same decision. It re-runs an installer over a suid `adguard_root_helper`, and this application performs no privileged operation of its own ([§8](#8-privileged-operations), `architecture.md` §1) — it detects and instructs, as it already does for the root helper and the certificate. Not calling it is what makes not measuring it free, and it joins the list in `handoff.md` §3 item 7 for that reason. `update_channel` stays unclassified on the same grounds.

**No ANSI escapes appear in any of the fourteen runs.** Not a counter-example to [§4](#4-ansi-escapes-are-unconditional), which is about the CLI ignoring every opt-out rather than about every command colouring its output — but it does mean the stripper is a no-op on this command, and that the test fixtures are plain text rather than escape-laden captures.

---

## 15. Userscripts

`userscripts` is the command behind [issue #9](https://github.com/dominik-najberg/AdGuard-UI-Linux/issues/9), and nothing about it was recorded here before this section: `architecture.md` §7 had put the feature out of scope twice, so the five subcommands had never been measured beyond confirming that `list` returned one row. Measured 15 August 2026 on v1.4.13, in a throwaway `$XDG_DATA_HOME` seeded with the real install's `proxy.yaml`, `userscripts/` directory and licence. The machine's own install was diffed afterwards and was untouched.

### The upstream moved, and this section is the evidence

§7's standing decision rested on a fact that is no longer true: *only AdGuard Extra is supported*, so the feature was one switch for one pre-enabled script. That claim came from `proxy.yaml`'s own comment, which still reads:

```yaml
# List of userscripts. Currently only AdGuard Extra is supported.
userscripts:
  - meta: 'userscripts/adguard-extra.meta.json'
    content: 'userscripts/adguard-extra.user.js'
```

**The comment is stale.** Two arbitrary third-party userscripts, written for this measurement and served over loopback HTTP, installed and sat alongside AdGuard Extra — all three enumerated by `list`, all three carried in the `userscripts:` list, no complaint from the CLI at any point:

```
[x] | Title: AdGuard Extra                                   2026-08-15 11:44:02
    |    ID: adguard-extra
[ ] | Title: Hello Sandbox                                   2026-08-15 11:44:42
    |    ID: hello
[x] | Title: Hello World                                     2026-08-15 11:45:27
    |    ID: hello-world
```

§7 said to re-check when `adguard-cli` moved and to revisit the decision if the upstream ever supported more. Both halves are now discharged: it moved, and the decision was revisited.

### Enabled is not a flag — it is presence in `proxy.yaml`

The most consequential finding, because every obvious guess is wrong. There is no `enabled` key anywhere: not in `proxy.yaml`'s entries, which carry only `meta` and `content`, and not in the script's own `.meta.json`, whose 100 keys were enumerated and hold nothing of the kind.

**A userscript is enabled when it appears in the `userscripts:` list, and disabled when it does not.** `disable` deletes the entry and rewrites the key — down to `userscripts: []` when it was the only one — and leaves both files on disk:

```
$ adguard-cli userscripts disable adguard-extra
Userscript 'AdGuard Extra' disabled successfully

  userscripts:                                        userscripts: []
-   - meta: 'userscripts/adguard-extra.meta.json'
-     content: 'userscripts/adguard-extra.user.js'
```

So the state a UI renders comes from **two sources that must be read together**: the `userscripts/` directory says what is *installed*, and `proxy.yaml` says what is *on*. Neither answers on its own — the directory cannot tell a disabled script from an enabled one, and the config cannot see a script that is installed but off.

It also means `proxy.yaml` is where an external change lands, so the same `watch.rs` subscription that reconciles the settings pages reconciles these switches. A `userscripts disable` typed in a terminal moves a row in this application without anything else being consulted.

**`enable` does not restore the file byte-for-byte.** The stock entry quotes its paths and the rewritten one does not:

```yaml
userscripts:
  - meta: userscripts/adguard-extra.meta.json
    content: userscripts/adguard-extra.user.js
```

Harmless as YAML and identical to `yaml-rust2`, but worth knowing before anyone compares a hash across an enable — [§5](#every-invocation-rewrites-proxyyaml) already says a moved hash means nothing until it has been diffed, and this is a diff that changes only quoting.

### Do not parse `userscripts list` — read the `.meta.json`

[§6](#6-do-not-parse-filters-list)'s argument, in a second place. The table is bold-escaped ([§4](#4-ansi-escapes-are-unconditional)) and carries a checkbox, a title, an id and a timestamp:

```
Installed userscripts:
    | Details                                                Last update
[x] | Title: AdGuard Extra                                   2026-07-29 21:03:22
    |    ID: adguard-extra
```

**It has no version and no description** — and the version is exactly what issue #9 asks to display. Both are on disk. Every script is a `<id>.meta.json` + `<id>.user.js` pair in `userscripts/`, where the id is the filename stem, and the JSON is strictly better data than the table:

| Key | Notes |
| --- | --- |
| `name`, `description` | plus `name:xx` / `description:xx` for ~40 languages — **localised, and the table is not** |
| `version` | what #9 asks for; `""` on a script whose source omits `@version` |
| `homepageURL`, `supportURL` | either may be `""`; AdGuard Extra carries both |
| `downloadURL` | where it came from — **this is what makes a reinstall possible** |
| `updateURL` | AdGuard Extra has one; a script installed from a plain URL gets `""` |
| `icon` | a base64 PNG data URI — 16,226 bytes of AdGuard Extra's 25,661-byte metadata file, and `""` for a script without one |
| `match`, `include`, `exclude`, `grant`, `connect`, `require`, `resource` | the metadata block's lists |
| `run-at`, `noframes`, `unsafe_csp_required` | behaviour |

The localised keys are BCP-47-shaped (`pt-PT`, `zh-HK`, `pt`, `zh`) where the filter database's `filter_localisation.lang` is POSIX (`pt_BR` — [§6](#localisation-tags-are-posix-not-bcp-47)). **The two are not interchangeable**, and `crate::locale` answers in the form the *database* uses, so anything reading these files converts rather than assuming a match.

### `enable`/`disable`/`remove` match by substring, and it is a trap

They do not take an id. They take a **case-insensitive substring matched against both the id and the title**, and there is no exact-match flag anywhere in `--help-all`. Measured, every one at exit 0:

```
$ adguard-cli userscripts disable "AdGuard Extra"    # the title works
Userscript 'AdGuard Extra' disabled successfully
$ adguard-cli userscripts disable "Hello"            # a partial title works
Userscript 'Hello Sandbox' disabled successfully
$ adguard-cli userscripts enable "ADGUARD-EXTRA"     # case is ignored
Userscript 'AdGuard Extra' enabled successfully
```

Convenient from a shell, and the reason it matters is the collision. With `hello` and `hello-world` both installed, the **exact id of an installed script is refused**:

```
$ adguard-cli userscripts disable hello
Multiple userscripts match 'hello'. Please specify more precisely:
  - Hello Sandbox (ID: hello)
  - Hello World (ID: hello-world)
```

There is nothing more precise to be specified. `hello` *is* that script's whole id, the flag that would disambiguate does not exist, and the same refusal blocks `enable` and `remove` alike.

**So a userscript whose id or title is a substring of another installed script's id or title cannot be switched on, switched off, or removed — by this application or by anyone using the CLI.** It is an upstream boundary and not a gap in the wrapper: no argument reaches past it. What the application can do is *see it coming* — it knows every id and every title, so it can compute the collision before rendering a control and say why the row is inert, rather than offering a switch that fails at exit 0. That is the same treatment `architecture.md` §7 records for the window position: build the half that exists, and name the other half as a boundary.

The condition is cheap to state and worth stating exactly, because a narrower version of it would be wrong: a script is unreachable when its id is a substring of *another* script's id **or title** — not merely of another id, since the match runs against both fields.

### `install` is network-only, and refuses a local file

Unlike `filters install`, which takes a URL or a path through the same positional and normalises the path to `file://` ([§6](#installing-a-custom-filter)), this takes a URL and nothing else. Measured, both at exit 0, nothing written:

```
$ adguard-cli userscripts install /tmp/hello.user.js
Failed to install userscript
$ adguard-cli userscripts install file:///tmp/hello.user.js
Failed to install userscript
```

**But `http://127.0.0.1:<port>/…` works**, which is what keeps a test suite honest: `userscripts_sandbox.rs` serves its fixtures from a loopback socket and reaches no network, exactly as `filters_sandbox.rs` reaches none through `file://`. The two suites arrive at hermeticity by different routes because the two commands accept different things.

```
$ adguard-cli userscripts install http://127.0.0.1:8731/hello.user.js
Userscript installed and enabled successfully
```

**`Failed to install userscript` is the only failure sentence, and it covers everything.** A 404, a body that is not a userscript, a local path and a `file://` URL all produce that one line — indistinguishable, so neither the wrapper nor the UI may claim to know which happened. The same rule [§6](#installing-a-custom-filter) states for `Failed to install the filter from URL`, with an even blunter message: this one does not even echo what was passed.

Note the confirmation says **"installed and enabled"**. A new script arrives switched on; there is no install-disabled path.

### Re-installing is the update path, and it silently re-enables

Installing a URL that is already installed is not refused — it overwrites in place. Measured by editing the served file's `@version` between two installs:

```
version before: 0.2.1
version after:  0.9.9
```

That makes *Reinstall* buildable from the recorded `downloadURL`, and it is the only update mechanism a single script has ([`check-update`](#14-checking-for-updates) refreshes userscripts wholesale but says only `Up to date` or otherwise for the component as a whole).

**It also turns a disabled script back on.** The script above was disabled before the second install and enabled after it, with no mention of the fact in the output. Anything offering a reinstall has to disclose that, because the user who disabled a script and then updates it did not ask for it to start running again.

### Every refusal is exit 0, and each has its own sentence

[§3](#3-exit-codes-are-only-half-trustworthy) once more. Success must be defined positively, and the refusals are worth telling apart because they mean different things:

| Output | Meaning | Recoverable? |
| --- | --- | --- |
| `Userscript 'X' enabled successfully` | it worked, or was already on | — |
| `Userscript 'X' disabled successfully` | it worked | — |
| `Userscript 'X' removed successfully` | files deleted, entry dropped | — |
| `Userscript installed and enabled successfully` | it worked | — |
| `Userscript 'X' is not enabled` | `disable` on something already off — a no-op, not a failure | nothing to do |
| `No userscripts matching 'x'` | no id or title contains the string | the id is wrong |
| `Multiple userscripts match 'x'` + a list | the substring trap above | **no** — permanent while both are installed |
| `Failed to install userscript` | 404, bad body, local path, bad host | retry with a different URL |

`enable` on an already-enabled script answers `enabled successfully` rather than the `is not enabled` form its opposite uses — so the two no-ops are **not** symmetrical, and only the `disable` one is detectable from the text.

### AdGuard's own four, and where they come from

The four userscripts AdGuard bundles with its Windows and Mac applications are
all installable on Linux from AdGuard's own CDN. Measured 15 August 2026, each
into a throwaway data directory:

| Script | id | URL | AdGuard ships it |
| --- | --- | --- | --- |
| AdGuard Extra | `adguard-extra` | `…/release/adguard-extra/1.0/adguard-extra.user.js` | **on** |
| AdGuard Popup Blocker | `popupblocker` | `…/release/popup-blocker/2.5/popupblocker.user.js` | **on** |
| AdGuard Assistant | `assistant` | `…/release/assistant/4.3/assistant.user.js` | off |
| Web of Trust | `wot` | `…/release/adguard-wot/1.0/wot.user.js` | off |

All on `https://userscripts.adtidy.org`. `adguard-cli` ships only *Extra*; the
other three install exactly as any third-party script does.

**The version in the path is a channel, not a release.** `…/assistant/4.3/…`
served Assistant **4.4.13** and `…/adguard-extra/1.0/…` served Extra **1.1.36**,
so these URLs stay current on their own and nothing here needs to track a
release. It also means a local copy of a script goes stale while its URL does
not — measured against four `.txt` copies taken from a Windows install, every
one of which was behind the CDN by at least a patch version.

**None of the four collides with another** under the substring rule above:
`adguard-extra`, `popupblocker`, `assistant` and `wot` are each absent from the
other three's ids and titles, so all four are switchable with all four
installed. That is a property of these particular names rather than a
guarantee, which is why `the_recommended_four_do_not_collide_with_each_other`
re-derives it from the rule rather than trusting the observation.

**`install` always enables**, so arriving in AdGuard's own default state takes
two commands for the two that ship off — install, then disable.

**None of these is proof of anything**, as everywhere in this file. `remove` reports success and the caller confirms by the pair of files being gone and the entry having left `proxy.yaml`; a switch confirms against `proxy.yaml`; an install confirms against the directory. The re-read is the verdict, and the sentence is only a hint about which re-read to expect.
