//! Handing AdGuard's own commands to a terminal the user drives.
//!
//! Shared by the two places that run one rather than show it: *Fix
//! certificate* on the Status page, which runs `cert`, and the window shown
//! when AdGuard CLI is missing, which runs AdGuard's installer (#51). Both
//! commands ask for a password, and both ask it themselves, in the terminal —
//! this application never sees it (`architecture.md` §6).

/// Run `command` in the first terminal emulator found, held open at the end so
/// its output can be read. The command is AdGuard's own and prompts for the
/// password itself; this only gives it a terminal to do that in.
///
/// Each emulator has its own way of being told what to run, so they are tried
/// in turn rather than guessed at through `x-terminal-emulator`'s `-e`, which
/// several of them treat as deprecated or as taking a single word.
pub fn open(command: &str) -> bool {
    use std::process::Command;

    let script = format!("{command}; printf '\\nPress Enter to close. '; read _");
    const TERMINALS: &[(&str, &[&str])] = &[
        ("gnome-terminal", &["--", "sh", "-c"]),
        // Fedora Workstation's terminal since 41, where gnome-terminal is no
        // longer installed. `--` and an argv is the form its own --help gives;
        // `-x` would want the whole command as one string to re-parse.
        ("ptyxis", &["--", "sh", "-c"]),
        ("kgx", &["-e", "sh", "-c"]),
        ("konsole", &["-e", "sh", "-c"]),
        ("xfce4-terminal", &["-x", "sh", "-c"]),
        ("alacritty", &["-e", "sh", "-c"]),
        ("kitty", &["sh", "-c"]),
        ("foot", &["sh", "-c"]),
        ("x-terminal-emulator", &["-e", "sh", "-c"]),
        ("xterm", &["-e", "sh", "-c"]),
    ];
    for (binary, args) in TERMINALS {
        if let Ok(mut child) = Command::new(binary).args(*args).arg(&script).spawn() {
            // Reaped on a thread of its own so a closed terminal does not
            // linger as a zombie for the life of the app.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}
