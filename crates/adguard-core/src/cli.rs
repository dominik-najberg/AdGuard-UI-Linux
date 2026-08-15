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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::model::{
    ComponentUpdate, Consent, Filter, FilterAction, FilterSet, License, ProxyStatus, UpdatePart,
    UpdateReport, Verdict,
};
use crate::paths;

/// Deadline for the local commands — `status`, `config get/set`.
///
/// They cost 10–30 ms measured, so this is not a performance budget; it is the
/// point past which the command is not coming back and the UI should say so
/// rather than hold a worker thread. Generous enough that a machine under load
/// does not trip it.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Deadline for `start` and `restart`, which are local but not quick when they
/// fail.
///
/// This list used to include `start` under [`LOCAL_TIMEOUT`], on the strength of
/// a *successful* start — measured at **1.1 s**, well inside 15 s. A failing one
/// is three orders of magnitude slower. Against an install holding a wedged
/// leftover process (see [`crate::orphan`]) the CLI waits on its own internal
/// deadline before giving up:
///
/// ```text
/// 10:37:21.870  AdGuardCli start_command: ...
/// 10:38:21.871  CSM response_from_listener: Client wait data from listener timeout
/// 10:38:21.881  SERVICE_FACADE start_internal: Failed to stop process manager
/// ```
///
/// **60.0 s**, then `Failed to start proxy server: An unknown error has
/// occurred` on stdout at exit 0. At 15 s the wrapper was killing that command
/// three quarters of the way through and reporting *"did not finish within 15s
/// and was stopped"* — a message about us, thrown over the top of the CLI's own
/// explanation, which was seconds from arriving.
///
/// So the deadline sits above AdGuard's, and stays a backstop rather than the
/// thing that normally fires. The cost is real and belongs to whoever hits it: a
/// start that is going to fail now holds the button for a minute. That is the
/// CLI's minute, and being told what happened at the end of it beats being told
/// nothing at 15 s.
const START_TIMEOUT: Duration = Duration::from_secs(90);

/// Both exports and the import. Generous because `export-settings` writes
/// 14.9 MB of which 51.1 MB raw is the filter catalogue (contract §13), and
/// because none of the three is a local read the way `config get` is.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(300);

