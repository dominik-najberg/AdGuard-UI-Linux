//! Tray indicator for AdGuard UI.
//!
//! Uses StatusNotifierItem over D-Bus (via `ksni`, which speaks zbus and so
//! needs no C headers — `libayatana-appindicator3-dev` is not installed on the
//! reference machine). GNOME requires the AppIndicator extension for this to
//! appear; `ubuntu-appindicators@ubuntu.com` ships enabled on Ubuntu.
//!
//! A separate binary from the GUI for now, so the GTK main loop and the tray's
//! async runtime stay out of each other's way. Merging them is a later call.

use std::process::Command;
use std::time::Duration;

use adguard_core::Cli;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Tray, TrayMethods};

const APP_ID: &str = "io.github.dominik-najberg.AdGuardUI";
const POLL_INTERVAL: Duration = Duration::from_secs(5);

struct AdGuardTray {
    cli: Cli,
    running: bool,
}

impl Tray for AdGuardTray {
    fn id(&self) -> String {
        APP_ID.to_owned()
    }

    fn title(&self) -> String {
        format!(
            "AdGuard — {}",
            if self.running { "running" } else { "stopped" }
        )
    }

    fn icon_name(&self) -> String {
        if self.running {
            "security-high-symbolic".to_owned()
        } else {
            "security-low-symbolic".to_owned()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let toggle_label = if self.running { "Stop" } else { "Start" };

        vec![
            StandardItem {
                label: "Open AdGuard UI".into(),
                activate: Box::new(|_: &mut Self| {
                    // Best-effort launch; the tray keeps working if it fails.
                    let _ = Command::new("adguard-ui").spawn();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: toggle_label.into(),
                activate: Box::new(|tray: &mut Self| {
                    // Fire and re-read: the poll loop below reconciles the
                    // real state, so we never assume this worked.
                    let _ = if tray.running {
                        tray.cli.stop()
                    } else {
                        tray.cli.start()
                    };
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = match Cli::discover() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("adguard-tray: {err}");
            std::process::exit(1);
        }
    };

    let running = cli.status().map(|s| s.running).unwrap_or(false);
    let tray = AdGuardTray {
        cli: cli.clone(),
        running,
    };

    let handle = match tray.spawn().await {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("adguard-tray: could not register tray icon: {err}");
            std::process::exit(1);
        }
    };

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let running = cli.status().map(|s| s.running).unwrap_or(false);
        handle.update(|tray: &mut AdGuardTray| tray.running = running).await;
    }
}
