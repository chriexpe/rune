//! Linux notifications on a worker thread: the XDG portal in Flatpak, otherwise the desktop's
//! `org.freedesktop.Notifications`. The first call probes the service to determine availability.
//! Reposting an id replaces its notification; clicks come back with the id and its user info.
use super::{ACTION, Action, Click, Notification, TOKEN, deliver};
use async_channel::{Receiver, Sender, unbounded};
use futures_lite::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;
use zbus::{Connection, MatchRule, MessageStream, Proxy, message::Type, zvariant::Value};

mod portal;
#[cfg(test)]
mod roundtrip_tests;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long the first call waits for the session bus and a notification server (which the bus
/// may start on demand) before it decides there is none.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// The action key a server invokes for a click on the notification itself.
const DEFAULT_ACTION: &str = "default";

/// What the server can do, from `GetCapabilities`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Caps {
    /// Clicks and buttons: without it the notification posts without either.
    pub(crate) actions: bool,
    /// The body is read as a small markup language, so it is escaped.
    pub(crate) markup: bool,
}

impl Caps {
    fn from_list(caps: &[String]) -> Self {
        Caps { actions: caps.iter().any(|c| c == "actions"), markup: caps.iter().any(|c| c == "body-markup") }
    }
}

/// Who posts, from `register`: the server shows the name and finds the app's desktop entry
/// (`<id>.desktop`) for its icon.
static IDENTITY: Mutex<Option<(String, String)>> = Mutex::new(None);

pub(crate) fn register(id: &str, name: &str) {
    *IDENTITY.lock().unwrap_or_else(|e| e.into_inner()) = Some((id.to_owned(), name.to_owned()));
}

struct Post { n: Notification, app_name: String, desktop_entry: Option<String> }

enum Command {
    Post(Post),
    Remove(Vec<String>),
}

fn backend() -> Option<&'static Sender<Command>> {
    static BACKEND: OnceLock<Option<Sender<Command>>> = OnceLock::new();
    BACKEND.get_or_init(start).as_ref()
}

/// Starts the worker and waits for it to reach a server, or not.
fn start() -> Option<Sender<Command>> {
    let (commands, receiver) = unbounded();
    let (ready, result) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rune-notify".into())
        .spawn(move || async_io::block_on(run(receiver, ready)))
        .ok()?;
    match result.recv_timeout(PROBE_TIMEOUT + Duration::from_millis(500)) {
        Ok(true) => Some(commands),
        _ => {
            commands.close();
            None
        }
    }
}

/// The worker drops its end when the connection is gone, which closes the channel.
pub(crate) fn available() -> bool {
    backend().is_some_and(|commands| !commands.is_closed())
}

pub(crate) fn post(n: &Notification) -> bool {
    let Some(commands) = backend() else { return false };
    let identity = IDENTITY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (app_name, desktop_entry) = match identity {
        Some((id, name)) => (name, Some(id)),
        None => (program_name(), None),
    };
    commands.try_send(Command::Post(Post { n: n.clone(), app_name, desktop_entry })).is_ok()
}

pub(crate) fn remove(ids: &[&str]) {
    if let Some(commands) = backend() {
        commands.try_send(Command::Remove(ids.iter().map(|id| (*id).to_owned()).collect())).ok();
    }
}

/// The executable's file name, for a process that never called `register`.
fn program_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|arg| std::path::Path::new(&arg).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

async fn timeout<T>(limit: Duration, future: impl Future<Output = T>) -> Option<T> {
    async { Some(future.await) }
        .or(async {
            async_io::Timer::after(limit).await;
            None
        })
        .await
}

/// One posted notification the server still shows, with the activation token it sent for the
/// click about to come.
struct Shown { server: u32, info: Vec<(String, String)>, token: Option<String> }

enum Event {
    Command(Option<Command>),
    /// One of the server's signals, or the stream's error for a message it couldn't read; None
    /// when the connection ends.
    Signal(Option<zbus::Result<zbus::Message>>),
}

async fn run(commands: Receiver<Command>, ready: mpsc::SyncSender<bool>) {
    let transport = Transport::detect(
        std::env::var_os("FLATPAK_ID").as_deref(),
        std::path::Path::new("/.flatpak-info").exists(),
    );
    run_on(Connection::session(), transport, commands, ready, deliver).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Fdo,
    Portal,
}

impl Transport {
    fn detect(flatpak_id: Option<&std::ffi::OsStr>, flatpak_info: bool) -> Self {
        if flatpak_info || flatpak_id.is_some_and(|id| !id.is_empty()) {
            Self::Portal
        } else {
            Self::Fdo
        }
    }
}

async fn run_on(
    connect: impl Future<Output = zbus::Result<Connection>>,
    transport: Transport,
    commands: Receiver<Command>,
    ready: mpsc::SyncSender<bool>,
    deliver: impl Fn(Click),
) {
    match transport {
        Transport::Fdo => run_fdo(connect, commands, ready, deliver).await,
        Transport::Portal => portal::run(connect, commands, ready, deliver).await,
    }
}

async fn notification_signals(
    conn: &Connection,
    name: &str,
    path: &str,
    interface: &str,
) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(name)?
        .interface(interface)?
        .path(path)?
        .build();
    MessageStream::for_match_rule(rule, conn, None).await
}

