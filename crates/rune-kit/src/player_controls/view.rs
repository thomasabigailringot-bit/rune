//! The view of [`PlayerControls`]: the stage, the bottom bar, the seek bar, the volume slider and
//! the centre signs.

use std::rc::Rc;

use rune::{Color, Cursor, Drag, El, Phase, Transition, Tx, col, el, icon, row, text};

use super::logic::{
    Flash, PlayerAction, PlayerChrome, PlayerEvent, PlayerFacts, VOLUME_STEP, clock_label, rate_label, step_rate,
};
use crate::{Kit, glyphs};

/// Seconds a key step of the seek bar (from assistive technology) moves by.
const SEEK_STEP: f64 = 5.0;
/// Height of the seek bar's hit area, and the widths of the volume slider.
const SEEK_HIT: f32 = 16.0;
const VOLUME_W: f32 = 64.0;
const BUTTON: f32 = 36.0;

type Reach<S> = fn(&mut S) -> &mut PlayerChrome;
type OnAction<S> = Rc<dyn Fn(&mut S, PlayerAction, &mut Tx)>;

/// Web-style controls over a video. See the [module docs](super).
pub struct PlayerControls<S> {
    facts: PlayerFacts,
    shown: bool,
    cursor_hidden: bool,
    flash: Option<Flash>,
    hover: Option<f64>,
    scrub: Option<f64>,
    reach: Option<Reach<S>>,
    on_action: Option<OnAction<S>>,
    top: Option<El>,
    trailing: Vec<El>,
    rates: Vec<f32>,
}

/// How handlers reach the chrome and the app, shared by every handler of one stage.
struct Wire<S> {
    reach: Option<Reach<S>>,
    on_action: Option<OnAction<S>>,
    facts: PlayerFacts,
}

impl<S: 'static> Wire<S> {
    fn event(&self, s: &mut S, event: PlayerEvent, tx: &mut Tx) {
        let Some(reach) = self.reach else { return };
        if let Some(due) = reach(s).step(event, tx.clock(), &self.facts) {
            tx.wake_at(due);
        }
    }

    fn act(&self, s: &mut S, action: PlayerAction, tx: &mut Tx) {
        self.event(s, PlayerEvent::Acted(action), tx);
        if let Some(on_action) = &self.on_action {
            on_action(s, action, tx);
        }
    }

    /// Pointer activity. When the controls stay as they are, the view has nothing to change.
    fn activity(&self, s: &mut S, tx: &mut Tx) {
        let Some(reach) = self.reach else { return };
        let now = tx.clock();
        let look = |c: &PlayerChrome| (c.shown(now, &self.facts), c.cursor_hidden(now, &self.facts));
        let before = look(reach(s));
        self.event(s, PlayerEvent::Activity, tx);
        let after = look(reach(s));
        if before == after && after.0 {
            tx.live_only();
        }
    }
}

