//! Windows: Media Foundation. Each clip that plays gets a decoder thread (`decoder`) that opens
//! it with the source reader, decodes on the GPU where the adapter has a hardware decoder (else
//! on the CPU) and copies each picture into a shared NV12 texture Rune samples in place
//! (`gpu`). Sound goes to the default output through WASAPI (`audio`), following the clock.
//!
//! The UI thread's part, in each frame that draws the clip: fold the handle's intent and the
//! decoder's news into the player's phase (`machine`), and swap in the picture for the refresh
//! the frame presents at (`vsync`). It then asks for its next frame when the next picture is
//! due, not every refresh. A clip nobody draws isn't pulled, so its decoder fills its queue and
//! sleeps; a muted one picks up where it left off when drawn again, and after a few seconds
//! undrawn the decoder lets go of everything but the picture on screen.
//!
//! Nothing here runs until a clip opens: Media Foundation, D3D11 and the audio stack load then.

mod audio;
mod decoder;
mod gpu;
mod live;
mod machine;
mod mf;
mod still;
mod vsync;

use super::*;
use decoder::{Decoded, Facts, Job, News, Pic, Pipe};
use machine::{Effect, Event, Phase};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// A muted clip undrawn this long picks up at its picture when drawn again, rather than
/// skipping what played while nobody watched.
const RESUME_GAP: Duration = Duration::from_millis(250);
/// A live clip undrawn this long is hidden (covered, minimized): its decoder steers the clock
/// itself, and the first frame drawn after it waits for a picture at the clock.
const LIVE_IDLE: Duration = Duration::from_millis(100);
/// A live clip that came back waits this long for its first picture before it counts as a stall.
const LIVE_RETURN: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct Native {
    phase: Phase,
    pipe: Option<Arc<Pipe>>,
    /// Seeks issued; the decoder's pictures carry the one they belong to.
    seeks: crate::machine::Seq,
    clock: Arc<audio::Clock>,
    facts: Option<Facts>,
    /// The picture on the surface: its unwrapped time.
    shown: Option<f64>,
    /// When a frame last drew the clip.
    pulled: Option<Instant>,
    /// The intent last folded into the phase.
    playing: bool,
    rate: f32,
    /// When the frame that draws the clip next should come.
    due: Option<Instant>,
    /// Headless pictures.
    still: Option<still::Still>,
    /// When the decoder was last started, until its first picture reached the surface
    /// (`RUNE_VIDEO_LOG`).
    opened: Option<Instant>,
    counts: Counts,
    /// Live: when the frame that draws the clip came back after a gap. The decoder kept the clock
    /// on the stream meanwhile, and no picture is due until the decoder has caught up to it:
    /// that is no stall, for a while.
    returning: Option<Instant>,
    /// Told of each picture that reaches the surface (`Player::set_picture_observer`).
    observer: Option<Arc<dyn Fn(f64, Instant) + Send + Sync>>,
}

/// What `Player::stats` reports, kept on the UI thread.
#[derive(Default)]
struct Counts {
    shown: u64,
    dropped: u64,
    late: u64,
    stalls: u64,
    /// Live: the last picture's push-to-presentation time in milliseconds.
    latency_ms: f32,
    /// Live: the offsets between the sound at the device and the picture presented, summed, counted
    /// and the largest, in seconds.
    sync_sum: f64,
    sync_n: u64,
    sync_max: f64,
}

// The reader in `still` stays on the headless thread that made it, under the Shared mutex.
unsafe impl Send for Native {}

impl Drop for Native {
    /// The clip's last handle and element went: its decoder stops now, not when it would park.
    fn drop(&mut self) {
        self.clock.set(None);
        if let Some(pipe) = self.pipe.take() {
            pipe.lock().stop = true;
            pipe.poke();
        }
    }
}

