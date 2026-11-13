use rune::{Cursor, Cx, El, Env, Runtime, Tx, WheelEvent, el};
use rune_kit::{Kit, PlayerAction, PlayerChrome, PlayerControls, PlayerFacts};

struct App {
    chrome: PlayerChrome,
    facts: PlayerFacts,
    now: f64,
    actions: Vec<PlayerAction>,
    skip: Option<f64>,
}

fn view(s: &App, _: &mut Cx<App>) -> El {
    let mut controls = PlayerControls::new(s.facts, &s.chrome, s.now);
    if let Some(seconds) = s.skip {
        controls = controls.skip(seconds);
    }
    controls
        .chrome(|s: &mut App| &mut s.chrome)
        .on_action(|s: &mut App, action, tx: &mut Tx| {
            s.now = tx.clock();
            s.actions.push(action);
        })
        .rates(&[0.5, 1.0, 2.0])
        .build(&Kit::neutral(true), el().key("clip"))
}

fn poll(s: &mut App, now: f64, _: &mut Tx) -> bool {
    s.now = now;
    s.chrome.poll(now, &s.facts)
}

const SIZE: (f32, f32) = (800.0, 450.0);
/// A point on the video, clear of the bars.
const VIDEO: (f32, f32) = (400.0, 150.0);

fn app(facts: PlayerFacts) -> Runtime<App> {
    let state = App { chrome: PlayerChrome::new(), facts, now: 0.0, actions: Vec::new(), skip: None };
    let mut rt = Runtime::new(state, view, SIZE, Env::new(true, 1.0));
    rt.poll = Some(poll);
    rt.frame(0.0);
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
    rt
}

fn paused() -> PlayerFacts {
    PlayerFacts { duration: Some(100.0), time: 20.0, ..PlayerFacts::default() }
}

fn playing() -> PlayerFacts {
    PlayerFacts { playing: true, ..paused() }
}

fn run(rt: &mut Runtime<App>, secs: f64) {
    for _ in 0..(secs * 60.0) as usize {
        rt.frame(1.0 / 60.0);
    }
}

fn rect(rt: &Runtime<App>, key: &str) -> Option<[f32; 4]> {
    rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key) && !e.exiting).map(|e| e.rect)
}

fn has(rt: &Runtime<App>, key: &str) -> bool {
    rect(rt, key).is_some()
}

#[test]
fn a_press_on_the_video_toggles_play() {
    let mut rt = app(paused());
    rt.pointer_down(VIDEO.0, VIDEO.1);
    rt.pointer_up(VIDEO.0, VIDEO.1);
    rt.frame(0.0);
    assert_eq!(rt.state.actions, [PlayerAction::TogglePlay]);
}

#[test]
fn the_wheel_changes_the_volume() {
    let mut rt = app(paused());
    rt.wheel_event(WheelEvent::new(0.0, 3.0).at(VIDEO.0, VIDEO.1));
    rt.wheel_event(WheelEvent::new(0.0, -3.0).at(VIDEO.0, VIDEO.1));
    rt.wheel_event(WheelEvent::new(0.0, 40.0).at(VIDEO.0, VIDEO.1).precise(true));
    rt.frame(0.0);
    assert_eq!(
        rt.state.actions,
        [PlayerAction::VolumeBy(0.05), PlayerAction::VolumeBy(-0.05), PlayerAction::VolumeBy(0.1)]
    );
}

