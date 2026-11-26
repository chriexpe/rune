//! Exercise the actual workers against mock services on private session buses.
use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command as ProcessCommand, Stdio};
use zbus::{connection::Builder, interface, object_server::SignalEmitter, zvariant::OwnedValue};

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn private_bus() -> (Bus, String) {
    let child = ProcessCommand::new("dbus-daemon")
        .args([
            "--session",
            "--nofork",
            "--print-address=1",
            "--nopidfile",
            "--address=unix:tmpdir=/tmp",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start private dbus-daemon");
    let mut bus = Bus(child);
    let mut address = String::new();
    BufReader::new(bus.0.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    assert!(!address.is_empty(), "private bus must print its address");
    (bus, address.trim().to_owned())
}

type Dict = HashMap<String, OwnedValue>;

#[derive(Debug)]
enum FdoCall {
    Post {
        replaces: u32,
        body: String,
        actions: Vec<String>,
        hints: Dict,
    },
    Close(u32),
}

struct Fdo(Sender<FdoCall>);
#[interface(name = "org.freedesktop.Notifications")]
impl Fdo {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["actions", "body-markup"]
    }

    #[allow(clippy::too_many_arguments)] // The Notify signature is specified by FDO.
    fn notify(
        &self,
        _app_name: &str,
        replaces: u32,
        _icon: &str,
        _title: &str,
        body: &str,
        actions: Vec<String>,
        hints: Dict,
        _expiry: i32,
    ) -> u32 {
        self.0
            .try_send(FdoCall::Post {
                replaces,
                body: body.to_owned(),
                actions,
                hints,
            })
            .unwrap();
        42
    }

    fn close_notification(&self, id: u32) {
        self.0.try_send(FdoCall::Close(id)).unwrap();
    }

    #[zbus(signal)]
    async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        token: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action: &str)
    -> zbus::Result<()>;
}

#[derive(Debug)]
enum PortalCall {
    Add(String, Dict),
    Remove(String),
}

struct Portal {
    calls: Sender<PortalCall>,
    version: u32,
}
#[interface(name = "org.freedesktop.portal.Notification")]
impl Portal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        self.version
    }

    fn add_notification(&self, id: &str, payload: Dict) {
        self.calls
            .try_send(PortalCall::Add(id.to_owned(), payload))
            .unwrap();
    }

    fn remove_notification(&self, id: &str) {
        self.calls
            .try_send(PortalCall::Remove(id.to_owned()))
            .unwrap();
    }

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: &str,
        action: &str,
        params: Vec<Value<'_>>,
    ) -> zbus::Result<()>;
}

async fn with_worker(
    address: &str,
    transport: Transport,
    scenario: impl AsyncFnOnce(Sender<Command>, Receiver<Click>),
) {
    let connect = async { Builder::address(address)?.build().await };
    let (commands, receiver) = unbounded();
    let (ready, result) = mpsc::sync_channel(1);
    let (clicks, clicked) = unbounded();
    let worker = run_on(connect, transport, receiver, ready, |click| {
        clicks.try_send(click).unwrap()
    });
    let completed = timeout(
        Duration::from_secs(10),
        futures_lite::future::zip(worker, scenario(commands, clicked)),
    )
    .await;
    assert_eq!(
        result.try_recv(),
        Ok(true),
        "notification service did not become available"
    );
    assert!(completed.is_some(), "round-trip timed out");
}

async fn send(commands: &Sender<Command>, n: Notification) {
    commands
        .send(Command::Post(Post {
            n,
            app_name: "Example".into(),
            desktop_entry: Some("com.example.App".into()),
        }))
        .await
        .ok()
        .unwrap();
}

fn text<'a>(dict: &'a Dict, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(|value| <&str>::try_from(value).ok())
}

