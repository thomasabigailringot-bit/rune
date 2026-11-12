//! A live video the app feeds: H.264 access units and AAC frames pushed as they arrive, decoded by
//! the platform's hardware decoder and drawn like any [`Player`]. The app demuxes (HLS, RTMP,
//! whatever it reads); Rune decodes, keeps a small jitter buffer, runs the clock and plays the sound.
//!
//! Times are 90 kHz ticks (MPEG-TS), on one timeline for both streams. Only their differences
//! count: the first picture's time is where the clock starts.
//!
//! The clock is made for live. It starts [`LiveConfig::target_delay`] behind the newest picture
//! pushed, follows the sound device while sound plays, speeds up a little (the sound is resampled)
//! when the queue grows past the target, and jumps forward when it is far behind. The target can
//! change while playing ([`LiveVideo::set_target_delay`]).
//! [`LiveVideo::discontinuity`] starts it again at the next keyframe.
//!
//! With [`LiveConfig::on_demand`] the video is a recording instead (a past broadcast): units carry
//! their media time, the clock starts at the first picture shown and runs at the player's rate,
//! pause keeps what is queued, and the app owns back-pressure ([`LiveVideo::buffered`]) and seeking
//! ([`LiveVideo::restart`]).

use super::{MediaSource, Player, steer};
use crate::time::Instant;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How a [`LiveVideo`] plays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveConfig {
    /// How far behind the newest picture pushed the clock runs: the jitter buffer. Smaller is
    /// closer to live; too small gaps the sound and shows pictures late.
    pub target_delay: Duration,
    /// Play the sound pushed with [`LiveVideo::push_audio`]. The player still starts muted:
    /// [`Player::set_muted`] turns it on.
    pub audio: bool,
    /// A recording, not a live stream: no steering, no jumps, no cutting of what waits. Units
    /// are stamped with their media time (90 kHz ticks from the start of the recording), and the
    /// app starts every run with [`LiveVideo::restart`]. Off by default.
    pub on_demand: bool,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            target_delay: Duration::from_millis(80),
            audio: true,
            on_demand: false,
        }
    }
}

/// The longest target delay: the queues hold that much plus a margin.
const MAX_TARGET: Duration = Duration::from_secs(60);

fn micros(delay: Duration) -> u64 {
    delay.min(MAX_TARGET).as_micros() as u64
}

/// One H.264 access unit in Annex B form (start codes). A keyframe carries its SPS and PPS; a
/// new SPS and PPS at a keyframe may change the picture size.
#[derive(Clone, Debug)]
pub struct AccessUnit {
    /// The unit's bytes, start codes included.
    pub data: Vec<u8>,
    /// Presentation time, 90 kHz ticks.
    pub pts: i64,
    /// An IDR picture the decoder can start from.
    pub keyframe: bool,
}

/// One AAC frame with its ADTS header.
#[derive(Clone, Debug)]
pub struct AacFrame {
    /// The frame's bytes, ADTS header included.
    pub data: Vec<u8>,
    /// Presentation time of its first sample, 90 kHz ticks.
    pub pts: i64,
}

/// A unit and the instant the app pushed it, to measure the latency the pipeline adds.
// Only the Windows decoder takes units out; other platforms keep what is pushed.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct Pushed<T> {
    pub item: T,
    pub at: Instant,
}

/// What the app pushed and the decoder thread has not taken yet.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Default)]
pub(crate) struct Queues {
    pub video: VecDeque<Pushed<AccessUnit>>,
    pub audio: VecDeque<Pushed<AacFrame>>,
    /// The timeline jumped: the decoder starts again at the next keyframe.
    pub discontinuity: bool,
    /// The app called [`LiveVideo::finish`].
    pub closed: bool,
    /// An on-demand run starts here (seconds): pictures and sound before it are not kept.
    pub restart_at: Option<f64>,
}

/// What the app pushed, for the decoder thread to take.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct LiveInput {
    pub config: LiveConfig,
    queues: Mutex<Queues>,
    ready: Condvar,
    /// The delay the clock aims at, in microseconds (see [`LiveVideo::set_target_delay`]).
    target: AtomicU64,
    /// The newest video time pushed, in ticks; `i64::MIN` before the first.
    newest: AtomicI64,
    /// The first time stamp the decoder saw, in ticks: where its seconds start.
    origin: AtomicI64,
    /// Access units pushed in all.
    pushed: AtomicU64,
    /// The recording's length in microseconds; `u64::MAX` when not known.
    duration: AtomicU64,
    /// Where the latest run of a recording starts, in microseconds ([`LiveVideo::restart`]).
    run_at: AtomicU64,
}