pub(super) fn created(shared: &Arc<Shared>) {
    // Media Foundation's network source sends no headers of ours.
    if !shared.headers.is_empty() {
        shared.ctl.lock().unwrap().fail("request headers aren't supported for clips on Windows yet".into());
    }
}

pub(super) fn enable() {}

pub(super) fn probe(native: &mut Native, source: &MediaSource, _: &[(String, String)], kind: Kind) -> Result<Meta, String> {
    let (still, meta) = still::open(source, kind)?;
    native.still = still;
    Ok(meta)
}

pub(super) fn decode(native: &mut Native, source: &MediaSource, headers: &[(String, String)], at: f64) -> Result<ByteFrame, String> {
    if native.still.is_none() {
        probe(native, source, headers, Kind::Video)?;
    }
    native.still.as_mut().ok_or("no video track")?.frame(at)
}

/// A handle edit: fold it in now, so pausing or muting a clip nobody draws goes quiet at once.
pub(super) fn apply(shared: &Arc<Shared>) {
    if !playback_enabled() {
        return;
    }
    if shared.kind == Kind::Audio {
        return drive(shared);
    }
    // Being pulled further up this stack: that pull folds it in.
    let Ok(mut native) = shared.native.try_lock() else { return };
    let mut c = shared.ctl.lock().unwrap();
    let target = vsync::snap(Instant::now());
    // Video opens when a frame draws it.
    native.reconcile(shared, &mut c, target, false);
    drop(c);
    drop(native);
    shared.settle(false);
}

/// Audio, from any thread (an edit, or its decoder's news): fold everything in. Sound has no
/// pictures to land, so a clip that opened plays from where it was asked to, and one whose
/// clock passed its end ends.
pub(super) fn drive(shared: &Arc<Shared>) {
    let Ok(mut native) = shared.native.try_lock() else { return };
    let mut c = shared.ctl.lock().unwrap();
    let now = Instant::now();
    native.reconcile(shared, &mut c, now, true);
    match native.phase {
        Phase::Buffering { at, .. } => {
            native.shown = Some(at);
            native.event(shared, &c, Event::Shown { pts: at, target: now, lead: 0.0, rate: f64::from(c.rate) });
        }
        Phase::Playing { clock } => {
            let generation = native.seeks.current().0;
            let ended = native.pipe.as_ref().is_some_and(|p| p.lock().ended == Some(generation));
            let duration = c.duration.unwrap_or(0.0);
            let pos = clock.position(now);
            if ended && duration > 0.0 && pos >= (clock.pos / duration).floor() * duration + duration {
                native.end(shared, &mut c, duration, duration);
            }
        }
        _ => {}
    }
    drop(c);
    drop(native);
    shared.settle(false);
}

pub(super) fn preload(shared: &Arc<Shared>) {
    let mut native = shared.native.lock().unwrap();
    let c = shared.ctl.lock().unwrap();
    if !c.failed() && native.pipe.is_none() {
        let play = c.playing();
        native.event(shared, &c, Event::Open { play });
    }
}

/// Live: how deep the queue is at the clock now (seconds); 0 for anything else.
fn live_depth(native: &Native, shared: &Shared) -> f64 {
    let MediaSource::Live(source) = &shared.source else { return 0.0 };
    let (Some(clock), Some(newest)) = (native.clock.get(), source.input().newest_seconds()) else { return 0.0 };
    newest - clock.position(Instant::now())
}

