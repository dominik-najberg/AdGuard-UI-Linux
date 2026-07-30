//! Process wrapper around the `adguard-cli` binary.
//!
//! This is the only place in the codebase allowed to invoke the CLI, because
//! it is the only place that knows the CLI's three sharp edges (all measured;
//! see `docs/cli-contract.md`):
//!
//! 1. ANSI bold escapes are emitted **unconditionally** — even when stdout is
//!    not a TTY, and `NO_COLOR` / `TERM=dumb` are ignored. Everything captured
//!    is stripped here so no call site can forget.
//! 2. A non-zero exit means only that the *argument parser* rejected the
//!    command line — i.e. a bug in our code. Every *semantic* failure
//!    (unknown key, wrong type, missing section) prints to **stdout** and
//!    exits **0**.
//! 3. Consequently a caller must never infer success from exit status. State
//!    changes follow act -> re-read -> reconcile.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::model::{FilterAction, FilterSet, ProxyStatus};
use crate::paths;

/// Deadline for the local commands — `status`, `config get/set`, `start`.
///
/// They cost 10–30 ms measured, so this is not a performance budget; it is the
/// point past which the command is not coming back and the UI should say so
/// rather than hold a worker thread. Generous enough that a machine under load
/// does not trip it.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Deadline for the commands that reach the network — `filters update`,
/// `check-update`, `update` (`architecture.md` §4).
///
/// Nothing wires these up yet. The constant exists so that when something does,
/// the choice is to name a deadline rather than to invent the whole mechanism.
#[allow(dead_code)]
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the deadline is checked while a child runs.
///
/// A plain sleep-poll rather than a signal or a `wait_timeout` dependency:
/// commands here finish in 10–30 ms, so this costs a couple of wake-ups on the
/// worker thread and keeps `adguard-core` free of another crate.
const POLL: Duration = Duration::from_millis(5);

/// How long the output is still collected for once the child has exited.
///
/// Reading a pipe ends when *every* write end closes, and the child's own exit
/// does not guarantee that: a descendant inherits the same descriptors and can
/// hold them open long after its parent is gone. Measured on this machine —
/// `sh -c "sleep 10 & echo done"` exits at once, and a reader waiting for EOF
/// sits there for the full ten seconds.
///
/// `adguard-cli start` leaves the proxy daemon behind, so that shape is the
/// normal case here rather than a curiosity. Without a bound, a timeout could
/// itself hang — the one thing it exists to prevent. Output that has not
/// arrived within this grace is given up on: an empty capture becomes an
/// honest [`Error::Unparseable`] one layer up, where a wedged worker thread
/// would become nothing at all.
const COLLECT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("AdGuard CLI not found — install adguard-cli, or set $ADGUARD_CLI to its path")]
    BinaryNotFound,

    #[error("could not execute {binary}: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },

    /// Exit status was non-zero, which per the contract means we built a
    /// malformed command line. This should never reach the user as a normal
    /// outcome; it is our bug.
    #[error("adguard-cli rejected `{args}` (exit {code}): {stderr}")]
    BadInvocation {
        args: String,
        code: i32,
        stderr: String,
    },

    #[error("could not interpret output of `adguard-cli {args}`: {output}")]
    Unparseable { args: String, output: String },

    /// The install has no active licence, so the command was refused before it
    /// could run.
    ///
    /// Contract §3 originally read exit 1 as "we built a malformed command
    /// line" — our bug — because that was the only way it had been seen. It is
    /// not: `status`, `license` and `filters list` all exit 1 in an unlicensed
    /// install, so a user whose licence lapsed would have been told
    /// *"adguard-cli rejected `status`"*, which blames the wrong party and
    /// suggests nothing they can act on.
    ///
    /// Carries the CLI's own sentence, for the same reason [`Self::Refused`]
    /// does: it is better wording than ours and it stays right if AdGuard
    /// changes it.
    #[error("{message}")]
    Unlicensed { message: String },

    /// The command outlived its deadline and was killed. Distinct from
    /// [`Self::BadInvocation`] because nothing was rejected and nothing is
    /// known: the command may equally have done its work and hung on the way
    /// out, so a caller must re-read state rather than assume it did nothing.
    #[error("`adguard-cli {args}` did not finish within {}s and was stopped", timeout.as_secs())]
    TimedOut {
        args: String,
        timeout: std::time::Duration,
    },

    /// A command the CLI accepted but refused to carry out. Exit code was 0
    /// and the explanation came back on stdout, so `message` is the CLI's own
    /// wording — suitable to show the user verbatim.
    #[error("{message}")]
    Refused { message: String },
}

/// Captured, ANSI-stripped result of one invocation.
#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
    #[allow(dead_code)]
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct Cli {
    binary: PathBuf,
    /// `$XDG_DATA_HOME` to run the CLI with, when it should not be the
    /// inherited one. See [`Self::with_xdg_data_home`].
    xdg_data_home: Option<PathBuf>,
}

