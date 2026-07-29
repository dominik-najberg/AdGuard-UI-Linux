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

use std::path::PathBuf;
use std::process::Command;

use crate::model::ProxyStatus;
use crate::paths;

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
}

impl Cli {
    /// Locate the CLI. Returns [`Error::BinaryNotFound`] when AdGuard CLI is
    /// not installed, so the UI can say so plainly instead of crashing.
    pub fn discover() -> Result<Self, Error> {
        paths::cli_binary()
            .map(|binary| Self { binary })
            .ok_or(Error::BinaryNotFound)
    }

    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }

    /// Run a subcommand and return its stripped output.
    ///
    /// TODO(v1): apply a timeout. Not needed for the fast local commands used
    /// here (~10-30 ms each), but mandatory before wiring up the network ones
    /// (`check-update`, `filters update`, `update`), which can hang.
    pub fn run(&self, args: &[&str]) -> Result<Output, Error> {
        let raw = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|source| Error::Spawn {
                binary: self.binary.display().to_string(),
                source,
            })?;

        let stdout = strip_ansi(&raw.stdout);
        let stderr = strip_ansi(&raw.stderr);

        if !raw.status.success() {
            return Err(Error::BadInvocation {
                args: args.join(" "),
                code: raw.status.code().unwrap_or(-1),
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
}

/// Strip ANSI escapes from raw process output and lossily decode as UTF-8.
fn strip_ansi(raw: &[u8]) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(raw)).into_owned()
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
}
