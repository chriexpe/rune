//! Unit and private-D-Bus tests for Linux notification delivery (`org.freedesktop.Notifications`
//! and `org.freedesktop.portal.Notification`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_executor::LocalExecutor;
use async_io::Timer;
use futures_lite::FutureExt;
use zbus::connection::Builder as ConnectionBuilder;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Address, Connection, interface};

use super::*;

struct PrivateBus {
    address: String,
    child: Child,
}

impl PrivateBus {
    fn start() -> Option<Self> {
        let mut child = ProcessCommand::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1", "--nopidfile"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut reader = BufReader::new(stdout);
        let mut address = String::new();
        if reader.read_line(&mut address).ok()? == 0 {
            drop(child.kill());
            return None;
        }
        Some(Self {
            address: address.trim().to_owned(),
            child,
        })
    }

    async fn connect(&self) -> Connection {
        let addr: Address = self.address.as_str().try_into().unwrap();
        ConnectionBuilder::address(addr).unwrap().build().await.unwrap()
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

struct RecordedFdoCall {
    app_name: String,
    replaces_id: u32,
    summary: String,
    body: String,
    suppress_sound: bool,
    sound_name: Option<String>,
    desktop_entry: Option<String>,
}

#[derive(Default)]
struct RecordedFdo {
    calls: Vec<RecordedFdoCall>,
    closed: Vec<u32>,
    next_id: u32,
}

struct MockFdo {
    state: Arc<Mutex<RecordedFdo>>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl MockFdo {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "body-markup".into(), "actions".into(), "sound".into()]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let mut s = self.state.lock().unwrap();
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            s.next_id += 1;
            s.next_id
        };
        let suppress_sound = hints
            .get("suppress-sound")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        let sound_name = hints
            .get("sound-name")
            .and_then(|v| String::try_from(v.clone()).ok());
        let desktop_entry = hints
            .get("desktop-entry")
            .and_then(|v| String::try_from(v.clone()).ok());
        let _ = (app_icon, actions);
        s.calls.push(RecordedFdoCall {
            app_name: app_name.to_owned(),
            replaces_id,
            summary: summary.to_owned(),
            body: body.to_owned(),
            suppress_sound,
            sound_name,
            desktop_entry,
        });
        id
    }

    fn close_notification(&self, id: u32) {
        self.state.lock().unwrap().closed.push(id);
    }

    #[zbus(signal)]
    async fn activation_token(emitter: &SignalEmitter<'_>, id: u32, token: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;
}

#[derive(Default)]
struct RecordedPortal {
    added: Vec<(String, String, String, Option<String>)>,
    removed: Vec<String>,
}

struct MockPortal {
    version: u32,
    state: Arc<Mutex<RecordedPortal>>,
}

#[interface(name = "org.freedesktop.portal.Notification")]
impl MockPortal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        self.version
    }

    fn add_notification(&self, id: &str, notification: HashMap<String, OwnedValue>) {
        let title = notification
            .get("title")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_default();
        let body = notification
            .get("body")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_default();
        let sound = notification
            .get("sound")
            .and_then(|v| String::try_from(v.clone()).ok());
        self.state
            .lock()
            .unwrap()
            .added
            .push((id.to_owned(), title, body, sound));
    }

    fn remove_notification(&self, id: &str) {
        self.state.lock().unwrap().removed.push(id.to_owned());
    }

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: &str,
        action: &str,
        parameter: Vec<Value<'_>>,
    ) -> zbus::Result<()>;
}

#[test]
fn transport_detects_flatpak_environment() {
    assert_eq!(detect_transport_from(None, false), Transport::Fdo);
    assert_eq!(detect_transport_from(Some(""), false), Transport::Fdo);
    assert_eq!(detect_transport_from(Some("io.github.chriexpe.Papo"), false), Transport::Portal);
    assert_eq!(detect_transport_from(None, true), Transport::Portal);
}