impl Cli {
    /// Locate the CLI. Returns [`Error::BinaryNotFound`] when AdGuard CLI is
    /// not installed, so the UI can say so plainly instead of crashing.
    pub fn discover() -> Result<Self, Error> {
        paths::cli_binary()
            .map(|binary| Self {
                binary,
                xdg_data_home: None,
            })
            .ok_or(Error::BinaryNotFound)
    }

    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }

    /// Run every invocation against a different `$XDG_DATA_HOME`.
    ///
    /// Measured: the CLI resolves its data directory as
    /// `$XDG_DATA_HOME/adguard-cli`, so pointing this at a scratch directory
    /// holding a copy of `proxy.yaml` gives a complete, throwaway AdGuard
    /// configuration to write to.
    ///
    /// That exists for the tests. The write path is the riskiest code in this
    /// crate and the only way to cover it honestly is to run the real binary,
    /// but doing that against the machine's own config means a test suite that
    /// edits the user's security settings — and leaves them edited if it
    /// panics. With this, `tests/config_sandbox.rs` exercises the same
    /// `config_set` used in anger, including the cases that would expose the
    /// proxy to the network, against a file nothing is listening on.
    ///
    /// The GUI never calls it: the application must act on the real config.
    pub fn with_xdg_data_home(mut self, dir: impl Into<PathBuf>) -> Self {
        self.xdg_data_home = Some(dir.into());
        self
    }

    /// Run a subcommand and return its stripped output.
    ///
    /// # `stdin` is closed deliberately
    ///
    /// Several commands ask for input when they can get it, and one of them is
    /// reachable from the Advanced page: `config set listen_address
    /// <non-loopback>` prompts *"Enter username for accessing proxy server:"*
    /// unless `listen_auth` is fully configured (contract §5).
    ///
    /// Everything measured about that prompt was measured with no TTY, where it
    /// gives up immediately and warns. But a child process inherits its
    /// parent's stdin, and a GUI started from a terminal has a real one — so
    /// the same call that no-ops in every test would sit there **forever**
    /// waiting for a username to be typed into a terminal the user has probably
    /// stopped looking at, holding a worker thread and leaving the switch that
    /// triggered it spinning.
    ///
    /// `Stdio::null()` makes the no-TTY path the only path, so the CLI behaves
    /// the way the contract doc records however the app was launched. Nothing
    /// here ever has anything to say on stdin.
    ///
    /// # It is also bounded in time
    ///
    /// Closing stdin removes the one *known* way to hang, but not the general
    /// one: [`Self::run_within`] kills anything that outlives its deadline, so
    /// a worker thread cannot be held forever by a command that never returns.
    pub fn run(&self, args: &[&str]) -> Result<Output, Error> {
        self.run_within(args, LOCAL_TIMEOUT)
    }

    /// [`Self::run`] with an explicit deadline.
    ///
    /// Two deadlines rather than one, because the two kinds of command differ
    /// by three orders of magnitude: the local ones cost 10–30 ms, while
    /// `filters update` reaches `filters.adtidy.org` and a real
    /// `HttpClientNetworkError` is already in this machine's logs
    /// (`architecture.md` §4). One value generous enough for the second would
    /// leave the first able to wedge a page for minutes, and one tight enough
    /// for the first would fail every update on a slow link.
    ///
    /// **The child is killed, not abandoned.** Returning while it still runs
    /// would leave a process writing `proxy.yaml` behind the back of a UI that
    /// has already given up on it — and the next invocation would contend with
    /// it for the same file.
    ///
    /// A timeout says nothing about whether the work happened, so
    /// [`Error::TimedOut`] is a distinct variant rather than folded into
    /// [`Error::BadInvocation`]: the caller still owes itself a re-read.
    pub fn run_within(&self, args: &[&str], timeout: Duration) -> Result<Output, Error> {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            // Piped rather than `output()`, which offers no way back in once it
            // has started waiting. Owning the pipes means owning the deadline.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(home) = &self.xdg_data_home {
            command.env("XDG_DATA_HOME", home);
        }

        let mut child = command.spawn().map_err(|source| Error::Spawn {
            binary: self.binary.display().to_string(),
            source,
        })?;

        // Both pipes are drained on threads of their own for the whole life of
        // the child. A pipe holds ~64 KB before it blocks the writer, and
        // `filters list --all` is larger than that — so waiting on the child
        // first and reading afterwards would deadlock: we would be waiting for
        // an exit that cannot happen until someone empties the pipe.
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => break None,
                Ok(None) => std::thread::sleep(POLL),
                Err(source) => {
                    return Err(Error::Spawn {
                        binary: self.binary.display().to_string(),
                        source,
                    })
                }
            }
        };

        let Some(status) = status else {
            // Kill the child — but only the child. Never the process group:
            // `adguard-cli start` deliberately leaves the proxy daemon behind,
            // and killing the group would take down the very thing the user
            // asked to start.
            let _ = child.kill();
            let _ = child.wait();
            // Nothing is collected. The readers cannot be waited on here: a
            // descendant may still hold the pipe (see `COLLECT_GRACE`), and a
            // timeout that can itself block is not a timeout. Dropping the
            // receivers detaches them; they end when the pipes do.
            drop(stdout);
            drop(stderr);
            return Err(Error::TimedOut {
                args: args.join(" "),
                timeout,
            });
        };

        // One deadline across both pipes, not one each: they are held open by
        // the same descendants and would otherwise cost twice the grace.
        let until = Instant::now() + COLLECT_GRACE;
        let stdout = strip_ansi(&collect(stdout, until));
        let stderr = strip_ansi(&collect(stderr, until));

        if !status.success() {
            // Exit 1 is not exclusively our own malformed command line, which
            // is what contract §3 first assumed. Sort the two apart before
            // blaming ourselves.
            if let Some(message) = licence_complaint(&stderr) {
                return Err(Error::Unlicensed { message });
            }
            return Err(Error::BadInvocation {
                args: args.join(" "),
                code: status.code().unwrap_or(-1),
                stderr: stderr.trim().to_owned(),
            });
        }

        Ok(Output { stdout, stderr })
    }

    pub fn version(&self) -> Result<String, Error> {
        Ok(self.run(&["--version"])?.stdout.trim().to_owned())
    }

    pub fn status(&self) -> Result<ProxyStatus, Error> {
        let out = self.run(&["status"])?;
        parse_status(&out.stdout).ok_or_else(|| Error::Unparseable {
            args: "status".to_owned(),
            output: out.stdout.clone(),
        })
    }

    /// Start the proxy. The caller must re-read [`Self::status`] afterwards
    /// rather than trusting this to have worked (contract rule 3).
    pub fn start(&self) -> Result<String, Error> {
        Ok(self.run(&["start"])?.stdout.trim().to_owned())
    }

    /// Stop the proxy. Re-read status afterwards.
    pub fn stop(&self) -> Result<String, Error> {
        Ok(self.run(&["stop"])?.stdout.trim().to_owned())
    }

    /// Restart the proxy. Re-read status afterwards.
    pub fn restart(&self) -> Result<String, Error> {
        Ok(self.run(&["restart"])?.stdout.trim().to_owned())
    }

    /// Add, enable or disable one filter.
    ///
    /// Exit status proves nothing here (contract §3): every semantic failure
    /// prints to stdout and exits 0 —
    ///
    /// ```text
    /// $ adguard-cli filters enable 3        # never added
    /// Before filters can be enabled, they must be added
    /// $ adguard-cli filters add 99999       # no such filter
    /// All specified filters have already been added or do not exist
    /// ```
    ///
    /// So success is defined positively, as the confirmation line the CLI
    /// prints when it did the work (`Filter [Title: ...] enabled`), and every
    /// other output shape becomes [`Error::Refused`] carrying the CLI's own
    /// first line. Treating an unrecognised shape as failure is deliberate:
    /// the alternative is reporting success for a no-op.
    ///
    /// This is an early, explanatory check only — the caller must still
    /// re-read the database to learn the resulting state.
    ///
    /// Negative IDs need no `--` guard: the user-rules sentinel
    /// (`-2147483648`) is measured to parse as a positional, not a flag.
    pub fn filter_action(
        &self,
        set: FilterSet,
        action: FilterAction,
        filter_id: i64,
    ) -> Result<(), Error> {
        let filter_id = filter_id.to_string();
        let mut args = set.cli_prefix().to_vec();
        args.push(action.subcommand());
        args.push(&filter_id);

        let out = self.run(&args)?;
        if confirms(&out.stdout, action.confirmation()) {
            Ok(())
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout).unwrap_or_else(|| {
                    format!("`adguard-cli {}` said nothing at all", args.join(" "))
                }),
            })
        }
    }

    /// Write one setting into `proxy.yaml`.
    ///
    /// The only sanctioned way to change the file: `config set` was measured to
    /// replace the single line and leave every surrounding comment intact,
    /// which no YAML serialiser would (contract §5).
    ///
    /// Like every other subcommand it exits **0** on failure and explains
    /// itself on stdout, so success is again defined positively — by the
    /// `Config has been updated` line. Measured refusals, all at exit 0 and all
    /// leaving the file untouched:
    ///
    /// ```text
    /// $ adguard-cli config set bogus_key true
    /// 'bogus_key' not found
    /// $ adguard-cli config set stealthmode.enabled bogus
    /// Invalid value type: The value of the setting must be an boolean
    /// $ adguard-cli config set https_filtering.filter_secure_dns_mode nope
    /// Invalid value for key `...`. Valid values are: off, transparent, redirect
    /// $ adguard-cli config set filters something
    /// This field is not a separate setting
    /// ```
    ///
    /// # This is not proof the value changed
    ///
    /// `Config has been updated` is necessary but **not sufficient**. It is
    /// printed for a no-op, and — measured — even when the CLI declined to make
    /// the requested change at all:
    ///
    /// ```text
    /// $ adguard-cli config set listen_address 0.0.0.0     # listen_auth off
    /// Enter username for accessing proxy server:
    /// Warning: No TTY for user input. ...
    /// listen_address = 127.0.0.1        <- the old value; the file is untouched
    /// Config has been updated
    /// ```
    ///
    /// So `Ok` means only *"the CLI accepted the command"*. The caller must
    /// still re-read `proxy.yaml` and render from that.
    /// # The `--` guard is not optional
    ///
    /// Both arguments are positionals, and CLI11 will still try to read a
    /// leading `-` as an option. Measured:
    ///
    /// ```text
    /// $ adguard-cli config set listen_auth.password --flag-shaped
    /// <value> is required                       # exit 1, nothing written
    /// $ adguard-cli config set listen_auth.password -abc
    /// <value> is required                       # exit 1, nothing written
    /// $ adguard-cli config set -- listen_auth.password --flag-shaped
    /// listen_auth.password = --flag-shaped      # exit 0, written
    /// ```
    ///
    /// `-1` happens to survive without the guard, because a negative number
    /// parses as a positional — which is what made the manual proxy ports look
    /// safe. A password or hostname beginning with `-` does not. Since the
    /// guard changes nothing for ordinary values (verified for `-1`, plain
    /// strings and every enum), it goes on unconditionally rather than being
    /// applied by a rule someone has to remember.
    ///
    /// It also improves the failure mode for a bad *key*: `'--bogus' not found`
    /// at exit 0, the ordinary semantic refusal, instead of a parse error our
    /// own error type describes as a bug in this crate.
    pub fn config_set(&self, key: &str, value: &str) -> Result<Applied, Error> {
        let out = self.run(&["config", "set", "--", key, value])?;

        if out.stdout.lines().map(str::trim).any(|line| line == CONFIG_UPDATED) {
            Ok(Applied {
                restart_required: mentions_restart(&out.stdout),
            })
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout)
                    .unwrap_or_else(|| format!("`config set {key}` said nothing at all")),
            })
        }
    }

    /// Set a boolean setting.
    ///
    /// Always writes lowercase `true`/`false`. The CLI also accepts `1`/`0`,
    /// but that writes a literal integer into the YAML where a bool belongs —
    /// legal to the CLI, a type-pun to every other reader. `True`, `TRUE`,
    /// `yes` and `on` are all rejected outright.
    pub fn set_bool(&self, key: &str, value: bool) -> Result<Applied, Error> {
        self.config_set(key, if value { "true" } else { "false" })
    }

    /// Set an integer setting.
    ///
    /// Negative values need no `--` guard: measured, `config set
    /// listen_ports.http_proxy -1` parses `-1` as a positional argument, not as
    /// a flag, and `--` is accepted but changes nothing. That matters because
    /// `-1` is how both manual proxy ports are switched off.
    ///
    /// The CLI checks only that the value *is* an integer. It will accept `0`,
    /// `65536`, `99999` and `-2` for a port, and `3.5` — which lands in the YAML
    /// as a float. Range-checking belongs to the caller; see
    /// [`crate::model::Setting::permits_number`].
    pub fn set_int(&self, key: &str, value: i64) -> Result<Applied, Error> {
        self.config_set(key, &value.to_string())
    }

    /// Set a string setting, keeping the value out of any error we return.
    ///
    /// `config set` echoes what it was given — `listen_auth.password = hunter2`
    /// — and [`Self::config_set`] quotes the CLI's first output line in
    /// [`Error::Refused`], which the UI shows verbatim. For a credential that
    /// would put the secret in a toast, so the value is scrubbed from the
    /// message on the way out.
    ///
    /// # The value is visible in `argv`
    ///
    /// `config set` takes the value as a command-line argument, so for the
    /// ~20 ms the process lives it is readable in `/proc/<pid>/cmdline` by
    /// anything running as this user. There is no way around it: the CLI's only
    /// other route for a credential is the interactive prompt, which needs a
    /// TTY (contract §7). Worth knowing; not worth refusing over on a
    /// single-user desktop, where anything able to read that could read
    /// `proxy.yaml` itself.
    pub fn set_secret(&self, key: &str, value: &str) -> Result<Applied, Error> {
        self.config_set(key, value)
            .map_err(|err| redact_error(err, value))
    }
}

