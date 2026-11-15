//! The switch for the AirPods' microphone over AAP, which `crate::mic`
//! offers as an input on the tokio side, reached over a watch channel; and
//! its row in Preferences, which says why it is not offered, when it is
//! not.
//!
//! The Preferences switch turns it off and on through the application's
//! stateful action [`ACTION`]. It starts as `crate::settings` saved it - off
//! unless turned on - and a change to it is saved there.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use tokio::sync::watch;

use crate::mic::Availability;
use crate::settings::Saver;

/// The application action that turns the microphone on and off, with a
/// boolean state. The Preferences switch is bound to `app.` and this.
pub(super) const ACTION: &str = "mic-over-aap";

/// What the row says under its title while the microphone can be offered.
const SUBTITLE: &str = "Experimental. Use the AirPods as a microphone; the music may pause \
                        briefly after each recording";

/// Adds the action `app.mic-over-aap` to `app`, starting as `settings` holds
/// it. Each change goes to `settings`, which saves it, and to `switch`,
/// which `crate::mic::Mic` reads.
pub fn add_action(app: &impl IsA<gio::Application>, settings: Saver, switch: watch::Sender<bool>) {
    let app = app.clone().upcast::<gio::Application>();
    let on = settings.get().mic_over_aap;
    if on {
        tracing::info!("the microphone over AAP on, as saved");
    }
    switch.send_replace(on);

    let action = gio::SimpleAction::new_stateful(ACTION, None, &on.to_variant());
    action.connect_change_state(move |action, state| {
        let Some(on) = state.and_then(|s| s.get::<bool>()) else {
            return;
        };
        action.set_state(&on.to_variant());
        tracing::info!("the microphone over AAP {}", if on { "on" } else { "off" });
        settings.change(|s| s.mic_over_aap = on);
        // Never waits: the watch keeps the latest alone.
        switch.send_replace(on);
    });
    app.add_action(&action);
}

/// The row for the switch, bound to [`ACTION`], as the other settings'
/// switches are to theirs. While the microphone cannot be offered - this
/// build has none, no decoder, no PipeWire - the row says why, from
/// `availability` as it changes, and is insensitive while the switch is
/// off: one that is on can always be turned off.
pub(super) fn row(availability: watch::Receiver<Availability>) -> adw::ActionRow {
    let switch = gtk::Switch::builder()
        .name("mic-over-aap")
        .action_name(format!("app.{ACTION}"))
        .valign(gtk::Align::Center)
        .build();
    let row = adw::ActionRow::builder()
        .name("mic-over-aap-row")
        .title("AirPods microphone")
        .subtitle(SUBTITLE)
        .activatable_widget(&switch)
        .build();
    row.add_suffix(&switch);
    follow(&row, &switch, availability);
    row
}

/// What the row shows for `availability`, with the switch `on` or off:
/// whether it is sensitive, and the line under its title.
pub(super) fn shown(availability: &Availability, on: bool) -> (bool, String) {
    match availability {
        // Not asked yet: PipeWire is asked once the switch is on.
        Availability::Unknown | Availability::Available => (true, SUBTITLE.to_string()),
        Availability::Unavailable(reason) => (on, format!("Not available: {reason}")),
    }
}

/// Draws `row` from `availability` and `switch` now, and again at every
/// change of either, on the main context, until the row is gone.
fn follow(
    row: &adw::ActionRow,
    switch: &gtk::Switch,
    mut availability: watch::Receiver<Availability>,
) {
    // Cloned out of the watch: never held across an await.
    let last = Rc::new(RefCell::new(availability.borrow_and_update().clone()));
    let draw = {
        let (row, switch, last) = (row.downgrade(), switch.downgrade(), last.clone());
        move || {
            let (Some(row), Some(switch)) = (row.upgrade(), switch.upgrade()) else {
                return;
            };
            let (sensitive, subtitle) = shown(&last.borrow(), switch.is_active());
            row.set_sensitive(sensitive);
            row.set_subtitle(&subtitle);
        }
    };
    draw();
    let redraw = draw.clone();
    switch.connect_active_notify(move |_| redraw());
    glib::spawn_future_local(async move {
        while availability.changed().await.is_ok() {
            *last.borrow_mut() = availability.borrow_and_update().clone();
            draw();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unavailable, the row says why, and is insensitive with the switch
    /// off but not with it on; otherwise it is sensitive and says what the
    /// switch does.
    #[test]
    fn says_why_it_is_not_offered() {
        let missing = Availability::Unavailable("this build has no microphone support".into());
        let (sensitive, subtitle) = shown(&missing, false);
        assert!(!sensitive);
        assert_eq!(
            subtitle,
            "Not available: this build has no microphone support"
        );
        assert!(shown(&missing, true).0);
        for availability in [Availability::Available, Availability::Unknown] {
            assert_eq!(shown(&availability, false), (true, SUBTITLE.to_string()));
        }
    }
}