#[test]
fn portal_payload_respects_portal_version_for_v2_fields() {
    let n = Notification::new("msg-1", "Lia · #general")
        .body("Hello")
        .sound(true);
    let v1 = portal_payload(&n, 1);
    assert!(v1.contains_key("title"));
    assert!(v1.contains_key("body"));
    assert!(v1.contains_key("default-action"));
    assert!(!v1.contains_key("sound"), "portal v1 must omit v2 sound field");
    assert!(!v1.contains_key("category"), "portal v1 must omit v2 category field");

    let v2 = portal_payload(&n, 2);
    assert_eq!(
        v2.get("sound").and_then(|v| <&str>::try_from(v).ok()),
        Some("default")
    );
    assert_eq!(
        v2.get("category").and_then(|v| <&str>::try_from(v).ok()),
        Some("im.received")
    );

    let silent_n = Notification::new("msg-2", "Lia").sound(false);
    let silent = portal_payload(&silent_n, 2);
    assert!(!silent.contains_key("sound"));
}

#[test]
fn fdo_hints_set_desktop_entry_category_and_sound() {
    let id = Identity::new("io.github.chriexpe.Papo", "Papo");
    let silent_n = Notification::new("1", "T");
    let silent = fdo_hints(&id, &silent_n);
    assert_eq!(silent.get("desktop-entry").and_then(|v| <&str>::try_from(v).ok()), Some("io.github.chriexpe.Papo"));
    assert_eq!(silent.get("category").and_then(|v| <&str>::try_from(v).ok()), Some("im.received"));
    assert_eq!(silent.get("suppress-sound").and_then(|v| bool::try_from(v).ok()), Some(true));
    assert!(!silent.contains_key("sound-name"));

    let loud_n = Notification::new("2", "T").sound(true);
    let loud = fdo_hints(&id, &loud_n);
    assert_eq!(loud.get("suppress-sound").and_then(|v| bool::try_from(v).ok()), Some(false));
    assert_eq!(loud.get("sound-name").and_then(|v| <&str>::try_from(v).ok()), Some("message-new-instant"));
}