/// Replace a secret with a placeholder wherever it appears.
///
/// An empty secret is left alone — every string contains it, and there is
/// nothing to hide.
fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_owned();
    }
    message.replace(secret, "<hidden>")
}

/// Scrub a secret out of every error field that could be carrying it.
///
/// Matched per variant rather than over `to_string()`, so the error keeps its
/// type. Three of these four variants quote the command line we built, which is
/// where a credential would appear: `BadInvocation` should now be unreachable
/// for a user-supplied value — that is what the `--` guard in
/// [`Cli::config_set`] is for — but a leak that depends on another function
/// staying correct is not one worth leaving in place.
///
/// The match is exhaustive on purpose. A new variant that quotes `args` will
/// not compile until it has been considered here, which is how `TimedOut`
/// arrived: `config set listen_auth.password <secret>` is exactly the kind of
/// call that could hit a deadline.
fn redact_error(err: Error, secret: &str) -> Error {
    match err {
        Error::Refused { message } => Error::Refused {
            message: redact(&message, secret),
        },
        Error::BadInvocation { args, code, stderr } => Error::BadInvocation {
            args: redact(&args, secret),
            code,
            stderr: redact(&stderr, secret),
        },
        Error::Unparseable { args, output } => Error::Unparseable {
            args: redact(&args, secret),
            output: redact(&output, secret),
        },
        Error::TimedOut { args, timeout } => Error::TimedOut {
            args: redact(&args, secret),
            timeout,
        },
        // Quotes no argument of ours today, but it carries a string the CLI
        // wrote and this match is the place that assumption gets checked.
        Error::Unlicensed { message } => Error::Unlicensed {
            message: redact(&message, secret),
        },
        // Carry no echo of the value.
        other @ (Error::BinaryNotFound | Error::Spawn { .. }) => other,
    }
}