/// Deadline for the commands that reach the network — `filters update`,
/// `check-update`, `update` (`architecture.md` §4), and [`Cli::activate`].
///
/// `activate` is the one wired up so far, and it is a mixed case worth stating.
/// Measured, its *first* leg is entirely local: 0.14 s in a fresh data
/// directory, which it seeds, and 0.01–0.02 s every time after — the log-in URL
/// it prints is derived on this machine, not fetched. The second leg is the
/// network one, because completing an activation means asking AdGuard whether
/// the log-in happened. One command, two very different costs, so it takes the
/// generous deadline.
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

    /// A command the CLI would not carry out, in its own words.
    ///
    /// The explanation came back on **stdout**, which is where this CLI puts
    /// them. Usually at exit 0 — every semantic failure does that — but not
    /// always: an initialisation race exits 1 and still prints on stdout, so
    /// the stream is what identifies this, not the status. Either way `message`
    /// is the CLI's own wording, suitable to show the user verbatim.
    #[error("{message}")]
    Refused { message: String },

    /// [`Cli::configure`] was called against a data directory that already has
    /// a `proxy.yaml`. **We** refused, before spawning anything.
    ///
    /// A variant of its own rather than a `bool` return, because this is the
    /// one error in this module that describes a bug rather than a condition:
    /// re-running the wizard resets the user's whole configuration, and the
    /// only thing standing between that and a user is this check. An error the
    /// caller must handle is harder to ignore than a documented precondition.
    #[error("{} already exists — `configure` would reset it", path.display())]
    AlreadyConfigured { path: PathBuf },

    /// [`Cli::filters_set_trusted`] was handed the user-rules sentinel. **We**
    /// refused, before spawning anything.
    ///
    /// The second error here that describes a bug rather than a condition, and
    /// it is [`Self::AlreadyConfigured`]'s reasoning exactly. `filters
    /// set-trusted` refuses a catalogue filter on its own — `Filter not
    /// custom` — but it accepts `-2147483648` and **writes**, and what that
    /// write turns off is the scriptlet and HTML rules in the user's own
    /// `user.txt`. It reports success while doing it, so nothing downstream
    /// would catch it: not the confirmation, not the re-read, which would
    /// faithfully report the flag we had just cleared.
    #[error("the user's own rules are not a subscribable list — their trust is not this application's to set")]
    UserRulesNotTrustable,

    /// A userscript name the CLI could not narrow to one script.
    ///
    /// Its own variant rather than a [`Self::Refused`] because the two mean
    /// opposite things to a caller. A refusal is a condition that may pass — a
    /// bad URL can be retyped, a lapsed licence renewed. This one **cannot**:
    /// `enable`, `disable` and `remove` match a case-insensitive substring
    /// against every id and title and offer no exact-match flag, so a script
    /// whose id is contained in another's is unreachable for as long as both
    /// are installed, and re-trying with the exact id — which is what a user
    /// would reasonably do — produces exactly this again (contract §15).
    ///
    /// `candidates` is what the CLI listed, so a caller can say which scripts
    /// collided rather than only that something did. Measured: the refusal is
    /// at exit 0 and leaves `proxy.yaml` untouched, so nothing needs undoing.
    ///
    /// [`crate::Userscript::ambiguous`] predicts this before a command is run,
    /// which is where the UI should act on it. This variant is the backstop for
    /// the case the prediction cannot cover: another window, or a terminal,
    /// installing a colliding script between the read and the click.
    #[error("`{name}` names more than one userscript ({}), and AdGuard offers no way to be more specific", candidates.join(", "))]
    AmbiguousUserscript {
        name: String,
        candidates: Vec<String>,
    },

    /// A blank userscript name. **We** refused, before spawning anything.
    ///
    /// The third error here that describes a bug rather than a condition, and
    /// it is [`Self::UserRulesNotTrustable`]'s reasoning exactly — a guard
    /// beside a call site is one somebody can add a second call site around.
    ///
    /// Measured, and the reason this is not merely tidiness: the empty string
    /// is a **wildcard**. Every id contains it, so on an install with one
    /// script `userscripts disable ""` disables that script and reports
    /// success — the user's only userscript, switched off by a name that names
    /// nothing. With two installed it is ambiguous instead, which means the
    /// damage depends on how many scripts happen to be present. Whitespace is
    /// not trimmed by the CLI (`'   '` matched nothing), but it is trimmed here
    /// so that a name which is blank in any sense is refused the same way.
    #[error("a userscript has to be named — the empty string matches every installed script")]
    UnnamedUserscript,
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
    /// **One prompt is the exception**, and it is why [`Self::run_answering`]
    /// exists: the annoyance-filter agreement (contract §7) does not take a
    /// default and carry on — it refuses the work. Closed stdin there means the
    /// Annoyances group cannot be switched on from this application at all.
    /// That one answer is written and the pipe is closed immediately behind it,
    /// so a *second* prompt in the same command still meets EOF and the
    /// no-hang guarantee above is unchanged.
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
        self.run_answering(args, timeout, None)
    }

    /// [`Self::run_within`], with one line typed at whatever asks first.
    ///
    /// `answer` is written to the child's stdin and the pipe is **closed behind
    /// it**, which is the whole safety story: the first prompt gets the line,
    /// every later one gets EOF and takes its default exactly as it does under
    /// [`Self::run`]. Nothing waits for a second answer that is not coming.
    ///
    /// The write is not on a thread of its own and does not need to be — a pipe
    /// buffers ~64 KB before it blocks a writer, and the only caller sends four
    /// bytes. A failed write is deliberately ignored: a child that has already
    /// exited is a closed pipe, and what happened is decided by reading its
    /// output, never by whether we managed to talk to it.
    ///
    /// The one caller is [`Self::filter_action`]; see contract §7 for the
    /// prompt it answers.
    pub fn run_answering(
        &self,
        args: &[&str],
        timeout: Duration,
        answer: Option<&str>,
    ) -> Result<Output, Error> {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(match answer {
                Some(_) => std::process::Stdio::piped(),
                None => std::process::Stdio::null(),
            })
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

        // Answer, then EOF: the handle is dropped at the end of this block and
        // the pipe closes with it.
        if let (Some(answer), Some(mut pipe)) = (answer, child.stdin.take()) {
            let _ = pipe.write_all(answer.as_bytes());
        }

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
            // is what contract §3 first assumed. Sort the cases apart before
            // blaming ourselves.
            if let Some(message) = licence_complaint(&stderr) {
                return Err(Error::Unlicensed { message });
            }
            // Nor does a failure always explain itself on stderr, which is the
            // other half of what §3 first assumed. Measured: two invocations
            // racing to initialise a data directory that has never been used
            // leave one exiting **1** with `Filter manager initialization
            // failed` on **stdout** and stderr empty — eight runs in twelve,
            // and the shape it needs is this app's own startup, where the
            // Status page's `status` and the licence read go out together.
            //
            // So the stream is the discriminator. CLI11 rejects a command line
            // on stderr; the program refusing to do the work prints on stdout,
            // exactly as it does at exit 0. Blaming that on our arguments would
            // be the lapsed-licence mistake again, in a new disguise.
            //
            // The *last* line, not the first, for the same reason
            // [`Cli::activate`] reads its own output that way: the one measured
            // shape here is a single line, so the two agree on it, but `activate`
            // opens with a menu prompt that was never asked and a failure of
            // *that* command would otherwise be reported to the user as "How do
            // you want to activate AdGuard CLI?".
            if stderr.trim().is_empty() {
                if let Some(message) = last_line(&stdout) {
                    return Err(Error::Refused { message });
                }
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
    ///
    /// # A start that failed used to come back as success
    ///
    /// It exits **0** either way — rule 3 again — so returning `Ok` for the
    /// whole output meant the Status page had nothing to show and said nothing
    /// at all. Measured against an install with a wedged leftover process
    /// (see [`crate::orphan`]), stderr empty, after 60 s:
    ///
    /// ```text
    /// Failed to start proxy server: An unknown error has occurred
    /// ```
    ///
    /// [`start_refusal`] recognises that, so the one shape known to mean
    /// failure becomes [`Error::Refused`] carrying AdGuard's own sentence.
    /// **Failure is defined positively here, not success** — the opposite of
    /// [`Self::config_set`] — because an unrecognised line must stay `Ok` and
    /// leave the verdict to the status re-read that follows. Defining success
    /// instead would turn any reworded confirmation into a start that reports
    /// failure while the proxy runs.
    ///
    /// The `Ok` value is the CLI's last line rather than its whole output: a
    /// start prints two kilobytes of redrawn log before its conclusion, which
    /// is not something to hand to a toast.
    pub fn start(&self) -> Result<String, Error> {
        self.lifecycle("start")
    }

    /// Stop the proxy. Re-read status afterwards.
    ///
    /// Deliberately *not* given [`Self::start`]'s failure check. `stop` against
    /// an install with nothing running answers `Failed to stop proxy server, it
    /// is not running` in 0.1 s at exit 0 — measured — and that is the ordinary
    /// outcome of stopping something already stopped, not an error worth a
    /// toast. The status re-read that follows every action is what decides.
    pub fn stop(&self) -> Result<String, Error> {
        Ok(self.run(&["stop"])?.stdout.trim().to_owned())
    }

    /// Restart the proxy. Re-read status afterwards.
    ///
    /// Shares [`Self::start`]'s deadline and its failure check, because it ends
    /// in a start and fails the same way when one cannot bind.
    pub fn restart(&self) -> Result<String, Error> {
        self.lifecycle("restart")
    }

    /// The shared half of [`Self::start`] and [`Self::restart`].
    fn lifecycle(&self, verb: &str) -> Result<String, Error> {
        let out = self.run_within(&[verb], START_TIMEOUT)?;
        match start_refusal(&out.stdout) {
            Some(message) => Err(Error::Refused { message }),
            None => Ok(last_line(&out.stdout).unwrap_or_default()),
        }
    }

    /// Refresh the filter lists, DNS filter lists, userscripts, Safe Browsing
    /// and CRLite, and ask whether a newer AdGuard CLI exists.
    ///
    /// **The command's name is the one misleading thing about it.** It is not a
    /// check: five of the six components are *updated*, and only the sixth — the
    /// application — is merely checked. Anything that describes this to a user
    /// has to say so, which is why nothing in the UI is labelled with the word
    /// "check" alone (contract §14).
    ///
    /// # What it does not tell you
    ///
    /// The exit status. Measured over fourteen runs, five of which failed a
    /// component: **every one of them exited 0 with empty stderr**. So this is
    /// the strongest form of contract §3's rule in the tree — the status is not
    /// merely half-trustworthy here, it carries no information about the outcome
    /// at all, and [`parse_update_report`] derives every verdict from the text.
    ///
    /// It does not name the component in its failures either. `Failed to update
    /// filters` is what both the HTTP filter and the DNS filter component print,
    /// so the pairing with the header is the only thing that distinguishes them
    /// and is done in the parser rather than left to a caller.
    ///
    /// # It needs no licence, and it takes seconds
    ///
    /// Unlike `status`, `license`, `filters list` and every `filters` write
    /// subcommand, this runs on an unlicensed install and really updates there —
    /// so the control that calls it needs no licence caveat.
    ///
    /// [`NETWORK_TIMEOUT`] rather than [`LOCAL_TIMEOUT`]: the measured range is
    /// 1.8–7.3 s, but this is the one command in the app that reaches
    /// `filters.adtidy.org` for everything at once, and a hang there is what the
    /// generous deadline exists for.
    ///
    /// Callers must still re-read whatever they display: `UpdateReport::changed`
    /// says a catalogue moved, and the catalogue itself says what it moved to.
    pub fn check_update(&self) -> Result<UpdateReport, Error> {
        let out = self.run_within(&["check-update"], NETWORK_TIMEOUT)?;
        parse_update_report(&out.stdout).ok_or_else(|| Error::Unparseable {
            args: "check-update".to_owned(),
            output: out.stdout.clone(),
        })
    }

    /// Read the licence.
    ///
    /// Refused outright while the install is unlicensed — exit 1 with the
    /// complaint on stderr, which [`Self::run_within`] has already turned into
    /// [`Error::Unlicensed`] by the time we get here. That is the whole reason
    /// activation cannot be driven by polling this: there is no status to poll
    /// for until the thing being waited on has already happened (contract §7).
    ///
    /// # The output is the most sensitive thing this CLI prints
    ///
    /// Owner e-mail and licence key, in full, on every successful read. So a
    /// parse failure must **not** quote what it could not parse, the way
    /// [`Self::status`] does: that message ends up in a row subtitle or a toast.
    /// [`redact_values`] keeps the shape and drops every value, which is enough
    /// to recognise a rewording and useless to anyone reading over a shoulder.
    pub fn license(&self) -> Result<License, Error> {
        let out = self.run(&["license"])?;
        parse_license(&out.stdout).ok_or_else(|| Error::Unparseable {
            args: "license".to_owned(),
            output: redact_values(&out.stdout),
        })
    }

    /// Begin — or complete — licence activation.
    ///
    /// The same command does both, which is what makes the flow work without a
    /// TTY. Measured on v1.4.13 with stdin closed, against an unlicensed
    /// sandbox:
    ///
    /// ```text
    /// $ adguard-cli activate
    /// How do you want to activate AdGuard CLI?
    /// Warning: No TTY for user input. Please visit https://link.adtidy.org/…&appid=<id>
    /// to log in, then run `adguard-cli activate` again to complete activation.
    /// ```
    ///
    /// Exit 0, on stdout, no ANSI. The first line is a menu prompt that never
    /// got asked; the second is the one worth acting on.
    ///
    /// # Why running it twice is not a poll
    ///
    /// Measured: the `appid` in that URL is **stable for a given data
    /// directory** — three invocations produced the identical link, and a
    /// second sandbox produced a different one. So "run `activate` again" does
    /// not start a fresh attempt that races the first; it asks after the one the
    /// user was already sent to log into. That is what makes a *finish* button
    /// the honest shape for this flow, rather than a lesser version of a poll
    /// nobody can write (`architecture.md` §5).
    ///
    /// # Only reached while the licence is not active
    ///
    /// What this prints against an already-licensed install is **not measured**,
    /// deliberately: the one install available to measure it on is the author's
    /// own, and `activate` is not a command to point at a working licence to
    /// see what happens. The UI therefore offers it only while `license` says
    /// the licence is not active, and decides the outcome by reading `license`
    /// afterwards rather than by believing anything printed here.
    pub fn activate(&self) -> Result<Activation, Error> {
        let out = self.run_within(&["activate"], NETWORK_TIMEOUT)?;
        Ok(match activation_url(&out.stdout) {
            Some(url) => Activation::NeedsLogin { url },
            None => Activation::Replied {
                message: last_line(&out.stdout)
                    .unwrap_or_else(|| "`adguard-cli activate` said nothing at all".to_owned()),
            },
        })
    }

    /// Where this `Cli` would find `proxy.yaml`.
    ///
    /// Respects [`Self::with_xdg_data_home`], which the free function in
    /// [`crate::paths`] cannot: that override is applied to the child's
    /// environment, not to ours.
    pub fn config_path(&self) -> Option<PathBuf> {
        match &self.xdg_data_home {
            Some(home) => Some(paths::config_file_under(home)),
            None => paths::config_file(),
        }
    }

    /// Seed a data directory that has never been configured.
    ///
    /// # Why this exists at all
    ///
    /// `architecture.md` §5 described the first-run assistant as "discrete
    /// `config set` calls", and contract §10 told this module never to invoke
    /// `configure`. Measured on v1.4.13, those two cannot both hold: until
    /// `proxy.yaml` exists, `config set` refuses every real key.
    ///
    /// ```text
    /// $ XDG_DATA_HOME=/tmp/fresh adguard-cli config set -- listen_ports.http_proxy 3128
    /// No configuration YAML file
    /// You can only configure the 'log_level' and 'update_channel'
    /// Run `adguard-cli configure` to configure AdGuard CLI, or `adguard-cli
    /// import-settings <path_to_zip>` to import settings from zip
    /// ```
    ///
    /// Exit 0, on stdout, nothing written — the ordinary semantic refusal. And
    /// nothing else creates the file: `config get`, `config set` and `activate`
    /// were each run against a virgin directory and none of them produced one.
    /// So a first run has exactly one way forward, and this is it.
    ///
    /// The two keys that *are* accepted first — `log_level` and
    /// `update_channel` — are a trap worth knowing about rather than a way
    /// round it. They print `Config has been updated` and persist into
    /// `adguard.conf`, so the confirmation is truthful about a file that is not
    /// the one anything reads.
    ///
    /// # What it does, measured
    ///
    /// With stdin closed (which [`Self::run`] guarantees) every prompt takes its
    /// default and names the key that changes it afterwards. Against a licensed
    /// directory with no `proxy.yaml`: exit **0**, all on **stdout**, 0.10 s.
    ///
    /// ```text
    /// Warning: No TTY available. Using default values for configuration.
    /// Please enter the new value of the HTTP proxy listen port [default: 3129]:
    /// Warning: No TTY for user input. Using default value (3129). Use
    /// `adguard-cli config set listen_ports.http_proxy` to change.
    /// …
    /// The proxy server is ready to start. You can start it by running `adguard-cli start`
    /// ```
    ///
    /// It leaves a complete 220-line `proxy.yaml` with all 105 of its upstream
    /// comments — the same shape as a real install's — plus `user.txt`,
    /// `dns_user.txt`, `https_exclusions.txt`, `browsers.yaml` and the CA
    /// certificate. Afterwards ordinary `config set` works and stays surgical.
    ///
    /// **One prompt is skipped in silence**: *"Do you want to install the
    /// certificate on the system?. You will need to enter your password"* is the
    /// only one with no no-TTY warning and no key. So the seeded state is HTTPS
    /// filtering **on** with its CA outside the system trust store — which is
    /// the caller's to surface, not something to paper over here (§6 rules out
    /// installing it for the user).
    ///
    /// # It is licence-gated
    ///
    /// Unlicensed it exits **1** with the usual complaint and usage dump on
    /// stderr, so this returns [`Error::Unlicensed`] like every other gated
    /// command. Note it seeds the file *anyway*, before reaching that gate — but
    /// without the CA, so the caller should activate first rather than lean on
    /// that.
    ///
    /// # The guard is the point
    ///
    /// Run against a directory that already has a `proxy.yaml`, `configure`
    /// takes a different branch entirely — its own strings are
    /// *"The initial configuration has already been completed. The running proxy
    /// server will be stopped, and the configuration will be reset."* and
    /// *"No TTY available. Proceeding with reconfiguration using default
    /// values."* With stdin closed there is no prompt to decline at, so that
    /// branch would proceed and take the user's whole configuration with it.
    ///
    /// That branch is deliberately **not** measured: the only licensed install
    /// available to try it on is the author's own, and the strings are clear
    /// enough that confirming them costs more than it settles. Instead the file
    /// is checked here, immediately before the spawn, and this is the only place
    /// in the codebase that names the `configure` subcommand.
    ///
    /// # Success is the file, not the echo
    ///
    /// As everywhere else in this module the confirmation line proves nothing,
    /// and here there is a stronger witness available: the file either exists
    /// afterwards or it does not. That is what decides.
    pub fn configure(&self) -> Result<(), Error> {
        let Some(path) = self.config_path() else {
            return Err(Error::Unparseable {
                args: "configure".to_owned(),
                output: "could not work out where proxy.yaml would live".to_owned(),
            });
        };
        if path.is_file() {
            return Err(Error::AlreadyConfigured { path });
        }

        let out = self.run(&["configure"])?;

        if path.is_file() {
            Ok(())
        } else {
            Err(Error::Refused {
                message: last_line(&out.stdout)
                    .unwrap_or_else(|| "`adguard-cli configure` said nothing at all".to_owned()),
            })
        }
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
    ///
    /// # The Annoyances group needs an answer, not just a command
    ///
    /// A list in that group (contract §7) raises an agreement on stdin before
    /// it will switch on. `consent` decides whether one is given, and the
    /// caller owes the user a sight of [`crate::model::ANNOYANCE_TERMS`] before
    /// passing [`Consent::Granted`] — this cannot check that and does not try.
    ///
    /// # `add` prints its confirmation *before* it refuses
    ///
    /// The refusal has to be looked for first, because the obvious success
    /// check passes right over it. Measured, at exit 0, stdin closed:
    ///
    /// ```text
    /// $ adguard-cli filters add 18
    /// Filter [Title: AdGuard Cookie Notices filter] added
    /// Please read carefully before enabling Annoyance filters
    /// …
    /// Enable these filters? (yes/no):
    /// Annoyance filters won't be enabled due to user's choice
    /// ```
    ///
    /// `confirms(…, "added")` is satisfied by line one, so reading in the
    /// obvious order reports success for a command that subscribed to the list
    /// and left it switched off — which is exactly what the user did not ask
    /// for, reported as though it were.
    pub fn filter_action(
        &self,
        set: FilterSet,
        action: FilterAction,
        filter_id: i64,
        consent: Consent,
    ) -> Result<(), Error> {
        let filter_id = filter_id.to_string();
        let mut args = set.cli_prefix().to_vec();
        args.push(action.subcommand());
        args.push(&filter_id);

        let answer = match consent {
            Consent::Granted => Some(ANNOYANCE_ACCEPT),
            Consent::Withheld => None,
        };
        let out = self.run_answering(&args, LOCAL_TIMEOUT, answer)?;

        if declined_annoyances(&out.stdout) {
            return Err(Error::Refused {
                message: "AdGuard did not get agreement to its annoyance-filter terms, so \
                          the list was not switched on"
                    .to_owned(),
            });
        }
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

    /// Subscribe to a list AdGuard's catalogue does not carry.
    ///
    /// Takes a URL, or a path to a local file — the CLI accepts both through
    /// the same positional and normalises a path to `file://…` on the way in.
    ///
    /// # Success is the usual shape; the failures are not
    ///
    /// Measured on v1.4.13 against both sets (`dns filters install` behaves
    /// identically). The confirmation is the `Filter [<something>] <verb>` form
    /// [`confirms`] already knows:
    ///
    /// ```text
    /// $ adguard-cli filters install -- https://example.org/list.txt
    /// Filter [Title: Example List] from URL: https://example.org/list.txt installed
    /// ```
    ///
    /// Everything else is a refusal at exit 0 on stdout, and there are only two
    /// shapes of it. A second install of a URL already present:
    ///
    /// ```text
    /// Filter with the specified URL already exists:
    /// [x] |       -10001 | Example List [non-trusted]    2026-07-31 00:51:48
    /// ```
    ///
    /// — note the trailing `filters list` table, which contract §6 says not to
    /// parse; [`first_line`] keeps the sentence and drops it. And, for
    /// everything else that can go wrong:
    ///
    /// ```text
    /// Failed to install the filter from URL: <whatever you passed>
    /// ```
    ///
    /// **That one sentence covers a 404, a refused connection, an unresolvable
    /// host, a missing file and a string that was never a URL.** They are
    /// indistinguishable in the output, so neither this function nor its caller
    /// may claim to know which happened.
    ///
    /// # Why the generous deadline
    ///
    /// This is the first caller of [`NETWORK_TIMEOUT`] that is genuinely a
    /// network command throughout. Measured, the CLI has a deadline of its own:
    /// a socket that accepts the connection and then never answers produces the
    /// failure sentence after **60 s**, at exit 0. That sits inside the 120 s
    /// here, so the wrapper's timeout is a backstop that should not normally
    /// fire — but a minute is long enough that the caller owes the user a
    /// progress state, and long enough that it will block every other
    /// config-path invocation for the duration (contract §3).
    ///
    /// # This is not proof anything was installed
    ///
    /// As everywhere else in this module the confirmation is not the effect, and
    /// here it is weaker than usual. The only thing checked about the content is
    /// whether it *begins* with HTML — `<html…` and `<!DOCTYPE html>` are
    /// refused, which catches a link that answers 200 with an error page.
    /// Nothing else is: JSON, prose and an empty file all install as filter
    /// lists holding no rules, and report success. It also echoes
    /// `Filter [Title: file:///…]` for a list with no `! Title:` header while
    /// storing that title as the empty string.
    ///
    /// So `Ok` means only that the CLI said it acted. The caller confirms
    /// against the database, where the new row is the evidence — see
    /// [`Catalogue::custom_filters`], which exists for this.
    ///
    /// [`Catalogue::custom_filters`]: crate::filters::Catalogue::custom_filters
    pub fn filters_install(&self, set: FilterSet, url: &str) -> Result<(), Error> {
        // The `--` guard is as mandatory here as for `config set`: measured,
        // `filters install -leading-dash` exits 1 with `<filter-url> is
        // required` on stderr and installs nothing.
        let mut args = set.cli_prefix().to_vec();
        args.push("install");
        args.push("--");
        args.push(url);

        let out = self.run_within(&args, NETWORK_TIMEOUT)?;
        if confirms(&out.stdout, "installed") {
            Ok(())
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout).unwrap_or_else(|| {
                    format!("`adguard-cli {}` said nothing at all", args.join(" "))
                }),
            })
        }
    }

    /// Let one custom list use privileged rules, or take that back.
    ///
    /// A trusted list may carry scriptlet and `$$`/HTML-filtering rules, which
    /// is to say it may run script in the pages the user visits. Untrusted is
    /// the default an install lands on, and the only reason to leave it is that
    /// the user vouches for the source.
    ///
    /// # No [`FilterSet`], because the DNS set has no such command
    ///
    /// Every other function here takes one. This cannot: measured on v1.4.13,
    /// `adguard-cli dns filters` has no `set-trusted` in its help and asking
    /// for it exits **1** with `A subcommand is required` on stderr. Taking a
    /// set and refusing one of its two values at run time would put a failure
    /// in the caller's hands that the type system can hold instead — DNS lists
    /// are hostname-only and the concept does not reach them.
    ///
    /// # The confirmation is a shape of its own
    ///
    /// Measured, at exit 0, on stdout, as the command's only line:
    ///
    /// ```text
    /// $ adguard-cli filters set-trusted -10001 true
    /// Filter with ID: -10001 successfully updated trust
    /// ```
    ///
    /// That is **not** the `Filter [<something>] <verb>` form every other
    /// filter command answers in, so [`confirms`] cannot see it and
    /// [`confirms_trust`] exists for this one command. The refusals are two,
    /// both at exit 0 on stdout, and neither shares an anchor with it:
    ///
    /// ```text
    /// Failed to update trust filter with ID: -99999: Filter not found
    /// Failed to update trust filter with ID: 2: Filter not custom
    /// ```
    ///
    /// The second is AdGuard enforcing what [`Filter::supports_trust`] also
    /// says: trust belongs to a list the user fetched, not to the catalogue.
    ///
    /// # This is not proof the flag moved
    ///
    /// As everywhere in this module. The same success line is printed for a
    /// no-op — setting a list trusted twice reports success twice — so the
    /// caller re-reads `is_trusted` from the catalogue and renders from that.
    ///
    /// # Licence-gated, like every other filter command
    ///
    /// Unlicensed it exits **1** with `You need to activate an AdGuard license
    /// to use this command` on stderr, which [`run_within`] already maps to
    /// [`Error::Unlicensed`]. Nothing here needs to know that, and it is
    /// recorded only because the first draft of this comment guessed at the
    /// **opposite** — that `add`, `enable` and `disable` kept working, which
    /// would have meant a page where a list could be switched but not trusted.
    /// Measured 6 August 2026 against a sandbox whose licence was taken away:
    /// `add`, `enable`, `disable`, `remove`, `set-title` and `set-trusted` all
    /// refuse identically and write nothing. There is no asymmetry to handle.
    ///
    /// [`run_within`]: Self::run_within
    /// [`Filter::supports_trust`]: crate::Filter::supports_trust
    pub fn filters_set_trusted(&self, filter_id: i64, trusted: bool) -> Result<(), Error> {
        // Refused here rather than at the call site, on the same grounds as
        // `configure`: this is the one id the CLI will accept and act on when
        // it must not, and a guard beside a call site is one somebody can add
        // a second call site around. Measured — `set-trusted -2147483648 false`
        // really writes, and what it turns off is the scriptlet and HTML rules
        // in the user's own `user.txt`, silently and with a success line.
        if filter_id == Filter::USER_RULES_ID {
            return Err(Error::UserRulesNotTrustable);
        }

        let filter_id = filter_id.to_string();
        // A negative id needs no `--` guard here either — `set-trusted -10001
        // true` parses as two positionals, exactly as `disable -10001` does
        // (contract §6). The value is ours and is always one of these two
        // words; a spelling the parser does not accept exits 1 on stderr,
        // which is `BadInvocation` and would be our bug.
        let args = [
            "filters",
            "set-trusted",
            &filter_id,
            if trusted { "true" } else { "false" },
        ];

        let out = self.run_within(&args, LOCAL_TIMEOUT)?;
        if confirms_trust(&out.stdout) {
            Ok(())
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout).unwrap_or_else(|| {
                    format!("`adguard-cli {}` said nothing at all", args.join(" "))
                }),
            })
        }
    }

    /// Switch a userscript on — put it back into `proxy.yaml`'s list.
    ///
    /// See [`Self::userscript_action`] for everything the three verbs share,
    /// which is nearly all of it.
    ///
    /// **`enable` on an already-enabled script reports success**, where its
    /// opposite reports `Userscript 'X' is not enabled`. The two no-ops are not
    /// symmetrical and only `disable`'s is visible in the text; nothing here
    /// depends on the difference, because the caller re-reads either way.
    pub fn userscripts_enable(&self, name: &str) -> Result<(), Error> {
        self.userscript_action("enable", name, "enabled successfully")
    }

    /// Switch a userscript off — take it out of `proxy.yaml`'s list.
    ///
    /// The files stay on disk: this is not a removal, and a script disabled
    /// here is still installed and still on the page (contract §15).
    pub fn userscripts_disable(&self, name: &str) -> Result<(), Error> {
        self.userscript_action("disable", name, "disabled successfully")
    }

    /// Delete a userscript: both files, and its entry if it had one.
    ///
    /// Unlike [`Self::userscripts_disable`] this is not reversible without the
    /// source — so the caller owes the user a confirmation first, exactly as
    /// custom-filter removal does (`architecture.md` §5).
    pub fn userscripts_remove(&self, name: &str) -> Result<(), Error> {
        self.userscript_action("remove", name, "removed successfully")
    }

    /// The shared half of the three userscript verbs.
    ///
    /// # The name is not an id, and that is the whole hazard
    ///
    /// All three take what the help calls a `<userscript-name>`, and it is
    /// **matched as a case-insensitive substring against every installed
    /// script's id *and* title**. Measured on v1.4.13 (contract §15):
    /// `adguard-extra`, `AdGuard Extra`, `ADGUARD-EX` and `Extra` all reach the
    /// same script. There is no exact-match flag in `--help-all`.
    ///
    /// Two consequences, and neither is avoidable by passing a better string:
    ///
    /// - **The empty string matches everything.** On a one-script install
    ///   `disable ""` switches that script off and reports success. Refused
    ///   here before spawning — see [`Error::UnnamedUserscript`].
    /// - **An id contained in another script's id or title is unreachable.**
    ///   `disable hello` is refused while `hello-world` is installed, even
    ///   though `hello` is the exact id. That comes back as
    ///   [`Error::AmbiguousUserscript`] carrying the candidates the CLI named,
    ///   and [`crate::Userscript::ambiguous`] predicts it so the UI can decline
    ///   to offer the control at all.
    ///
    /// # The `--` guard is mandatory, and measured so
    ///
    /// Both the name and the URL are positionals, and CLI11 still reads a
    /// leading `-` as an option. Measured:
    ///
    /// ```text
    /// $ adguard-cli userscripts disable -bogus
    /// <userscript-name> is required          # exit 1, nothing done
    /// $ adguard-cli userscripts disable -- -bogus
    /// No userscripts matching '-bogus'       # exit 0, parsed
    /// ```
    ///
    /// A userscript id is a filename stem and AdGuard chooses it, so a leading
    /// dash is unlikely — but "unlikely" is what the guard costs nothing to
    /// stop being load-bearing about. It changes nothing for ordinary names,
    /// verified above for `adguard-extra` in both directions.
    ///
    /// # Success is positive, and it is not proof
    ///
    /// Every refusal is at exit 0 ([§3](../docs/cli-contract.md)), so the
    /// confirmation is matched rather than the status read, and even then it
    /// only means the CLI said it acted. The caller re-reads `proxy.yaml` and
    /// the directory, which is what [`crate::userscripts::read`] is for.
    fn userscript_action(&self, verb: &str, name: &str, confirmation: &str) -> Result<(), Error> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::UnnamedUserscript);
        }

        let args = ["userscripts", verb, "--", name];
        let out = self.run_within(&args, LOCAL_TIMEOUT)?;

        // Before the confirmation check, not after: the ambiguous refusal names
        // no verb and would otherwise fall through to the generic `Refused`
        // below, losing the candidate list that is the only useful thing in it.
        if let Some(candidates) = ambiguous_userscripts(&out.stdout) {
            return Err(Error::AmbiguousUserscript {
                name: name.to_owned(),
                candidates,
            });
        }
        if confirms_userscript(&out.stdout, confirmation) {
            Ok(())
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout).unwrap_or_else(|| {
                    format!("`adguard-cli {}` said nothing at all", args.join(" "))
                }),
            })
        }
    }

    /// Install a userscript from a URL.
    ///
    /// # It is network-only, unlike `filters install`
    ///
    /// The one place these two commands diverge, and it is measured rather than
    /// assumed (contract §15). `filters install` takes a URL *or* a local path
    /// through the same positional; this takes a URL. A path and a `file://`
    /// URL are both refused:
    ///
    /// ```text
    /// $ adguard-cli userscripts install /tmp/hello.user.js
    /// Failed to install userscript
    /// $ adguard-cli userscripts install file:///tmp/hello.user.js
    /// Failed to install userscript
    /// ```
    ///
    /// So the row that calls this asks for an http(s) URL and says so, rather
    /// than letting a user discover it by having a file picker fail. A loopback
    /// HTTP server does work, which is what keeps `userscripts_sandbox.rs`
    /// hermetic without reaching the network.
    ///
    /// # One sentence covers every failure
    ///
    /// `Failed to install userscript` is printed for a 404, a body that is not
    /// a userscript, an unresolvable host, a local path and a string that was
    /// never a URL — and unlike the filter command's version it does not even
    /// echo what was passed. **Neither this function nor its caller may claim
    /// to know which happened.**
    ///
    /// # The `--` guard prevents a silent no-op here, not just a rejection
    ///
    /// Worse than the `disable` case above, and the reason the guard is not
    /// optional. Measured:
    ///
    /// ```text
    /// $ adguard-cli userscripts install -http://example.org/x.user.js
    /// Install a userscript from URL          # the help text
    /// Usage: adguard-cli userscripts install [OPTIONS] <userscript-url>
    /// …                                      # exit 0, nothing installed
    /// ```
    ///
    /// The leading `-h` is read as `--help`, so the command prints usage, exits
    /// **0**, and installs nothing. Without the guard that is a success status
    /// on a command that did not run — the one shape this module works hardest
    /// to never return `Ok` for. With it, the same string reaches the installer
    /// and fails honestly.
    ///
    /// # Re-installing is the update path, and it re-enables
    ///
    /// A URL already installed is not refused: the pair is overwritten in place
    /// (measured, version `0.2.1` -> `0.9.9`) **and a disabled script is
    /// switched back on**, silently. Anything offering this as *Reinstall* has
    /// to disclose that, because a user who disabled a script did not ask for
    /// updating it to start it running again.
    ///
    /// [`NETWORK_TIMEOUT`] for the reason [`Self::filters_install`] gives: this
    /// is a network command throughout, and a caller owes the user a progress
    /// state for it.
    pub fn userscripts_install(&self, url: &str) -> Result<(), Error> {
        let url = url.trim();
        if url.is_empty() {
            return Err(Error::UnnamedUserscript);
        }

        let args = ["userscripts", "install", "--", url];
        let out = self.run_within(&args, NETWORK_TIMEOUT)?;
        if confirms_userscript(&out.stdout, "installed and enabled successfully") {
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

    /// Add one value to a list-valued setting.
    ///
    /// The sequence keys — `filters`, `userscripts`, `apps`,
    /// `dns_filtering.filters` — are the ones `config get` refuses; they are
    /// written with `list-add`/`list-remove` rather than `config set`.
    ///
    /// # This does not deduplicate
    ///
    /// Measured against a sandbox: adding a value the list already holds
    /// appends it a **second time**, exits 0, and prints `Config has been
    /// updated` like any other success.
    ///
    /// ```text
    /// $ adguard-cli config list-add -- dns_filtering.filters dns_user.txt
    /// filters:
    ///   - 'dns_user.txt'
    ///   - 'dns_user.txt'
    ///
    /// Config has been updated
    /// ```
    ///
    /// So a caller driving a *toggle* off one of these lists must read the list
    /// and decide membership itself, calling this only when it would change
    /// something. Issuing it off a stale read corrupts the list instead of
    /// no-opping, and nothing in the output distinguishes the two.
    pub fn list_add(&self, key: &str, value: &str) -> Result<Applied, Error> {
        self.config_list("list-add", key, value)
    }

    /// Remove one value from a list-valued setting.
    ///
    /// Removing a value that is not there is a silent success — exit 0, the
    /// unchanged list echoed, `Config has been updated` — so this is safe to
    /// issue speculatively in a way [`Self::list_add`] is not.
    ///
    /// # The echo of an empty list is not what lands in the file
    ///
    /// Removing the **last** element prints `filters:` with nothing after it,
    /// which looks like a null. The file gets a proper `filters: []`, which
    /// [`crate::config::Config::list_at`] reads as `Some(vec![])`. An earlier
    /// revision of this comment believed the echo; re-read the file, as for
    /// every other write in this module.
    pub fn list_remove(&self, key: &str, value: &str) -> Result<Applied, Error> {
        self.config_list("list-remove", key, value)
    }

    /// Write a settings zip, and return **where it actually went**.
    ///
    /// The path is read back off the confirmation line rather than predicted,
    /// because `-o` decides between "a folder to put it in" and "the archive
    /// itself" by whether the path already exists (contract §13). An existing
    /// directory gets a generated `adguard-cli_<date>_<time>.zip` inside it;
    /// anything else *becomes* the archive, at that exact name, with no `.zip`
    /// appended. Which of the two happened is a fact about the filesystem at
    /// the moment of the call, so the caller cannot work it out and must not
    /// try — and both forms print the answer.
    ///
    /// Slow for a reason that is not the user's settings: **51.1 MB of the
    /// 51.8 MB is `agflm_standard.db`**, the redownloadable filter catalogue.
    /// Anything driving this needs a progress state and a generous timeout.
    pub fn export_settings(&self, output: &Path) -> Result<PathBuf, Error> {
        self.exported(&["export-settings", "-o"], output, "export-settings")
    }

    /// Write a logs zip, and return where it actually went.
    ///
    /// **This bundle discloses the configuration.** It carries `proxy.yaml`
    /// and does *not* carry `access.log` — measured twice, contract §13 — so
    /// it is less sensitive than assumed about browsing and more sensitive
    /// than assumed about settings. Whatever offers this has to say so.
    pub fn export_logs(&self, output: &Path) -> Result<PathBuf, Error> {
        self.exported(&["export-logs", "-o"], output, "export-logs")
    }

    /// The shared half of the two exports.
    ///
    /// Success is defined **positively**, by a confirmation line that carries a
    /// path, the same rule [`Self::config_set`] follows: an unrecognised line
    /// is a refusal rather than a success with an unknown path, because the
    /// only thing a caller can do with this result is show the user where their
    /// data went.
    fn exported(&self, verb: &[&str], output: &Path, what: &str) -> Result<PathBuf, Error> {
        // **No `--` here**, unlike every `config` call. Measured 2 August 2026:
        // `export-settings -o -- <path>` exits **1** with *"The following
        // argument was not expected"*, because `--` ends option parsing and
        // these subcommands have no positional to catch the path. §5's guard is
        // about option *values that look like options*; it does not generalise
        // to an option's own argument.
        let out = self.run_within(&[verb[0], verb[1], &output.to_string_lossy()], EXPORT_TIMEOUT)?;

        // Matched on the **success** prefix, not on `zip: `. The failure line
        // is `Failed to export logs to zip: <path>` — it carries the same
        // token and the same path, at exit 0, so a parser keyed on `zip: `
        // reports the archive it just failed to write. Measured, and it is
        // reachable in one click: see below.
        out.stdout
            .lines()
            .filter_map(|line| line.split_once("successfully exported to zip: "))
            .map(|(_, path)| PathBuf::from(path.trim()))
            .next_back()
            .ok_or_else(|| Error::Refused {
                message: first_line(&out.stdout)
                    .unwrap_or_else(|| format!("`{what}` said nothing at all")),
            })
    }

    /// Import a settings zip over this install.
    ///
    /// **The caller must have classified the archive first.** `import-settings`
    /// accepts a *logs* zip at exit 0 with wording identical to the correct
    /// case and leaves a partial install (contract §13), so exit status and
    /// output cannot tell the two apart and this method cannot either. That is
    /// what [`crate::zip::classify`] is for, and why it is not called from
    /// here: a check the caller can forget is worse than no check, so the
    /// decision belongs at the point where the file was chosen and can still
    /// be rejected with an explanation.
    ///
    /// **`-i` is required**, unlike the exports' optional `-o`.
    ///
    /// What an import does *not* destroy, measured: the licence and the CA
    /// survive it — `adguard.conf` is untouched. A confirmation dialog may not
    /// warn about losing them, because it would be false.
    pub fn import_settings(&self, input: &Path) -> Result<(), Error> {
        // No `--`, for the same reason as the exports above.
        let out = self.run_within(
            &["import-settings", "-i", &input.to_string_lossy()],
            EXPORT_TIMEOUT,
        )?;
        if out.stdout.contains("successfully imported") {
            Ok(())
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout)
                    .unwrap_or_else(|| "`import-settings` said nothing at all".to_owned()),
            })
        }
    }

    /// The shared half of [`Self::list_add`] and [`Self::list_remove`].
    ///
    /// Success is defined positively by `Config has been updated`, exactly as
    /// for [`Self::config_set`], and every other output shape is a refusal. The
    /// refusal worth recognising is the one for a key that is not a sequence:
    ///
    /// ```text
    /// $ adguard-cli config list-add -- dns_filtering.fallbacks 1.1.1.1
    /// This field is not a list setting
    /// Please run `... config set <key> <value>` to set a new value
    /// ```
    ///
    /// which is the mirror of what `config get` says about a list key, and is
    /// how the three DNS server settings were shown to be scalars.
    ///
    /// The `--` guard is as mandatory here as it is for `config set`, and fails
    /// the same way without it — measured, `list-add dns_filtering.filters
    /// -weird.txt` exits **1** with `<value> is required` on stderr and writes
    /// nothing, while the same call with `--` is accepted.
    ///
    /// One value per call, though the usage dump shows up to three positionals
    /// are accepted: a three-value call whose middle value is refused cannot be
    /// attributed to the value that caused it.
    fn config_list(&self, verb: &str, key: &str, value: &str) -> Result<Applied, Error> {
        let out = self.run(&["config", verb, "--", key, value])?;

        if out.stdout.lines().map(str::trim).any(|line| line == CONFIG_UPDATED) {
            Ok(Applied {
                restart_required: mentions_restart(&out.stdout),
            })
        } else {
            Err(Error::Refused {
                message: first_line(&out.stdout)
                    .unwrap_or_else(|| format!("`config {verb} {key}` said nothing at all")),
            })
        }
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
        // Echoes an argument this crate passed — a userscript name. No caller
        // reaches it through a secret-bearing command today, since the only
        // route here is `config set` on a credential key; it is redacted anyway
        // for `Unlicensed`'s reason, that a leak depending on two functions
        // staying unrelated is not one worth leaving in place. The candidates
        // are AdGuard's own words about scripts it found and are treated the
        // same way for the same reason.
        Error::AmbiguousUserscript { name, candidates } => Error::AmbiguousUserscript {
            name: redact(&name, secret),
            candidates: candidates
                .iter()
                .map(|candidate| redact(candidate, secret))
                .collect(),
        },
        // Carry no echo of the value. `AlreadyConfigured` holds only a path we
        // derived ourselves, and it — like `UserRulesNotTrustable` and
        // `UnnamedUserscript`, which hold nothing at all — is raised before any
        // argument is passed.
        other @ (Error::BinaryNotFound
        | Error::Spawn { .. }
        | Error::AlreadyConfigured { .. }
        | Error::UserRulesNotTrustable
        | Error::UnnamedUserscript) => other,
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

/// What [`Cli::activate`] came back with.
///
/// Two variants rather than a `Result`, because neither of these is a failure:
/// the CLI did what it was asked, and what it asked for in return is the
/// interesting part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// Log in at this URL, then run `activate` again. The measured no-TTY path,
    /// and the only one the UI has a flow for.
    NeedsLogin { url: String },

    /// Something else — an install that is already activated, or a shape that
    /// changed upstream. Unmeasured, so nothing is inferred from it: the caller
    /// reads `license` to learn where the install actually stands, and shows
    /// this sentence only to explain itself if that read still refuses.
    Replied { message: String },
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

/// Recognise `filters set-trusted`'s confirmation, which is not [`confirms`]'s
/// shape.
///
/// The one filter command that answers in a form of its own. Measured on
/// v1.4.13, at exit 0, on stdout:
///
/// ```text
/// Filter with ID: -10001 successfully updated trust
/// ```
///
/// `Filter with ID:` is not `Filter [`, so the house matcher returns false for
/// a command that worked — a caller reusing it would report every successful
/// change as a refusal, which is the failure this function exists to prevent.
///
/// **Anchored at both ends**, because only the tail carries the verdict. Both
/// refusals name the same id in the same place and differ from success only in
/// how the line opens and closes:
///
/// ```text
/// Failed to update trust filter with ID: -99999: Filter not found
/// Failed to update trust filter with ID: 2: Filter not custom
/// ```
///
/// What sits between the anchors is the id echoed back from the argument, so
/// checking it would only re-assert what we passed. It is left alone.
fn confirms_trust(stdout: &str) -> bool {
    stdout
        .lines()
        .map(str::trim)
        .any(|line| {
            line.starts_with("Filter with ID:") && line.ends_with("successfully updated trust")
        })
}

/// Recognise a userscript command's confirmation.
///
/// A third shape, sharing an anchor with neither [`confirms`] (`Filter [<x>]
/// <verb>`) nor [`confirms_trust`] (`Filter with ID: …`). Measured on v1.4.13,
/// at exit 0, on stdout, each as the command's only line:
///
/// ```text
/// Userscript 'AdGuard Extra' enabled successfully
/// Userscript 'AdGuard Extra' disabled successfully
/// Userscript 'AdGuard Extra' removed successfully
/// Userscript installed and enabled successfully
/// ```
///
/// Note the fourth: `install` does **not** name the script, because it has not
/// read the metadata yet at the point it prints. So the matcher cannot require
/// a quoted name, and anchors on the leading `Userscript` and the trailing
/// verb phrase alone.
///
/// The suffix match is what keeps `disabled successfully` from being satisfied
/// by an `enabled successfully` line — `ends_with` on the whole phrase, rather
/// than `contains`, because "enabled" is a substring of "disabled" and a
/// careless `contains("enabled")` would report every disable as a success.
fn confirms_userscript(stdout: &str, confirmation: &str) -> bool {
    stdout
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with("Userscript") && line.ends_with(confirmation))
}

