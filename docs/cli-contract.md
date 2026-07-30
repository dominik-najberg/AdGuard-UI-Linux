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
| `adguard_cli_nm` (Native Messaging host) | Locked to specific browser extension IDs via manifests. No AdGuard manifest is installed on this machine — `install-browser-integration` has never been run. Impersonating a browser extension is fragile and rude. |
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

Process startup is ~10–30 ms. Polling `status` on a 1–2 s timer is entirely affordable; there is no need for a persistent connection or a caching daemon.

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
- List-valued keys (`filters`, `userscripts`, `apps`) are not scalars — `config get filters` refuses. Use `list-add`/`list-remove`, or edit an auxiliary file via `--list-file`.
- **`config get` does not mask secrets.** `config get listen_auth.password` prints `listen_auth.password = admin` in full; only `config show` masks, as `password: <set>`. So `config get` is not a safe thing to log.
- `config reset <key>` restores the shipped default and confirms in the same way (`log_level` → `info`). Not used yet; the obvious home for it is a "restore default" affordance per row.

### Measuring writes without touching the real config

**The CLI resolves its data directory as `$XDG_DATA_HOME/adguard-cli`.** Pointing that at a scratch directory holding a copy of `proxy.yaml` gives a complete, throwaway AdGuard configuration:

```bash
XDG_DATA_HOME=/tmp/sandbox adguard-cli config set listen_address 0.0.0.0
```

This is how everything in this section was measured, and `Cli::with_xdg_data_home` exposes it to `tests/config_sandbox.rs`. It matters because the interesting write behaviours are the ones nobody should provoke on a real machine: exposing the proxy on `0.0.0.0`, blanking the proxy password, setting a listen port to a value that takes the listener down.

Two limits:

- A sandbox is an **unlicensed** install, and copying `gm.db` across does not change that, so the licence evidently lives elsewhere. `status`, `license` and `filters list` all fail there with exit 1 (see [§3](#exit-1-is-usually-our-bug-but-not-always)). The `config` family, `--version` and `activate` need no licence and behave exactly as they do for real — `activate` because it is the command that exists to *fix* an unlicensed install, which makes a sandbox the only honest place to exercise it.
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

### List writes: `list-add` and `list-remove`

A handful of keys hold YAML sequences rather than scalars — `filters`, `userscripts`, `apps`, and `dns_filtering.filters`. `config get` refuses them (§5 above), and they are written with `config list-add` / `config list-remove` instead. Measured against a sandbox seeded from the real file:

| Invocation | Exit | stdout | Effect |
| --- | --- | --- | --- |
| `list-add -- dns_filtering.filters extra.txt` | 0 | the whole list, then `Config has been updated` | **one** line added |
| `list-add` of a value **already in the list** | 0 | the same shape, showing the value twice | **a duplicate is appended** |
| `list-remove` of a value **not** in the list | 0 | the unchanged list + `Config has been updated` | nothing |
| `list-remove` of the **last** element | 0 | `filters:` with nothing after it | the key is left **null**, not `[]` |
| `list-add -- <a scalar key> <value>` | 0 | `This field is not a list setting` + advice to use `config set` | nothing |
| `list-add <key> -leading-dash` (no `--`) | **1** | *(empty)* — `<value> is required` on **stderr** | nothing |

Five things follow, and three of them are traps.

**The write is as surgical as `config set`.** One line added, and the comment count does not move — measured at 220 → 221 lines with 105 comments either side. So the no-YAML-writes rule (§5 opening) covers list keys too, and a `list_add_disturbs_exactly_one_line` assertion is worth having for the same reason `a_write_disturbs_exactly_one_line` is.

**`list-add` does not deduplicate.** Adding a value the list already holds appends it a second time and reports success, which makes the confirmation line useless as evidence yet again. Anything driving a *toggle* off a list — the DNS user-rules row is exactly this — must read the list, decide membership itself, and issue the call only when it would change something. Re-issuing on a stale read silently corrupts the list rather than no-opping.

**Removing the last element leaves a null, and a null is not an empty list.** The key becomes a bare `filters:`, which `yaml-rust2` reads as `Yaml::Null`, so `Config::list_at` — which matches `Yaml::Array` only — answers `None`. `None` is the crate's "unreadable" answer, so a row rendering it by the usual rule would go *unavailable* the instant the user emptied the list, having just successfully emptied it. The next invocation normalises the key to `[]` (§5, "every invocation rewrites `proxy.yaml`"), so the state is transient — which makes it worse, not better, because it heals before anyone investigating can see it. A membership test therefore has to read null and absent as *"the list does not contain this"*, and reserve `None` for a key that is a scalar or a mapping — something that genuinely cannot be interpreted as a list at all.

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

---

## 7. Commands that need a TTY

`configure` and `activate` are interactive and cannot be driven headlessly. So, conditionally, is one `config set`.

- **`configure`** — a wizard that writes the same keys we can set individually. The GUI reimplements it as a first-run assistant calling `config set`. Never invoke it.
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
- No rotation policy is configured by us — `proxy.log` was already 8 MB.
- There is **no push or event mechanism**. A live view must tail the file.

`har_writer` (`enabled`, `location`) is the richer alternative for debugging but writes full HAR dumps — too heavy for an always-on UI.

**Treat live stats as v2.** Nothing in the CLI provides a counter or stats endpoint.

---

## 10. Wrapper-layer checklist

Anything in the `adguard-cli` wrapper crate must:

1. Strip ANSI from every captured stream.
2. Treat exit 1 as *our* bug — with the three exceptions in [§3](#exit-1-is-usually-our-bug-but-not-always) — and detect user-facing failure by matching stdout text. A failure whose only text is on stdout is never our command line, whatever the exit status.
3. Verify state changes by re-reading state, not by trusting exit 0.
4. Never write `proxy.yaml`, and never write the `.db` files.
5. Never invoke `configure`, or any other command that expects a TTY to be useful. `activate` is the one exception, and only because closing stdin makes its no-TTY branch the only branch: it prints a log-in link and returns rather than waiting for anything ([§7](#7-commands-that-need-a-tty)).
6. Apply a timeout — network commands (`check-update`, `filters update`, `update`) can hang; a filter update failure was observed in the logs (`HttpClientNetworkError` reaching `filters.adtidy.org`).
7. Run off the GTK main thread.
8. Spawn with **stdin closed**, so a command that would prompt cannot hang ([§7](#the-wrapper-closes-stdin-so-no-tty-is-the-only-path)).
9. Pass `--` before any user-supplied key or value ([§5](#the----guard-is-mandatory)).
10. Range-check numbers itself — `config set` only type-checks ([§5](#config-set-type-checks-and-nothing-else)).
11. Keep secrets out of error text. `config set` echoes the value it was given, and our own `BadInvocation` quotes the whole command line, so a refused password write would otherwise leak into any toast that shows it. `Cli::set_secret` scrubs every variant that carries our arguments.
