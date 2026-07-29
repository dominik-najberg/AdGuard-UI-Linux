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

**The rule:** exit code 1 means the *argument parser* (CLI11) rejected the command line, and the message goes to **stderr**. Every *semantic* failure — unknown config key, wrong key type, missing section — prints to **stdout** and exits **0**.

Consequences for the wrapper layer:

- A non-zero exit is a programming error in our code (we built a malformed command line), not a user-facing condition. Log it loudly; it should never reach the user as a normal outcome.
- Real failures must be detected by **matching output text**. This is inherently brittle: pin the patterns in one place, and treat an unrecognised output shape as failure rather than success.
- Never conclude "the operation worked" from exit 0 alone. For state changes, re-read the resulting state and verify.

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

Serialising a parsed YAML document back over it would delete all of that. `serde_yaml` does not round-trip comments.

**Rule: all writes go through `adguard-cli config set|reset|list-add|list-remove`.** Read freely with `serde_yaml`; never write.

`config show` is a **rendered view, not the file**:

- Large sections are collapsed to `<folded> enabled` / `<folded> disabled`.
- Secrets are masked — `config show listen_auth` prints `password: <set>` where the file contains `password: 'admin'`.
- Therefore: parse `proxy.yaml` for real values, and use `config show` only when mirroring the CLI's own presentation.

Key syntax facts:

- Dotted paths work for scalars: `config get stealthmode.enabled`, `config get listen_ports.http_proxy`.
- `config show <section>` accepts **top-level** sections only. Nested ones fail: `config show anti_dpi` → `not found`, even though `stealthmode.anti_dpi` exists in the file. Expand the parent instead.
- List-valued keys (`filters`, `userscripts`, `apps`) are not scalars — `config get filters` refuses. Use `list-add`/`list-remove`, or edit an auxiliary file via `--list-file`.

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

`configure` and `activate` are interactive and cannot be driven headlessly.

- **`configure`** — a wizard that writes the same keys we can set individually. The GUI reimplements it as a first-run assistant calling `config set`. Never invoke it.
- **`activate`** — browser-based licence flow. Without a TTY it prints: *"No TTY for user input. Please visit <url> to log in, then run `activate` again."* The GUI should open that URL with `gtk::UriLauncher` and then poll `license` until status becomes `APP_ACTIVE`. `activate` is absent from `--help-all` but is a real command.

---

## 8. Privileged operations

`adguard_root_helper` is **not setuid** (`-rwxr-xr-x potworny potworny`) and the package ships **no polkit policy** — a search of `/usr/share/polkit-1/actions/` and `/etc/polkit-1/` for "adguard" returns nothing.

So there is no existing escalation path to reuse. Anything needing root — `proxy_mode: auto` (system-wide traffic redirection), system DNS filtering, installing the CA into the system trust store — requires us to author our own polkit action. See `architecture.md` §6.

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
2. Treat exit 1 as *our* bug; detect user-facing failure by matching stdout text.
3. Verify state changes by re-reading state, not by trusting exit 0.
4. Never write `proxy.yaml`, and never write the `.db` files.
5. Never invoke `configure`, `activate` (bare), or any command expecting a TTY.
6. Apply a timeout — network commands (`check-update`, `filters update`, `update`) can hang; a filter update failure was observed in the logs (`HttpClientNetworkError` reaching `filters.adtidy.org`).
7. Run off the GTK main thread.