impl<S: 'static> PlayerControls<S> {
    /// The controls for what the app shows now. `now` is the app clock in seconds.
    pub fn new(facts: PlayerFacts, chrome: &PlayerChrome, now: f64) -> Self {
        Self {
            facts,
            shown: chrome.shown(now, &facts),
            cursor_hidden: chrome.cursor_hidden(now, &facts),
            flash: chrome.flash(now),
            hover: chrome.hover(),
            scrub: chrome.scrub(),
            reach: None,
            on_action: None,
            top: None,
            trailing: Vec::new(),
            rates: Vec::new(),
        }
    }

    /// How handlers reach the app's [`PlayerChrome`]: `.chrome(|s: &mut App| &mut s.chrome)`.
    pub fn chrome(mut self, reach: fn(&mut S) -> &mut PlayerChrome) -> Self {
        self.reach = Some(reach);
        self
    }

    /// Every user request goes here, after the chrome has recorded it.
    pub fn on_action(mut self, f: impl Fn(&mut S, PlayerAction, &mut Tx) + 'static) -> Self {
        self.on_action = Some(Rc::new(f));
        self
    }

    /// A bar over the top of the video (a back button, a title). It hides with the controls.
    pub fn top(mut self, content: El) -> Self {
        self.top = Some(content);
        self
    }

    /// The app's own buttons in the bar, before the full screen button.
    pub fn trailing(mut self, buttons: Vec<El>) -> Self {
        self.trailing = buttons;
        self
    }

    /// The playback rates, ascending. A button shows the rate and a press goes to the next one.
    pub fn rates(mut self, rates: &[f32]) -> Self {
        self.rates = rates.to_vec();
        self
    }

    /// The stage: `video` (the app's own video element, sized to the stage) on black, with the
    /// controls over it. The app reports `playing: true` while a video loads, so no play sign
    /// shows over a spinner.
    pub fn build(self, k: &Kit, video: El) -> El {
        let wire = Rc::new(Wire { reach: self.reach, on_action: self.on_action.clone(), facts: self.facts });
        let mut stage = el()
            .key("player")
            .role("group")
            .label("Video player")
            .w_full()
            .h_full()
            .fill(Color::BLACK)
            .child(video.w_full().h_full());
        if !self.facts.playing {
            stage = stage.child(centre(k, "player-paused", sign(k, &glyphs::PLAY, 36.0, 72.0)));
        }
        if let Some(flash) = self.flash {
            stage = stage.child(centre(k, "player-flash", flash_pill(k, flash)));
        }
        if self.shown {
            if let Some(top) = &self.top {
                // The pointer over the top bar keeps the controls up, as over the bottom bar.
                let w = wire.clone();
                stage = stage.child(top_bar(k, top.clone()).on_hover_change(move |s: &mut S, over, tx: &mut Tx| {
                    w.event(s, PlayerEvent::BarHover(over), tx);
                }));
            }
            stage = stage.child(bottom_bar(k, &self, &wire));
        }
        if self.cursor_hidden {
            stage = stage.cursor(Cursor::Hidden);
        }
        let w = wire.clone();
        let stage = stage.on_press_tx(move |s: &mut S, tx: &mut Tx| w.act(s, PlayerAction::TogglePlay, tx));
        let w = wire.clone();
        let stage = stage.on_double_press_tx(move |s: &mut S, tx: &mut Tx| w.act(s, PlayerAction::ToggleFullScreen, tx));
        let w = wire.clone();
        let stage = stage.on_pointer_move_local(move |s: &mut S, _, tx: &mut Tx| w.activity(s, tx));
        let w = wire.clone();
        let stage = stage.on_hover_change(move |s: &mut S, inside, tx: &mut Tx| {
            if !inside {
                w.event(s, PlayerEvent::PointerLeft, tx);
            }
        });
        let w = wire;
        stage.on_wheel(move |s: &mut S, e, tx: &mut Tx| {
            let by = match e.dy {
                0.0 => return false,
                dy if e.precise => dy / 400.0,
                dy => VOLUME_STEP.copysign(dy),
            };
            w.act(s, PlayerAction::VolumeBy(by), tx);
            true
        })
    }
}

/// Light on dark over the video, composited the way the window blends.
fn light(k: &Kit, a: f32) -> Color {
    k.on(Color::WHITE.alpha(a), Color::BLACK)
}

fn scrim(k: &Kit, a: f32) -> Color {
    k.on(Color::BLACK.alpha(a), Color::BLACK)
}

fn fade(k: &Kit, e: El) -> El {
    e.transition(Transition::opacity()).token_motion(k.motion.overlay)
}

/// A layer over the whole stage that centres `content` and lets presses through.
fn centre(k: &Kit, key: &str, content: El) -> El {
    fade(k, el().key(key).pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).center().child(content))
}

fn sign(k: &Kit, glyph: &'static rune::Icon, glyph_size: f32, size: f32) -> El {
    el().size(size, size).center().radius(size / 2.0).fill(scrim(k, 0.55)).child(
        icon(glyph, glyph_size).color(Color::WHITE).a11y_hidden(),
    )
}