pub(super) fn stats(shared: &Shared) -> PlayerStats {
    let native = shared.native.lock().unwrap();
    let (silent, played) = native.clock.silence();
    PlayerStats {
        shown: native.counts.shown,
        dropped: native.counts.dropped,
        late: native.counts.late + native.pipe.as_ref().map_or(0, |p| p.late.load(Ordering::Relaxed)),
        stalls: native.counts.stalls,
        jumps: match &shared.source {
            MediaSource::Live(live) => live.input().jumps(),
            _ => 0,
        },
        queue_ms: (live_depth(&native, shared) * 1000.0) as f32,
        latency_ms: native.counts.latency_ms,
        // The clock itself: a live clip's decoder steers it while nothing draws.
        clock_rate: native.clock.get().map_or(1.0, |clock| clock.rate as f32),
        decoder_hold: native.pipe.as_ref().map_or(0, |p| p.hold.load(Ordering::Relaxed)),
        av_offset_mean_ms: if native.counts.sync_n > 0 { (native.counts.sync_sum / native.counts.sync_n as f64 * 1000.0) as f32 } else { 0.0 },
        av_offset_max_ms: (native.counts.sync_max * 1000.0) as f32,
        sound_gaps: native.clock.gaps(),
        sound_silent_frames: silent,
        sound_frames: played,
        hardware: native.pipe.as_ref().and_then(|p| match p.hardware.load(Ordering::Relaxed) {
            0 => None,
            h => Some(h == 2),
        }),
        queued: native.pipe.as_ref().map_or(0, |p| p.lock().frames.len()),
    }
}

pub(super) fn set_observer(shared: &Shared, observer: Arc<dyn Fn(f64, Instant) + Send + Sync>) {
    shared.native.lock().unwrap().observer = Some(observer);
}

/// Audio: where the clock has it now. The clock runs only while the sound does, so it's always
/// the one to read. A fed recording: the clock too while it runs, drawn or not (no picture shows
/// behind a minimized window, and the position goes on); else the time of the last restart or
/// picture.
pub(super) fn live_time(shared: &Shared, _: bool) -> Option<f64> {
    if shared.source.on_demand() {
        let native = shared.native.try_lock().ok()?;
        // A restart not folded in yet: the clock is the one of the run before.
        if shared.ctl.lock().unwrap().seek.is_some() {
            return None;
        }
        let clock = native.clock.get()?;
        let position = clock.position(Instant::now());
        return Some(shared.source.duration().map_or(position, |end| position.min(end)));
    }
    if shared.kind != Kind::Audio {
        return None;
    }
    let native = shared.native.try_lock().ok()?;
    let clock = native.clock.get()?;
    let duration = native.facts.map(|f| f.meta.duration).filter(|d| *d > 0.0)?;
    Some(clock.position(Instant::now()).rem_euclid(duration))
}

/// When the frame that draws the clip next should come; None while nothing is due.
pub(super) fn due(shared: &Shared) -> Option<Instant> {
    shared.native.try_lock().ok()?.due
}

pub(super) fn pull(shared: &Arc<Shared>, surface: &NativeSurface, target: Instant) -> bool {
    let now = Instant::now();
    let target = vsync::snap(target.max(now));
    let mut native = shared.native.lock().unwrap();
    let mut c = shared.ctl.lock().unwrap();
    let gap = native.pulled.map(|p| now.saturating_duration_since(p));
    native.pulled = Some(now);
    if let Some(pipe) = &native.pipe {
        pipe.lock().touched = Some(now);
    }
    if shared.source.on_demand() {
        native.note_return(gap, now);
    } else if shared.source.is_live() {
        native.join_clock(gap, now);
    }
    native.reconcile(shared, &mut c, target, true);
    if c.failed() {
        native.due = None;
        return false;
    }
    let lead = vsync::period().as_secs_f64() / 2.0;
    if let (Phase::Playing { .. }, Some(gap), Some(at)) = (native.phase, gap, native.shown)
        && gap > RESUME_GAP
        && !native.sounding(shared, &c)
        && !shared.source.is_live()
    {
        native.event(shared, &c, Event::Resumed { target, at, lead });
    }
    let shown = native.shown;
    native.show(shared, &mut c, surface, target, lead);
    let (active, mut due) = native.next(&c, now);
    // A live video draws every refresh: a timer's wake lands off the refresh grid, and a picture
    // pulled a refresh late is a picture skipped.
    if shared.source.is_live() {
        due = None;
    }
    native.due = due;
    let cost = now.elapsed().as_nanos();
    if decoder::logging_frames() {
        let new = native.shown.filter(|s| shown != Some(*s)).map_or(String::new(), |s| format!(" picture {s:.4}"));
        let at = |t: Instant| t.saturating_duration_since(now).as_micros();
        decoder::log!("pull {cost} ns, target +{} us{new}, due +{:?} us", at(target), due.map(at));
    }
    active
}