async fn next_event(commands: &Receiver<Command>, signals: &mut MessageStream) -> Event {
    async { Event::Command(commands.recv().await.ok()) }
        .or(async { Event::Signal(signals.next().await) })
        .await
}

async fn run_fdo(
    connect: impl Future<Output = zbus::Result<Connection>>,
    commands: Receiver<Command>,
    ready: mpsc::SyncSender<bool>,
    deliver: impl Fn(Click),
) {
    let setup = async {
        let conn = connect.await?;
        let proxy = Proxy::new(&conn, NAME, PATH, NAME).await?;
        // One stream for every signal, so `ActivationToken` stays ahead of its `ActionInvoked`;
        // only the server's, so no other peer on the bus can fake a click.
        let signals = notification_signals(&conn, NAME, PATH, NAME).await?;
        let caps: Vec<String> = proxy.call("GetCapabilities", &()).await?;
        Ok::<_, zbus::Error>((proxy, signals, Caps::from_list(&caps)))
    };
    let Some(Ok((proxy, mut signals, caps))) = timeout(PROBE_TIMEOUT, setup).await else {
        ready.send(false).ok();
        return;
    };
    ready.send(true).ok();
    let mut shown: HashMap<String, Shown> = HashMap::new();
    loop {
        match next_event(&commands, &mut signals).await {
            Event::Command(None) => return,
            Event::Command(Some(Command::Post(post))) => {
                let replaces = shown.get(&post.n.id).map_or(0, |s| s.server);
                let actions = actions_for(&post.n, caps);
                let body = body_for(&post.n.body, caps);
                let mut hints: HashMap<&str, Value> = HashMap::new();
                hints.insert("suppress-sound", Value::from(!post.n.sound));
                if post.n.sound {
                    hints.insert("sound-name", Value::from("message-new-instant"));
                }
                if let Some(entry) = post.desktop_entry.as_deref() {
                    hints.insert("desktop-entry", Value::from(entry));
                }
                let args = (post.app_name.as_str(), replaces, "", post.n.title.as_str(), body.as_str(), actions, hints, -1i32);
                if let Some(Ok(server)) = timeout(RPC_TIMEOUT, proxy.call::<_, _, u32>("Notify", &args)).await {
                    shown.insert(post.n.id, Shown { server, info: post.n.info, token: None });
                }
            }
            Event::Command(Some(Command::Remove(ids))) => {
                for id in ids {
                    if let Some(s) = shown.remove(&id) {
                        let _ = timeout(RPC_TIMEOUT, proxy.call::<_, _, ()>("CloseNotification", &(s.server,))).await;
                    }
                }
            }
            // A message the stream couldn't read; the connection carries on.
            Event::Signal(Some(Err(_))) => {}
            Event::Signal(Some(Ok(message))) => {
                let header = message.header();
                let body = message.body();
                match header.member().map(|m| m.as_str()) {
                    // It comes just before the `ActionInvoked` it belongs to; the server sends
                    // other apps' too, which match none of ours.
                    Some("ActivationToken") => {
                        let Ok((server, token)) = body.deserialize::<(u32, String)>() else { continue };
                        if let Some(s) = shown.values_mut().find(|s| s.server == server) {
                            s.token = Some(token);
                        }
                    }
                    Some("ActionInvoked") => {
                        let Ok((server, key)) = body.deserialize::<(u32, String)>() else { continue };
                        if let Some((id, s)) = shown.iter_mut().find(|(_, s)| s.server == server) {
                            deliver(click_for(
                                id,
                                &s.info,
                                (key != DEFAULT_ACTION).then_some(key.as_str()),
                                s.token.take(),
                            ));
                        }
                    }
                    Some("NotificationClosed") => {
                        if let Ok((server, _reason)) = body.deserialize::<(u32, u32)>() {
                            shown.retain(|_, s| s.server != server);
                        }
                    }
                    _ => {}
                }
            }
            // The stream ends only with the connection; returning drops `commands`, so
            // `available` turns false.
            Event::Signal(None) => return,
        }
    }
}

/// The `Notify` action list: key, label pairs, flattened. A click on the notification itself
/// invokes `default`; each button its own id. Text boxes have no equivalent and are left out.
pub(crate) fn actions_for(n: &Notification, caps: Caps) -> Vec<&str> {
    if !caps.actions {
        return Vec::new();
    }
    let mut list = vec![DEFAULT_ACTION, ""];
    for action in &n.actions {
        if let Action::Button { id, label } = action {
            list.extend([id.as_str(), label.as_str()]);
        }
    }
    list
}

/// The body as the server reads it: escaped where it would take it for markup.
pub(crate) fn body_for(body: &str, caps: Caps) -> String {
    if !caps.markup {
        return body.to_owned();
    }
    let mut out = String::with_capacity(body.len());
    for c in body.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// The click for notification `id`: its user info, plus the button and activation token when present.
fn click_for(
    id: &str,
    info: &[(String, String)],
    action: Option<&str>,
    token: Option<String>,
) -> Click {
    let mut info = info.to_vec();
    if let Some(key) = action {
        info.push((ACTION.to_owned(), key.to_owned()));
    }
    if let Some(token) = token {
        info.push((TOKEN.to_owned(), token));
    }
    Click {
        id: id.to_owned(),
        info,
    }
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