fn flash_pill(k: &Kit, flash: Flash) -> El {
    let glyph = match flash {
        Flash::Play => Some(&glyphs::PLAY),
        Flash::Pause => Some(&glyphs::PAUSE),
        Flash::Muted => Some(&glyphs::VOLUME_OFF),
        _ => None,
    };
    match (glyph, flash.text()) {
        (Some(g), _) => sign(k, g, 28.0, 56.0),
        (None, text_) => el()
            .h(48.0)
            .px(20.0)
            .center()
            .radius(24.0)
            .fill(scrim(k, 0.55))
            .child(text(text_.unwrap_or_default()).font(rune::TextStyle { size: 18.0, line_height: 24.0, ..k.ty.ui_medium }).color(Color::WHITE)),
    }
}

fn top_bar(k: &Kit, content: El) -> El {
    let fill = [(0.0, scrim(k, 0.7)), (1.0, scrim(k, 0.0))];
    fade(
        k,
        row()
            .key("player-top")
            .pin(Some(0.0), Some(0.0), Some(0.0), None)
            .items_center()
            .px(12.0)
            .pt(8.0)
            .pb(24.0)
            .linear_gradient(std::f32::consts::PI, fill)
            .child(content),
    )
}

/// A square icon button in light ink.
fn control<S: 'static>(
    k: &Kit,
    wire: &Rc<Wire<S>>,
    key: &str,
    label: &str,
    glyph: &'static rune::Icon,
    action: PlayerAction,
) -> El {
    let w = wire.clone();
    let e = el().key(key).role("button").label(label).help(label).size(BUTTON, BUTTON).center().radius(k.radius.md);
    e.hover(light(k, 0.14))
        .press(light(k, 0.24))
        .child(icon(glyph, 20.0).color(Color::WHITE).a11y_hidden())
        .on_press_tx(move |s: &mut S, tx: &mut Tx| w.act(s, action, tx))
}

fn bottom_bar<S: 'static>(k: &Kit, c: &PlayerControls<S>, wire: &Rc<Wire<S>>) -> El {
    let f = &c.facts;
    let play = control(
        k,
        wire,
        "player-play",
        if f.playing { "Pause" } else { "Play" },
        if f.playing { &glyphs::PAUSE } else { &glyphs::PLAY },
        PlayerAction::TogglePlay,
    );
    let level = if f.muted { 0.0 } else { f.volume };
    let mute = control(
        k,
        wire,
        "player-mute",
        if f.muted { "Unmute" } else { "Mute" },
        if level <= 0.0 { &glyphs::VOLUME_OFF } else { &glyphs::VOLUME },
        PlayerAction::ToggleMute,
    );
    let mut buttons = row()
        .key("player-controls")
        .h(BUTTON + 4.0)
        .items_center()
        .gap(2.0)
        .child(play)
        .child(mute)
        .child(volume_slider(k, wire, level))
        .child(time_label(k, c))
        .child(el().grow(1.0));
    for e in &c.trailing {
        buttons = buttons.child(e.clone());
    }
    if !c.rates.is_empty() {
        buttons = buttons.child(rate_button(k, wire, f.rate, &c.rates));
    }
    let (glyph, label) = if f.full_screen {
        (&glyphs::EXIT_FULL_SCREEN, "Exit full screen")
    } else {
        (&glyphs::FULL_SCREEN, "Full screen")
    };
    buttons = buttons.child(control(k, wire, "player-full-screen", label, glyph, PlayerAction::ToggleFullScreen));
    // Presses on the buttons' row stay there; the video under the rest of the bar still gets them.
    let buttons = buttons.on_press(|_: &mut S| {});
    let fill = [(0.0, scrim(k, 0.0)), (1.0, scrim(k, 0.7))];
    let mut bar = col().key("player-bar").pin(Some(0.0), None, Some(0.0), Some(0.0)).px(12.0).pb(6.0).pt(28.0);
    bar = bar.linear_gradient(std::f32::consts::PI, fill);
    if !f.live {
        bar = bar.child(seek_bar(k, c, wire));
    }
    let w = wire.clone();
    fade(k, bar.child(buttons).on_hover_change(move |s: &mut S, over, tx: &mut Tx| {
        w.event(s, PlayerEvent::BarHover(over), tx);
    }))
}