/// What an accepted `config set` implies for the running proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// The CLI said the change will not reach the running proxy until it is
    /// restarted. It only says this when the proxy is up and it could not
    /// apply the setting live, so it is worth passing straight to the user.
    pub restart_required: bool,
}

/// The line `config set` prints when it accepted the command.
const CONFIG_UPDATED: &str = "Config has been updated";

/// Did the CLI ask for a restart?
///
/// Two shapes exist in the binary — *"To apply changes, you need to restart the
/// proxy server by running `… restart`"* and *"Failed to apply settings to
/// running proxy server"*. Both mean the same thing to the user, and both are
/// matched loosely because the first interpolates the binary's own path.
fn mentions_restart(stdout: &str) -> bool {
    let lowered = stdout.to_ascii_lowercase();
    lowered.contains("restart the proxy server")
        || lowered.contains("failed to apply settings to running proxy server")
}

/// Did the CLI confirm it acted, as in `Filter [Title: EasyList] enabled`?
///
/// `add` prints two lines — added, then enabled — so a match anywhere in the
/// output counts. The `Filter [` prefix keeps this from matching the advice
/// line of a failure ("To add a filter, run `adguard-cli filters add`").
fn confirms(stdout: &str, verb: &str) -> bool {
    stdout
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with("Filter [") && line.contains(']') && line.ends_with(verb))
}

fn first_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Strip ANSI escapes from raw process output and lossily decode as UTF-8.
fn strip_ansi(raw: &[u8]) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(raw)).into_owned()
}