#[test]
fn fdo_roundtrip_preserves_replace_actions_and_token_and_opts_into_sound() {
    async_io::block_on(async {
        let (_bus, address) = private_bus();
        let (calls, received) = unbounded();
        let server = Builder::address(address.as_str())
            .unwrap()
            .name(NAME)
            .unwrap()
            .serve_at(PATH, Fdo(calls))
            .unwrap()
            .build()
            .await
            .unwrap();
        let emitter = SignalEmitter::new(&server, PATH).unwrap();
        with_worker(&address, Transport::Fdo, async |commands, clicks| {
            let n = Notification::new("msg", "New message")
                .body("a < b")
                .info("channel", "old")
                .action("open", "Open");
            send(&commands, n.clone()).await;
            let FdoCall::Post {
                replaces,
                body,
                actions,
                hints,
            } = received.recv().await.unwrap()
            else {
                panic!("expected Notify")
            };
            assert_eq!(replaces, 0);
            assert_eq!(body, "a &lt; b");
            assert_eq!(actions, ["default", "", "open", "Open"]);
            assert_eq!(text(&hints, "desktop-entry"), Some("com.example.App"));
            assert!(bool::try_from(&hints["suppress-sound"]).unwrap());
            assert!(!hints.contains_key("sound-name"));
            send(&commands, n.sound(true)).await;
            let FdoCall::Post {
                replaces, hints, ..
            } = received.recv().await.unwrap()
            else {
                panic!("expected replacement")
            };
            assert_eq!(replaces, 42);
            assert!(!bool::try_from(&hints["suppress-sound"]).unwrap());
            assert_eq!(text(&hints, "sound-name"), Some("message-new-instant"));
            Fdo::activation_token(&emitter, 42, "xdg-token")
                .await
                .unwrap();
            Fdo::action_invoked(&emitter, 42, "open").await.unwrap();
            let click = clicks.recv().await.unwrap();
            assert_eq!(click.id, "msg");
            assert_eq!(click.get("channel"), Some("old"));
            assert_eq!(click.action(), Some("open"));
            assert_eq!(click.activation_token(), Some("xdg-token"));
            commands
                .send(Command::Remove(vec!["msg".into()]))
                .await
                .ok()
                .unwrap();
            assert!(matches!(received.recv().await.unwrap(), FdoCall::Close(42)));
        })
        .await;
    });
}

fn portal_roundtrip(version: u32) {
    async_io::block_on(async {
        let (_bus, address) = private_bus();
        let (calls, received) = unbounded();
        let server = Builder::address(address.as_str())
            .unwrap()
            .name(portal::NAME)
            .unwrap()
            .serve_at(portal::PATH, Portal { calls, version })
            .unwrap()
            .build()
            .await
            .unwrap();
        let emitter = SignalEmitter::new(&server, portal::PATH).unwrap();
        with_worker(&address, Transport::Portal, async |commands, clicks| {
            let n = Notification::new("msg", "New message").action("app.open", "Open");
            send(&commands, n.clone().info("channel", "old")).await;
            let PortalCall::Add(id, payload) = received.recv().await.unwrap() else {
                panic!("expected AddNotification")
            };
            assert_eq!(id, "msg");
            assert_eq!(text(&payload, "title"), Some("New message"));
            assert_eq!(text(&payload, "default-action"), Some("default"));
            assert_eq!(text(&payload, "sound"), (version >= 2).then_some("silent"));
            assert_eq!(
                text(&payload, "category"),
                (version >= 2).then_some("im.received")
            );
            let buttons = Vec::<Dict>::try_from(payload["buttons"].try_clone().unwrap()).unwrap();
            let action = text(&buttons[0], "action").unwrap();
            assert!(!action.starts_with("app."));
            send(&commands, n.sound(true).info("channel", "new")).await;
            let PortalCall::Add(id, payload) = received.recv().await.unwrap() else {
                panic!("expected replacement")
            };
            assert_eq!(id, "msg");
            assert_eq!(text(&payload, "sound"), (version >= 2).then_some("default"));
            let params = if version >= 2 {
                vec![Value::from(HashMap::from([(
                    "activation-token",
                    Value::from("portal-token"),
                )]))]
            } else {
                Vec::new()
            };
            Portal::action_invoked(&emitter, "msg", action, params)
                .await
                .unwrap();
            let click = clicks.recv().await.unwrap();
            assert_eq!(click.id, "msg");
            assert_eq!(click.action(), Some("app.open"));
            assert_eq!(click.get("channel"), Some("new"));
            assert_eq!(
                click.activation_token(),
                (version >= 2).then_some("portal-token")
            );
            Portal::action_invoked(&emitter, "msg", "default", Vec::new())
                .await
                .unwrap();
            assert_eq!(clicks.recv().await.unwrap().action(), None);
            commands
                .send(Command::Remove(vec!["msg".into()]))
                .await
                .ok()
                .unwrap();
            assert!(
                matches!(received.recv().await.unwrap(), PortalCall::Remove(id) if id == "msg")
            );
        })
        .await;
    });
}

#[test]
fn portal_v1_roundtrip_omits_v2_keys_even_with_sound_requested() {
    portal_roundtrip(1);
}

#[test]
fn portal_v2_roundtrip_controls_sound_and_forwards_activation_token() {
    portal_roundtrip(2);
}

#[test]
fn flatpak_uses_the_portal_when_either_sandbox_marker_is_present() {
    use std::ffi::OsStr;
    assert_eq!(Transport::detect(None, false), Transport::Fdo);
    assert_eq!(
        Transport::detect(Some(OsStr::new("")), false),
        Transport::Fdo
    );
    assert_eq!(Transport::detect(None, true), Transport::Portal);
    assert_eq!(
        Transport::detect(Some(OsStr::new("com.example.App")), false),
        Transport::Portal
    );
}