impl LiveInput {
    fn new(config: LiveConfig) -> Self {
        Self {
            config,
            target: AtomicU64::new(micros(config.target_delay)),
            queues: Mutex::default(),
            ready: Condvar::new(),
            newest: AtomicI64::new(i64::MIN),
            // A recording's stamps are its media time: its seconds start at 0.
            origin: AtomicI64::new(if config.on_demand { 0 } else { i64::MIN }),
            pushed: AtomicU64::new(0),
            duration: AtomicU64::new(u64::MAX),
            run_at: AtomicU64::new(0),
        }
    }

    /// The newest picture time pushed, in the decoder's seconds; None before the first.
    pub(crate) fn newest_seconds(&self) -> Option<f64> {
        let (t, origin) = (
            self.newest.load(Ordering::Acquire),
            self.origin.load(Ordering::Acquire),
        );
        (t != i64::MIN && origin != i64::MIN).then(|| (t - origin) as f64 / 90_000.0)
    }

    /// The delay the clock aims at, in seconds.
    pub(crate) fn target_seconds(&self) -> f64 {
        self.target.load(Ordering::Relaxed) as f64 / 1e6
    }

    /// The decoder's time zero, in ticks (None: not set yet).
    pub(crate) fn set_origin(&self, ticks: Option<i64>) {
        self.origin
            .store(ticks.unwrap_or(i64::MIN), Ordering::Release);
    }

    /// Takes everything pushed, waiting up to `patience` for something.
    pub(crate) fn take(&self, patience: Duration) -> Queues {
        let mut q = self.queues.lock().unwrap();
        if q.video.is_empty() && q.audio.is_empty() && !q.discontinuity && !q.closed {
            q = self.ready.wait_timeout(q, patience).unwrap().0;
        }
        Queues {
            video: std::mem::take(&mut q.video),
            audio: std::mem::take(&mut q.audio),
            discontinuity: std::mem::take(&mut q.discontinuity),
            closed: q.closed,
            restart_at: q.restart_at.take(),
        }
    }

    /// Access units pushed so far.
    pub(crate) fn pushed(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }
}

/// The source of a [`LiveVideo`]'s player: what [`MediaSource::Live`] holds.
#[derive(Clone)]
pub struct LiveSource {
    input: Arc<LiveInput>,
}

impl LiveSource {
    /// What the decoder thread reads.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn input(&self) -> &Arc<LiveInput> {
        &self.input
    }

    /// The recording's length, if the app gave it ([`LiveVideo::set_duration`]).
    pub(crate) fn duration(&self) -> Option<f64> {
        match self.input.duration.load(Ordering::Relaxed) {
            u64::MAX => None,
            us => Some(us as f64 / 1e6),
        }
    }
}

impl PartialEq for LiveSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.input, &other.input)
    }
}
impl Eq for LiveSource {}
impl std::fmt::Debug for LiveSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSource")
            .field("config", &self.input.config)
            .finish()
    }
}

/// A live video: a [`Player`] (draw it with `el().video(&live, fit)`; it dereferences to one)
/// whose pictures and sound the app pushes. It has no seeking and no looping, no duration unless
/// the app sets one for a recording, and opens when the first keyframe arrives.
#[derive(Clone)]
pub struct LiveVideo {
    player: Player,
    input: Arc<LiveInput>,
}

impl std::ops::Deref for LiveVideo {
    type Target = Player;
    fn deref(&self) -> &Player {
        &self.player
    }
}

impl LiveVideo {
    /// A live video, muted. Call [`Player::play`] to start it (it plays once pictures arrive).
    pub fn new(config: LiveConfig) -> Self {
        let input = Arc::new(LiveInput::new(config));
        let player = Player::new(MediaSource::Live(LiveSource {
            input: input.clone(),
        }));
        Self { player, input }
    }