/// Recognise the CLI complaining that the install is not licensed.
///
/// Measured on v1.4.13 against a sandbox `$XDG_DATA_HOME`, which is unlicensed
/// by construction — `status`, `license` and `filters list` each exit 1 with
/// stdout empty and exactly this on stderr:
///
/// ```text
/// You need to activate an AdGuard license to use this command
/// ```
///
/// Matched on two loose tokens rather than that sentence. An exact comparison
/// would break on any rewording — including the British spelling this codebase
/// uses in its own prose — and the failure mode of missing it is the bug being
/// fixed here, back again. The tokens are specific enough that a genuinely
/// malformed command line will not trip them: the CLI's usage errors name the
/// option or the value, not activation.
///
/// # Only two of its twenty lines are worth showing
///
/// That sentence is not all the CLI prints. It follows it with a full usage
/// dump — every subcommand, one per line — and then the one line the user can
/// act on:
///
/// ```text
/// You need to activate an AdGuard license to use this command
///   <18 lines of usage>
/// You can activate your AdGuard license by running `…/adguard-cli activate`
/// ```
///
/// This error ends up in a row subtitle, so the dump is dropped and the
/// complaint is joined to the advice. Keeping the whole thing would technically
/// be "the CLI's own wording" and would render as an unreadable blob.
fn licence_complaint(stderr: &str) -> Option<String> {
    let haystack = stderr.to_ascii_lowercase();
    if !(haystack.contains("licen") && haystack.contains("activat")) {
        return None;
    }

    let mut lines = stderr.lines().map(str::trim).filter(|line| !line.is_empty());
    let complaint = lines.next()?;

    // The advice names the command to run. Recognised by its own two tokens
    // rather than its exact phrasing, for the same reason as above.
    let advice = stderr.lines().map(str::trim).find(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("you can") && line.contains("activat")
    });

    Some(match advice {
        // Joined with a dash rather than a full stop: the CLI's sentence
        // carries no terminal punctuation and inventing some would be
        // editing its words rather than arranging them.
        Some(advice) if advice != complaint => format!("{complaint} — {advice}"),
        _ => complaint.to_owned(),
    })
}

/// Read one of the child's pipes to exhaustion on a thread of its own.
///
/// See [`Cli::run_within`]: the child cannot exit while a full pipe is blocking
/// its writes, so the reading has to overlap the waiting rather than follow it.
///
/// A channel rather than a `JoinHandle`, because a handle can only be joined
/// unconditionally and this wait has to be bounded — see [`COLLECT_GRACE`].
/// Dropping the receiver detaches the thread, which is what the timeout path
/// wants.
fn drain(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // A read error means a truncated capture, not a failed command. The
        // exit status is what decides success, and partial output parses or it
        // does not — both are already handled.
        let _ = pipe.read_to_end(&mut buffer);
        // Failure means the caller gave up on us and went home.
        let _ = tx.send(buffer);
    });
    rx
}

/// Collect what [`drain`] read, waiting no later than `until`.
///
/// Empty output on expiry rather than a block: an unparseable result is a
/// visible, recoverable failure, and a worker thread that never returns is not.
fn collect(pipe: Option<std::sync::mpsc::Receiver<Vec<u8>>>, until: Instant) -> Vec<u8> {
    let remaining = until.saturating_duration_since(Instant::now());
    pipe.and_then(|rx| rx.recv_timeout(remaining).ok())
        .unwrap_or_default()
}

/// Parse `adguard-cli status`.
///
/// Returns `None` if the running/stopped line is absent, which means the
/// output shape changed and we should report failure rather than guess.
fn parse_status(stdout: &str) -> Option<ProxyStatus> {
    let mut status = ProxyStatus::default();
    let mut saw_state = false;

    for line in stdout.lines().map(str::trim) {
        if line.contains("proxy server is") {
            // Order matters: "is not running" also contains "running".
            status.running = !line.contains("is not running");
            saw_state = true;
        } else if let Some(endpoint) = line.strip_prefix("HTTP proxy is listening on ") {
            status.http_proxy = Some(endpoint.trim().to_owned());
        } else if let Some(endpoint) = line.strip_prefix("SOCKS5 proxy is listening on ") {
            status.socks5_proxy = Some(endpoint.trim().to_owned());
        } else if line.starts_with("Manual DNS proxy is") {
            status.manual_dns_proxy = is_enabled(line);
        } else if line.starts_with("System-wide automatic filtering is") {
            status.system_wide_filtering = is_enabled(line);
        } else if line.starts_with("System-wide DNS filtering is") {
            status.system_dns_filtering = is_enabled(line);
        }
    }

    saw_state.then_some(status)
}

