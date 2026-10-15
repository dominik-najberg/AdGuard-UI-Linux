//! What name the tray registers itself under, against a watcher of our own.
//!
//! The module header in `lib.rs` has the incident. After the update to 1.11.0
//! the copy the updater `exec`d kept the PID, so it asked for the same
//! `org.kde.StatusNotifierItem-<pid>-<n>` the old copy had just given up, and
//! Ubuntu's AppIndicator extension dropped the item. The fix is to register
//! under the connection's unique name instead, which the bus never reuses.
//! This proves that is the name a watcher is handed.
//!
//! **A private bus, not the session's.** `dbus-daemon --session` on a socket of
//! its own, with a stub watcher on it. Nothing reaches the desktop's real tray,
//! and the test cannot be confused by the items already there. Skips when
//! `dbus-daemon` is not installed, as a bare container may not have it.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The watcher's half of the protocol, recording what it is asked to register.
struct Watcher {
    registered: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.registered.lock().unwrap().push(service.to_owned());
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registered.lock().unwrap().clone()
    }
}

/// The private bus, stopped when the test ends however it ends.
struct Bus(Child);

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_item_is_registered_under_the_connections_unique_name() {
    let Ok(mut child) = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        eprintln!("skipping: dbus-daemon is not installed");
        return;
    };
    let mut address = String::new();
    BufReader::new(child.stdout.take().expect("piped stdout"))
        .read_line(&mut address)
        .expect("dbus-daemon prints its address");
    let _bus = Bus(child);
    // The only test in this binary, so nothing else reads the variable while
    // it changes; and `ksni` takes the session bus from it.
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address.trim());

    let registered = Arc::new(Mutex::new(Vec::new()));
    let (ready, watching) = std::sync::mpsc::channel();
    let recorder = Arc::clone(&registered);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the watcher");
        runtime.block_on(async move {
            let _connection = zbus::connection::Builder::session()
                .expect("the private bus")
                .name("org.kde.StatusNotifierWatcher")
                .expect("a valid name")
                .serve_at("/StatusNotifierWatcher", Watcher { registered: recorder })
                .expect("a valid path")
                .build()
                .await
                .expect("the watcher takes its name");
            ready.send(()).expect("the test is waiting");
            std::future::pending::<()>().await;
        });
    });
    watching
        .recv_timeout(Duration::from_secs(5))
        .expect("the watcher came up");

    let tray = adguard_tray::spawn(adguard_tray::State::default()).expect("the tray registers");

    let services = registered.lock().unwrap().clone();
    assert_eq!(services.len(), 1, "{services:?}");
    assert!(
        services[0].starts_with(':'),
        "registered as {}, not as the connection's unique name",
        services[0]
    );

    // And no name with the PID in it is held either, so there is nothing for
    // an `exec`d copy to ask for again.
    let names = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the check")
        .block_on(async {
            let connection = zbus::Connection::session().await.expect("the private bus");
            zbus::fdo::DBusProxy::new(&connection)
                .await
                .expect("the bus's own interface")
                .list_names()
                .await
                .expect("the names on the bus")
        });
    let pid_names: Vec<_> = names
        .iter()
        .map(|name| name.as_str())
        .filter(|name| name.starts_with("org.kde.StatusNotifierItem-"))
        .collect();
    assert!(pid_names.is_empty(), "{pid_names:?}");

    drop(tray);
}
