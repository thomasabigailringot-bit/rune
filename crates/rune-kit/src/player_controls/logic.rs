//! The pure part of [`PlayerControls`](super::PlayerControls): what the app tells the controls,
//! what the user asks, the web player's keys, and the chrome state machine that decides when the
//! controls show. Nothing here builds an `El`.

use rune::{Key, KeyEvent};

/// Seconds without pointer or key activity after which controls hide while a video plays.
pub const HIDE_AFTER: f64 = 3.0;
/// Seconds a centre flash (Play, Pause, a volume level) stays up.
pub const FLASH_SECS: f64 = 0.5;
/// How much one volume key press or wheel notch changes the volume.
pub const VOLUME_STEP: f32 = 0.05;

/// What the app tells the controls each view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerFacts {
    pub playing: bool,
    /// The play position in seconds.
    pub time: f64,
    pub duration: Option<f64>,
    /// Seconds played plus downloaded, when the app knows.
    pub buffered: Option<f64>,
    /// 0 to 1.
    pub volume: f32,
    pub muted: bool,
    pub full_screen: bool,
    /// A live stream: no seek bar, no time, a LIVE mark.
    pub live: bool,
    pub rate: f32,
}

impl Default for PlayerFacts {
    fn default() -> Self {
        Self {
            playing: false,
            time: 0.0,
            duration: None,
            buffered: None,
            volume: 1.0,
            muted: false,
            full_screen: false,
            live: false,
            rate: 1.0,
        }
    }
}

/// What the user asks. The app executes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerAction {
    TogglePlay,
    /// A seconds position while the seek bar is dragged.
    Scrub(f64),
    /// A seconds position on release or by key.
    Seek(f64),
    SeekBy(f64),
    /// An absolute level from the slider.
    Volume(f32),
    VolumeBy(f32),
    ToggleMute,
    ToggleFullScreen,
    Rate(f32),
}

/// The short feedback in the middle of the video after an action.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Flash {
    Play,
    Pause,
    Volume(f32),
    Muted,
    SeekBy(f64),
    Rate(f32),
}

impl Flash {
    /// A short text for flashes that have no icon.
    pub fn text(self) -> Option<String> {
        match self {
            Flash::Volume(v) => Some(format!("{} %", (v.clamp(0.0, 1.0) * 100.0).round() as i32)),
            Flash::SeekBy(s) => Some(format!("{}{} s", if s < 0.0 { "-" } else { "+" }, s.abs().round() as i64)),
            Flash::Rate(r) => Some(rate_label(r)),
            Flash::Play | Flash::Pause | Flash::Muted => None,
        }
    }
}

