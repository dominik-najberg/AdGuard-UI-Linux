//! Tray indicator for AdGuard UI — StatusNotifierItem over D-Bus, via `ksni`.
//!
//! `ksni` speaks SNI through zbus and so needs no C headers, which matters
//! because `libayatana-appindicator3-dev` is not installed on the reference
//! machine. GNOME has no native tray; the item appears only while an
//! AppIndicator extension is running (`ubuntu-appindicators@ubuntu.com` ships
//! enabled on Ubuntu). Registration failing is therefore a normal outcome, not
//! an error — see [`spawn`].
//!
//! ## This is a library, and it does not touch AdGuard
//!
//! It was a second binary until the process model was settled. Two processes
//! meant two independent writers to `proxy.yaml` with neither observing the
//! other, so a tray toggle left an open window showing stale state until the
//! user pressed refresh — and `architecture.md` §3's own refresh policy ("~2 s
//! while a window is open, ~10 s when only the tray is visible") is not even
//! expressible across a process boundary.
//!
//! So the GUI owns the process and this is the view layer. It holds no [`Cli`],
//! reads no config, and runs no timer. It renders a [`State`] it is handed and
//! emits a [`Command`] when the user picks something; every AdGuard call happens
//! on the GUI side, through the same act -> re-read -> reconcile path a click on
//! the Protection page takes.
//!
//! That is also what keeps the menu responsive. `ksni` invokes menu callbacks on
//! its own async runtime and its documentation is explicit that they must not
//! block — "avoid blocking operations here or the menu will freeze ... hand off
//! work to your main application logic". The previous binary called the blocking
//! `Cli::stop` directly inside a callback, on a `current_thread` runtime, which
//! froze the D-Bus service for as long as stopping the proxy took. Here a
//! callback only puts a value on an unbounded channel.
//!
//! [`Cli`]: adguard_core::Cli

use std::sync::{Arc, Mutex};

use adguard_core::Toggle;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Tray as KsniTray, TrayMethods};

/// Must match the GTK application ID and the `.desktop` filename.
const APP_ID: &str = "io.github.dominik-najberg.AdGuardUI";

/// How long to wait for the tray to register before giving up on it.
///
/// Registration is a D-Bus round-trip, and this blocks the caller because the
/// answer decides whether closing the window quits the application. Bounded so
/// an unresponsive bus cannot stop the GUI from starting.
const REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start the tray thread: {0}")]
    Thread(String),

    #[error("could not build the tray's async runtime: {0}")]
    Runtime(String),

    /// No StatusNotifierItem host accepted the registration — on GNOME, almost
    /// always a missing or disabled AppIndicator extension. The application must
    /// carry on without a tray.
    #[error("could not register a tray icon: {0}")]
    Register(String),

    #[error("the tray did not register within {}s", REGISTER_TIMEOUT.as_secs())]
    Timeout,
}

/// Everything the tray displays, as last observed by the GUI.
///
/// `None` in [`Self::toggles`] means the key could not be read from
/// `proxy.yaml`; the item is shown insensitive rather than unchecked, for the
/// same reason the Protection page renders an "unavailable" row — claiming ad
/// blocking is off when we merely could not read the setting is the more
/// dangerous of the two errors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub running: bool,
    /// One entry per [`Toggle::ALL`], in that order.
    pub toggles: Vec<Option<bool>>,
}

impl State {
    fn toggle(&self, index: usize) -> Option<bool> {
        self.toggles.get(index).copied().flatten()
    }
}

/// Something the user asked for from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Present the main window, raising it if it is already open.
    ShowWindow,
    StartProxy,
    StopProxy,
    SetToggle { toggle: Toggle, on: bool },
    /// Quit the application outright — the only way out once closing the window
    /// merely hides it.
    Quit,
}

/// A registered tray icon, owned by the GUI.
pub struct Tray {
    commands: async_channel::Receiver<Command>,
    /// The latest state, shared with the tray thread.
    ///
    /// A channel of states would queue: the GUI pushes on every status poll,
    /// and if the tray stalled on a D-Bus round-trip the backlog would grow and
    /// then replay stale frames. Only the newest state is ever interesting, so
    /// it lives in one slot and `notify` merely says "look again".
    shared: Arc<Mutex<State>>,
    /// Capacity 1: a pending wake-up already covers any number of changes.
    notify: async_channel::Sender<()>,
}

impl Tray {
    /// Menu activations, to be drained on the GTK main loop.
    pub fn commands(&self) -> &async_channel::Receiver<Command> {
        &self.commands
    }

    /// Hand the tray a new state. Never blocks, so it is safe from the GTK
    /// main thread.
    ///
    /// An unchanged state is dropped rather than forwarded. Without that the
    /// 2-second status poll would drive a D-Bus update every 2 seconds for the
    /// lifetime of the session, redrawing a menu nobody asked about.
    pub fn set_state(&self, state: State) {
        match self.shared.lock() {
            Ok(mut slot) => {
                if *slot == state {
                    return;
                }
                *slot = state;
            }
            // Poisoned: the tray thread panicked mid-update. Nothing useful to
            // do here, and it must not take the GUI down with it.
            Err(_) => return,
        }
        // Full means a wake-up is already queued, which is all we needed.
        let _ = self.notify.try_send(());
    }
}