#[test]
fn dragging_the_seek_bar_scrubs_then_seeks_once() {
    let mut rt = app(paused());
    let [x, y, w, h] = rect(&rt, "player-seek").expect("the seek bar shows");
    let (y, left) = (y + h / 2.0, x);
    rt.pointer_move(left + w * 0.25, y);
    rt.frame(0.0);
    rt.pointer_down(left + w * 0.25, y);
    rt.pointer_move(left + w * 0.5, y);
    rt.frame(0.0);
    rt.pointer_move(left + w * 0.75, y);
    rt.frame(0.0);
    rt.pointer_up(left + w * 0.75, y);
    rt.frame(0.0);
    let seconds: Vec<(bool, f64)> = rt
        .state
        .actions
        .iter()
        .filter_map(|a| match *a {
            PlayerAction::Scrub(t) => Some((false, t)),
            PlayerAction::Seek(t) => Some((true, t)),
            _ => None,
        })
        .collect();
    assert!(seconds.len() >= 3, "{seconds:?}");
    let (last, scrubs) = seconds.split_last().unwrap();
    assert!(scrubs.iter().all(|(seek, _)| !seek), "only scrubs before the release: {seconds:?}");
    assert!(last.0 && (last.1 - 75.0).abs() < 1.0, "one Seek near 75 s at the end: {seconds:?}");
    assert!(scrubs.windows(2).all(|p| p[0].1 <= p[1].1), "the scrub follows the pointer: {seconds:?}");
    assert!(!rt.state.chrome.dragging());
}

#[test]
fn a_cancelled_drag_does_not_seek() {
    let mut rt = app(paused());
    let [x, y, w, h] = rect(&rt, "player-seek").unwrap();
    rt.pointer_down(x + w * 0.5, y + h / 2.0);
    rt.pointer_move(x + w * 0.6, y + h / 2.0);
    rt.frame(0.0);
    rt.key(rune::KeyEvent { key: rune::Key::Escape, mods: rune::Mods::default() });
    rt.frame(0.0);
    rt.pointer_up(x + w * 0.6, y + h / 2.0);
    rt.frame(0.0);
    let seeks = rt.state.actions.iter().filter(|a| matches!(a, PlayerAction::Seek(_))).count();
    assert_eq!(seeks, 0, "{:?}", rt.state.actions);
}

#[test]
fn the_bar_shows_while_paused_and_a_big_play_sign_too() {
    let mut rt = app(paused());
    for key in ["player-bar", "player-play", "player-seek", "player-time", "player-mute", "player-volume"] {
        assert!(has(&rt, key), "{key}");
    }
    assert!(has(&rt, "player-rate") && has(&rt, "player-full-screen") && has(&rt, "player-paused"));
    run(&mut rt, 10.0);
    assert!(has(&rt, "player-bar"), "a paused video keeps its controls");
    assert!(!rt.state.chrome.cursor_hidden(rt.state.now, &rt.state.facts));
}

#[test]
fn a_playing_bar_hides_after_three_seconds_and_comes_back_on_a_move() {
    let mut rt = app(playing());
    assert!(!has(&rt, "player-paused"));
    rt.pointer_move(VIDEO.0, VIDEO.1);
    rt.frame(0.0);
    assert!(has(&rt, "player-bar"), "the pointer move is activity");
    assert_ne!(rt.cursor(), Cursor::Hidden);
    run(&mut rt, 2.5);
    assert!(has(&rt, "player-bar") && rt.cursor() != Cursor::Hidden);
    run(&mut rt, 1.0);
    assert!(!has(&rt, "player-bar"), "hidden after 3 s");
    assert_eq!(rt.cursor(), Cursor::Hidden);
    rt.pointer_move(VIDEO.0 + 10.0, VIDEO.1);
    rt.frame(0.0);
    assert!(has(&rt, "player-bar"), "back on a move");
    assert_ne!(rt.cursor(), Cursor::Hidden);
}

#[test]
fn a_hidden_bar_takes_no_presses() {
    let mut rt = app(playing());
    rt.pointer_move(VIDEO.0, VIDEO.1);
    run(&mut rt, 3.5);
    assert!(!has(&rt, "player-bar"));
    let bottom = (400.0, SIZE.1 - 20.0);
    rt.pointer_down(bottom.0, bottom.1);
    rt.pointer_up(bottom.0, bottom.1);
    rt.frame(0.0);
    assert_eq!(rt.state.actions, [PlayerAction::TogglePlay], "the press reached the video, not a button");
}

