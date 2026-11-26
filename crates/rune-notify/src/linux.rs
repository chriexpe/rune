//! Linux desktop notifications over D-Bus:
//! - Inside a Flatpak (`FLATPAK_ID` or `/.flatpak-info`): `org.freedesktop.portal.Notification`.
//! - Outside a Flatpak: `org.freedesktop.Notifications` (with `desktop-entry`, `category`,
//!   sound hint, `replaces_id`, and `ActivationToken` forwarding on click).

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use async_channel::{Receiver, Sender};
use async_executor::LocalExecutor;
use futures_lite::{FutureExt, StreamExt};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, Proxy};

use super::{ACTION, ACTIVATION_TOKEN, Action, Click, Identity, Notification};

const FDO_DEST: &str = "org.freedesktop.Notifications";
const FDO_PATH: &str = "/org/freedesktop/Notifications";
const FDO_IFACE: &str = "org.freedesktop.Notifications";

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Notification";

const DEFAULT_ACTION: &str = "default";
const MAX_TRACKED: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Fdo,
    Portal,
}

enum Command {
    Post(Notification),
    Remove(Vec<String>),
}

static IDENTITY: OnceLock<Mutex<Option<Identity>>> = OnceLock::new();
static WORKER: OnceLock<Sender<Command>> = OnceLock::new();

fn identity_cell() -> &'static Mutex<Option<Identity>> {
    IDENTITY.get_or_init(|| Mutex::new(None))
}

pub(super) fn register(identity: &Identity) -> Result<(), String> {
    if identity.id.trim().is_empty() {
        return Err("notification identity id cannot be empty".into());
    }
    *identity_cell().lock().unwrap_or_else(|e| e.into_inner()) = Some(identity.clone());
    Ok(())
}

pub(super) fn registered() -> bool {
    identity_cell().lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn current_identity() -> Identity {
    identity_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

pub(super) fn ensure_started() {
    let _ = worker();
}

pub(super) fn post(n: Notification) -> bool {
    worker().try_send(Command::Post(n)).is_ok()
}

pub(super) fn remove(ids: &[&str]) {
    let ids = ids.iter().map(|s| (*s).to_owned()).collect();
    drop(worker().try_send(Command::Remove(ids)));
}

fn worker() -> &'static Sender<Command> {
    WORKER.get_or_init(|| {
        let (tx, rx) = async_channel::bounded(128);
        let transport = detect_transport();
        drop(
            std::thread::Builder::new()
                .name("rune-notify".into())
                .spawn(move || {
                    let exec = LocalExecutor::new();
                    async_io::block_on(exec.run(async move {
                        if let Ok(conn) = Connection::session().await {
                            run_worker(conn, transport, rx, current_identity, super::deliver).await;
                        }
                    }));
                }),
        );
        tx
    })
}

fn detect_transport() -> Transport {
    detect_transport_from(
        std::env::var_os("FLATPAK_ID").as_deref().and_then(|v| v.to_str()),
        Path::new("/.flatpak-info").exists(),
    )
}

fn detect_transport_from(flatpak_id: Option<&str>, flatpak_info_exists: bool) -> Transport {
    if flatpak_info_exists || flatpak_id.is_some_and(|id| !id.trim().is_empty()) {
        Transport::Portal
    } else {
        Transport::Fdo
    }
}

#[derive(Default)]
struct Tracked {
    order: VecDeque<String>,
    by_id: HashMap<String, Vec<(String, String)>>,
    fdo_to_id: HashMap<u32, String>,
    id_to_fdo: HashMap<String, u32>,
    tokens: HashMap<u32, String>,
}

impl Tracked {
    fn record(&mut self, id: String, info: Vec<(String, String)>, fdo_id: Option<u32>) {
        if let Some(old_fdo) = self.id_to_fdo.get(&id).copied()
            && fdo_id != Some(old_fdo)
        {
            self.fdo_to_id.remove(&old_fdo);
            self.tokens.remove(&old_fdo);
        }
        self.by_id.insert(id.clone(), info);
        if let Some(nid) = fdo_id {
            self.id_to_fdo.insert(id.clone(), nid);
            self.fdo_to_id.insert(nid, id.clone());
        }
        self.order.retain(|existing| existing != &id);
        self.order.push_back(id);
        while self.order.len() > MAX_TRACKED {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
                if let Some(old_fdo) = self.id_to_fdo.remove(&oldest) {
                    self.fdo_to_id.remove(&old_fdo);
                    self.tokens.remove(&old_fdo);
                }
            }
        }
    }

    fn fdo_replaces(&self, id: &str) -> u32 {
        self.id_to_fdo.get(id).copied().unwrap_or(0)
    }

    fn set_token(&mut self, fdo_id: u32, token: String) {
        if !token.is_empty() && self.fdo_to_id.contains_key(&fdo_id) {
            self.tokens.insert(fdo_id, token);
        }
    }

    fn click_fdo(&mut self, fdo_id: u32, action_key: &str) -> Option<Click> {
        let id = self.fdo_to_id.get(&fdo_id)?.clone();
        let mut info = self.by_id.get(&id).cloned().unwrap_or_default();
        if action_key != DEFAULT_ACTION && !action_key.is_empty() {
            info.push((ACTION.to_owned(), action_key.to_owned()));
        }
        if let Some(token) = self.tokens.remove(&fdo_id) {
            info.push((ACTIVATION_TOKEN.to_owned(), token));
        }
        Some(Click { id, info })
    }

    fn click_portal(&self, id: &str, action: &str) -> Option<Click> {
        let mut info = self.by_id.get(id)?.clone();
        if action != DEFAULT_ACTION && !action.is_empty() {
            info.push((ACTION.to_owned(), action.to_owned()));
        }
        Some(Click { id: id.to_owned(), info })
    }

    fn close_fdo(&mut self, fdo_id: u32) {
        if let Some(id) = self.fdo_to_id.get(&fdo_id) {
            self.id_to_fdo.remove(id);
        }
    }

    fn remove_id(&mut self, id: &str) -> Option<u32> {
        self.by_id.remove(id);
        self.order.retain(|existing| existing != id);
        let fdo = self.id_to_fdo.remove(id);
        if let Some(nid) = fdo {
            self.fdo_to_id.remove(&nid);
            self.tokens.remove(&nid);
        }
        fdo
    }
}