impl Native {
    /// Steps the phase and carries out what it asks.
    fn event(&mut self, shared: &Arc<Shared>, c: &Control, event: Event) {
        let before = self.phase;
        let effects = machine::step(&mut self.phase, event);
        if decoder::logging() && std::mem::discriminant(&before) != std::mem::discriminant(&self.phase) {
            decoder::log!("{:?} -> {:?}", before, self.phase);
        }
        for effect in effects {
            match effect {
                Effect::Open { at } => self.open(shared, c, at),
                Effect::Seek { to } => {
                    let generation = self.seeks.next().0;
                    if let Some(pipe) = &self.pipe {
                        pipe.lock().seek = Some((to, generation));
                        pipe.poke();
                    }
                    self.shown = None;
                }
                Effect::Clock(anchor) => self.clock.set(anchor),
            }
        }
    }

    fn open(&mut self, shared: &Arc<Shared>, c: &Control, at: f64) {
        let weak = Arc::downgrade(shared);
        let job = Job {
            source: shared.source.clone(),
            kind: shared.kind,
            // Picture times count on through loops; the clip starts at its own time.
            start: c.duration.filter(|d| *d > 0.0).map_or(at, |d| at.rem_euclid(d)),
            generation: self.seeks.current().0,
            looping: c.looping,
            sound: (!c.muted).then_some(c.volume),
            notify: Box::new(move || {
                let Some(shared) = weak.upgrade() else { return };
                match shared.kind {
                    Kind::Video => shared.wake_windows(),
                    Kind::Audio => drive(&shared),
                }
            }),
            clock: self.clock.clone(),
            started: Instant::now(),
        };
        self.opened = Some(job.started);
        self.pipe = Some(decoder::start(job));
    }

    /// Sound is playing: the clip's clock runs on whether or not it's drawn.
    /// A fed video has sound before its facts: they come with its first picture, and its first sound
    /// can arrive before that (a whole segment at once). The decoder drops sound that is not wanted.
    /// A file asks for sound before its facts too: the decoder knows from the file whether it has
    /// any, and an ask withdrawn until the facts arrive would deselect the audio stream just after
    /// the open, so that it resumes where the reader stands and the first seconds stay silent.
    fn sounding(&self, shared: &Shared, c: &Control) -> bool {
        let fed = matches!(&shared.source, MediaSource::Live(live) if live.input().config.audio);
        !c.muted && (fed || self.facts.is_none_or(|f| f.sound.is_some()))
    }