#[test]
fn a_button_press_does_not_toggle_play() {
    let mut rt = app(paused());
    let (x, y) = rt.find("player-mute").unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert_eq!(rt.state.actions, [PlayerAction::ToggleMute]);
    let (x, y) = rt.find("player-rate").unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert_eq!(rt.state.actions[1], PlayerAction::Rate(2.0));
    let [bx, by, bw, _] = rect(&rt, "player-bar").unwrap();
    rt.pointer_down(bx + bw / 2.0, by + 10.0);
    rt.pointer_up(bx + bw / 2.0, by + 10.0);
    rt.frame(0.0);
    assert_eq!(rt.state.actions.len(), 3, "the bar's gradient lets the press through to the video");
}

#[test]
fn a_flash_follows_an_action_and_fades() {
    let mut rt = app(paused());
    let (x, y) = rt.find("player-play").unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert!(has(&rt, "player-flash"));
    run(&mut rt, 1.0);
    assert!(!has(&rt, "player-flash"));
}

#[test]
fn live_has_no_seek_bar_and_shows_the_mark() {
    let rt = app(PlayerFacts { live: true, playing: false, duration: None, ..PlayerFacts::default() });
    assert!(has(&rt, "player-bar") && !has(&rt, "player-seek"));
    let live = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("player-live")).unwrap();
    assert_eq!(live.label.as_deref(), Some("Live"));
    assert!(!has(&rt, "player-time") && !has(&rt, "player-back") && !has(&rt, "player-forward"));
}

fn dvr(behind: bool) -> PlayerFacts {
    PlayerFacts {
        live: true,
        dvr: true,
        behind_live: behind,
        time: if behind { 150.0 } else { 900.0 },
        duration: Some(900.0),
        ..PlayerFacts::default()
    }
}

fn press(rt: &mut Runtime<App>, key: &str) {
    let (x, y) = rt.find(key).unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
}

fn info(rt: &Runtime<App>, key: &str) -> rune::ElementInfo {
    rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key) && !e.exiting).unwrap()
}

#[test]
fn dvr_live_has_a_seek_bar_and_a_live_button() {
    let mut rt = app(dvr(true));
    assert!(has(&rt, "player-seek") && has(&rt, "player-live"));
    assert_eq!(info(&rt, "player-live").label.as_deref(), Some("Go to live"));
    assert_eq!(info(&rt, "player-time").label.as_deref(), Some("-12:30"));
    let [x, y, w, h] = rect(&rt, "player-seek").unwrap();
    rt.pointer_move(x + w * 0.5, y + h / 2.0);
    rt.frame(0.0);
    run(&mut rt, 1.0);
    assert!(rt.elements().into_iter().filter_map(|e| e.text).any(|t| t == "-7:30"), "behind live at the pointer");
    rt.pointer_move(x + w, y + h / 2.0);
    rt.frame(0.0);
    run(&mut rt, 1.0);
    assert!(rt.elements().into_iter().filter_map(|e| e.text).any(|t| t == "LIVE"));
    let edge = app(dvr(false));
    assert!(has(&edge, "player-seek") && has(&edge, "player-live") && !has(&edge, "player-time"));
    assert_eq!(info(&edge, "player-live").label.as_deref(), Some("Live"));
}

#[test]
fn pressing_live_when_behind_goes_live() {
    let mut rt = app(dvr(true));
    press(&mut rt, "player-live");
    assert_eq!(rt.state.actions, [PlayerAction::GoLive]);
}

#[test]
fn pressing_live_at_the_edge_sends_nothing() {
    let mut rt = app(dvr(false));
    press(&mut rt, "player-live");
    assert!(!rt.state.actions.contains(&PlayerAction::GoLive), "{:?}", rt.state.actions);
}

fn skipping(facts: PlayerFacts) -> Runtime<App> {
    let mut rt = app(facts);
    rt.update(|s, _| s.skip = Some(15.0));
    run(&mut rt, 0.5);
    rt
}