/// The `ksni::Tray` implementation. Renders [`State`]; emits [`Command`].
struct Indicator {
    state: State,
    commands: async_channel::Sender<Command>,
}

impl Indicator {
    /// Unbounded, so this cannot block the menu. A closed channel means the GUI
    /// has gone away and the process is on its way out.
    fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }
}

impl KsniTray for Indicator {
    /// Left-clicking the icon opens the menu rather than firing an activate
    /// action. With quick toggles in the menu, the menu *is* the point.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        APP_ID.to_owned()
    }

    fn title(&self) -> String {
        format!(
            "AdGuard — {}",
            if self.state.running {
                "running"
            } else {
                "stopped"
            }
        )
    }

    fn icon_name(&self) -> String {
        if self.state.running {
            "security-high-symbolic".to_owned()
        } else {
            "security-low-symbolic".to_owned()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![
            StandardItem {
                label: "Open AdGuard UI".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::ShowWindow)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if self.state.running {
                    "Stop proxy".into()
                } else {
                    "Start proxy".into()
                },
                activate: Box::new(|this: &mut Self| {
                    // Decided from the state as it is now, not as it was when
                    // this menu was built.
                    this.send(if this.state.running {
                        Command::StopProxy
                    } else {
                        Command::StartProxy
                    });
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

        for (index, toggle) in Toggle::ALL.into_iter().enumerate() {
            let known = self.state.toggle(index);
            items.push(
                CheckmarkItem {
                    label: toggle.title().into(),
                    checked: known.unwrap_or(false),
                    // Unreadable in proxy.yaml: shown, but not actionable.
                    enabled: known.is_some(),
                    activate: Box::new(move |this: &mut Self| {
                        let on = !this.state.toggle(index).unwrap_or(false);
                        this.send(Command::SetToggle { toggle, on });
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Register a tray icon and start serving it on a thread of its own.
///
/// `ksni` needs a tokio runtime and the GUI runs a glib main loop, so the two
/// cannot share a thread. The runtime here is `current_thread` and does almost
/// nothing: it serves D-Bus and waits on one channel. There is no timer and no
/// AdGuard call on this side.
///
/// Returns [`Error::Register`] when no host accepted the item, which on GNOME
/// means the AppIndicator extension is missing. **Callers must treat that as a
/// normal outcome** and carry on windowed — an application that exits because
/// its tray icon could not appear is worse than one without a tray icon.
pub fn spawn(initial: State) -> Result<Tray, Error> {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (notify_tx, notify_rx) = async_channel::bounded(1);
    let shared = Arc::new(Mutex::new(initial.clone()));

    // Registration is awaited so the caller learns whether a tray exists: that
    // decides whether closing the window hides the app or quits it, and getting
    // it wrong would leave the user with no way to reach either.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let thread_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("adguard-tray".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(Error::Runtime(err.to_string())));
                    return;
                }
            };

            runtime.block_on(async move {
                let indicator = Indicator {
                    state: initial,
                    commands: command_tx,
                };
                let handle = match indicator.spawn().await {
                    Ok(handle) => {
                        let _ = ready_tx.send(Ok(()));
                        handle
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(Error::Register(err.to_string())));
                        return;
                    }
                };

                // Purely reactive. The channel closes when the GUI drops the
                // `Tray`, which ends the thread and with it the icon.
                while notify_rx.recv().await.is_ok() {
                    // The lock is held only to clone a handful of bools, with
                    // no await inside it, so a std mutex is right here.
                    let Ok(state) = thread_shared.lock().map(|state| state.clone()) else {
                        break; // poisoned
                    };
                    handle.update(|indicator: &mut Indicator| indicator.state = state).await;
                }
            });
        })
        .map_err(|err| Error::Thread(err.to_string()))?;

    match ready_rx.recv_timeout(REGISTER_TIMEOUT) {
        Ok(Ok(())) => Ok(Tray {
            commands: command_rx,
            shared,
            notify: notify_tx,
        }),
        Ok(Err(err)) => Err(err),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(Error::Thread("the tray thread stopped before registering".to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(running: bool, toggles: Vec<Option<bool>>) -> State {
        State { running, toggles }
    }

    /// `toggles` is positional against `Toggle::ALL`, so a short or empty vector
    /// must read as unknown rather than panicking — it is what a failed
    /// `Config::load` leaves behind.
    #[test]
    fn a_missing_entry_reads_as_unknown() {
        let empty = State::default();
        for index in 0..Toggle::ALL.len() {
            assert_eq!(empty.toggle(index), None);
        }

        let partial = state(true, vec![Some(true)]);
        assert_eq!(partial.toggle(0), Some(true));
        assert_eq!(partial.toggle(1), None);
    }

    /// The dedupe in `set_state` rests on this: the 2 s status poll pushes an
    /// identical state most of the time, and forwarding it would redraw the menu
    /// over D-Bus every 2 seconds for the whole session.
    #[test]
    fn equal_states_compare_equal() {
        let toggles = vec![Some(true), None, Some(false)];
        assert_eq!(state(true, toggles.clone()), state(true, toggles.clone()));
        assert_ne!(state(false, toggles.clone()), state(true, toggles.clone()));
        assert_ne!(state(true, vec![Some(true)]), state(true, vec![Some(false)]));
        assert_ne!(state(true, toggles), state(true, vec![]));
    }
}