    /// Folds the decoder's news and the handle's intent into the phase.
    fn reconcile(&mut self, shared: &Arc<Shared>, c: &mut Control, target: Instant, may_open: bool) {
        let news = self.pipe.as_ref().map(|p| std::mem::take(&mut p.lock().news)).unwrap_or_default();
        for news in news {
            match news {
                News::Opened(facts) => {
                    self.facts = Some(facts);
                    if !c.opened() {
                        c.open(Ok(facts.meta));
                    }
                    self.event(shared, c, Event::Opened);
                }
                News::Failed(e) => {
                    if decoder::logging() {
                        decoder::log!("failed: {e}");
                    }
                    c.fail(e);
                    self.pipe = None;
                    self.event(shared, c, Event::Failed);
                }
                News::Resized(facts) => {
                    self.facts = Some(facts);
                    c.size = facts.meta.size;
                    c.generation += 1;
                }
                News::Parked => {
                    self.pipe = None;
                    let at = self.shown.unwrap_or(0.0);
                    self.event(shared, c, Event::Released { at });
                }
            }
        }
        if c.failed() {
            self.pipe = None;
            self.event(shared, c, Event::Failed);
            return;
        }
        let lead = vsync::period().as_secs_f64() / 2.0;
        if let Some(to) = c.seek.take() {
            // A fed video cannot seek by itself: the app restarts its run (`LiveVideo::restart`).
            let event = if shared.source.on_demand() { Event::Restart { to } } else { Event::Seek { to } };
            self.event(shared, c, event);
        }
        if c.playing() != self.playing {
            self.playing = c.playing();
            let event = if self.playing {
                Event::Play { target, lead, rate: f64::from(c.rate) }
            } else {
                Event::Pause { at: self.shown.unwrap_or_else(|| self.at()) }
            };
            self.event(shared, c, event);
        }
        if c.rate != self.rate {
            self.rate = c.rate;
            self.event(shared, c, Event::Rate { target, rate: f64::from(c.rate) });
        }
        let sound = self.sounding(shared, c).then_some(c.volume);
        if let Some(pipe) = &self.pipe {
            let mut inbox = pipe.lock();
            if inbox.looping != c.looping || inbox.sound != sound || inbox.play != c.playing() {
                inbox.looping = c.looping;
                inbox.sound = sound;
                inbox.play = c.playing();
                drop(inbox);
                pipe.poke();
            }
        }
        // A clip with no picture yet, or one that plays, opens; a paused one keeps its picture.
        if let Phase::Idle { .. } = self.phase
            && may_open
            && (c.playing() || (self.shown.is_none() && shared.kind == Kind::Video))
        {
            self.event(shared, c, Event::Open { play: c.playing() });
        }
    }

    /// The phase's time, for a pause before any picture showed.
    fn at(&self) -> f64 {
        match self.phase {
            Phase::Idle { at } | Phase::Opening { at, .. } | Phase::Buffering { at, .. } | Phase::Paused { at } | Phase::Ended { at } => at,
            Phase::Playing { clock } => clock.pos,
            Phase::Failed => 0.0,
        }
    }