fn fdo_actions(n: &Notification) -> Vec<String> {
    let mut actions = vec![DEFAULT_ACTION.to_owned(), "Open".to_owned()];
    for action in &n.actions {
        if let Action::Button { id, label } = action {
            actions.push(id.clone());
            actions.push(label.clone());
        }
    }
    actions
}

fn fdo_hints<'a>(identity: &'a Identity, n: &'a Notification) -> HashMap<&'a str, Value<'a>> {
    let mut hints: HashMap<&'a str, Value<'a>> = HashMap::new();
    if !identity.id.trim().is_empty() {
        hints.insert("desktop-entry", Value::from(identity.id.as_str()));
    }
    hints.insert("category", Value::from("im.received"));
    hints.insert("suppress-sound", Value::from(!n.sound));
    if n.sound {
        hints.insert("sound-name", Value::from("message-new-instant"));
    }
    hints
}

fn fdo_app_icon(identity: &Identity) -> String {
    identity
        .icon
        .as_ref()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| identity.id.clone())
}

fn portal_payload<'a>(n: &'a Notification, portal_version: u32) -> HashMap<&'a str, Value<'a>> {
    let mut dict: HashMap<&'a str, Value<'a>> = HashMap::new();
    dict.insert("title", Value::from(n.title.as_str()));
    if !n.body.is_empty() {
        dict.insert("body", Value::from(n.body.as_str()));
    }
    dict.insert("default-action", Value::from(DEFAULT_ACTION));
    dict.insert("default-action-target", Value::from(n.id.as_str()));
    dict.insert("priority", Value::from("normal"));
    let buttons: Vec<HashMap<&'a str, Value<'a>>> = n
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::Button { id, label } => {
                let mut b = HashMap::new();
                b.insert("label", Value::from(label.as_str()));
                b.insert("action", Value::from(id.as_str()));
                b.insert("target", Value::from(n.id.as_str()));
                Some(b)
            }
            Action::Reply { .. } => None,
        })
        .collect();
    if !buttons.is_empty() {
        dict.insert("buttons", Value::from(buttons));
    }
    if portal_version >= 2 {
        dict.insert("category", Value::from("im.received"));
        if n.sound {
            dict.insert("sound", Value::from("default"));
        }
    }
    dict
}

async fn run_worker<F, D>(
    conn: Connection,
    transport: Transport,
    rx: Receiver<Command>,
    get_identity: F,
    deliver: D,
) where
    F: Fn() -> Identity,
    D: Fn(Click),
{
    match transport {
        Transport::Fdo => run_fdo(conn, rx, get_identity, deliver).await,
        Transport::Portal => run_portal(conn, rx, deliver).await,
    }
}

fn fdo_body(body: &str, escape_markup: bool) -> std::borrow::Cow<'_, str> {
    if !escape_markup || !body.contains(['&', '<', '>']) {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut escaped = String::with_capacity(body.len());
    for ch in body.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    std::borrow::Cow::Owned(escaped)
}

