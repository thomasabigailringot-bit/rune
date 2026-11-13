//! Web-style video player controls over any video element: a play button, a seek bar with a
//! buffered fill and a hover time, volume, the playback rate, full screen, a centre flash for
//! what a key did, and controls that hide while the video plays.
//!
//! The pieces follow [`ToastStack`](crate::ToastStack)'s pattern:
//!
//! - **Logic:** [`PlayerFacts`] is what the app tells the controls each view, [`PlayerAction`] is
//!   what the user asks, and [`player_key`] maps the web player's keys to actions. All of it is
//!   pure.
//! - **State:** the app owns a [`PlayerChrome`] (when the controls show, the seek hover, the
//!   flash). Handlers step it, and `Tx::wake_at` runs the view when the controls must hide. The
//!   app's `poll` calls [`PlayerChrome::poll`] and returns its answer, and stores the clock it
//!   gets for the next view.
//! - **View:** [`PlayerControls`] builds the stage around the app's video element and sends every
//!   request to `on_action`. The app executes it on its player.
//!
//! The app's `on_key` calls [`player_key`] and executes what it returns, and records the key's
//! activity with `chrome.step(PlayerEvent::Acted(action), ..)` (or `PlayerEvent::Activity` for a
//! key that means nothing here).

mod logic;
mod view;

pub use logic::{
    FLASH_SECS, Flash, HIDE_AFTER, PlayerAction, PlayerChrome, PlayerEvent, PlayerFacts, VOLUME_STEP, clock_label,
    player_key, rate_label, step_rate,
};
pub use view::PlayerControls;