fn is_enabled(line: &str) -> bool {
    line.ends_with("enabled") && !line.ends_with("disabled")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real captured output, including the ANSI bold the CLI emits even when
    /// piped. Keep the escapes: the stripper is part of what is under test.
    const RUNNING: &str = "The AdGuard proxy server is running\n\
         HTTP proxy is listening on \x1b[1m127.0.0.1:3129\x1b[0m\n\
         SOCKS5 proxy is listening on \x1b[1m127.0.0.1:1081\x1b[0m\n\
         Manual DNS proxy is \x1b[1mdisabled\x1b[0m\n\
         System-wide automatic filtering is \x1b[1mdisabled\x1b[0m\n\
         System-wide DNS filtering is \x1b[1mdisabled\x1b[0m\n\
         You can stop the proxy server by running `adguard-cli stop`\n";

    const STOPPED: &str = "The AdGuard proxy server is not running\n\
         You can start the proxy server by running `adguard-cli start`\n";

    fn stripped(s: &str) -> String {
        strip_ansi(s.as_bytes())
    }

    #[test]
    fn strips_ansi_escapes() {
        let out = stripped(RUNNING);
        assert!(!out.contains('\x1b'), "escapes survived: {out:?}");
        assert!(out.contains("HTTP proxy is listening on 127.0.0.1:3129"));
    }

    #[test]
    fn parses_running_status() {
        let status = parse_status(&stripped(RUNNING)).expect("should parse");
        assert!(status.running);
        assert_eq!(status.http_proxy.as_deref(), Some("127.0.0.1:3129"));
        assert_eq!(status.socks5_proxy.as_deref(), Some("127.0.0.1:1081"));
        assert!(!status.manual_dns_proxy);
        assert!(!status.system_wide_filtering);
        assert!(!status.system_dns_filtering);
    }

    #[test]
    fn parses_stopped_status() {
        let status = parse_status(&stripped(STOPPED)).expect("should parse");
        assert!(!status.running);
        assert_eq!(status.http_proxy, None);
        assert_eq!(status.socks5_proxy, None);
    }

    /// "is not running" contains "running" — the substring order trap.
    #[test]
    fn not_running_is_not_mistaken_for_running() {
        assert!(!parse_status("The AdGuard proxy server is not running")
            .unwrap()
            .running);
    }

    /// An unrecognised shape must fail loudly, not silently report "stopped".
    #[test]
    fn unrecognised_output_is_rejected() {
        assert!(parse_status("something else entirely").is_none());
        assert!(parse_status("").is_none());
    }

    #[test]
    fn distinguishes_enabled_from_disabled() {
        assert!(is_enabled("Manual DNS proxy is enabled"));
        assert!(!is_enabled("Manual DNS proxy is disabled"));
    }

    /// Captured from v1.4.13. `add` confirms twice; the failures are the ones
    /// that arrive with exit code 0 and must not read as success.
    const ADDED: &str = "Filter [Title: AdGuard Tracking Protection filter] added\n\
         Filter [Title: AdGuard Tracking Protection filter] enabled\n";
    const ENABLED: &str = "Filter [Title: AdGuard Base filter] enabled\n";
    const DISABLED: &str = "Filter [Title: AdGuard Base filter] disabled\n";
    const NOT_ADDED: &str = "Before filters can be enabled, they must be added\n\
         To add a filter, run `adguard-cli filters add`\n";
    const NO_SUCH_FILTER: &str = "All specified filters have already been added or do not exist\n";

    #[test]
    fn recognises_each_confirmation() {
        assert!(confirms(ADDED, FilterAction::Add.confirmation()));
        assert!(confirms(ENABLED, FilterAction::Enable.confirmation()));
        assert!(confirms(DISABLED, FilterAction::Disable.confirmation()));
    }

    /// `add` enables as a side effect, which is what lets one switch flip
    /// handle a filter that was never subscribed to.
    #[test]
    fn add_also_confirms_enabled() {
        assert!(confirms(ADDED, FilterAction::Enable.confirmation()));
    }

    /// "disabled" ends in "abled" — close enough to trip a sloppy match.
    #[test]
    fn disabled_is_not_read_as_enabled() {
        assert!(!confirms(DISABLED, FilterAction::Enable.confirmation()));
        assert!(!confirms(ENABLED, FilterAction::Disable.confirmation()));
    }

    /// Both of these exit 0. Reporting them as success would leave a switch
    /// on for a filter that is still off.
    #[test]
    fn semantic_failures_are_not_confirmations() {
        for output in [NOT_ADDED, NO_SUCH_FILTER, "", "\n \n"] {
            for action in [FilterAction::Add, FilterAction::Enable, FilterAction::Disable] {
                assert!(
                    !confirms(output, action.confirmation()),
                    "{output:?} read as {action:?} success"
                );
            }
        }
    }

    #[test]
    fn refusal_message_is_the_cli_first_line() {
        assert_eq!(
            first_line(NOT_ADDED).as_deref(),
            Some("Before filters can be enabled, they must be added")
        );
        assert_eq!(first_line("\n\n"), None);
    }

    /// The confirmation arrives bold, like everything else the CLI prints.
    #[test]
    fn confirmation_survives_ansi_stripping() {
        let bold = "Filter [Title: \x1b[1mAdGuard Base filter\x1b[0m] enabled\n";
        assert!(confirms(&stripped(bold), FilterAction::Enable.confirmation()));
    }

    // ---- `config set`, all captured from v1.4.13 ----

    /// The plain success: an echo of the resulting value, then the confirmation.
    const SET_OK: &str = "stealthmode.enabled = true\nConfig has been updated\n";

    /// With `show_hints: true` the hint lands **between** the echo and the
    /// confirmation, which is why nothing may be matched positionally.
    const SET_OK_WITH_HINT: &str = "https_filtering.enabled = true\n\
         To use HTTPS filtering on your device, you need to install a certificate on it. \
         You can find a guide on how to install it here: `https://link.adtidy.org/forward.html\
         ?action=how_to_install_cert&from=certificate&app=corelibs`\n\
         Config has been updated\n";

    /// Setting a coupled key echoes more than one line before confirming.
    const SET_OK_MULTILINE: &str =
        "listen_address = 0.0.0.0\nlisten_auth = true\n  username = admin\nConfig has been updated\n";

    const UNKNOWN_KEY: &str = "'bogus_key_xyz' not found\n";
    const WRONG_TYPE: &str = "Invalid value type: The value of the setting must be an boolean\n";
    const BAD_ENUM: &str = "Invalid value for key `https_filtering.filter_secure_dns_mode`. \
         Valid values are: off, transparent, redirect\n";
    const NOT_A_SETTING: &str = "This field is not a separate setting\n\
         Please run `adguard-cli config list-add <key> <value>` to add a new value\n";

    fn accepted(stdout: &str) -> bool {
        stdout.lines().map(str::trim).any(|line| line == CONFIG_UPDATED)
    }

    #[test]
    fn recognises_an_accepted_set() {
        for output in [SET_OK, SET_OK_WITH_HINT, SET_OK_MULTILINE] {
            assert!(accepted(output), "{output:?} should read as accepted");
        }
    }

    /// Every one of these exits 0 and leaves the file untouched. Reading any of
    /// them as success would leave a switch showing a state that never landed.
    #[test]
    fn set_failures_are_not_confirmations() {
        for output in [UNKNOWN_KEY, WRONG_TYPE, BAD_ENUM, NOT_A_SETTING, "", "\n \n"] {
            assert!(!accepted(output), "{output:?} read as success");
        }
    }

    /// The refusal shown to the user is the CLI's own first line — it is
    /// better wording than anything we would invent, and names the valid
    /// values in the enum case.
    #[test]
    fn set_refusal_carries_the_cli_wording() {
        assert_eq!(
            first_line(BAD_ENUM).as_deref(),
            Some(
                "Invalid value for key `https_filtering.filter_secure_dns_mode`. \
                 Valid values are: off, transparent, redirect"
            )
        );
        assert_eq!(first_line(UNKNOWN_KEY).as_deref(), Some("'bogus_key_xyz' not found"));
    }

    /// A stray "Config has been updated" inside a longer sentence is not the
    /// confirmation line; the match is on the whole trimmed line.
    #[test]
    fn confirmation_must_be_the_whole_line() {
        assert!(!accepted("Config has been updated for some other thing\n"));
        assert!(accepted("  Config has been updated  \n"));
    }

    /// Printed only when the proxy is running and the setting could not be
    /// applied live — the user needs to know their change is not in effect yet.
    #[test]
    fn detects_the_restart_advice() {
        assert!(mentions_restart(
            "ad_blocking_enabled = false\n\
             To apply changes, you need to restart the proxy server by running \
             `/home/you/.local/bin/adguard-cli restart`\n\
             Config has been updated\n"
        ));
        assert!(mentions_restart("Failed to apply settings to running proxy server\n"));
    }

    /// The common case is a stopped proxy, or one that took the change live.
    #[test]
    fn no_restart_advice_when_the_cli_did_not_ask() {
        for output in [SET_OK, SET_OK_WITH_HINT, SET_OK_MULTILINE] {
            assert!(!mentions_restart(output), "{output:?} should need no restart");
        }
    }

    /// The CLI echoes the value it was given, so a refusal carrying its first
    /// line can carry a password into a toast. Measured shape:
    /// `listen_auth.password = hunter2`.
    #[test]
    fn a_secret_is_scrubbed_from_a_refusal() {
        let message = redact("listen_auth.password = hunter2", "hunter2");
        assert_eq!(message, "listen_auth.password = <hidden>");
        assert!(!message.contains("hunter2"));
    }

    /// Redacting the empty string would replace nothing and match everywhere.
    #[test]
    fn redacting_an_empty_secret_is_a_no_op() {
        assert_eq!(redact("listen_auth.password = ", ""), "listen_auth.password = ");
    }

    /// A password may occur more than once, and may be a substring of the key
    /// or of surrounding words — every occurrence has to go.
    #[test]
    fn redaction_covers_every_occurrence() {
        let message = redact("password rejected: password", "password");
        assert!(!message.contains("password"), "{message:?}");
    }

    /// `BadInvocation` quotes the command line, which is where a credential
    /// would show up if the `--` guard ever stopped working. Every variant that
    /// echoes our arguments has to be scrubbed, not just the refusal.
    #[test]
    fn every_error_variant_that_quotes_arguments_is_scrubbed() {
        let leaked = |err: &Error| err.to_string().contains("hunter2");

        let bad = redact_error(
            Error::BadInvocation {
                args: "config set listen_auth.password hunter2".to_owned(),
                code: 1,
                stderr: "<value> is required".to_owned(),
            },
            "hunter2",
        );
        assert!(!leaked(&bad), "{bad}");

        let unparseable = redact_error(
            Error::Unparseable {
                args: "config set listen_auth.password hunter2".to_owned(),
                output: "hunter2".to_owned(),
            },
            "hunter2",
        );
        assert!(!leaked(&unparseable), "{unparseable}");

        let refused = redact_error(
            Error::Refused {
                message: "listen_auth.password = hunter2".to_owned(),
            },
            "hunter2",
        );
        assert!(!leaked(&refused), "{refused}");

        let timed_out = redact_error(
            Error::TimedOut {
                args: "config set listen_auth.password hunter2".to_owned(),
                timeout: Duration::from_secs(15),
            },
            "hunter2",
        );
        assert!(!leaked(&timed_out), "{timed_out}");
    }

    /// A `Cli` pointed at something other than `adguard-cli`.
    ///
    /// The fields are private and `discover` only ever finds the real binary,
    /// but this module is a child of the one that defines them — which is the
    /// whole reason the timeout can be tested against `sleep` and `cat` instead
    /// of against AdGuard.
    fn cli_for(binary: &str) -> Cli {
        Cli {
            binary: PathBuf::from(binary),
            xdg_data_home: None,
        }
    }

    /// The point of the whole exercise: a command that would never return is
    /// stopped, and says so, rather than holding the worker thread that ran it.
    #[test]
    fn a_command_that_hangs_is_killed_and_reported() {
        let started = Instant::now();
        let err = cli_for("/bin/sleep")
            .run_within(&["60"], Duration::from_millis(300))
            .expect_err("sleep 60 should not finish inside 300ms");

        assert!(
            matches!(err, Error::TimedOut { .. }),
            "expected a timeout, got {err:?}"
        );
        // Generously bounded: the assertion is that it returned near the
        // deadline rather than near the sleep, not that the timer is precise.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned after {:?}, so it waited for the command",
            started.elapsed()
        );
    }

    /// The deadline must not cost anything in the ordinary case.
    #[test]
    fn a_command_that_finishes_is_untouched() {
        let out = cli_for("/bin/echo")
            .run_within(&["hello"], Duration::from_secs(10))
            .expect("echo should succeed");
        assert_eq!(out.stdout.trim(), "hello");
    }

    /// A non-zero exit is still a `BadInvocation`, not a timeout — the switch
    /// from `output()` to spawn-and-poll must not have changed what failure
    /// means.
    #[test]
    fn a_failing_command_is_still_a_bad_invocation() {
        let err = cli_for("/bin/false")
            .run_within(&[], Duration::from_secs(10))
            .expect_err("false should fail");
        assert!(
            matches!(err, Error::BadInvocation { code: 1, .. }),
            "expected exit 1, got {err:?}"
        );
    }

    /// The exact stderr an unlicensed v1.4.13 produces, captured from a sandbox
    /// `$XDG_DATA_HOME`. Kept verbatim so a CLI upgrade that reworded it shows
    /// up here as a failing test rather than as a user being blamed for our bug.
    const UNLICENSED: &str = "You need to activate an AdGuard license to use this command";

    #[test]
    fn a_lapsed_licence_is_not_reported_as_our_bug() {
        let err = cli_for("/bin/sh")
            .run_within(
                &["-c", &format!("echo '{UNLICENSED}' >&2; exit 1")],
                Duration::from_secs(10),
            )
            .expect_err("exit 1 is still a failure");

        assert!(
            matches!(err, Error::Unlicensed { .. }),
            "expected a licence error, got {err:?}"
        );
        // The user sees the CLI's own sentence, not "adguard-cli rejected `status`".
        assert_eq!(err.to_string(), UNLICENSED);
    }

    /// The other half of the same decision: a real malformed command line must
    /// still be `BadInvocation`, or this fix would hide our own bugs instead.
    #[test]
    fn an_ordinary_exit_one_is_still_our_bug() {
        let err = cli_for("/bin/sh")
            .run_within(
                &["-c", "echo 'unknown setting: nonsense.key' >&2; exit 1"],
                Duration::from_secs(10),
            )
            .expect_err("exit 1 is a failure");

        assert!(
            matches!(err, Error::BadInvocation { code: 1, .. }),
            "expected BadInvocation, got {err:?}"
        );
    }

    /// What the real v1.4.13 actually writes to stderr, abridged in the middle.
    ///
    /// The first probe of this only looked at line one, which is how the usage
    /// dump nearly ended up rendered inside an `AdwActionRow` subtitle.
    const UNLICENSED_FULL: &str = "You need to activate an AdGuard license to use this command\n\
        /home/you/.local/bin/adguard-cli\n\
        \x20 CLI for controlling AdGuard\n\
        \x20 Options:\n\
        \x20   -v,--version                Display program version information and exit\n\
        \x20 Commands:\n\
        \x20   activate                    Activate an AdGuard license\n\
        \x20   stop                        Stop the AdGuard proxy server\n\
        \n\
        You can activate your AdGuard license by running `/home/you/.local/bin/adguard-cli activate`";

    /// Keep the complaint and the one actionable line; drop the usage dump.
    #[test]
    fn the_usage_dump_is_not_part_of_the_message() {
        let message = licence_complaint(UNLICENSED_FULL).expect("should be a licence problem");

        assert!(message.starts_with(UNLICENSED), "{message}");
        assert!(message.contains(" — "), "the two sentences must be separated: {message}");
        assert!(message.contains("adguard-cli activate"), "{message}");
        assert!(
            !message.contains("Display program version"),
            "the usage dump leaked into a row subtitle: {message}"
        );
        assert_eq!(message.lines().count(), 1, "must fit one subtitle: {message}");
    }

    /// The match is on two loose tokens, so check it is neither too narrow to
    /// survive a rewording nor loose enough to swallow an unrelated failure.
    #[test]
    fn the_licence_match_tolerates_rewording_without_swallowing_everything() {
        for reworded in [
            UNLICENSED,
            "You need to activate an AdGuard licence to use this command",
            "ERROR: license not activated",
            "Please activate your AdGuard License first.",
        ] {
            assert!(
                licence_complaint(reworded).is_some(),
                "should read as a licence problem: {reworded:?}"
            );
        }

        for unrelated in [
            "unknown setting: nonsense.key",
            "error: unexpected argument '--nope' found",
            "Value is required for the 'listen_address' setting",
            "",
        ] {
            assert!(
                licence_complaint(unrelated).is_none(),
                "should NOT read as a licence problem: {unrelated:?}"
            );
        }
    }

    /// A descendant holding the pipe open must not wedge the caller.
    ///
    /// `sh` exits immediately here; the backgrounded `sleep` inherits its
    /// stdout and holds the write end for ten seconds. This is not a contrived
    /// shape — `adguard-cli start` leaves the proxy daemon behind the same way,
    /// which makes it the one invocation where waiting for EOF after the child
    /// exits would hang a worker thread indefinitely.
    ///
    /// The cost of the bound is the output: `done` is lost. That is the trade,
    /// and it is the right way round — an empty capture surfaces as an
    /// `Unparseable` error, while a blocked thread surfaces as nothing at all.
    #[test]
    fn a_descendant_holding_the_pipe_cannot_wedge_the_caller() {
        let started = Instant::now();
        let result = cli_for("/bin/sh").run_within(
            &["-c", "sleep 10 & echo done"],
            Duration::from_secs(30),
        );

        assert!(result.is_ok(), "the child itself exited cleanly: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "waited {:?} — that is the grandchild's sleep, not the grace",
            started.elapsed()
        );
    }

    /// Output larger than a pipe buffer (~64 KB) must not deadlock.
    ///
    /// This is the failure the reader threads exist to prevent: `filters list
    /// --all` is bigger than one bufferful, so waiting for the child before
    /// reading would hang forever on a full pipe. If that regresses, this test
    /// hits its deadline and fails as a timeout rather than hanging the suite.
    ///
    /// The volume is produced by the child, not passed to it — a 300 KB
    /// argument exceeds `MAX_ARG_STRLEN` and fails to spawn at all, which
    /// proves nothing about pipes.
    #[test]
    fn output_larger_than_a_pipe_buffer_is_captured_whole() {
        let out = cli_for("/usr/bin/seq")
            .run_within(&["100000"], Duration::from_secs(20))
            .expect("seq should succeed, not deadlock");
        assert_eq!(out.stdout.lines().count(), 100_000);
        assert!(
            out.stdout.len() > 400_000,
            "expected well over one pipe buffer, got {} bytes",
            out.stdout.len()
        );
    }
}