#[test]
fn skip_buttons_seek_by_the_option() {
    let mut rt = skipping(dvr(true));
    assert_eq!(info(&rt, "player-back").label.as_deref(), Some("Back 15 seconds"));
    assert_eq!(info(&rt, "player-forward").label.as_deref(), Some("Forward 15 seconds"));
    press(&mut rt, "player-back");
    press(&mut rt, "player-forward");
    assert_eq!(rt.state.actions, [PlayerAction::SeekBy(-15.0), PlayerAction::SeekBy(15.0)]);
}

#[test]
fn forward_is_disabled_at_the_live_edge() {
    let mut rt = skipping(dvr(false));
    assert!(has(&rt, "player-back") && has(&rt, "player-forward"));
    press(&mut rt, "player-forward");
    assert!(rt.state.actions.is_empty(), "{:?}", rt.state.actions);
    press(&mut rt, "player-back");
    assert_eq!(rt.state.actions, [PlayerAction::SeekBy(-15.0)]);
}

#[test]
fn live_without_a_past_has_no_skip_buttons() {
    let rt = skipping(PlayerFacts { live: true, ..PlayerFacts::default() });
    assert!(!has(&rt, "player-seek") && !has(&rt, "player-back") && !has(&rt, "player-forward"));
    assert!(has(&rt, "player-live"));
}

#[test]
fn a_recording_with_skip_has_both_buttons_and_none_without() {
    let rt = skipping(paused());
    assert!(has(&rt, "player-back") && has(&rt, "player-forward"));
    let rt = app(paused());
    assert!(!has(&rt, "player-back") && !has(&rt, "player-forward"));
}

#[test]
fn labels_and_roles() {
    let rt = app(paused());
    let info = |key: &str| rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap();
    assert_eq!(info("player-play").label.as_deref(), Some("Play"));
    assert_eq!(info("player-mute").label.as_deref(), Some("Mute"));
    assert_eq!(info("player-seek").role, "slider");
    assert_eq!(info("player-volume").role, "slider");
    assert_eq!(info("player-time").label.as_deref(), Some("0:20 / 1:40"));
}

#[test]
fn the_hover_time_follows_the_pointer_above_the_bar() {
    let mut rt = app(paused());
    let [x, y, w, h] = rect(&rt, "player-seek").unwrap();
    assert!(!has(&rt, "player-seek-label"));
    let px = x + w * 0.5;
    rt.pointer_move(px, y + h / 2.0);
    rt.frame(0.0);
    run(&mut rt, 1.0);
    let [lx, ly, lw, lh] = rect(&rt, "player-seek-label").expect("the label shows on hover");
    assert!(ly + lh <= y + 1.0, "above the bar");
    assert!((lx + lw / 2.0 - px).abs() < 3.0, "centred on the pointer: {lx} {lw} {px}");
    let text = rt.elements().into_iter().filter_map(|e| e.text).find(|t| t == "0:50");
    assert!(text.is_some(), "the time under the pointer is 0:50 of 1:40");
    rt.pointer_move(VIDEO.0, VIDEO.1);
    rt.frame(0.0);
    run(&mut rt, 1.0);
    assert!(!has(&rt, "player-seek-label"));
}

#[test]
fn the_volume_slider_sets_an_absolute_level() {
    let mut rt = app(paused());
    let [x, y, w, h] = rect(&rt, "player-volume").unwrap();
    rt.pointer_down(x + w * 0.5, y + h / 2.0);
    rt.pointer_up(x + w * 0.5, y + h / 2.0);
    rt.frame(0.0);
    let Some(PlayerAction::Volume(v)) = rt.state.actions.first().copied() else { panic!("{:?}", rt.state.actions) };
    assert!((v - 0.5).abs() < 0.05, "{v}");
}

#[test]
fn a_pinned_chrome_keeps_a_playing_bar_up() {
    let mut rt = app(playing());
    rt.update(|s, _| s.chrome.pinned = true);
    rt.pointer_move(VIDEO.0, VIDEO.1);
    run(&mut rt, 6.0);
    assert!(has(&rt, "player-bar"));
}