/// `m:ss`, or `h:mm:ss` from one hour.
pub fn clock_label(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// `1x`, `1.5x`, `0.25x`.
pub fn rate_label(rate: f32) -> String {
    let s = format!("{rate:.2}");
    format!("{}x", s.trim_end_matches('0').trim_end_matches('.'))
}

/// The rate after or before `current` in `rates` (ascending). At an end it stops, or with `wrap`
/// goes round to the other end. `None` when there is nowhere to go.
pub fn step_rate(rates: &[f32], current: f32, forward: bool, wrap: bool) -> Option<f32> {
    let eps = 1e-3;
    let next = if forward {
        rates.iter().copied().find(|r| *r > current + eps).or_else(|| wrap.then(|| rates.first().copied()).flatten())
    } else {
        rates.iter().copied().rev().find(|r| *r < current - eps).or_else(|| wrap.then(|| rates.last().copied()).flatten())
    };
    next.filter(|r| (r - current).abs() > eps)
}

/// The web player's keys. `None` when the key means nothing here, or the action cannot apply:
/// seek keys do nothing on a live stream, and the tenth, Home and End keys need a duration.
pub fn player_key(event: &KeyEvent, facts: &PlayerFacts, rates: &[f32]) -> Option<PlayerAction> {
    use PlayerAction::*;
    let m = event.mods;
    if m.ctrl || m.cmd || m.alt {
        return None;
    }
    let seek = |a: PlayerAction| (!facts.live).then_some(a);
    match &event.key {
        Key::Left => seek(SeekBy(-5.0)),
        Key::Right => seek(SeekBy(5.0)),
        Key::Up => Some(VolumeBy(VOLUME_STEP)),
        Key::Down => Some(VolumeBy(-VOLUME_STEP)),
        Key::Home => seek(Seek(0.0)),
        Key::End => facts.duration.and_then(|d| seek(Seek(d))),
        Key::Text(t) => {
            let mut chars = t.chars();
            let (c, None) = (chars.next()?, chars.next()) else { return None };
            match c.to_ascii_lowercase() {
                ' ' | 'k' => Some(TogglePlay),
                'j' => seek(SeekBy(-10.0)),
                'l' => seek(SeekBy(10.0)),
                'm' => Some(ToggleMute),
                'f' => Some(ToggleFullScreen),
                '<' => step_rate(rates, facts.rate, false, false).map(Rate),
                '>' => step_rate(rates, facts.rate, true, false).map(Rate),
                ',' if m.shift => step_rate(rates, facts.rate, false, false).map(Rate),
                '.' if m.shift => step_rate(rates, facts.rate, true, false).map(Rate),
                d @ '0'..='9' if !m.shift => {
                    let tenth = f64::from(d as u8 - b'0');
                    facts.duration.and_then(|total| seek(Seek(total * tenth / 10.0)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// What happens to the controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerEvent {
    /// The pointer moved over the stage, or a key came.
    Activity,
    /// The pointer left the stage.
    PointerLeft,
    /// The pointer came onto or left the bar (the controls, the seek bar included).
    BarHover(bool),
    /// A drag of the seek bar began.
    DragBegan,
    /// It ended, released or cancelled.
    DragEnded,
    /// The pointer is at this fraction (0 to 1) of the seek bar, or left it.
    SeekHover(Option<f64>),
    /// The seek bar is dragged to this position in seconds.
    Scrubbed(f64),
    /// The user asked for this. It records activity and the flash.
    Acted(PlayerAction),
}

/// The state of the controls. The app owns one, as it owns a [`ToastStack`](crate::ToastStack),
/// and steps it with [`PlayerChrome::step`]. Times are the app clock in seconds (`Tx::clock`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerChrome {
    activity: Option<f64>,
    over_stage: bool,
    over_bar: bool,
    dragging: bool,
    hover: Option<f64>,
    scrub: Option<f64>,
    flash: Option<(Flash, f64)>,
    /// Set by the app while one of its own menus is open: the controls stay up.
    pub pinned: bool,
    seen: (bool, bool),
}

impl PlayerChrome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies `event` at `now`. Returns the clock time at which the view must run again (give it
    /// to `Tx::wake_at`), or `None` when nothing is due.
    pub fn step(&mut self, event: PlayerEvent, now: f64, facts: &PlayerFacts) -> Option<f64> {
        match event {
            PlayerEvent::Activity => {
                self.activity = Some(now);
                self.over_stage = true;
            }
            PlayerEvent::PointerLeft => self.over_stage = false,
            PlayerEvent::BarHover(over) => {
                self.over_bar = over;
                self.activity = Some(now);
                if !over {
                    self.hover = None;
                }
            }
            PlayerEvent::DragBegan => {
                self.dragging = true;
                self.activity = Some(now);
            }
            PlayerEvent::DragEnded => {
                self.dragging = false;
                self.scrub = None;
                self.activity = Some(now);
            }
            PlayerEvent::SeekHover(fraction) => self.hover = fraction.map(|f| f.clamp(0.0, 1.0)),
            PlayerEvent::Scrubbed(seconds) => self.scrub = Some(seconds),
            PlayerEvent::Acted(action) => {
                self.activity = Some(now);
                if let Some(flash) = self.flash_for(action, facts) {
                    self.flash = Some((flash, now));
                }
            }
        }
        self.wake(now, facts)
    }

    fn flash_for(&self, action: PlayerAction, facts: &PlayerFacts) -> Option<Flash> {
        Some(match action {
            PlayerAction::TogglePlay if facts.playing => Flash::Pause,
            PlayerAction::TogglePlay => Flash::Play,
            PlayerAction::Volume(v) => Flash::Volume(v.clamp(0.0, 1.0)),
            PlayerAction::VolumeBy(d) => Flash::Volume((facts.volume + d).clamp(0.0, 1.0)),
            PlayerAction::ToggleMute if facts.muted => Flash::Volume(facts.volume),
            PlayerAction::ToggleMute => Flash::Muted,
            PlayerAction::SeekBy(s) => Flash::SeekBy(s),
            PlayerAction::Rate(r) => Flash::Rate(r),
            PlayerAction::Scrub(_) | PlayerAction::Seek(_) | PlayerAction::ToggleFullScreen => return None,
        })
    }

    /// When the view must run again, if something is due: the controls hide, or the flash ends.
    pub fn wake(&self, now: f64, facts: &PlayerFacts) -> Option<f64> {
        let hide = match self.activity {
            Some(a) if facts.playing && !self.pinned && !self.dragging && !self.over_bar && now < a + HIDE_AFTER => {
                Some(a + HIDE_AFTER)
            }
            _ => None,
        };
        let flash = self.flash.map(|(_, t)| t + FLASH_SECS).filter(|end| *end > now);
        match (hide, flash) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether the controls show: not playing, pinned, dragging, the pointer over the bar, or
    /// activity less than [`HIDE_AFTER`] seconds ago.
    pub fn shown(&self, now: f64, facts: &PlayerFacts) -> bool {
        !facts.playing
            || self.pinned
            || self.dragging
            || self.over_bar
            || self.activity.is_some_and(|a| now - a < HIDE_AFTER)
    }

    /// The pointer is over the video and the controls are hidden: hide the cursor too.
    pub fn cursor_hidden(&self, now: f64, facts: &PlayerFacts) -> bool {
        self.over_stage && !self.shown(now, facts)
    }

    /// The flash while it is younger than [`FLASH_SECS`].
    pub fn flash(&self, now: f64) -> Option<Flash> {
        self.flash.filter(|(_, t)| now - *t < FLASH_SECS).map(|(f, _)| f)
    }

    /// The seek bar's hover position as a fraction, while the pointer is over it.
    pub fn hover(&self) -> Option<f64> {
        self.hover
    }

    /// The position in seconds a drag of the seek bar holds.
    pub fn scrub(&self) -> Option<f64> {
        self.scrub
    }

    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// For the app's `poll`: whether what the view shows changed since the last call (the controls
    /// hid, the flash ended), so the poll returns true and the view runs again.
    pub fn poll(&mut self, now: f64, facts: &PlayerFacts) -> bool {
        let seen = (self.shown(now, facts), self.flash(now).is_some());
        std::mem::replace(&mut self.seen, seen) != seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn text(s: &str) -> KeyEvent {
        KeyEvent { key: Key::Text(s.into()), mods: Mods::default() }
    }
    fn key(key: Key) -> KeyEvent {
        KeyEvent { key, mods: Mods::default() }
    }
    fn vod() -> PlayerFacts {
        PlayerFacts { playing: true, time: 30.0, duration: Some(200.0), ..PlayerFacts::default() }
    }
    const RATES: [f32; 5] = [0.5, 1.0, 1.25, 1.5, 2.0];

    #[test]
    fn labels() {
        assert_eq!(clock_label(0.0), "0:00");
        assert_eq!(clock_label(42.9), "0:42");
        assert_eq!(clock_label(83.0), "1:23");
        assert_eq!(clock_label(3599.0), "59:59");
        assert_eq!(clock_label(3600.0), "1:00:00");
        assert_eq!(clock_label(5025.0), "1:23:45");
        assert_eq!(clock_label(-4.0), "0:00");
        assert_eq!(clock_label(f64::NAN), "0:00");
        assert_eq!(rate_label(1.0), "1x");
        assert_eq!(rate_label(1.5), "1.5x");
        assert_eq!(rate_label(0.25), "0.25x");
    }

    #[test]
    fn flash_texts() {
        assert_eq!(Flash::Volume(0.6).text().as_deref(), Some("60 %"));
        assert_eq!(Flash::SeekBy(10.0).text().as_deref(), Some("+10 s"));
        assert_eq!(Flash::SeekBy(-5.0).text().as_deref(), Some("-5 s"));
        assert_eq!(Flash::Rate(1.5).text().as_deref(), Some("1.5x"));
        assert_eq!(Flash::Play.text(), None);
    }

    #[test]
    fn keys_map_to_actions() {
        use PlayerAction::*;
        let f = vod();
        for t in [" ", "k", "K"] {
            assert_eq!(player_key(&text(t), &f, &RATES), Some(TogglePlay), "{t:?}");
        }
        assert_eq!(player_key(&text("j"), &f, &RATES), Some(SeekBy(-10.0)));
        assert_eq!(player_key(&text("l"), &f, &RATES), Some(SeekBy(10.0)));
        assert_eq!(player_key(&key(Key::Left), &f, &RATES), Some(SeekBy(-5.0)));
        assert_eq!(player_key(&key(Key::Right), &f, &RATES), Some(SeekBy(5.0)));
        assert_eq!(player_key(&key(Key::Up), &f, &RATES), Some(VolumeBy(0.05)));
        assert_eq!(player_key(&key(Key::Down), &f, &RATES), Some(VolumeBy(-0.05)));
        assert_eq!(player_key(&text("m"), &f, &RATES), Some(ToggleMute));
        assert_eq!(player_key(&text("f"), &f, &RATES), Some(ToggleFullScreen));
        assert_eq!(player_key(&text("5"), &f, &RATES), Some(Seek(100.0)));
        assert_eq!(player_key(&text("0"), &f, &RATES), Some(Seek(0.0)));
        assert_eq!(player_key(&key(Key::Home), &f, &RATES), Some(Seek(0.0)));
        assert_eq!(player_key(&key(Key::End), &f, &RATES), Some(Seek(200.0)));
        assert_eq!(player_key(&text("x"), &f, &RATES), None);
        assert_eq!(player_key(&text("ab"), &f, &RATES), None);
        assert_eq!(player_key(&key(Key::Escape), &f, &RATES), None);
    }

    #[test]
    fn rate_keys_step_and_stop_at_the_ends() {
        use PlayerAction::Rate;
        let at = |rate| PlayerFacts { rate, ..vod() };
        assert_eq!(player_key(&text(">"), &at(1.0), &RATES), Some(Rate(1.25)));
        assert_eq!(player_key(&text("<"), &at(1.0), &RATES), Some(Rate(0.5)));
        assert_eq!(player_key(&text(">"), &at(2.0), &RATES), None);
        assert_eq!(player_key(&text("<"), &at(0.5), &RATES), None);
        assert_eq!(player_key(&text(">"), &at(1.1), &RATES), Some(Rate(1.25)));
        let shifted = |s: &str| KeyEvent { key: Key::Text(s.into()), mods: Mods { shift: true, ..Mods::default() } };
        assert_eq!(player_key(&shifted("."), &at(1.0), &RATES), Some(Rate(1.25)));
        assert_eq!(player_key(&shifted(","), &at(1.0), &RATES), Some(Rate(0.5)));
        assert_eq!(player_key(&text("."), &at(1.0), &RATES), None, "plain period is not a rate key");
        assert_eq!(player_key(&text(">"), &at(1.0), &[]), None);
        assert_eq!(step_rate(&RATES, 2.0, true, true), Some(0.5), "the button wraps");
        assert_eq!(step_rate(&RATES, 0.5, false, true), Some(2.0));
        assert_eq!(step_rate(&[1.0], 1.0, true, true), None);
    }

    #[test]
    fn live_and_unknown_duration_drop_seek_keys() {
        use PlayerAction::*;
        let live = PlayerFacts { live: true, duration: None, ..vod() };
        for k in [text("j"), text("l"), key(Key::Left), key(Key::Right), key(Key::Home), key(Key::End), text("3")] {
            assert_eq!(player_key(&k, &live, &RATES), None, "{k:?}");
        }
        assert_eq!(player_key(&text(" "), &live, &RATES), Some(TogglePlay));
        assert_eq!(player_key(&key(Key::Up), &live, &RATES), Some(VolumeBy(0.05)));
        let unknown = PlayerFacts { duration: None, ..vod() };
        for k in [text("3"), key(Key::End)] {
            assert_eq!(player_key(&k, &unknown, &RATES), None, "{k:?}");
        }
        assert_eq!(player_key(&text("j"), &unknown, &RATES), Some(SeekBy(-10.0)));
        assert_eq!(player_key(&key(Key::Home), &unknown, &RATES), Some(Seek(0.0)));
    }

    #[test]
    fn modified_keys_are_ignored() {
        let f = vod();
        for mods in [
            Mods { ctrl: true, ..Mods::default() },
            Mods { cmd: true, ..Mods::default() },
            Mods { alt: true, ..Mods::default() },
        ] {
            assert_eq!(player_key(&KeyEvent { key: Key::Text("k".into()), mods }, &f, &RATES), None);
            assert_eq!(player_key(&KeyEvent { key: Key::Left, mods }, &f, &RATES), None);
        }
    }

    #[test]
    fn controls_hide_three_seconds_after_the_last_activity() {
        let f = vod();
        let mut c = PlayerChrome::new();
        assert!(!c.shown(10.0, &f), "a playing video starts with its controls hidden");
        assert_eq!(c.step(PlayerEvent::Activity, 10.0, &f), Some(13.0));
        assert!(c.shown(12.99, &f));
        assert!(!c.shown(13.0, &f));
        assert_eq!(c.step(PlayerEvent::Activity, 12.0, &f), Some(15.0), "activity moves the deadline");
        assert!(c.shown(14.9, &f));
        assert_eq!(c.wake(15.0, &f), None, "nothing is due once hidden");
    }

    #[test]
    fn paused_video_always_shows_its_controls() {
        let f = PlayerFacts { playing: false, ..vod() };
        let mut c = PlayerChrome::new();
        assert!(c.shown(100.0, &f));
        assert_eq!(c.step(PlayerEvent::Activity, 100.0, &f), None, "no hide wake while paused");
        assert!(c.shown(200.0, &f));
        assert!(!c.cursor_hidden(200.0, &f));
    }

    #[test]
    fn pinned_drag_and_bar_hover_keep_the_controls_up() {
        let f = vod();
        let mut c = PlayerChrome::new();
        c.step(PlayerEvent::Activity, 0.0, &f);
        c.pinned = true;
        assert!(c.shown(50.0, &f));
        c.pinned = false;
        assert!(!c.shown(50.0, &f));
        assert_eq!(c.step(PlayerEvent::DragBegan, 50.0, &f), None);
        assert!(c.shown(80.0, &f) && c.dragging());
        assert_eq!(c.step(PlayerEvent::DragEnded, 80.0, &f), Some(83.0), "release restarts the countdown");
        assert!(!c.shown(83.0, &f));
        c.step(PlayerEvent::BarHover(true), 90.0, &f);
        assert!(c.shown(200.0, &f));
        assert_eq!(c.step(PlayerEvent::BarHover(false), 200.0, &f), Some(203.0));
        assert!(!c.shown(203.0, &f));
    }

    #[test]
    fn cursor_hides_only_with_the_controls_and_over_the_video() {
        let f = vod();
        let mut c = PlayerChrome::new();
        assert!(!c.cursor_hidden(10.0, &f), "the pointer is not over the video yet");
        c.step(PlayerEvent::Activity, 10.0, &f);
        assert!(!c.cursor_hidden(12.0, &f), "the controls show");
        assert!(c.cursor_hidden(13.0, &f));
        c.step(PlayerEvent::PointerLeft, 13.5, &f);
        assert!(!c.cursor_hidden(14.0, &f), "the pointer left the video");
        c.step(PlayerEvent::Activity, 20.0, &f);
        assert!(!c.cursor_hidden(20.0, &f));
    }

    #[test]
    fn flash_records_the_action_and_expires() {
        let f = vod();
        let mut c = PlayerChrome::new();
        assert_eq!(c.step(PlayerEvent::Acted(PlayerAction::TogglePlay), 5.0, &f), Some(5.5), "the flash ends first");
        assert_eq!(c.flash(5.0), Some(Flash::Pause));
        assert_eq!(c.flash(5.49), Some(Flash::Pause));
        assert_eq!(c.flash(5.5), None);
        assert!(c.shown(7.0, &f), "an action is activity");
        let paused = PlayerFacts { playing: false, ..f };
        c.step(PlayerEvent::Acted(PlayerAction::TogglePlay), 6.0, &paused);
        assert_eq!(c.flash(6.0), Some(Flash::Play));
        c.step(PlayerEvent::Acted(PlayerAction::VolumeBy(0.05)), 7.0, &f);
        assert_eq!(c.flash(7.0), Some(Flash::Volume(1.0)), "the level clamps");
        c.step(PlayerEvent::Acted(PlayerAction::ToggleMute), 8.0, &f);
        assert_eq!(c.flash(8.0), Some(Flash::Muted));
        c.step(PlayerEvent::Acted(PlayerAction::SeekBy(-5.0)), 9.0, &f);
        assert_eq!(c.flash(9.0), Some(Flash::SeekBy(-5.0)));
        c.step(PlayerEvent::Acted(PlayerAction::Rate(1.5)), 9.2, &f);
        assert_eq!(c.flash(9.2), Some(Flash::Rate(1.5)));
        c.step(PlayerEvent::Acted(PlayerAction::Seek(40.0)), 9.4, &f);
        assert_eq!(c.flash(9.45), Some(Flash::Rate(1.5)), "a seek by drag has no flash of its own");
    }

    #[test]
    fn poll_reports_changes_once() {
        let f = vod();
        let mut c = PlayerChrome::new();
        assert!(!c.poll(0.0, &f));
        c.step(PlayerEvent::Activity, 1.0, &f);
        assert!(c.poll(1.0, &f), "the controls appeared");
        assert!(!c.poll(2.0, &f));
        assert!(c.poll(4.0, &f), "the controls hid");
        assert!(!c.poll(5.0, &f));
    }

    #[test]
    fn seek_hover_and_scrub_are_kept_while_they_last() {
        let f = vod();
        let mut c = PlayerChrome::new();
        c.step(PlayerEvent::SeekHover(Some(1.5)), 0.0, &f);
        assert_eq!(c.hover(), Some(1.0));
        c.step(PlayerEvent::BarHover(false), 1.0, &f);
        assert_eq!(c.hover(), None);
        c.step(PlayerEvent::Scrubbed(12.0), 1.0, &f);
        assert_eq!(c.scrub(), Some(12.0));
        c.step(PlayerEvent::DragEnded, 2.0, &f);
        assert_eq!(c.scrub(), None);
    }
}