fn time_label<S: 'static>(k: &Kit, c: &PlayerControls<S>) -> El {
    let style = k.ty.ui_compact.features("'tnum' 1");
    let f = &c.facts;
    if f.live {
        return row()
            .key("player-time")
            .label("Live")
            .items_center()
            .gap(6.0)
            .px(8.0)
            .child(el().size(8.0, 8.0).radius(4.0).fill(k.c.danger.icon))
            .child(text("LIVE").font(style).color(Color::WHITE));
    }
    let now = c.scrub.unwrap_or(f.time);
    let label = match f.duration {
        Some(d) => format!("{} / {}", clock_label(now), clock_label(d)),
        None => clock_label(now),
    };
    el().key("player-time").label(label.clone()).px(8.0).child(text(label).font(style).color(Color::WHITE))
}

fn rate_button<S: 'static>(k: &Kit, wire: &Rc<Wire<S>>, rate: f32, rates: &[f32]) -> El {
    let w = wire.clone();
    let rates = rates.to_vec();
    el().key("player-rate")
        .role("button")
        .label("Playback speed")
        .a11y_value(rate_label(rate))
        .h(BUTTON)
        .min_w(BUTTON)
        .px(8.0)
        .center()
        .radius(k.radius.md)
        .hover(light(k, 0.14))
        .press(light(k, 0.24))
        .child(text(rate_label(rate)).font(k.ty.ui_compact.features("'tnum' 1")).color(Color::WHITE).a11y_hidden())
        .on_press_tx(move |s: &mut S, tx: &mut Tx| {
            if let Some(next) = step_rate(&rates, rate, true, true) {
                w.act(s, PlayerAction::Rate(next), tx);
            }
        })
}

fn frac(x: f32, w: f32) -> f64 {
    if w > 0.0 { f64::from((x / w).clamp(0.0, 1.0)) } else { 0.0 }
}

fn volume_slider<S: 'static>(k: &Kit, wire: &Rc<Wire<S>>, level: f32) -> El {
    let level = level.clamp(0.0, 1.0);
    let track = el().pin(Some(0.0), Some(7.0), Some(0.0), None).h(3.0).radius(1.5).fill(light(k, 0.3));
    let fill = el().w_frac(level).h(3.0).radius(1.5).fill(Color::WHITE);
    let thumb = el().size(10.0, 10.0).radius(5.0).fill(Color::WHITE).ml(-5.0);
    let (w, step) = (wire.clone(), wire.clone());
    el().key("player-volume")
        .role("slider")
        .label("Volume")
        .range_value(f64::from(level), 0.0, 1.0)
        .a11y_value(format!("{} %", (level * 100.0).round() as i32))
        .w(VOLUME_W)
        .h(SEEK_HIT)
        .child(track)
        .child(row().pin(Some(0.0), Some(7.0), Some(0.0), None).h(3.0).child(fill))
        .child(row().pin(Some(0.0), Some(3.0), Some(0.0), None).h(10.0).items_center().child(el().w_frac(level)).child(thumb))
        .on_drag(move |s: &mut S, d: &mut Drag, tx: &mut Tx| {
            if d.phase != Phase::Up {
                w.act(s, PlayerAction::Volume(frac(d.x, d.w) as f32), tx);
            }
        })
        .on_step(move |s: &mut S, delta: i32, tx: &mut Tx| step.act(s, PlayerAction::VolumeBy(delta as f32 * VOLUME_STEP), tx))
}