    /// Puts the picture for `target` on the surface and notes where playback stands.
    fn show(&mut self, shared: &Arc<Shared>, c: &mut Control, surface: &NativeSurface, target: Instant, lead: f64) {
        let Some(pipe) = self.pipe.clone() else { return };
        let generation = self.seeks.current().0;
        let frame_period = self.facts.map_or(1.0 / 60.0, |f| f.meta.frame);
        let jumped;
        let picked = {
            let mut inbox = pipe.lock();
            // A recording shows nothing of a run older than the newest restart: its time is the
            // position at every moment.
            let newest_run = match &shared.source {
                MediaSource::Live(live) => live.input().restarted().unwrap_or(0),
                _ => 0,
            };
            while inbox.frames.front().is_some_and(|f| f.generation != generation || f.run < newest_run) {
                inbox.frames.pop_front();
            }
            jumped = std::mem::take(&mut inbox.jump);
            match self.phase {
                _ if jumped => None,
                Phase::Buffering { .. } => inbox.frames.pop_front(),
                Phase::Paused { .. } if self.shown.is_none() => inbox.frames.pop_front(),
                Phase::Playing { clock } => {
                    let pos = clock.position(target);
                    let mut newest: Option<Decoded> = None;
                    while inbox.frames.front().is_some_and(|f| f.pts <= pos) {
                        // A picture passed over for a later one was due and never shown: a dropped
                        // one if it was under a frame and a half late (the display drew fewer
                        // frames than the clip has), a late one if more.
                        if let Some(passed) = &newest {
                            if pos - passed.pts > frame_period * 1.5 {
                                if self.counts.late == 0 && decoder::logging() {
                                    decoder::log!("live: first late picture: {:.3} s behind the clock at {:.3}", pos - passed.pts, pos);
                                }
                                self.counts.late += 1;
                            } else {
                                self.counts.dropped += 1;
                            }
                        }
                        newest = inbox.frames.pop_front();
                    }
                    newest
                }
                _ => None,
            }
        };
        if jumped {
            // The stream's timeline jumped: the clock starts again at the next picture. A
            // recording needs nothing more: its restart already holds the clock at the new position.
            if !shared.source.on_demand() {
                let at = self.shown.unwrap_or(0.0);
                self.event(shared, c, Event::Stalled { at });
            }
            return;
        }
        let Some(frame) = picked else {
            if shared.source.on_demand() {
                self.note_over(shared, c, &pipe);
            }
            self.check_end(shared, c, &pipe, generation, target);
            return;
        };
        let run = frame.run;
        pipe.poke();
        let pts = frame.pts;
        if let Some(pushed) = frame.pushed {
            self.counts.latency_ms = target.saturating_duration_since(pushed).as_secs_f32() * 1000.0;
        }
        if let Pic::Gpu(_, value) = &frame.picture {
            pipe.surfaced.fetch_max(*value, Ordering::Release);
        }
        self.put(c, surface, frame);
        self.returning = None;
        self.counts.shown += 1;
        if let Phase::Playing { .. } = self.phase
            && let Some(heard) = self.clock.heard_at(target)
        {
            // The sound at the device against the picture presented now: both in media time.
            let offset = heard - pts;
            self.counts.sync_sum += offset;
            self.counts.sync_n += 1;
            self.counts.sync_max = self.counts.sync_max.max(offset.abs());
        }
        if let MediaSource::Live(live) = &shared.source {
            live.input().picture_shown(run);
        }
        if let Some(observer) = &self.observer {
            observer(pts, target);
        }
        if let Phase::Buffering { .. } = self.phase {
            // A live clock starts its delay behind the first picture: the jitter buffer.
            // What the stream already holds past this picture counts toward the delay, so a
            // start on a burst does not wait a second time.
            let delay = match &shared.source {
                MediaSource::Live(live) => {
                    let input = live.input();
                    let held = input.newest_seconds().map_or(0.0, |newest| (newest - pts).max(0.0));
                    (input.target_seconds() - held).max(0.0)
                }
                _ => 0.0,
            };
            self.event(shared, c, Event::Shown { pts: pts - delay, target, lead, rate: f64::from(c.rate) });
        }
    }

    /// A recording whose newest run was taken in to its end, with no picture left to show: it is
    /// over. A run that gave no picture at all (the media ends before its position, or has a gap
    /// to the end) leaves the player waiting for one, so it ends there.
    fn note_over(&mut self, shared: &Arc<Shared>, c: &mut Control, pipe: &Pipe) {
        let MediaSource::Live(live) = &shared.source else { return };
        let input = live.input();
        let Some(run) = input.restarted() else { return };
        let drained = {
            let inbox = pipe.lock();
            inbox.run_ended == Some(run) && inbox.frames.is_empty()
        };
        if !drained {
            return;
        }
        input.run_over(run);
        if let Phase::Buffering { at, .. } = self.phase {
            self.end(shared, c, at, 0.0);
        }
    }

    /// A live clip's clock is steered by its decoder thread (`live`), drawn or not, and that
    /// thread also starts and holds it while nothing draws: the phase takes the clock as it is
    /// now (running, or held for a refill), with no anchor of its own, and a frame after a gap
    /// notes that it came back.
    fn join_clock(&mut self, gap: Option<Duration>, now: Instant) {
        match (self.phase, self.clock.get()) {
            (Phase::Playing { .. } | Phase::Buffering { play: true, .. }, Some(clock)) => self.phase = Phase::Playing { clock },
            (Phase::Playing { clock }, None) => self.phase = Phase::Buffering { at: self.shown.unwrap_or(clock.pos), play: true },
            _ => return,
        }
        self.note_return(gap, now);
    }

