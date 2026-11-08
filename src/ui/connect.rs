//! `app.connect`: asks BlueZ to connect a pair, by its MAC as the action's
//! string target - the window's connect banner runs it, and anything else
//! that offers a connect can. Not a command: there is no AAP link to send one
//! on. The call is `bluez::connect_device`, on the tokio runtime, its result
//! back on a `oneshot`; AAP comes as for any connect - see
//! `crate::attachment`.
//!
//! A failure is logged, and toasted in the window when the window's banner
//! asked for it ([`press`]). The pairs a connect is under way to, whatever
//! started it, are a [`Connecting`] the window follows.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use bluer::Address;
use gtk::{gio, glib};

use crate::bluez;

/// The action's name, under `app.`.
pub const ACTION: &str = "connect";

thread_local! {
    /// The pair the window's banner is asking for, and the window's toasts,
    /// while [`press`] runs the action: how the action tells the window's
    /// press from any other. Only ever set on the main thread, around an
    /// activation that runs before it returns
    /// (`docs/protocol/linux/desktop.md#gapplication`).
    static FROM_WINDOW: RefCell<Option<(Address, adw::ToastOverlay)>> =
        const { RefCell::new(None) };
}

/// The pairs a connect is under way to, on the main context, and whoever
/// follows them: each follower is called after every change. Clones share
/// it.
#[derive(Clone, Default)]
pub struct Connecting(Rc<ConnectingInner>);

#[derive(Default)]
struct ConnectingInner {
    pending: RefCell<HashSet<Address>>,
    followers: RefCell<Vec<Rc<dyn Fn()>>>,
}

impl Connecting {
    /// Whether a connect to `mac` is under way.
    pub fn contains(&self, mac: Address) -> bool {
        self.0.pending.borrow().contains(&mac)
    }

    /// Calls `follower` after every change, from now on.
    pub fn follow(&self, follower: impl Fn() + 'static) {
        self.0.followers.borrow_mut().push(Rc::new(follower));
    }

    /// Records a connect to `mac` as under way; false if one was already.
    fn start(&self, mac: Address) -> bool {
        let started = self.0.pending.borrow_mut().insert(mac);
        if started {
            self.changed();
        }
        started
    }

    /// Records the connect to `mac` as ended.
    fn end(&self, mac: Address) {
        if self.0.pending.borrow_mut().remove(&mac) {
            self.changed();
        }
    }

    /// Calls each follower, from a copy of the list, so that one may follow
    /// or look again.
    fn changed(&self) {
        let followers = self.0.followers.borrow().clone();
        for follower in followers {
            follower();
        }
    }
}

/// Adds `app.connect` to `app`. Each activation connects the pair its target
/// names, unless a connect to that pair is under way already: an activation
/// while one is under way does nothing, rather than BlueZ refusing it as in progress
/// (`docs/protocol/linux/bluez.md#device1connect`). Returns the pairs a
/// connect is under way to.
pub fn add_action(app: &adw::Application, runtime: tokio::runtime::Handle) -> Connecting {
    let action = gio::SimpleAction::new(ACTION, Some(glib::VariantTy::STRING));
    let pending = Connecting::default();
    let connecting = pending.clone();
    action.connect_activate(move |_, target| {
        let Some(mac) = target
            .and_then(|t| t.str())
            .and_then(|t| t.parse::<Address>().ok())
        else {
            tracing::warn!("app.{ACTION} needs a MAC, got {target:?}");
            return;
        };
        let toasts = FROM_WINDOW.with_borrow(|asked| {
            asked
                .as_ref()
                .filter(|(asked, _)| *asked == mac)
                .map(|(_, toasts)| toasts.clone())
        });
        if !pending.start(mac) {
            tracing::debug!("already connecting {mac}");
            return;
        }
        tracing::info!("asking BlueZ to connect {mac}");
        // One bound over the bus connection and the call together.
        let (done, connected) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let connecting = async {
                let conn = zbus::connection::Builder::system()?.build().await?;
                bluez::connect_device(&conn, mac).await
            };
            let result = tokio::time::timeout(bluez::DEVICE_CONNECT_LIMIT, connecting)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "no answer to the connect within {:?}",
                        bluez::DEVICE_CONNECT_LIMIT
                    )
                })
                .and_then(|result| result);
            let _ = done.send(result);
        });
        let pending = pending.clone();
        glib::spawn_future_local(async move {
            // A sender dropped unsent: the task ended without an answer.
            let result = connected
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the connect to {mac} did not finish")));
            pending.end(mac);
            match result {
                Ok(()) => tracing::info!("BlueZ connected {mac}"),
                Err(e) => {
                    tracing::warn!("failed to connect {mac}: {e:#}");
                    if let Some(toasts) = toasts {
                        toasts.add_toast(adw::Toast::new("Couldn’t connect the AirPods"));
                    }
                }
            }
        });
    });
    app.add_action(&action);
    connecting
}

/// Runs `app.connect` for `mac` as the window's banner does: a failure is
/// toasted in `toasts` as well as logged.
pub fn press(app: &gtk::Application, mac: Address, toasts: &adw::ToastOverlay) {
    FROM_WINDOW.set(Some((mac, toasts.clone())));
    app.activate_action(ACTION, Some(&mac.to_string().to_variant()));
    FROM_WINDOW.set(None);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const MAC: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);

    /// A follower is called once per change, and only for one: a second
    /// start while the connect is under way, and an end for a pair with
    /// none under way, change nothing.
    #[test]
    fn followers_hear_each_change_once() {
        let connecting = Connecting::default();
        let calls = Rc::new(Cell::new(0));
        let counted = calls.clone();
        connecting.follow(move || counted.set(counted.get() + 1));

        assert!(connecting.start(MAC));
        assert_eq!(calls.get(), 1);
        assert!(connecting.contains(MAC));

        assert!(!connecting.start(MAC), "started twice");
        assert_eq!(calls.get(), 1);

        connecting.end(MAC);
        assert_eq!(calls.get(), 2);
        assert!(!connecting.contains(MAC));

        connecting.end(MAC);
        assert_eq!(calls.get(), 2, "an end with none under way");
    }
}