/// The names a userscript command could not choose between, if it refused.
///
/// Measured, at exit 0, with `proxy.yaml` left untouched:
///
/// ```text
/// Multiple userscripts match 'hello'. Please specify more precisely:
///   - Hello Sandbox (ID: hello)
///   - Hello World (ID: hello-world)
/// ```
///
/// Returns the candidate lines with their `- ` stripped, so a caller can name
/// what collided; `None` when the output is not this refusal at all. An empty
/// vector is possible in principle — the header with no list under it — and is
/// returned as `Some(vec![])` rather than `None`, because the refusal did
/// happen and the caller must not read it as a success for want of details.
fn ambiguous_userscripts(stdout: &str) -> Option<Vec<String>> {
    let mut lines = stdout.lines().map(str::trim);
    lines.find(|line| line.starts_with("Multiple userscripts match"))?;
    Some(
        lines
            .filter_map(|line| line.strip_prefix("- "))
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// What the CLI's `Enable these filters? (yes/no):` prompt accepts.
///
/// The newline is the answer — without it the CLI is still waiting when the
/// pipe closes, and an unterminated line reads as no answer at all. `y` was not
/// measured and is not guessed at: this is the word the prompt names.
const ANNOYANCE_ACCEPT: &str = "yes\n";

/// The line `filters add` / `filters enable` prints when the annoyance-filter
/// agreement went unanswered or was refused.
///
/// Measured on v1.4.13, on **stdout**, at exit **0** — the ordinary semantic
/// refusal of contract §3, and the only trace in the output that the enable
/// half did not happen.
const ANNOYANCE_DECLINED: &str = "Annoyance filters won't be enabled due to user's choice";

fn declined_annoyances(stdout: &str) -> bool {
    stdout.lines().any(|line| line.trim() == ANNOYANCE_DECLINED)
}

/// The one line that means a `start` or `restart` did not take.
///
/// Matched on its opening, not the whole sentence: the tail is AdGuard's
/// description of the cause — `An unknown error has occurred` is the measured
/// one, and a build that names a real reason should have that shown rather than
/// go unrecognised.
///
/// Anchored at the start of a line so it cannot be tripped by the redrawn log
/// block a start prints above its conclusion, where the same words could appear
/// inside a longer message.
///
/// Note the prefix says *start* for both verbs. `restart` was not measured in
/// the failing state — the only install available to wedge on purpose is the
/// author's own, and once cleared it does not come back on demand — but it ends
/// in the same start, and the cost of being wrong here is an unrecognised
/// failure falling through to the status re-read, which is exactly where it went
/// before.
const START_FAILED: &str = "Failed to start proxy server";

fn start_refusal(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(START_FAILED))
        .map(str::to_owned)
}

fn first_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The last non-empty line, which is where this CLI puts its conclusions.
///
/// Used only by [`Cli::activate`], where the first line is a menu prompt that
/// was never asked ("How do you want to activate AdGuard CLI?") and the line
/// worth showing is the one after it.
fn last_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()
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

/// Parse `adguard-cli license`.
///
/// Defined positively on the **status** line, the way [`parse_status`] is
/// defined on the running/stopped line: it is the field every decision is made
/// from, and a reading without one is an output shape we do not recognise
/// rather than a licence in an unknown state. Owner and key are read where they
/// appear and left empty where they do not — a display can say so, and neither
/// is worth failing the whole read over.
fn parse_license(stdout: &str) -> Option<License> {
    let mut license = License::default();
    let mut saw_status = false;

    for line in stdout.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("License owner:") {
            license.owner = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("License key:") {
            license.key = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("License status:") {
            license.status = value.trim().to_owned();
            saw_status = !license.status.is_empty();
        }
    }

    saw_status.then_some(license)
}

/// What a `check-update` header line opens with.
const CHECKING: &str = "Checking ";
/// And what it closes with. Three full stops, as measured — not an ellipsis.
const UPDATES: &str = " updates...";

/// Parse `adguard-cli check-update`.
///
/// The output is pairs — a `Checking <name> updates...` line, then a verdict on
/// the next — and three things about that shape decide how this is written
/// (contract §14):
///
/// **The verdict is meaningless without its header**, because `Failed to update
/// filters` is what both filter components print. So a verdict is never held
/// anywhere but beside the name it answers.
///
/// **A first run prints `Created data directory <path>` before the first
/// header.** Everything ahead of the first header is skipped rather than
/// treated as a verdict with nothing to belong to, which is what would make a
/// first run — the very run a new install performs — unparseable.
///
/// **A header the CLI never answered is kept, not dropped.** Truncated output
/// means a component whose outcome is unknown, and a report that silently
/// listed five components where AdGuard named six would be a UI claiming
/// completeness it does not have. It arrives as [`Verdict::Unrecognised`]
/// carrying an empty sentence.
///
/// `None` only when not a single header was seen, which is the [§3] rule about
/// unrecognised shapes: fail loudly, rather than hand back an empty report that
/// reads exactly like a run in which nothing needed doing.
///
/// [§3]: ../../../docs/cli-contract.md
fn parse_update_report(stdout: &str) -> Option<UpdateReport> {
    let mut components: Vec<ComponentUpdate> = Vec::new();
    let mut pending: Option<UpdatePart> = None;

    let unanswered = |part: UpdatePart| ComponentUpdate {
        part,
        verdict: Verdict::Unrecognised,
        said: String::new(),
    };

    for line in stdout.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        match header(line) {
            Some(part) => {
                // Two headers in a row: the previous component was announced and
                // never answered.
                if let Some(previous) = pending.replace(part) {
                    components.push(unanswered(previous));
                }
            }
            // Only the line immediately after a header is a verdict. Anything
            // else — the created-directory line, or noise between pairs — falls
            // through here with nothing pending and is ignored.
            None => {
                if let Some(part) = pending.take() {
                    components.push(ComponentUpdate {
                        part,
                        verdict: Verdict::classify(line),
                        said: line.to_owned(),
                    });
                }
            }
        }
    }

    if let Some(part) = pending.take() {
        components.push(unanswered(part));
    }

    (!components.is_empty()).then_some(UpdateReport { components })
}

/// The component a `Checking … updates...` line announces, or `None` for any
/// other line.
///
/// Both ends are required. Matching the prefix alone would read the CLI's own
/// `Checking for updates` prose — were it ever to print any — as a component
/// named `for`.
fn header(line: &str) -> Option<UpdatePart> {
    line.strip_prefix(CHECKING)
        .and_then(|rest| rest.strip_suffix(UPDATES))
        .map(UpdatePart::from_header)
}

/// Keep the shape of some output and drop every value in it.
///
/// For the one failure path that would otherwise quote `license` output
/// verbatim into a subtitle. `Label: <hidden>` per line says which fields were
/// there and which the parser did not recognise, which is what a rewording
/// looks like; a line with no label at all is dropped whole, since there is no
/// way to tell a heading from a value.
///
/// Joined onto one line for the same reason [`licence_complaint`] drops the
/// usage dump: the destination is an `AdwActionRow` subtitle.
fn redact_values(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(':') {
            Some((label, _)) => format!("{}: <hidden>", label.trim()),
            None => "<hidden>".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The scheme an activation link must have before it is handed to a launcher.
const HTTPS: &str = "https://";

/// Pull the log-in URL out of `activate`'s no-TTY message.
///
/// The URL sits mid-sentence — *"Please visit &lt;url&gt; to log in, then run
/// …"* — so there is no prefix to strip and no field to take. It is found by
/// scheme instead, as the first whitespace-delimited token that begins one,
/// with sentence punctuation trimmed off both ends. The measured link carries a
/// query string (`?action=activate&app=cli&appid=…`) and no whitespace, so
/// splitting on whitespace keeps it whole; the trimming is for the shapes this
/// CLI uses elsewhere, which wrap things it wants you to type in backticks.
///
/// **`https://` only, and that is a security bar rather than tidiness.**
/// Whatever comes back from here is handed to `gtk::UriLauncher`, which hands it
/// to the desktop's registered handler for whatever scheme it names — so the
/// scheme is the part that must not be taken from parsed text. Everything else
/// about the URL is AdGuard's business; this is ours.
fn activation_url(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .map(|token| {
            token
                .trim_start_matches(['(', '[', '<', '"', '\'', '`'])
                .trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'', '`'])
        })
        // A bare scheme is not a link. Nothing measured produces one; the check
        // costs a clause and the alternative is launching "https://".
        .find(|token| token.starts_with(HTTPS) && token.len() > HTTPS.len())
        .map(str::to_owned)
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

    /// The tail of a real failing `start`, captured against an install holding
    /// a wedged leftover process. Exit 0, stderr empty, 60 s in — the log block
    /// above the conclusion is what the ANSI stripper leaves behind, and it is
    /// kept here because the recogniser has to see past it.
    const START_FAILED_OUTPUT: &str =
        "01.08.2026 10:43:14.501442 ERROR [119992] SERVICE_FACADE start_internal: Failed\n\
         01.08.2026 10:43:14.501476 INFO  [119992] AdGuardCli ~AdGuardCli: Stop CLI App\n\
         Failed to start proxy server: An unknown error has occurred\n";

    /// The tail of a real successful `start` — 1.1 s, same exit code.
    const START_OK_OUTPUT: &str = "The AdGuard proxy server is running\n\
         HTTP proxy is listening on 127.0.0.1:3129\n\
         Manual DNS proxy is disabled\n\
         You can check the status of the proxy server by running `adguard-cli status`\n";

    #[test]
    fn recognises_a_start_that_failed() {
        assert_eq!(
            start_refusal(START_FAILED_OUTPUT).as_deref(),
            Some("Failed to start proxy server: An unknown error has occurred")
        );
    }

    /// The whole point of defining *failure* positively: anything else, known
    /// or not, stays a success and leaves the verdict to the status re-read.
    #[test]
    fn a_successful_start_is_not_a_refusal() {
        assert_eq!(start_refusal(START_OK_OUTPUT), None);
        assert_eq!(start_refusal(""), None);
        assert_eq!(start_refusal("Some wording nobody has measured yet"), None);
    }

    /// The `SERVICE_FACADE start_internal: Failed` line sits in the log block of
    /// every failing start and says the same thing far less usefully. Anchoring
    /// at the line start is what keeps it from being the sentence we show.
    #[test]
    fn the_log_block_is_not_mistaken_for_the_conclusion() {
        let logs_only = "01.08.2026 10:43:14 ERROR SERVICE_FACADE start_internal: Failed\n";
        assert_eq!(start_refusal(logs_only), None);
    }

    /// A start prints its conclusion last, and `Ok` carries that line rather
    /// than the two kilobytes of log above it.
    #[test]
    fn success_reports_only_the_last_line() {
        assert_eq!(
            last_line(START_OK_OUTPUT).as_deref(),
            Some("You can check the status of the proxy server by running `adguard-cli status`")
        );
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

    // ---- `filters install`, all captured from v1.4.13 ----

    const INSTALLED: &str =
        "Filter [Title: Online Malicious URL Blocklist] from URL: \
         https://filters.adtidy.org/extension/chromium/filters/208.txt installed\n";

    /// A list with no `! Title:` header. The CLI names the URL as the title
    /// here and then stores the title as `''` — see `Filter::display_name`.
    const INSTALLED_UNTITLED: &str =
        "Filter [Title: file:///tmp/untitled.txt] from URL: file:///tmp/untitled.txt installed\n";

    /// Re-installing a URL already present. The `filters list` table that
    /// follows is the one contract §6 says not to parse.
    const ALREADY_INSTALLED: &str = "Filter with the specified URL already exists:\n\
         \x20   |           ID | Title                                   Last update        \n\
         [x] |       -10001 | Claude Probe List [non-trusted]         2026-07-31 00:51:48\n";

    /// The single sentence every fetch failure collapses into — a 404, a
    /// refused connection, an unresolvable host, a missing file, or a string
    /// that was never a URL.
    const INSTALL_FAILED: &str =
        "Failed to install the filter from URL: https://no-such-host-probe.invalid/list.txt\n";

    #[test]
    fn recognises_the_install_confirmation() {
        assert!(confirms(INSTALLED, "installed"));
        assert!(confirms(INSTALLED_UNTITLED, "installed"));
    }

    /// Both refusals exit 0, so reading either as success would leave the page
    /// claiming a list it never installed.
    #[test]
    fn install_refusals_are_not_confirmations() {
        for output in [ALREADY_INSTALLED, INSTALL_FAILED, "", "\n \n"] {
            assert!(!confirms(output, "installed"), "{output:?} read as installed");
        }
    }

    /// `Failed to install the filter from URL: …` ends with whatever was
    /// passed, so a URL whose last path segment is the confirmation verb puts
    /// the word "installed" at the end of a *failure* line. The `Filter [`
    /// prefix is what keeps them apart.
    #[test]
    fn a_url_ending_in_the_verb_does_not_fake_a_confirmation() {
        let failed = "Failed to install the filter from URL: https://example.org/installed\n";
        assert!(!confirms(failed, "installed"));
    }

    /// The duplicate begins `Filter with`, one character class away from the
    /// `Filter [` that means success.
    #[test]
    fn install_refusal_message_drops_the_filters_list_table() {
        assert_eq!(
            first_line(ALREADY_INSTALLED).as_deref(),
            Some("Filter with the specified URL already exists:")
        );
        assert_eq!(
            first_line(INSTALL_FAILED).as_deref(),
            Some("Failed to install the filter from URL: https://no-such-host-probe.invalid/list.txt")
        );
    }

    // ---- `filters set-trusted`, all captured from v1.4.13 ----

    /// The whole of a successful invocation's output. One line, and the only
    /// filter confirmation in the CLI that is not `Filter [<something>] <verb>`.
    const TRUST_OK: &str = "Filter with ID: -10001 successfully updated trust\n";

    /// An id no row has. The shape a stale page sends: the user presses the
    /// control on a list another window already removed.
    const TRUST_NOT_FOUND: &str =
        "Failed to update trust filter with ID: -99999: Filter not found\n";

    /// AdGuard refusing what `Filter::supports_trust` also refuses. Reachable
    /// only through a bug on our side, and pinned so that bug is a toast rather
    /// than a silent success.
    const TRUST_NOT_CUSTOM: &str =
        "Failed to update trust filter with ID: 2: Filter not custom\n";

    #[test]
    fn recognises_the_trust_confirmation() {
        assert!(confirms_trust(TRUST_OK));
    }

    /// Both refusals exit 0, exactly like the successes they sit beside.
    #[test]
    fn trust_refusals_are_not_confirmations() {
        for output in [TRUST_NOT_FOUND, TRUST_NOT_CUSTOM, "", "\n \n"] {
            assert!(!confirms_trust(output), "{output:?} read as a trust change");
        }
    }

    /// The house matcher cannot see this command's success, which is the whole
    /// reason `confirms_trust` exists. If AdGuard ever moves `set-trusted` onto
    /// the `Filter [` shape this test fails, and that is the point — the change
    /// would be an opportunity to delete a function, not a silent equivalence.
    #[test]
    fn the_house_matcher_does_not_recognise_a_trust_confirmation() {
        for verb in ["trust", "updated trust", "installed", "enabled"] {
            assert!(!confirms(TRUST_OK, verb), "confirms(.., {verb:?}) matched");
        }
    }

    /// A custom list is titled by its author, or by the URL when it has no
    /// `! Title:` header, and neither is checked by anything. A title that is
    /// itself a copy of the success line must not make a refusal read as one —
    /// which is why the match is anchored at both ends of a single line rather
    /// than being a `contains`.
    #[test]
    fn a_title_quoting_the_success_line_does_not_fake_a_confirmation() {
        let hostile =
            "Failed to update trust filter with ID: -10001 successfully updated trust: Filter not found\n";
        assert!(!confirms_trust(hostile));
    }

    /// The trust refusals are one-liners with no table behind them, so what the
    /// user is shown is the CLI's own sentence entire.
    #[test]
    fn trust_refusal_messages_are_shown_whole() {
        assert_eq!(
            first_line(TRUST_NOT_FOUND).as_deref(),
            Some("Failed to update trust filter with ID: -99999: Filter not found")
        );
        assert_eq!(
            first_line(TRUST_NOT_CUSTOM).as_deref(),
            Some("Failed to update trust filter with ID: 2: Filter not custom")
        );
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

    /// The third thing exit 1 can mean, after "our command line" and "no
    /// licence": the program ran, refused, and said so on **stdout**.
    ///
    /// Measured, and reachable from this app's own startup — `status` and the
    /// licence read racing to initialise a data directory that has never been
    /// used leave one of them exactly here. Reported as `BadInvocation` it read
    /// as *"adguard-cli rejected `license` (exit 1): "* with nothing after the
    /// colon: our bug, according to us, with no evidence and no advice.
    #[test]
    fn a_failure_that_explains_itself_on_stdout_is_not_our_bug() {
        let err = cli_for("/bin/sh")
            .run_within(
                &["-c", "echo 'Filter manager initialization failed'; exit 1"],
                Duration::from_secs(10),
            )
            .expect_err("exit 1 is a failure");

        assert!(
            matches!(&err, Error::Refused { message } if message == "Filter manager initialization failed"),
            "expected the CLI's own sentence, got {err:?}"
        );
        // What the user reads is the CLI's line, not a claim about our
        // arguments.
        assert_eq!(err.to_string(), "Filter manager initialization failed");
    }

    /// The whole point of the annoyance path: the answer has to arrive on the
    /// child's stdin, and the pipe has to close behind it so a second read
    /// meets EOF instead of waiting.
    #[test]
    fn an_answer_reaches_the_child_and_the_pipe_closes_behind_it() {
        let out = cli_for("/bin/sh")
            .run_answering(
                &["-c", "read first; echo \"got [$first]\"; read second || echo 'then EOF'"],
                Duration::from_secs(10),
                Some(ANNOYANCE_ACCEPT),
            )
            .expect("the shell should exit 0");

        assert_eq!(out.stdout.trim(), "got [yes]\nthen EOF");
    }

    /// With no answer offered, stdin is closed exactly as it always was. The
    /// guarantee on [`Cli::run`] is not weakened by the parameter existing.
    #[test]
    fn no_answer_still_means_closed_stdin() {
        let out = cli_for("/bin/sh")
            .run_answering(
                &["-c", "read line || echo 'EOF immediately'"],
                Duration::from_secs(10),
                None,
            )
            .expect("the shell should exit 0");

        assert_eq!(out.stdout.trim(), "EOF immediately");
    }

    /// `filters add` on an annoyance list prints its own success line **first**
    /// and refuses four lines later, so reading in the obvious order reports a
    /// subscription the user did not ask for as though the switch had worked.
    ///
    /// The transcript is the measured one, v1.4.13, exit 0, all on stdout.
    #[test]
    fn an_added_annoyance_list_that_was_not_enabled_is_a_refusal() {
        let err = cli_for("/bin/sh")
            .run_within(
                &[
                    "-c",
                    "echo 'Filter [Title: AdGuard Cookie Notices filter] added'; \
                     echo 'Please read carefully before enabling Annoyance filters'; \
                     echo \"Annoyance filters won't be enabled due to user's choice\"",
                ],
                Duration::from_secs(10),
            )
            .map_err(|err| err.to_string())
            .and_then(|out| {
                // `filter_action`'s own order of checks, against output a real
                // `add` produced.
                assert!(confirms(&out.stdout, FilterAction::Add.confirmation()));
                if declined_annoyances(&out.stdout) {
                    Err("declined".to_owned())
                } else {
                    Ok(())
                }
            });

        assert_eq!(err, Err("declined".to_owned()), "the refusal must outrank the confirmation");
    }

    /// And the refusal line must not be matched loosely: an ordinary filter
    /// command that happens to mention annoyances is still a success.
    #[test]
    fn only_the_measured_refusal_line_counts() {
        assert!(declined_annoyances(
            "Filter [Title: X] added\nAnnoyance filters won't be enabled due to user's choice\n"
        ));
        assert!(!declined_annoyances(
            "Filter [Title: AdGuard Other Annoyances filter] enabled\n"
        ));
    }

    /// `activate` opens with a menu prompt nobody answered, so a failure of
    /// *that* command must not be reported to the user as "How do you want to
    /// activate AdGuard CLI?".
    #[test]
    fn a_dangling_prompt_is_not_the_failure_message() {
        let err = cli_for("/bin/sh")
            .run_within(
                &[
                    "-c",
                    "echo 'How do you want to activate AdGuard CLI?'; \
                     echo 'Filter manager initialization failed'; exit 1",
                ],
                Duration::from_secs(10),
            )
            .expect_err("exit 1 is a failure");

        assert_eq!(err.to_string(), "Filter manager initialization failed");
    }

    /// The discriminator is the stream, so a message on stderr must stay our
    /// bug even when stdout also has something in it.
    #[test]
    fn stderr_still_wins_when_both_streams_speak() {
        let err = cli_for("/bin/sh")
            .run_within(
                &["-c", "echo noise; echo '<value> is required' >&2; exit 1"],
                Duration::from_secs(10),
            )
            .expect_err("exit 1 is a failure");
        assert!(
            matches!(err, Error::BadInvocation { .. }),
            "expected BadInvocation, got {err:?}"
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

    // ---- `license` and `activate`, both captured from v1.4.13 ----

    /// The real shape, with a key of the right length and an owner that is not
    /// anybody's. Measured: three lines, exit 0, nothing on stderr, and — alone
    /// among this CLI's output — no ANSI escapes at all.
    const LICENSE_READING: &str = "License owner: someone@example.com\n\
         License key: ABCDEFGH12345678\n\
         License status: APP_ACTIVE\n";

    #[test]
    fn parses_the_licence_reading() {
        let license = parse_license(LICENSE_READING).expect("should parse");
        assert_eq!(license.owner, "someone@example.com");
        assert_eq!(license.status, License::ACTIVE);
        assert!(license.is_active());
        assert_eq!(license.masked_key(), "••••••••••••5678");
    }

    /// The status line is what every decision is made from, so a reading
    /// without one is an unrecognised shape — not a licence in an unknown
    /// state, and certainly not an inactive one.
    #[test]
    fn a_reading_with_no_status_is_rejected() {
        for output in [
            "License owner: someone@example.com\nLicense key: ABCDEFGH12345678\n",
            "License status:\n",
            "something else entirely",
            "",
        ] {
            assert!(
                parse_license(output).is_none(),
                "{output:?} read as a licence"
            );
        }
    }

    /// The other two fields are worth having and not worth failing over: a
    /// reading that names only the status still tells the user where they
    /// stand.
    #[test]
    fn owner_and_key_are_optional_around_the_status() {
        let license = parse_license("License status: APP_ACTIVE\n").expect("should parse");
        assert!(license.is_active());
        assert!(license.owner.is_empty());
        assert!(license.masked_key().is_empty());
    }

    /// The failure path for a reading we cannot parse must not do what
    /// [`Error::Unparseable`] does everywhere else and quote the output: that
    /// message is destined for a row subtitle, and this output holds the key.
    #[test]
    fn an_unparseable_reading_is_not_quoted_in_full() {
        let redacted = redact_values(LICENSE_READING);

        assert!(!redacted.contains("ABCDEFGH12345678"), "{redacted}");
        assert!(!redacted.contains("someone@example.com"), "{redacted}");
        // Enough shape left to recognise a rewording from a bug report.
        assert!(redacted.contains("License key: <hidden>"), "{redacted}");
        assert_eq!(redacted.lines().count(), 1, "must fit one subtitle");
    }

    /// A line with no label could be a heading or a value, and there is no way
    /// to tell — so it goes entirely.
    #[test]
    fn a_line_with_no_label_is_dropped_whole() {
        assert_eq!(redact_values("ABCDEFGH12345678\n"), "<hidden>");
    }

    /// The redactor is only worth anything if `license` actually calls it, and
    /// the two tests above would pass with that call deleted — they exercise
    /// the helper, not the wiring.
    ///
    /// So this one goes through [`Cli::license`] itself. `echo` stands in for
    /// the CLI and prints back the one argument it is given, which is an
    /// unrecognisable reading: the error must carry the *shape* of what came
    /// back and none of it. Remove the `redact_values` call and the message
    /// reads `…: license` instead.
    #[test]
    fn license_redacts_what_it_could_not_parse() {
        let err = cli_for("/bin/echo")
            .license()
            .expect_err("`license` is not a licence reading");

        assert!(
            matches!(&err, Error::Unparseable { output, .. } if output == "<hidden>"),
            "the reading reached the error unredacted: {err:?}"
        );
    }

    /// Exactly what an unlicensed sandbox printed, with the install's own id
    /// swapped out. The first line is a menu prompt that was never asked; the
    /// URL sits mid-sentence in the second.
    const ACTIVATE_NO_TTY: &str = "How do you want to activate AdGuard CLI?\n\
         Warning: No TTY for user input. Please visit \
         https://link.adtidy.org/forward.html?action=activate&app=cli\
         &appid=0123456789abcdef0123456789abcdef to log in, then run \
         `adguard-cli activate` again to complete activation.\n";

    #[test]
    fn finds_the_activation_url_in_the_measured_message() {
        let url = activation_url(ACTIVATE_NO_TTY).expect("should find a link");
        assert_eq!(
            url,
            "https://link.adtidy.org/forward.html?action=activate&app=cli\
             &appid=0123456789abcdef0123456789abcdef"
        );
    }

    /// The link ends in a query string, so anything that stopped at `&` or `=`
    /// would open a page that could not identify this install.
    #[test]
    fn the_query_string_is_part_of_the_url() {
        let url = activation_url(ACTIVATE_NO_TTY).unwrap();
        assert!(url.contains("appid="), "{url}");
        assert!(url.contains("&app=cli"), "{url}");
    }

    /// It is found mid-sentence, so the sentence must not come with it.
    #[test]
    fn sentence_punctuation_is_not_part_of_the_url() {
        for line in [
            "Please visit https://example.com/activate.",
            "Please visit https://example.com/activate,",
            "Please visit `https://example.com/activate`",
            "Please visit (https://example.com/activate)",
        ] {
            assert_eq!(
                activation_url(line).as_deref(),
                Some("https://example.com/activate"),
                "{line:?}"
            );
        }
    }

    /// This string is handed to the desktop's handler for whatever scheme it
    /// names, so the scheme is the one part that may not come from parsed text.
    #[test]
    fn only_an_https_link_is_offered_to_a_launcher() {
        for output in [
            "Please visit http://link.adtidy.org/activate to log in",
            "Please visit file:///etc/passwd to log in",
            "Please visit javascript:alert(1) to log in",
            "Please visit https:// to log in",
            "How do you want to activate AdGuard CLI?",
            "",
        ] {
            assert!(
                activation_url(output).is_none(),
                "{output:?} produced a link to launch"
            );
        }
    }

    /// With no link there is nothing to open, and the CLI's conclusion is the
    /// last line rather than the first — the first is the prompt.
    #[test]
    fn without_a_link_the_conclusion_is_the_last_line() {
        assert_eq!(
            last_line(ACTIVATE_NO_TTY).as_deref().map(|line| line
                .starts_with("Warning: No TTY")),
            Some(true)
        );
        assert_eq!(
            last_line("How do you want to activate AdGuard CLI?\nAlready activated\n").as_deref(),
            Some("Already activated")
        );
        assert_eq!(last_line("\n \n"), None);
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

    // ---- check-update, contract §14 ----
    //
    // All four fixtures are real captures from 9 August 2026, v1.4.13, and
    // carry no ANSI escapes because this command emits none — which is why
    // they are written as plain text where `RUNNING` above is not.

    /// A run in which nothing needed doing, except the two components that say
    /// `Updated` on every run of a working install.
    const CLEAN: &str = "Checking filters updates...\n\
         Up to date\n\
         Checking DNS filters updates...\n\
         Up to date\n\
         Checking userscripts updates...\n\
         Up to date\n\
         Checking SafebrowsingV2 updates...\n\
         Updated\n\
         Checking CRLite updates...\n\
         Updated\n\
         Checking app updates...\n\
         Up to date\n";

    /// The **DNS** filters failed here. Exit was 0 and stderr was empty.
    const DNS_FAILED: &str = "Checking filters updates...\n\
         Up to date\n\
         Checking DNS filters updates...\n\
         Failed to update filters\n\
         Checking userscripts updates...\n\
         Up to date\n\
         Checking SafebrowsingV2 updates...\n\
         Updated\n\
         Checking CRLite updates...\n\
         Updated\n\
         Checking app updates...\n\
         Up to date\n";

    /// The **HTTP** filters failed here — and the sentence is the same one.
    const FILTERS_FAILED: &str = "Checking filters updates...\n\
         Failed to update filters\n\
         Checking DNS filters updates...\n\
         Up to date\n\
         Checking userscripts updates...\n\
         Up to date\n\
         Checking SafebrowsingV2 updates...\n\
         Updated\n\
         Checking CRLite updates...\n\
         Updated\n\
         Checking app updates...\n\
         Up to date\n";

    /// A first run against a virgin `$XDG_DATA_HOME`, opening with a line that
    /// is not part of any pair.
    const FIRST_RUN: &str = "Created data directory /tmp/sandbox/adguard-cli\n\
         Checking filters updates...\n\
         1 filter(s) updated\n\
         Checking DNS filters updates...\n\
         Failed to update filters\n\
         Checking userscripts updates...\n\
         Up to date\n\
         Checking SafebrowsingV2 updates...\n\
         Up to date\n\
         Checking CRLite updates...\n\
         Up to date\n\
         Checking app updates...\n\
         Up to date\n";

    fn report(stdout: &str) -> UpdateReport {
        parse_update_report(stdout).expect("should parse")
    }

    #[test]
    fn parses_a_clean_check_update() {
        let report = report(CLEAN);
        let parts: Vec<_> = report.components.iter().map(|c| c.part.clone()).collect();
        assert_eq!(
            parts,
            vec![
                UpdatePart::Filters,
                UpdatePart::DnsFilters,
                UpdatePart::Userscripts,
                UpdatePart::SafeBrowsing,
                UpdatePart::CrLite,
                UpdatePart::App,
            ],
            "the six components, in the order AdGuard listed them"
        );
        assert!(report.failures().next().is_none());
        assert_eq!(report.app_notice(), None, "an up-to-date app has nothing to say");
        assert!(!report.changed(&UpdatePart::Filters));
        assert!(report.changed(&UpdatePart::SafeBrowsing));
    }

    /// **The trap in contract §14.** `Failed to update filters` is printed for
    /// a failure of either filter component and names neither, so the header is
    /// the only thing that says which one. Two real captures differing in
    /// nothing but which header the identical sentence sits under.
    #[test]
    fn check_update_pairs_each_verdict_with_its_header() {
        let dns = report(DNS_FAILED);
        let http = report(FILTERS_FAILED);

        let said = "Failed to update filters";
        assert_eq!(dns.part(&UpdatePart::DnsFilters).unwrap().said, said);
        assert_eq!(http.part(&UpdatePart::Filters).unwrap().said, said);

        assert_eq!(dns.part(&UpdatePart::DnsFilters).unwrap().verdict, Verdict::Failed);
        assert_eq!(dns.part(&UpdatePart::Filters).unwrap().verdict, Verdict::UpToDate);
        assert_eq!(http.part(&UpdatePart::Filters).unwrap().verdict, Verdict::Failed);
        assert_eq!(http.part(&UpdatePart::DnsFilters).unwrap().verdict, Verdict::UpToDate);
    }

    /// A first run — the one every new install performs — opens with a line
    /// that belongs to no pair. Reading it as a verdict would shift every
    /// component onto the wrong header.
    #[test]
    fn check_update_skips_the_created_directory_line() {
        let report = report(FIRST_RUN);
        assert_eq!(report.components.len(), 6);
        assert_eq!(report.components[0].part, UpdatePart::Filters);
        assert_eq!(report.components[0].said, "1 filter(s) updated");
        assert!(report.changed(&UpdatePart::Filters), "a count is a change");
        assert_eq!(report.part(&UpdatePart::DnsFilters).unwrap().verdict, Verdict::Failed);
    }

    /// Every one of the fourteen measured runs exited 0, including the five that
    /// failed a component. Nothing in the parse may consult the status, and a
    /// caller must be able to see the failure that exit 0 concealed.
    #[test]
    fn a_failed_component_survives_a_successful_exit() {
        let report = report(DNS_FAILED);
        let failed: Vec<_> = report.failures().map(|c| c.part.clone()).collect();
        assert_eq!(failed, vec![UpdatePart::DnsFilters]);
    }

    /// Of the two ways to misread a reworded sentence, only one loses something
    /// the user needed. A failure must never be classified as a change — not
    /// even one that ends in the word the change rule matches.
    #[test]
    fn failure_is_classified_before_success() {
        assert_eq!(Verdict::classify("Failed to update filters"), Verdict::Failed);
        assert_eq!(
            Verdict::classify("Failed after 3 filter(s) updated"),
            Verdict::Failed,
            "the failure rule has to win, or a bad run reads as a good one"
        );
    }

    #[test]
    fn classifies_every_measured_verdict() {
        assert_eq!(Verdict::classify("Up to date"), Verdict::UpToDate);
        assert_eq!(Verdict::classify("Updated"), Verdict::Changed);
        assert_eq!(Verdict::classify("1 filter(s) updated"), Verdict::Changed);
        assert_eq!(Verdict::classify("1 DNS filter(s) updated"), Verdict::Changed);
        assert_eq!(Verdict::classify("Failed to update filters"), Verdict::Failed);
    }

    /// What the app line says when an update exists has never been observed
    /// (contract §14), so anything that is not `Up to date` has to reach the
    /// user as AdGuard's own words rather than as an interpretation of them.
    #[test]
    fn an_unmeasured_app_verdict_is_repeated_verbatim() {
        let stdout = "Checking app updates...\nA new version 1.5.0 is available\n";
        let report = report(stdout);
        assert_eq!(report.part(&UpdatePart::App).unwrap().verdict, Verdict::Unrecognised);
        assert_eq!(report.app_notice(), Some("A new version 1.5.0 is available"));
    }

    /// A failed app *check* is not a release.
    ///
    /// It is already reported by `failures()`, so letting it through
    /// `app_notice` too would show one event twice — the second time as a
    /// notice recommending `adguard-cli update`, which is advice derived from a
    /// check that did not finish.
    #[test]
    fn a_failed_app_check_is_a_failure_and_not_a_notice() {
        let report = report("Checking app updates...\nFailed to check for updates\n");
        assert_eq!(report.part(&UpdatePart::App).unwrap().verdict, Verdict::Failed);
        assert_eq!(report.app_notice(), None, "a failed check is not news of a release");
        assert_eq!(report.failures().count(), 1, "and it is not silently swallowed either");
    }

    /// An announced-but-unanswered app header would otherwise reach the page as
    /// `Some("")` — a notice with nothing in it, saying only that something is
    /// wrong. `failures()` does not catch this one, so the guard has to.
    #[test]
    fn an_empty_app_sentence_is_not_a_notice() {
        let report = report("Checking app updates...\n");
        assert_eq!(report.part(&UpdatePart::App).unwrap().verdict, Verdict::Unrecognised);
        assert_eq!(report.app_notice(), None);
    }

    /// Truncated output means a component whose outcome is unknown. Dropping it
    /// would leave a report that lists five where AdGuard named six, and reads
    /// as complete.
    #[test]
    fn an_unanswered_header_is_kept_as_unrecognised() {
        let report = report("Checking filters updates...\nUp to date\nChecking app updates...\n");
        assert_eq!(report.components.len(), 2);
        let app = report.part(&UpdatePart::App).unwrap();
        assert_eq!(app.verdict, Verdict::Unrecognised);
        assert!(app.said.is_empty());
    }

    /// A component this build has never heard of is shown under the CLI's own
    /// name rather than dropped from a report the user reads as complete.
    #[test]
    fn an_unknown_component_is_carried_rather_than_dropped() {
        let report = report("Checking quantum updates...\nUp to date\n");
        assert_eq!(report.components[0].part, UpdatePart::Other("quantum".to_owned()));
        assert_eq!(report.components[0].part.title(), "quantum");
        assert_eq!(report.components[0].verdict, Verdict::UpToDate);
    }

    /// An output shape with no header at all is a failure, not an empty report
    /// — which would render exactly like a run in which nothing needed doing.
    #[test]
    fn unrecognised_check_update_output_is_rejected() {
        assert!(parse_update_report("").is_none());
        assert!(parse_update_report("something else entirely").is_none());
        assert!(
            parse_update_report("Created data directory /tmp/x/adguard-cli").is_none(),
            "the skipped line is not a report on its own"
        );
    }

    /// Both ends of the header are required, so ordinary prose beginning
    /// `Checking ` cannot become a component named after its second word.
    #[test]
    fn a_header_needs_both_of_its_ends() {
        assert_eq!(header("Checking filters updates..."), Some(UpdatePart::Filters));
        assert_eq!(header("Checking for updates"), None);
        assert_eq!(header("Up to date"), None);
    }

    /// The catalogue re-reads key on this, and a page is re-read because
    /// something was said to have moved — never because nothing was said.
    #[test]
    fn a_component_the_cli_did_not_mention_did_not_change() {
        let report = report("Checking app updates...\nUp to date\n");
        assert!(!report.changed(&UpdatePart::Filters));
        assert!(!report.changed(&UpdatePart::DnsFilters));
        assert_eq!(report.part(&UpdatePart::Filters), None);
    }

    // --- userscripts (contract §15) ---

    /// The four measured confirmations, each matched by its own phrase.
    #[test]
    fn userscript_confirmations_are_recognised() {
        for (output, phrase) in [
            ("Userscript 'AdGuard Extra' enabled successfully", "enabled successfully"),
            ("Userscript 'AdGuard Extra' disabled successfully", "disabled successfully"),
            ("Userscript 'AdGuard Extra' removed successfully", "removed successfully"),
            ("Userscript installed and enabled successfully", "installed and enabled successfully"),
        ] {
            assert!(confirms_userscript(output, phrase), "{output:?} did not confirm {phrase:?}");
        }
    }

    /// "enabled" is a substring of "disabled", so a matcher built on `contains`
    /// would report every successful *enable* as a successful *disable* — and
    /// the UI would settle the switch in the wrong position while believing the
    /// CLI agreed with it.
    #[test]
    fn an_enable_does_not_confirm_a_disable() {
        let enabled = "Userscript 'AdGuard Extra' enabled successfully";
        assert!(!confirms_userscript(enabled, "disabled successfully"));

        let disabled = "Userscript 'AdGuard Extra' disabled successfully";
        assert!(confirms_userscript(disabled, "disabled successfully"));
    }

    /// The measured no-op and the measured miss are refusals, not successes.
    #[test]
    fn the_no_op_and_the_miss_confirm_nothing() {
        for output in [
            "Userscript 'Hello Sandbox' is not enabled",
            "No userscripts matching 'no-such-script'",
            "Failed to install userscript",
        ] {
            for phrase in ["enabled successfully", "disabled successfully", "removed successfully"] {
                assert!(!confirms_userscript(output, phrase), "{output:?} confirmed {phrase:?}");
            }
        }
    }

    /// `install` does not name the script it installed, so the matcher must not
    /// require a quoted name.
    #[test]
    fn the_install_confirmation_names_no_script() {
        assert!(confirms_userscript(
            "Userscript installed and enabled successfully",
            "installed and enabled successfully"
        ));
    }

    /// The ambiguous refusal, with the candidates the caller needs to name what
    /// collided.
    #[test]
    fn the_ambiguous_refusal_yields_its_candidates() {
        let output = "Multiple userscripts match 'hello'. Please specify more precisely:\n                        - Hello Sandbox (ID: hello)\n  - Hello World (ID: hello-world)";
        let candidates = ambiguous_userscripts(output).expect("recognised as ambiguous");
        assert_eq!(
            candidates,
            ["Hello Sandbox (ID: hello)", "Hello World (ID: hello-world)"]
        );
    }

    /// Everything that is not that refusal answers `None`, so an ordinary
    /// success is never mistaken for one.
    #[test]
    fn other_output_is_not_ambiguous() {
        for output in [
            "Userscript 'AdGuard Extra' enabled successfully",
            "No userscripts matching 'x'",
            "",
        ] {
            assert!(ambiguous_userscripts(output).is_none(), "{output:?}");
        }
    }

    /// The header alone still means the command was refused. Reporting `None`
    /// for want of a candidate list would let the caller fall through to the
    /// success check on a command that did nothing.
    #[test]
    fn the_header_alone_is_still_a_refusal() {
        let candidates =
            ambiguous_userscripts("Multiple userscripts match 'x'. Please specify more precisely:")
                .expect("still a refusal");
        assert!(candidates.is_empty());
    }

    /// The empty-string wildcard never reaches the CLI. Measured: on a
    /// one-script install `disable ""` switches that script off and reports
    /// success, so this guard is the only thing between a blank name and the
    /// user's only userscript.
    #[test]
    fn a_blank_userscript_name_is_refused_before_spawning() {
        let Ok(cli) = Cli::discover() else { return };
        for name in ["", "   ", "\t"] {
            assert!(
                matches!(cli.userscripts_disable(name), Err(Error::UnnamedUserscript)),
                "{name:?} was not refused"
            );
            assert!(matches!(cli.userscripts_enable(name), Err(Error::UnnamedUserscript)));
            assert!(matches!(cli.userscripts_remove(name), Err(Error::UnnamedUserscript)));
            assert!(matches!(cli.userscripts_install(name), Err(Error::UnnamedUserscript)));
        }
    }
}
