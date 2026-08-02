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

**The echo says a title the database does not have.** A list with no `! Title:` header is confirmed as `Filter [Title: file:///…/untitled.txt]` while the `title` column is set to the empty string. The localised name in [§6](#do-not-parse-filters-list) then falls back through `COALESCE` to that same empty string — custom filters have no `filter_localisation` rows at all — so the row renders with **no name whatsoever** unless the UI supplies its own fallback. This is the section's own rule landing in a new place: the confirmation is not the effect, even when the confirmation is the friendlier of the two.

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

### The wrapper closes stdin, so "no TTY" is the only path

Everything measured about that prompt was measured without a TTY, where the CLI gives up immediately and warns. But a child process inherits its parent's stdin, and **a GUI started from a terminal has a real one** — so the same call that no-ops in every test would sit there indefinitely waiting for a username to be typed into a terminal the user has stopped looking at, holding a worker thread and leaving the control that triggered it spinning.

`Cli::run` therefore spawns with `Stdio::null()`. It makes the no-TTY behaviour deterministic however the app was launched, and nothing here has anything to say on stdin anyway. It is not a substitute for the precondition check — a silent no-op is still a silent no-op — but it removes the hang.

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