async fn run_fdo<F, D>(conn: Connection, rx: Receiver<Command>, get_identity: F, deliver: D)
where
    F: Fn() -> Identity,
    D: Fn(Click),
{
    let Ok(proxy) = Proxy::new(&conn, FDO_DEST, FDO_PATH, FDO_IFACE).await else { return };
    let escape_markup = proxy
        .call::<_, _, Vec<String>>("GetCapabilities", &())
        .await
        .is_ok_and(|caps| caps.iter().any(|cap| cap == "body-markup"));
    let Ok(mut tokens_stream) = proxy.receive_signal("ActivationToken").await else { return };
    let Ok(mut invoked_stream) = proxy.receive_signal("ActionInvoked").await else { return };
    let Ok(mut closed_stream) = proxy.receive_signal("NotificationClosed").await else { return };

    let mut tracked = Tracked::default();
    enum Event {
        Cmd(Option<Command>),
        Token(Option<(u32, String)>),
        Invoked(Option<(u32, String)>),
        Closed(Option<(u32, u32)>),
    }

    loop {
        let cmd_fut = async { Event::Cmd(rx.recv().await.ok()) };
        let tok_fut = async {
            Event::Token(
                tokens_stream
                    .next()
                    .await
                    .and_then(|msg| msg.body().deserialize::<(u32, String)>().ok()),
            )
        };
        let inv_fut = async {
            Event::Invoked(
                invoked_stream
                    .next()
                    .await
                    .and_then(|msg| msg.body().deserialize::<(u32, String)>().ok()),
            )
        };
        let cls_fut = async {
            Event::Closed(
                closed_stream
                    .next()
                    .await
                    .and_then(|msg| msg.body().deserialize::<(u32, u32)>().ok()),
            )
        };

        match cmd_fut.or(tok_fut).or(inv_fut).or(cls_fut).await {
            Event::Cmd(Some(Command::Post(n))) => {
                let identity = get_identity();
                let replaces = tracked.fdo_replaces(&n.id);
                let app_name = if identity.name.is_empty() {
                    identity.id.as_str()
                } else {
                    identity.name.as_str()
                };
                let app_icon = fdo_app_icon(&identity);
                let actions = fdo_actions(&n);
                let action_refs: Vec<&str> = actions.iter().map(String::as_str).collect();
                let hints = fdo_hints(&identity, &n);
                let body = fdo_body(&n.body, escape_markup);
                let reply: Result<u32, _> = proxy
                    .call(
                        "Notify",
                        &(
                            app_name,
                            replaces,
                            app_icon.as_str(),
                            n.title.as_str(),
                            body.as_ref(),
                            action_refs,
                            hints,
                            -1_i32,
                        ),
                    )
                    .await;
                if let Ok(nid) = reply {
                    tracked.record(n.id, n.info, Some(nid));
                }
            }
            Event::Cmd(Some(Command::Remove(ids))) => {
                for id in ids {
                    if let Some(nid) = tracked.remove_id(&id) {
                        drop(proxy.call::<_, _, ()>("CloseNotification", &(nid,)).await);
                    }
                }
            }
            Event::Cmd(None) => break,
            Event::Token(Some((nid, token))) => tracked.set_token(nid, token),
            Event::Invoked(Some((nid, action_key))) => {
                if let Some(click) = tracked.click_fdo(nid, &action_key) {
                    deliver(click);
                }
            }
            Event::Closed(Some((nid, _reason))) => tracked.close_fdo(nid),
            Event::Token(None) | Event::Invoked(None) | Event::Closed(None) => {}
        }
    }
}

async fn run_portal<D>(conn: Connection, rx: Receiver<Command>, deliver: D)
where
    D: Fn(Click),
{
    let Ok(proxy) = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, PORTAL_IFACE).await else {
        return;
    };
    let version: u32 = proxy.get_property("version").await.unwrap_or(1);
    let Ok(mut invoked_stream) = proxy.receive_signal("ActionInvoked").await else { return };

    let mut tracked = Tracked::default();
    enum Event {
        Cmd(Option<Command>),
        Invoked(Option<(String, String, Vec<OwnedValue>)>),
    }

    loop {
        let cmd_fut = async { Event::Cmd(rx.recv().await.ok()) };
        let inv_fut = async {
            Event::Invoked(
                invoked_stream
                    .next()
                    .await
                    .and_then(|msg| msg.body().deserialize::<(String, String, Vec<OwnedValue>)>().ok()),
            )
        };

        match cmd_fut.or(inv_fut).await {
            Event::Cmd(Some(Command::Post(n))) => {
                let payload = portal_payload(&n, version);
                let reply: Result<(), _> = proxy.call("AddNotification", &(n.id.as_str(), payload)).await;
                if reply.is_ok() {
                    tracked.record(n.id, n.info, None);
                }
            }
            Event::Cmd(Some(Command::Remove(ids))) => {
                for id in ids {
                    let _ = tracked.remove_id(&id);
                    drop(
                        proxy
                            .call::<_, _, ()>("RemoveNotification", &(id.as_str(),))
                            .await,
                    );
                }
            }
            Event::Cmd(None) => break,
            Event::Invoked(Some((id, action, _params))) => {
                if let Some(click) = tracked.click_portal(&id, &action) {
                    deliver(click);
                }
            }
            Event::Invoked(None) => {}
        }
    }
}

#[cfg(test)]
mod tests;