fn seek_bar<S: 'static>(k: &Kit, c: &PlayerControls<S>, wire: &Rc<Wire<S>>) -> El {
    let f = &c.facts;
    let total = f.duration.filter(|d| *d > 0.0);
    let at = |seconds: f64| total.map_or(0.0, |d| (seconds / d).clamp(0.0, 1.0));
    let played = at(c.scrub.unwrap_or(f.time));
    let active = c.hover.is_some() || c.scrub.is_some();
    let height = if active { 5.0 } else { 3.0 };
    let top = (SEEK_HIT - height) / 2.0;
    let mut seek = el()
        .key("player-seek")
        .role("slider")
        .label("Seek")
        .range_value(played, 0.0, 1.0)
        .w_full()
        .h(SEEK_HIT)
        .child(el().pin(Some(0.0), Some(top), Some(0.0), None).h(height).radius(height / 2.0).fill(light(k, 0.28)));
    if let Some(b) = f.buffered {
        let buffered = row().pin(Some(0.0), Some(top), Some(0.0), None).h(height);
        seek = seek.child(buffered.child(el().w_frac(at(b) as f32).h(height).radius(height / 2.0).fill(light(k, 0.28))));
    }
    let fill = el().w_frac(played as f32).h(height).radius(height / 2.0).fill(k.c.bg_accent);
    seek = seek.child(row().pin(Some(0.0), Some(top), Some(0.0), None).h(height).child(fill));
    if active {
        let thumb = el().size(13.0, 13.0).radius(6.5).fill(k.c.bg_accent).ml(-6.5);
        let place = row().pin(Some(0.0), Some((SEEK_HIT - 13.0) / 2.0), Some(0.0), None).h(13.0).items_center();
        seek = seek.child(place.child(el().w_frac(played as f32)).child(thumb));
    }
    if let (Some(h), Some(d)) = (c.hover, total) {
        let label = text(clock_label(c.scrub.unwrap_or(h * d))).font(k.ty.small_medium.features("'tnum' 1")).color(Color::WHITE);
        let chip = el().px(6.0).py(2.0).radius(k.radius.sm).fill(scrim(k, 0.75)).child(label);
        // A zero-width column at the pointer centres the chip on it. Near an end it stays inside.
        let at = h.clamp(0.06, 0.94) as f32;
        let anchor = col().key("player-seek-label").w(0.0).items_center().child(chip);
        seek = seek.child(row().pin(Some(0.0), Some(-30.0), Some(0.0), None).child(el().w_frac(at)).child(anchor));
    }
    let Some(d) = total else { return seek };
    let width = rune::memo::layout("player-seek").map_or(0.0, |r| r[2]);
    let (w, drag, step) = (wire.clone(), wire.clone(), wire.clone());
    seek.on_pointer_move_local(move |s: &mut S, p, tx: &mut Tx| {
        if width > 0.0 {
            w.event(s, PlayerEvent::SeekHover(Some(frac(p.x, width))), tx);
        }
    })
    .on_drag(move |s: &mut S, d_: &mut Drag, tx: &mut Tx| {
        let f = frac(d_.x, d_.w);
        let seconds = f * d;
        match d_.phase {
            Phase::Down | Phase::Move => {
                if d_.phase == Phase::Down {
                    drag.event(s, PlayerEvent::DragBegan, tx);
                }
                drag.event(s, PlayerEvent::SeekHover(Some(f)), tx);
                drag.event(s, PlayerEvent::Scrubbed(seconds), tx);
                drag.act(s, PlayerAction::Scrub(seconds), tx);
            }
            Phase::Up => {
                drag.event(s, PlayerEvent::DragEnded, tx);
                if !d_.cancelled() {
                    drag.act(s, PlayerAction::Seek(seconds), tx);
                }
            }
        }
    })
    .on_step(move |s: &mut S, delta: i32, tx: &mut Tx| step.act(s, PlayerAction::SeekBy(f64::from(delta) * SEEK_STEP), tx))
}
