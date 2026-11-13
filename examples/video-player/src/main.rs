//! Play a movie or audio file, or a URL. `cargo run -p video-player -- clip.mov` opens a video
//! with `rune_kit::PlayerControls`: click to pause or play, double click for full screen, the
//! wheel for the volume, a seek bar, and controls that hide while it plays. An audio file (mp3,
//! m4a, wav) shows a progress bar and waits for a tap, since audio is audible. With no argument
//! it plays the four-frame test clip, looping and muted. `-- --snap out.png --at 0.6` renders
//! the frame at 0.6 s headless.
//!
//! Keys (video): the web player's. Space or k plays or pauses, j and l seek 10 s, left and right
//! seek 5 s, up and down change the volume, m mutes, f is full screen, 0 to 9 jump to a tenth,
//! Home and End go to the ends, < and > change the rate.

use rune::tok::{self, type_};
use rune::{AppSpec, Cx, El, KeyEvent, Player, PlayerState, SurfaceFit, Tx, Waker, col, el, row, text};
use rune_kit::{Kit, PlayerAction, PlayerChrome, PlayerControls, PlayerEvent, PlayerFacts, player_key};
use std::time::Duration;

const WIDTH: f32 = 480.0;
const AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "aif", "aiff", "flac", "ogg"];
const RATES: &[f32] = &[0.5, 1.0, 1.25, 1.5, 2.0];

struct Viewer {
    clip: Player,
    chrome: PlayerChrome,
    /// The window clock of the last poll, for the controls' hide timing.
    now: f64,
    full_screen: bool,
}

fn state() -> Viewer {
    let source = rune::extra_args()
        .first()
        .cloned()
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/rune/tests/fixtures/rgbw.mp4").into());
    let ext = source.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    let viewer = |clip| Viewer { clip, chrome: PlayerChrome::new(), now: 0.0, full_screen: false };
    if AUDIO.contains(&ext.as_str()) {
        return viewer(Player::audio(source));
    }
    let clip = Player::new(source.into());
    clip.set_looping(true);
    clip.set_muted(true);
    clip.play();
    viewer(clip)
}

/// The clip calls the waker when it loads, ends or fails, so the label follows.
fn setup(s: &mut Viewer, waker: Waker) {
    s.clip.set_waker(waker);
}

/// What the controls show. A clip that is loading counts as playing, so no play sign covers it.
fn facts(s: &Viewer) -> PlayerFacts {
    let c = &s.clip;
    PlayerFacts {
        playing: c.is_playing() || matches!(c.state(), PlayerState::Loading),
        time: c.time(),
        duration: c.duration(),
        volume: c.volume(),
        muted: c.muted(),
        full_screen: s.full_screen,
        rate: c.rate(),
        ..PlayerFacts::default()
    }
}

/// Progress: re-run the view about 30 times a second while the clip plays and its position shows,
/// and never otherwise.
fn poll(s: &mut Viewer, now: f64, tx: &mut Tx) -> bool {
    s.now = now;
    if s.clip.is_playing() {
        tx.wake_after(Duration::from_millis(33));
    }
    let facts = facts(s);
    let changed = s.chrome.poll(now, &facts);
    changed || (s.clip.is_playing() && (s.clip.is_audio() || s.chrome.shown(now, &facts)))
}

fn toggle(s: &mut Viewer) {
    if s.clip.is_playing() { s.clip.pause() } else { s.clip.play() }
}

/// Does what the user asked of the player.
fn execute(s: &mut Viewer, action: PlayerAction, tx: &mut Tx) {
    let clip = &s.clip;
    let seek = |to: f64| clip.seek(to.clamp(0.0, clip.duration().unwrap_or(f64::MAX)));
    match action {
        PlayerAction::TogglePlay => toggle(s),
        PlayerAction::Scrub(t) | PlayerAction::Seek(t) => seek(t),
        PlayerAction::SeekBy(d) => seek(clip.time() + d),
        PlayerAction::Volume(v) => set_volume(clip, v),
        PlayerAction::VolumeBy(d) => set_volume(clip, if clip.muted() { d.max(0.0) } else { clip.volume() + d }),
        PlayerAction::ToggleMute => clip.set_muted(!clip.muted()),
        PlayerAction::ToggleFullScreen => {
            s.full_screen = !s.full_screen;
            tx.toggle_full_screen();
        }
        PlayerAction::Rate(r) => clip.set_rate(r),
    }
}

fn set_volume(clip: &Player, volume: f32) {
    clip.set_volume(volume.clamp(0.0, 1.0));
    clip.set_muted(false);
}

fn on_key(s: &mut Viewer, event: &KeyEvent, tx: &mut Tx) -> bool {
    if s.clip.is_audio() {
        return false;
    }
    let facts = facts(s);
    let Some(action) = player_key(event, &facts, RATES) else { return false };
    if let Some(due) = s.chrome.step(PlayerEvent::Acted(action), tx.clock(), &facts) {
        tx.wake_at(due);
    }
    execute(s, action, tx);
    true
}

fn view(s: &Viewer, _: &mut Cx<Viewer>) -> El {
    if !s.clip.is_audio() {
        let video = el().key("video").video(&s.clip, SurfaceFit::Fit);
        return PlayerControls::new(facts(s), &s.chrome, s.now)
            .chrome(|s: &mut Viewer| &mut s.chrome)
            .on_action(execute)
            .rates(RATES)
            .build(&Kit::neutral(true), video);
    }
    let t = tok::theme(true);
    let label = match s.clip.state() {
        PlayerState::Loading => "Loading".to_string(),
        PlayerState::Playing => "Playing, tap to pause".to_string(),
        PlayerState::Ready => "Paused, tap to play".to_string(),
        PlayerState::Ended => "Ended, tap to replay".to_string(),
        PlayerState::Failed(e) => e,
        _ => String::new(),
    };
    let progress = s.clip.duration().filter(|d| *d > 0.0).map_or(0.0, |d| (s.clip.time() / d) as f32);
    let bar = row()
        .key("progress")
        .w(WIDTH)
        .h(4.0)
        .radius(2.0)
        .fill(t.control_track)
        .child(el().w(WIDTH * progress.clamp(0.0, 1.0)).h(4.0).radius(2.0).fill(t.indicator));
    col().w_full().h_full().center().gap(12.0).fill(t.bg).on_press(toggle).child(bar).child(text(label).font(type_::BODY).color(t.text_secondary))
}

fn main() {
    rune::run(AppSpec { setup: Some(setup), poll: Some(poll), on_key: Some(on_key), ..AppSpec::new("Video player", (520.0, 340.0), state, view) });
}