#[test]
fn fdo_round_trip_posts_replaces_delivers_click_with_activation_token_and_closes() {
    let Some(bus) = PrivateBus::start() else { return };
    let exec = LocalExecutor::new();
    async_io::block_on(exec.run(async {
        let recorded = Arc::new(Mutex::new(RecordedFdo::default()));
        let server_conn = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(FDO_DEST)
            .unwrap()
            .serve_at(FDO_PATH, MockFdo { state: Arc::clone(&recorded) })
            .unwrap()
            .build()
            .await
            .unwrap();

        let client_conn = bus.connect().await;
        let (tx, rx) = async_channel::bounded(16);
        let (click_tx, click_rx) = async_channel::bounded(4);
        let worker_task = exec.spawn(async move {
            run_worker(
                client_conn,
                Transport::Fdo,
                rx,
                || Identity::new("io.github.chriexpe.Papo", "Papo"),
                move |click| {
                    drop(click_tx.try_send(click));
                },
            )
            .await;
        });

        tx.send(Command::Post(
            Notification::new("msg-1", "Lia · #general")
                .body("Hi there")
                .sound(true)
                .info("channel", "ch-1"),
        ))
        .await
        .unwrap();

        // Wait for the server to observe Notify.
        for _ in 0..50 {
            if !recorded.lock().unwrap().calls.is_empty() {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        {
            let guard = recorded.lock().unwrap();
            assert_eq!(guard.calls.len(), 1);
            let call = &guard.calls[0];
            assert_eq!(call.app_name, "Papo");
            assert_eq!(call.replaces_id, 0);
            assert_eq!(call.summary, "Lia · #general");
            assert_eq!(call.body, "Hi there");
            assert!(!call.suppress_sound, "suppress-sound should be false when sound(true)");
            assert_eq!(call.sound_name.as_deref(), Some("message-new-instant"));
            assert_eq!(call.desktop_entry.as_deref(), Some("io.github.chriexpe.Papo"));
        }

        // Re-posting the same string id replaces the previous numeric id (1).
        tx.send(Command::Post(
            Notification::new("msg-1", "Lia · #general")
                .body("Updated")
                .info("channel", "ch-1"),
        ))
        .await
        .unwrap();
        for _ in 0..50 {
            if recorded.lock().unwrap().calls.len() >= 2 {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        assert_eq!(recorded.lock().unwrap().calls[1].replaces_id, 1);

        // Emit ActivationToken then ActionInvoked as KWin / GNOME Shell do.
        let iface_ref = server_conn
            .object_server()
            .interface::<_, MockFdo>(FDO_PATH)
            .await
            .unwrap();
        MockFdo::activation_token(iface_ref.signal_emitter(), 1, "xdg-token-42")
            .await
            .unwrap();
        MockFdo::action_invoked(iface_ref.signal_emitter(), 1, "default")
            .await
            .unwrap();

        let click = async { Some(click_rx.recv().await.unwrap()) }
            .or(async {
                Timer::after(Duration::from_secs(2)).await;
                None
            })
            .await
            .expect("expected FDO click");
        assert_eq!(click.id, "msg-1");
        assert_eq!(click.get("channel"), Some("ch-1"));
        assert_eq!(click.action(), None);
        assert_eq!(click.activation_token(), Some("xdg-token-42"));

        // Remove closes notification 1.
        tx.send(Command::Remove(vec!["msg-1".into()])).await.unwrap();
        for _ in 0..50 {
            if !recorded.lock().unwrap().closed.is_empty() {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        assert_eq!(recorded.lock().unwrap().closed, vec![1]);

        drop(tx);
        worker_task.await;
    }));
}

#[test]
fn portal_round_trip_adds_invokes_and_removes_notification() {
    let Some(bus) = PrivateBus::start() else { return };
    let exec = LocalExecutor::new();
    async_io::block_on(exec.run(async {
        let recorded = Arc::new(Mutex::new(RecordedPortal::default()));
        let server_conn = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(PORTAL_DEST)
            .unwrap()
            .serve_at(
                PORTAL_PATH,
                MockPortal {
                    version: 2,
                    state: Arc::clone(&recorded),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();

        let client_conn = bus.connect().await;
        let (tx, rx) = async_channel::bounded(16);
        let (click_tx, click_rx) = async_channel::bounded(4);
        let worker_task = exec.spawn(async move {
            run_worker(
                client_conn,
                Transport::Portal,
                rx,
                || Identity::new("io.github.chriexpe.Papo", "Papo"),
                move |click| {
                    drop(click_tx.try_send(click));
                },
            )
            .await;
        });

        tx.send(Command::Post(
            Notification::new("msg-9", "Lia")
                .body("Direct message")
                .sound(true)
                .info("message", "m-9"),
        ))
        .await
        .unwrap();

        for _ in 0..50 {
            if !recorded.lock().unwrap().added.is_empty() {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        {
            let guard = recorded.lock().unwrap();
            assert_eq!(guard.added.len(), 1);
            assert_eq!(
                guard.added[0],
                ("msg-9".into(), "Lia".into(), "Direct message".into(), Some("default".into()))
            );
        }

        let iface_ref = server_conn
            .object_server()
            .interface::<_, MockPortal>(PORTAL_PATH)
            .await
            .unwrap();
        MockPortal::action_invoked(
            iface_ref.signal_emitter(),
            "msg-9",
            "default",
            vec![Value::from("msg-9")],
        )
        .await
        .unwrap();

        let click = async { Some(click_rx.recv().await.unwrap()) }
            .or(async {
                Timer::after(Duration::from_secs(2)).await;
                None
            })
            .await
            .expect("expected portal click");
        assert_eq!(click.id, "msg-9");
        assert_eq!(click.get("message"), Some("m-9"));
        assert_eq!(click.action(), None);

        tx.send(Command::Remove(vec!["msg-9".into()])).await.unwrap();
        for _ in 0..50 {
            if !recorded.lock().unwrap().removed.is_empty() {
                break;
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        assert_eq!(recorded.lock().unwrap().removed, vec!["msg-9".to_owned()]);

        drop(tx);
        worker_task.await;
    }));
}

#[test]
fn fdo_body_escapes_markup_only_when_advertised() {
    assert_eq!(fdo_body("1 < 2 & 3 > 2", true), "1 &lt; 2 &amp; 3 &gt; 2");
    assert_eq!(fdo_body("1 < 2 & 3 > 2", false), "1 < 2 & 3 > 2");
    assert_eq!(fdo_body("plain text", true), "plain text");
}