    /// A frame came back after a gap: the decoder needs a moment to catch up to the clock, and no
    /// picture is due until then. That is no stall, for a while.
    fn note_return(&mut self, gap: Option<Duration>, now: Instant) {
        if gap.is_some_and(|g| g > LIVE_IDLE) {
            self.returning = Some(now);
        }
    }

    /// Playing with nothing due: the end, or a decoder that fell behind.
    fn check_end(&mut self, shared: &Arc<Shared>, c: &mut Control, pipe: &Pipe, generation: u32, target: Instant) {
        let Phase::Playing { clock } = self.phase else { return };
        let (empty, ended) = {
            let inbox = pipe.lock();
            (inbox.frames.is_empty(), inbox.ended == Some(generation))
        };
        if !empty {
            return;
        }
        let pos = clock.position(target);
        let frame = self.facts.map_or(1.0 / 30.0, |f| f.meta.frame);
        let duration = c.duration.unwrap_or(0.0);
        let shown = self.shown.unwrap_or(clock.pos);
        if ended {
            // The last picture may start a frame past the duration (60 fps against an audio track
            // that ends first): it still belongs to the loop it ends.
            let end = if duration > 0.0 { ((shown - frame) / duration).floor().max(0.0) * duration + duration } else { shown + frame };
            if pos >= end.max(shown + frame) {
                self.end(shared, c, shown, duration);
            }
        } else if pos > shown + frame * 2.0 + 0.1 && !self.returning.is_some_and(|at| at.elapsed() < LIVE_RETURN) {
            self.counts.stalls += 1;
            self.event(shared, c, Event::Stalled { at: shown + frame });
        }
    }

    /// The last picture showed (or the last sound played) and the decoder didn't loop. Looping
    /// turned on since starts the clip again from zero.
    fn end(&mut self, shared: &Arc<Shared>, c: &mut Control, at: f64, duration: f64) {
        // A fed video has no duration in the control: its end is its last picture.
        if c.observe(if duration > 0.0 { duration } else { at }, true) {
            c.seek = Some(0.0);
            return;
        }
        self.event(shared, c, Event::Ended { at });
        self.playing = c.playing();
    }

    /// Puts `frame` on the surface.
    fn put(&mut self, c: &mut Control, surface: &NativeSurface, frame: Decoded) {
        let Some(facts) = self.facts else { return };
        let Some(picture) = facts.picture else { return };
        let id = c.next_frame_id();
        match frame.picture {
            Pic::Gpu(planes, value) => surface.push_decoded(planes, value, id, picture.format, picture.color, picture.orient),
            Pic::Cpu(bytes) => {
                if let Err(e) = surface.push_bytes_oriented(bytes, id, picture.orient) {
                    c.fail(e);
                    return;
                }
            }
        }
        let duration = facts.meta.duration;
        if duration > 0.0 && self.shown.is_some_and(|s| (s / duration).floor() < (frame.pts / duration).floor()) {
            // A loop is news for anything showing progress from zero.
            c.generation += 1;
        }
        self.shown = Some(frame.pts);
        c.time = if duration > 0.0 { frame.pts.rem_euclid(duration).min(duration) } else { frame.pts };
        if let Some(opened) = self.opened.take() {
            crate::marks::mark("video.first_frame.shown");
            if decoder::logging() {
                decoder::log!("first picture on the surface at {} us", decoder::us(opened));
            }
        }
    }

    /// Whether frames should keep coming, and when the next is due.
    fn next(&self, c: &Control, now: Instant) -> (bool, Option<Instant>) {
        let Phase::Playing { clock } = self.phase else { return (false, None) };
        let frame = self.facts.map_or(1.0 / 30.0, |f| f.meta.frame);
        let next = self.pipe.as_ref().and_then(|p| p.lock().frames.front().map(|f| f.pts));
        let shown = self.shown.unwrap_or(clock.pos);
        let pos = next.unwrap_or(shown + frame);
        let _ = c;
        (true, Some(clock.when(pos).max(now)))
    }
}
