//! XDG Notification portal transport. Unlike FDO, the portal uses the app's string IDs.
use super::{
    Command, DEFAULT_ACTION, Event, Notification, PROBE_TIMEOUT, RPC_TIMEOUT, click_for,
    next_event, notification_signals, timeout,
};
use crate::{Action, Click};
use async_channel::Receiver;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedValue, Value},
};

pub(super) const NAME: &str = "org.freedesktop.portal.Desktop";
pub(super) const PATH: &str = "/org/freedesktop/portal/desktop";
pub(super) const INTERFACE: &str = "org.freedesktop.portal.Notification";
// Prefix button IDs so `app.*` IDs cannot become exported D-Bus actions or collide with the default.
const BUTTON_PREFIX: &str = "rune.";
// The portal does not signal dismissal, so retain metadata for a bounded number of recent posts.
const MAX_TRACKED: usize = 256;

fn payload(n: &Notification, version: u32) -> HashMap<&str, Value<'_>> {
    let mut dict = HashMap::from([
        ("title", Value::from(n.title.as_str())),
        ("default-action", Value::from(DEFAULT_ACTION)),
    ]);
    if !n.body.is_empty() {
        dict.insert("body", Value::from(n.body.as_str()));
    }
    let buttons: Vec<_> = n
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::Button { id, label } => Some(HashMap::from([
                ("label", Value::from(label.as_str())),
                ("action", Value::from(format!("{BUTTON_PREFIX}{id}"))),
            ])),
            Action::Reply { .. } => None,
        })
        .collect();
    if !buttons.is_empty() {
        dict.insert("buttons", Value::from(buttons));
    }
    if version >= 2 {
        dict.insert("category", Value::from("im.received"));
        dict.insert(
            "sound",
            Value::from(if n.sound { "default" } else { "silent" }),
        );
    }
    dict
}

pub(super) async fn run(
    connect: impl Future<Output = zbus::Result<Connection>>,
    commands: Receiver<Command>,
    ready: mpsc::SyncSender<bool>,
    deliver: impl Fn(Click),
) {
    let setup = async {
        let conn = connect.await?;
        let proxy = Proxy::new(&conn, NAME, PATH, INTERFACE).await?;
        let signals = notification_signals(&conn, NAME, PATH, INTERFACE).await?;
        let version = proxy.get_property::<u32>("version").await?;
        Ok::<_, zbus::Error>((proxy, signals, version))
    };
    let Some(Ok((proxy, mut signals, version))) = timeout(PROBE_TIMEOUT, setup).await else {
        ready.send(false).ok();
        return;
    };
    ready.send(true).ok();
    let mut shown = VecDeque::<(String, Vec<(String, String)>)>::new();
    loop {
        match next_event(&commands, &mut signals).await {
            Event::Command(Some(Command::Post(post))) => {
                let n = post.n;
                let args = (n.id.as_str(), payload(&n, version));
                if let Some(Ok(())) =
                    timeout(RPC_TIMEOUT, proxy.call("AddNotification", &args)).await
                {
                    shown.retain(|(id, _)| *id != n.id);
                    if shown.len() == MAX_TRACKED {
                        shown.pop_front();
                    }
                    shown.push_back((n.id, n.info));
                }
            }
            Event::Command(Some(Command::Remove(ids))) => {
                for id in ids {
                    shown.retain(|(shown_id, _)| *shown_id != id);
                    // Portal notifications can outlast this process.
                    let _ = timeout(
                        RPC_TIMEOUT,
                        proxy.call::<_, _, ()>("RemoveNotification", &(id.as_str(),)),
                    )
                    .await;
                }
            }
            Event::Signal(Some(Ok(message))) => {
                if message.header().member().map(|m| m.as_str()) != Some("ActionInvoked") {
                    continue;
                }
                let Ok((id, action, params)) = message
                    .body()
                    .deserialize::<(String, String, Vec<OwnedValue>)>()
                else {
                    continue;
                };
                let Some((_, info)) = shown.iter().find(|(shown_id, _)| *shown_id == id) else {
                    continue;
                };
                let button = action.strip_prefix(BUTTON_PREFIX);
                if action == DEFAULT_ACTION || button.is_some() {
                    let token = (version >= 2).then(|| activation_token(params)).flatten();
                    deliver(click_for(&id, info, button, token));
                }
            }
            Event::Signal(Some(Err(_))) => {}
            Event::Command(None) | Event::Signal(None) => return,
        }
    }
}

fn activation_token(params: Vec<OwnedValue>) -> Option<String> {
    // No action target is sent, so platform data (if present) is the first parameter.
    let mut data = HashMap::<String, OwnedValue>::try_from(params.into_iter().next()?).ok()?;
    String::try_from(data.remove("activation-token")?)
        .ok()
        .filter(|token| !token.is_empty())
}
