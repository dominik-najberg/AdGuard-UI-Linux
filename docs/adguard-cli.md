# AdGuard CLI — Reference

Documentation for `adguard-cli` v1.4.13 as installed on this machine.

`adguard-cli` is AdGuard's standalone Linux ad blocker. It runs as a **local filtering proxy** (HTTP + SOCKS5) that applications route traffic through. It is not a system daemon by default — it forks into the background as your user and keeps all state under your home directory.

---

## Table of contents

- [Installation layout](#installation-layout)
- [Quick start](#quick-start)
- [Command reference](#command-reference)
  - [Lifecycle: start / stop / restart / status](#lifecycle)
  - [configure](#configure)
  - [cert](#cert)
  - [config](#config)
  - [filters](#filters)
  - [dns](#dns)
  - [userscripts](#userscripts)
  - [Updates: check-update / update](#updates)
  - [License: license / activate / reset-license](#license)
  - [Backup: export-settings / import-settings / export-logs](#backup-and-diagnostics)
  - [speed](#speed)
  - [install-browser-integration](#install-browser-integration)
- [Configuration file reference](#configuration-file-reference)
- [Shell completion](#shell-completion)
- [Recipes](#recipes)

---

## Installation layout

This install is **user-local** (no root, nothing in `/opt` or `/etc`).

| Path | Purpose |
| --- | --- |
| `~/.local/bin/adguard-cli` | Symlink on `$PATH` → the real binary |
| `~/.local/opt/adguard-cli/` | Program directory |
| `~/.local/share/adguard-cli/` | Data directory: config, databases, logs, certs |

Contents of the program directory:

| File | Purpose |
| --- | --- |
| `adguard-cli` | Main binary |
| `adguard_cli_nm` | Native Messaging host (browser integration) |
| `adguard_root_helper` | Privileged helper for system-wide modes |
| `certutil`, `install_cert.sh` | Certificate installation into browser/system trust stores |
| `bash-completion.sh` | Bash completion script |
| `defaults.zip` | Bundled default filters/settings |
| `*.sig` | Signatures for the shipped binaries |

Contents of the data directory:

| File | Purpose |
| --- | --- |
| `proxy.yaml` | **Main configuration file** |
| `adguard.conf` | Encrypted blob: license and account state |
| `adguard.pid`, `agcli.socket` | Runtime PID file and control socket |
| `AdGuard CLI CA.pem`, `SSL/` | Generated HTTPS-filtering root CA |
| `user.txt` | Your custom HTTP filtering rules |
| `dns_user.txt` | Your custom DNS filtering rules |
| `https_exclusions.txt` | Domains excluded from HTTPS filtering |
| `browsers.yaml` | Per-browser filtering actions, included by `proxy.yaml` |
| `agflm_standard.db`, `agflm_dns.db` | Compiled filter list databases |
| `sfbr.db`, `crlitedb/` | Safe Browsing and CRLite databases |
| `userscripts/` | Installed userscripts (metadata + JS) |
| `logs/` | `app.log`, `proxy.log`, `access.log` |

---

## Quick start

```bash
adguard-cli configure   # interactive wizard: ports, HTTPS filtering, filters
```

```bash
adguard-cli start && adguard-cli status
```

Then point your applications at the proxy. On this machine that is:

- HTTP proxy — `127.0.0.1:3129`
- SOCKS5 proxy — `127.0.0.1:1081`

For HTTPS filtering to work, the generated root CA must be trusted by the client — see [`cert`](#cert).

Global options:

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Help for the current command |
| `--help-all` | Recursive help — all commands and options |
| `-v`, `--version` | Print version (`AdGuard CLI v1.4.13`) |

> **Scripting note.** The CLI emits ANSI bold escapes **unconditionally** — even when its output is piped rather than attached to a terminal. `NO_COLOR=1` and `TERM=dumb` have no effect. Strip them yourself, e.g. `adguard-cli status | sed -e 's/\x1b\[[0-9;]*m//g'`.

---

## Command reference

### Lifecycle

```bash
adguard-cli start
adguard-cli stop
adguard-cli restart
adguard-cli status
```

`start` options:

| Option | Meaning |
| --- | --- |
| `--no-fork` | Run in the foreground — use this under systemd/supervisor |
| `--pid-file <path>` | Write the process ID to a file |
| `--ppid-file <path>` | Write the parent process ID to a file |
| `--log-to-file` | Redirect process output to a file; only meaningful with `--no-fork` |

`status` reports the listening endpoints and which filtering modes are live:

```
The AdGuard proxy server is running
HTTP proxy is listening on 127.0.0.1:3129
SOCKS5 proxy is listening on 127.0.0.1:1081
Manual DNS proxy is disabled
System-wide automatic filtering is disabled
System-wide DNS filtering is disabled
```

### configure

```bash
adguard-cli configure
```

Interactive wizard covering proxy mode, listen ports, HTTPS filtering, and filter selection. It writes to `proxy.yaml` — the same file `config set` edits. Requires a TTY.

### cert

```bash
adguard-cli cert
adguard-cli cert --firefox-profile abcd1234.MyProfile
```

Generates the HTTPS-filtering root CA (`AdGuard CLI CA`) and installs it into the system/NSS trust stores. Without a trusted CA, HTTPS filtering breaks TLS for every client.

Firefox does **not** use the system store — it keeps its own NSS database per profile. Pass the profile *directory name* (as found in `~/.mozilla/firefox/`) via `--firefox-profile` to add the certificate there. Repeat once per profile.

### config

Reads and writes `proxy.yaml`. You may also edit that file directly.

```bash
adguard-cli config show [<section-name>] [--list-file <path>]
adguard-cli config get <key>
adguard-cli config set <key> <value>
adguard-cli config reset [<key>] [--all]
adguard-cli config list-add    <key> <value> [<value>] [<value>]  [--list-file <path>]
adguard-cli config list-remove <key> <value>                      [--list-file <path>]
```

**Keys are dotted paths.** Nested settings are addressed with `.`:

```bash
adguard-cli config get stealthmode.enabled       # -> stealthmode.enabled = false
adguard-cli config get listen_ports.http_proxy   # -> listen_ports.http_proxy = 3129
adguard-cli config set listen_address 0.0.0.0
```

**Folded sections.** `config show` with no argument collapses large sections, printing `<folded> enabled` / `<folded> disabled`. Pass the section name to expand it:

```bash
adguard-cli config show stealthmode
adguard-cli config show https_filtering
```

Foldable sections are: `listen_auth`, `https_filtering`, `dns_filtering`, `safebrowsing`, `crlite`, `stealthmode`, `outbound_proxy`, `har_writer`.

`config show <section>` only accepts **top-level** section names. Nested sub-sections (e.g. `anti_dpi` inside `stealthmode`) report `not found` — expand the parent instead.

**Lists** (`filters`, `userscripts`, `apps`, …) are not scalar settings. `config get filters` refuses with *"This field is not a separate setting"*; use `config show filters` to inspect, and `list-add` / `list-remove` to modify. `--list-file` points these operations at a different YAML file (e.g. `browsers.yaml`) instead of `proxy.yaml`.

**Reset.** `config reset <key>` restores one setting to its default; `config reset --all` restores everything.

> **Note on error handling.** Exit codes only report *argument-parsing* failures. An unknown subcommand, a missing argument, or a bad flag exits **1** and writes to **stderr**. But *semantic* failures — unknown config key, wrong key type, missing section — print to **stdout** and exit **0**:
>
> ```
> adguard-cli config get bogus_key   # prints "'bogus_key' not found", exits 0
> adguard-cli bogus-subcommand       # prints to stderr, exits 1
> ```
>
> In scripts, do not treat exit 0 as success for these commands — match the output text instead. (When checking this yourself, avoid a pipe: `cmd | head; echo $?` reports `head`'s status, not the CLI's.)

### filters

Manages HTTP/HTTPS content filters.

```bash
adguard-cli filters list [--all]
adguard-cli filters add <filter-id>...
adguard-cli filters install <filter-url> [--trusted] [--title <text>]
adguard-cli filters remove <filter-id>
adguard-cli filters enable  <filter-id>...
adguard-cli filters disable <filter-id>...
adguard-cli filters set-trusted <filter-id> <true|false>
adguard-cli filters set-title   <filter-id> <title>
adguard-cli filters update
```

- `list` shows only added filters; `list --all` shows the full catalog grouped by category (Ad blocking, Privacy, Social widgets, Annoyances, Security, Other, Language-specific). `[x]` marks an enabled filter.
- `add` takes built-in filters by numeric ID or by name, and accepts several at once. IDs come from `list --all` — e.g. `2` = AdGuard Base filter, `3` = AdGuard Tracking Protection, `4` = AdGuard Social Media.
- `install` adds a **custom** filter from a URL or a local file path.
- `--trusted` / `set-trusted` allows the list to use privileged rule types (such as scriptlet and `$$`/HTML-filtering rules). Only mark lists you actually trust — a trusted list can inject script into pages.
- `filters update` refreshes filters, DNS filters, userscripts, Safe Browsing, CRLite, **and** checks for app updates. It is the same operation as [`check-update`](#updates).

```bash
adguard-cli filters add 3 17          # tracking protection + URL tracking
adguard-cli filters install https://example.org/my.txt --title "My list"
adguard-cli filters disable 2
```

### dns

DNS-level filtering. The subcommands mirror `filters`, against a separate DNS filter catalog.

```bash
adguard-cli dns filters list [--all]
adguard-cli dns filters add <filter-id>...
adguard-cli dns filters install <filter-url> [--title <text>]
adguard-cli dns filters remove  <filter-id>
adguard-cli dns filters enable  <filter-id>...
adguard-cli dns filters disable <filter-id>...
```

Differences from HTTP `filters`:

- No `--trusted` / `set-trusted` — DNS lists are hostname-only, so the trusted-rule concept does not apply.
- `dns filters update` is deprecated; its help redirects you to `adguard-cli check-update`.

The DNS catalog is grouped into General, Other, and Regional — e.g. `1` = AdGuard DNS filter, `5` = OISD Blocklist Small, `33` = Steven Black's List, `48` = HaGeZi's Pro Blocklist.

DNS filtering must also be switched on in the config; it is off by default:

```bash
adguard-cli config set dns_filtering.enabled true
adguard-cli restart
```

### userscripts

```bash
adguard-cli userscripts list
adguard-cli userscripts install <userscript-url>
adguard-cli userscripts remove  <userscript-name>
adguard-cli userscripts enable  <userscript-name>
adguard-cli userscripts disable <userscript-name>
```

Userscripts are injected into filtered pages. `install` takes a URL only (not a local path). `remove`/`enable`/`disable` take the script's **name/ID** as shown by `list` — e.g. `adguard-extra`, not the display title "AdGuard Extra". AdGuard Extra ships pre-installed.

Scripts are stored as a `.meta.json` + `.user.js` pair under `~/.local/share/adguard-cli/userscripts/` and referenced from the `userscripts:` list in `proxy.yaml`.

### Updates

```bash
adguard-cli check-update
adguard-cli update [-v|--verbose]
```

- `check-update` refreshes filters, DNS filters, userscripts, Safe Browsing, CRLite, and checks whether a newer app version exists. Identical to `filters update`.
- `update` updates the **application itself** by running the bundled update script. `--verbose` shows that script's output.

### License

```bash
adguard-cli license         # show owner, key, and status
adguard-cli activate        # activate a license (undocumented in --help-all)
adguard-cli reset-license   # unbind this installation from its license
```

`license` prints the license owner, key, and status (`APP_ACTIVE` when in force).

`activate` does not appear in `--help-all` but is a real command (it is listed in the bundled bash completion). It opens a browser-based activation flow and offers activation with an existing subscription. It requires a TTY — without one it prints the activation URL and asks you to re-run `activate` after logging in. If a license is already active it declines and points you at `license`.

`reset-license` releases the license from this machine — needed before moving the license to different hardware.

### Backup and diagnostics

```bash
adguard-cli export-settings  [-o|--output <path>]
adguard-cli import-settings  -i|--input <path>       # required
adguard-cli export-logs      [-o|--output <path>]
```

`export-settings` writes a settings zip; `export-logs` bundles `app.log`, `proxy.log`, and `access.log` for support reports. For both, `--output` may be a file path or a directory (the filename is then chosen automatically). `import-settings` restores from a settings zip and **requires** `--input`.

```bash
adguard-cli export-settings -o ~/backups/
adguard-cli import-settings -i ~/backups/settings.zip
```

### speed

```bash
adguard-cli speed [--json] [-c|--chunk <bytes>]...
```

Benchmarks cryptographic operations and HTTPS filtering throughput. `--chunk` is repeatable and sets the message sizes tested; the default set is `16, 256, 1350, 8192, 16384` bytes. `--json` emits machine-readable output.

```bash
adguard-cli speed --json -c 1350 -c 65536
```

### install-browser-integration

```bash
adguard-cli install-browser-integration
adguard-cli install-browser-integration -u|--uninstall
```

Installs (or removes) the Native Messaging manifests that let the AdGuard browser extension talk to the local `adguard_cli_nm` host.

---

## Configuration file reference

Main file: `~/.local/share/adguard-cli/proxy.yaml`. Values below are this machine's current settings, shown as a worked example of the schema.

### Top level

| Key | Example | Meaning |
| --- | --- | --- |
| `proxy_mode` | `auto` | `manual` — apps opt in by pointing at the proxy. `auto` filters system-wide via the root helper. This machine was moved to `auto` by its owner on 1 August 2026; the row read `manual` until then. |
| `listen_address` | `127.0.0.1` | Interface the proxy binds to. `0.0.0.0` exposes it to the LAN — pair with `listen_auth`. |
| `listen_ports.http_proxy` | `3129` | HTTP proxy port |
| `listen_ports.socks5_proxy` | `1081` | SOCKS5 proxy port |
| `filtered_ports` | `80:5221,5300:49151` | Destination port ranges that get filtered |
| `worker_threads` | `4` | Proxy worker thread count |
| `outbound_interface` | `null` | Bind outgoing connections to a specific interface |
| `ad_blocking_enabled` | `true` | Master switch for ad blocking |
| `adguard_headers_enabled` | `false` | Add AdGuard's own HTTP headers |
| `auto_enable_language_filters` | `true` | Adds and enables catalogue filters for **the language of the pages you visit as well as your system locale**. Never disables anything. Corrected 2 August 2026 — the previous gloss said "matching your system language", which dropped the half that runs continuously |
| `filters` | `flm://`, `user.txt` | Active HTTP filter sources. `flm://` = the managed filter-list database; `user.txt` = your custom rules. |
| `userscripts` | list of meta/content pairs | Installed userscripts |
| `apps` | see below | Per-application filtering rules |
| `log_level` | `info` | Logging verbosity |
| `access_log_file` | `access.log` | Access log filename. Resolved against `<data>/logs/`, **not** the data dir — measured 2 August 2026, the file is at `~/.local/share/adguard-cli/logs/access.log`. Do not generalise that base to the other relative keys; see contract §9 |
| `update_channel` | `release` | App update channel |
| `send_crash_reports` | `false` | Crash telemetry |
| `show_hints` | `true` | CLI hint text — measured, it lands between the echo and the confirmation of a `config set` (contract §5) |
| `show_notifications` | `false` | **Unmeasured.** The file's comment says only *"show protection status notification"* and names no mechanism. An earlier revision of this row glossed it "desktop notifications"; that was this project's guess, and `handoff.md` §3 item 8 carries it as an open question because the answer decides whether it collides with this app's tray |

### `listen_auth`

Proxy authentication — `enabled`, `username`, `password`. Turn this on before ever binding to a non-loopback address.

### `https_filtering`

| Key | Example | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Master switch for MITM HTTPS filtering |
| `root_certificate_name` | `AdGuard CLI CA` | CN of the generated root CA |
| `certificates_cache` | `.` | Certificate cache directory |
| `exclusions` | `https_exclusions.txt` | Domains never MITM'd (banking, etc.) |
| `filter_ev_certificates` | `false` | Also filter Extended Validation certs |
| `enable_tls13` | `true` | TLS 1.3 support |
| `ocsp_check_enabled` | `true` | OCSP revocation checks |
| `enforce_certificate_transparency` | `true` | Require CT compliance |
| `http3_filtering_enabled` | `true` | Filter HTTP/3 (QUIC) |
| `filter_secure_dns_mode` | `transparent` | How DoH/DoT from clients is handled |
| `encrypted_client_hello` | `false` | ECH support |

### `dns_filtering`

`enabled`, `upstream`, `fallbacks`, `bootstraps` (all `default` unless overridden), `filters` (list, includes `dns_user.txt`), `block_ech`, and `listen_port` (`-1` = no standalone DNS listener).

### `safebrowsing` / `crlite`

`safebrowsing`: `enabled`, `send_anonymous_statistics`. Blocks known malware/phishing hosts.
`crlite`: `enabled`. Certificate revocation checking via Mozilla's CRLite.

### `stealthmode`

Privacy hardening, disabled by default. Notable keys:

- Cookies — `block_third_party_cookies` (+ `_min` lifetime cap in minutes), `block_first_party_cookies` (+ `_min`), `disable_third_party_cache`
- Identity — `hide_ip` / `custom_ip`, `hide_user_agent` / `custom_user_agent`, `remove_referrer_from_third_party_requests` / `custom_referrer`
- Signals — `send_do_not_track_signals`, `hide_search_queries`, `remove_x_client_data_header`, `block_third_party_authorization`
- Browser APIs — `block_web_rtc`, `block_browser_push_api`, `block_browser_location_api`, `block_browser_flash`, `block_browser_java`
- `anti_dpi` — nested sub-section (view it via `config show stealthmode`)

### `outbound_proxy`

Chain AdGuard's traffic through an upstream proxy: `enabled`, `mode` (`HTTP`/SOCKS), `host`, `port`, `username`, `password`, `trust_any_certificate`, `udp_through_socks5_enabled`.

### `har_writer`

`enabled`, `location` — dumps traffic as HAR files for debugging.

### `apps`

Per-application filtering, evaluated top to bottom; the first match wins. Actions are `default` (filter), `bypass_https` (skip HTTPS filtering only), and `bypass` (do not filter at all). Only applies in automatic/system-wide proxy modes.

```yaml
apps:
  - include-list: browsers.yaml     # pulls in per-browser rules
  - name: '*vpn*'
    action: 'bypass'
    skip_outbound_proxy: true
  - name: '*'                       # catch-all
    action: 'bypass_https'
```

`browsers.yaml` sets `action: default` for known browsers (firefox, chrome, chromium, brave, vivaldi, librewolf, tor-browser, waterfox, opera, qutebrowser, and others) so browsers get full filtering while everything else is left alone. Edit it with `--list-file`:

```bash
adguard-cli config list-add apps --list-file ~/.local/share/adguard-cli/browsers.yaml
```

---

## Shell completion

A bash completion script ships with the install but is not wired up automatically:

```bash
source ~/.local/opt/adguard-cli/bash-completion.sh
```

Add that line to `~/.bashrc` to make it permanent.

---

## Recipes

**Run in the foreground under systemd**

```bash
adguard-cli start --no-fork --pid-file /run/user/1000/adguard-cli.pid
```

**Turn on tracking protection and stealth mode**

```bash
adguard-cli filters add 3 17
adguard-cli config set stealthmode.enabled true
adguard-cli restart
```

**Enable DNS filtering with a stronger blocklist**

```bash
adguard-cli config set dns_filtering.enabled true
adguard-cli dns filters add 48
adguard-cli restart
```

**Expose the proxy to the LAN, with authentication**

```bash
adguard-cli config set listen_auth.enabled true
adguard-cli config set listen_auth.username myuser
adguard-cli config set listen_auth.password 'a-strong-password'
adguard-cli config set listen_address 0.0.0.0
adguard-cli restart
```

**Trust the CA in a Firefox profile**

```bash
ls ~/.mozilla/firefox/            # find the profile directory name
adguard-cli cert --firefox-profile abcd1234.MyProfile
```

**Back up and restore**

```bash
adguard-cli export-settings -o ~/backups/
adguard-cli import-settings -i ~/backups/settings.zip
```

**Collect logs for a support ticket**

```bash
adguard-cli export-logs -o ~/adguard-logs.zip
```

**Refresh everything, then check for an app update**

```bash
adguard-cli check-update
adguard-cli update --verbose
```