    /// Hands over one access unit. Never blocks. Units go in decode order, as they arrive.
    pub fn push_video(&self, unit: AccessUnit) {
        self.input.newest.fetch_max(unit.pts, Ordering::AcqRel);
        self.input.pushed.fetch_add(1, Ordering::Relaxed);
        let mut q = self.input.queues.lock().unwrap();
        q.video.push_back(Pushed {
            item: unit,
            at: Instant::now(),
        });
        drop(q);
        self.input.ready.notify_all();
    }

    /// Hands over one AAC frame. Never blocks. Ignored when [`LiveConfig::audio`] is off.
    pub fn push_audio(&self, frame: AacFrame) {
        if !self.input.config.audio {
            return;
        }
        let mut q = self.input.queues.lock().unwrap();
        q.audio.push_back(Pushed {
            item: frame,
            at: Instant::now(),
        });
        drop(q);
        self.input.ready.notify_all();
    }

    /// The stream's timeline jumped (a new session, an ad, a gap): drop what is queued and start
    /// the clock again at the next keyframe.
    pub fn discontinuity(&self) {
        let mut q = self.input.queues.lock().unwrap();
        q.video.clear();
        q.audio.clear();
        q.discontinuity = true;
        drop(q);
        self.input.newest.store(i64::MIN, Ordering::Release);
        self.input.ready.notify_all();
    }

    /// No more units will come: what is queued still plays, then the video ends.
    pub fn finish(&self) {
        self.input.queues.lock().unwrap().closed = true;
        self.input.ready.notify_all();
    }

    /// On-demand only: starts a new run at `at` (a seek, a quality switch, the first run). Drops
    /// what is queued and in flight, sound included, reopens the input after [`LiveVideo::finish`],
    /// and sets the time base so that [`Player::time`] reports `at` and then follows the pictures.
    /// The app pushes units from a keyframe at or before `at`; pictures and sound before `at` are
    /// decoded and not shown. The clock starts at the first picture shown, playing or not (a
    /// paused video shows it and holds). Does nothing for a live video.
    pub fn restart(&self, at: Duration) {
        if !self.input.config.on_demand {
            return;
        }
        let seconds = at.as_secs_f64();
        self.input.run_at.store(at.as_micros().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        let mut q = self.input.queues.lock().unwrap();
        q.video.clear();
        q.audio.clear();
        q.discontinuity = true;
        q.closed = false;
        q.restart_at = Some(seconds);
        drop(q);
        self.input.newest.store(i64::MIN, Ordering::Release);
        self.player.send(super::Event::Seek(seconds));
        self.input.ready.notify_all();
    }

    /// The recording's length, so that [`Player::duration`] answers. `None` for unknown.
    pub fn set_duration(&self, duration: Option<Duration>) {
        let us = duration.map_or(u64::MAX, |d| d.as_micros().min(u128::from(u64::MAX - 1)) as u64);
        self.input.duration.store(us, Ordering::Relaxed);
    }

    /// On-demand: media pushed and not yet shown (the newest unit pushed minus [`Player::time`]),
    /// so the app can stop reading while enough is queued. Pausing does not drain it. The position
    /// is never before the start of the latest run: until a picture of that run shows, the time
    /// can still be that of a picture of the run before it.
    pub fn buffered(&self) -> Duration {
        let run_at = self.input.run_at.load(Ordering::Relaxed) as f64 / 1e6;
        Duration::from_secs_f64(steer::buffered(self.input.newest_seconds(), self.player.time().max(run_at)))
    }

    /// The configuration this video was made with, with the delay it aims at now.
    pub fn config(&self) -> LiveConfig {
        LiveConfig {
            target_delay: self.target_delay(),
            ..self.input.config
        }
    }

    /// How far behind the newest picture pushed the clock aims to run now.
    pub fn target_delay(&self) -> Duration {
        Duration::from_micros(self.input.target.load(Ordering::Relaxed))
    }

    /// Changes the jitter buffer of a playing video (at most 60 s). A larger delay slows the clock
    /// a little until the queue holds it; a smaller one speeds it up until the queue shrinks to
    /// it. Only a change of more than a second beyond the target jumps, as for any backlog.
    pub fn set_target_delay(&self, delay: Duration) {
        self.input.target.store(micros(delay), Ordering::Relaxed);
    }
}
