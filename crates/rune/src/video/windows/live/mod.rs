//! A live video's decoder thread (`LiveVideo`): takes the access units and AAC frames the app
//! pushed, decodes the pictures with the H.264 transform (`h264`) into the player's GPU ring and
//! the sound with the AAC transform (`aac`) into the sound output, and reports to the UI through
//! the same pipe a clip's decoder uses. No container, no source reader and no probe: a picture is
//! on the queue as soon as the transform gives it.
//!
//! This thread owns the clock's speed (`crate::video::steer` says how it follows the stream) and
//! steers it whether or not a window draws, so the sound stays on the stream behind a covered or
//! minimized window. Decoded pictures wait for the UI (`Native::show`). While nothing draws (no
//! frame for `REST`) no picture is decoded at all: the access units wait from the last keyframe
//! before the clock, and decoding restarts there when a frame draws again.

mod aac;
mod h264;

use super::audio::{self, Output};
use super::machine::Anchor;
use super::decoder::{Decoded, Facts, Job, News, Pic, Pipe, log, logging, us};
use super::gpu::{self, Device, RING_LIVE, Ring};
use super::mf::{self, Library};
use crate::native_surface::windows::DecodedPlanes;
use crate::time::Instant;
use crate::video::Meta;
use crate::video::live::{AacFrame, AccessUnit, LiveInput, Pushed};
use crate::video::steer;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_DECODER, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{IMFDXGIBuffer, IMFSample};
use windows::core::Interface;

/// Decoded pictures waiting for their time past which the oldest is dropped: the UI isn't drawing
/// (covered, say) and the stream must go on.
const MAX_QUEUE: usize = 40;
/// Decoded pictures kept ahead of the clock: enough to ride out a slow wake of this thread,
/// few enough to leave the texture ring room.
const AHEAD: usize = 8;
/// A live clip undrawn this long is at rest: its decoder decodes no pictures, and its clock is
/// steered here. (Shorter hitches of the UI are not worth dropping the pictures for.)
const REST: Duration = Duration::from_millis(500);
/// A clip's clock is steered here once no frame has drawn it for this long; the frame that
/// draws it steers it otherwise.
const UNDRAWN: Duration = Duration::from_millis(100);
/// How long the thread waits for something to arrive before it looks at the pipe again.
const PATIENCE: Duration = Duration::from_millis(4);

/// 90 kHz ticks as Media Foundation's 100 ns units.
fn hundred_ns(ticks: i64) -> i64 {
    ticks * 1_000 / 9
}

pub(super) fn run(pipe: &Pipe, job: Job, input: &Arc<LiveInput>) {
    let mut live = match Live::open(input.clone(), &job) {
        Ok(live) => live,
        Err(e) => {
            pipe.lock().news.push(News::Failed(e));
            (job.notify)();
            return;
        }
    };
    if let Err(e) = live.work(pipe, &job) {
        pipe.lock().news.push(News::Failed(e));
        (job.notify)();
    }
}

struct Live {
    input: Arc<LiveInput>,
    lib: &'static Library,
    device: Option<Arc<Device>>,
    video: h264::Video,
    ring: Option<Ring>,
    audio: Option<aac::Audio>,
    output: Option<Output>,
    facts: Option<Facts>,
    /// The first time stamp seen, in 90 kHz ticks: where seconds start.
    origin: Option<i64>,
    /// A keyframe has gone in since the start or the last discontinuity.
    synced: bool,
    /// Access units in the transform: (time in 100 ns units, when pushed).
    pending: VecDeque<(i64, Instant)>,
    /// Access units that wait for their turn to be decoded.
    backlog: VecDeque<Pushed<AccessUnit>>,
    /// After a jump: pictures before this time (seconds) are decoded but not kept.
    skip_until: Option<f64>,
    /// The sound output failed to open (logged once).
    sound_failed: bool,
    /// Where the next sound block starts, if the transform stamps none.
    sound_next: f64,
    clock: Arc<audio::Clock>,
    /// Steers the clock.
    follower: steer::Follower,
    /// At rest: no frame draws the clip (`REST`), and no picture is decoded.
    resting: bool,
    /// The stream's timeline jumped while nothing drew: the clock starts again on the new one.
    reanchor: bool,
}

impl Live {
    fn open(input: Arc<LiveInput>, job: &Job) -> Result<Self, String> {
        let lib = mf::library()?;
        let software = std::env::var_os("RUNE_VIDEO_DECODE").is_some_and(|v| v == "software");
        let device = gpu::device().ok();
        let manager = device
            .as_ref()
            .filter(|_| !software)
            .map(|d| d.manager(lib))
            .transpose()?;
        let video = h264::Video::new(lib, manager.as_ref())?;
        if logging() {
            log!(
                "live: H.264 decoder ready at {} us ({})",
                us(job.started),
                if video.hardware {
                    "D3D11 manager"
                } else {
                    "CPU"
                }
            );
        }
        job.clock.set_varispeed(true);
        Ok(Self {
            input,
            lib,
            device,
            video,
            ring: None,
            audio: None,
            output: None,
            facts: None,
            origin: None,
            synced: false,
            pending: VecDeque::new(),
            backlog: VecDeque::new(),
            skip_until: None,
            sound_failed: false,
            sound_next: 0.0,
            clock: job.clock.clone(),
            follower: steer::Follower::default(),
            resting: false,
            reanchor: false,
        })
    }

    fn work(&mut self, pipe: &Pipe, job: &Job) -> Result<(), String> {
        loop {
            let (sound, idle) = {
                let s = pipe.lock();
                if s.stop {
                    return Ok(());
                }
                (s.sound, s.touched.map_or(Duration::ZERO, |t| t.elapsed()))
            };
            self.follow_sound(sound);
            let queues = self.input.take(PATIENCE);
            if queues.discontinuity {
                self.backlog.clear();
                self.restart(pipe);
            }
            // Pictures are decoded only a little ahead of the clock. The app's jitter buffer
            // (`target_delay`) waits here as access units, which cost far less than pictures.
            self.backlog.extend(queues.video);
            if !self.resting {
                self.bound_backlog(pipe);
            }
            // Sound first: the first picture's facts say whether there is sound to play.
            for frame in queues.audio {
                self.sound_frame(frame, sound.is_some());
            }
            self.follow(pipe, idle);
            let resting = self.rest(pipe, idle);
            while !resting && pipe.lock().frames.len() < AHEAD {
                let Some(unit) = self.backlog.pop_front() else { break };
                self.video_unit(unit, pipe, job)?;
            }
            if queues.closed && !self.backlog.is_empty() {
                // `take` no longer waits once the input is closed.
                std::thread::sleep(PATIENCE);
            } else if queues.closed && self.input.pushed() > 0 {
                let generation = job.generation;
                pipe.lock().ended = Some(generation);
                (job.notify)();
                return Ok(());
            }
        }
    }

    /// Nobody takes the pictures (the window is covered, say) while the stream goes on: more than
    /// `steer::backlog_limit` of video waiting is cut to the keyframe that keeps the target, and
    /// the sound before it goes too. The clock then rejoins by the jump path, with nothing stale
    /// to show on the way.
    fn bound_backlog(&mut self, pipe: &Pipe) {
        let target = self.input.target_seconds();
        let units = self
            .backlog
            .iter()
            .map(|u| (u.item.pts as f64 / 90_000.0, u.item.keyframe));
        let Some(cut) = steer::backlog_cut(units, target) else { return };
        if let Some(first) = self.backlog.front() {
            self.seconds(first.item.pts);
        }
        let floor = self.seconds((cut.floor * 90_000.0) as i64);
        self.backlog.drain(..cut.drop);
        self.video.flush();
        self.pending.clear();
        self.skip_until = Some(floor);
        let mut lost = cut.drop as u64;
        {
            let mut s = pipe.lock();
            let before = s.frames.len();
            s.frames.retain(|frame| frame.pts >= floor);
            lost += (before - s.frames.len()) as u64;
        }
        if let Some(output) = &self.output {
            output.trim(floor, steer::backlog_limit(target));
        }
        if pipe.late.fetch_add(lost, Ordering::Relaxed) == 0 && logging() {
            log!("live: {lost} pictures dropped, more than {:.1} s of video was waiting", steer::backlog_limit(target));
        }
    }

    /// Steers the clock: faster for a backlog, slower for a thin queue, forward when it is far
    /// behind. The clock runs only while the UI shows the clip (`Native`); from then on this
    /// thread is the only one that changes its speed.
    fn follow(&mut self, pipe: &Pipe, idle: Duration) {
        if self.reanchor {
            self.rejoin(pipe, idle);
            return;
        }
        let Some(clock) = self.clock.get() else { return };
        let Some(newest) = self.input.newest_seconds() else { return };
        let wanted = self.input.target_seconds();
        let now = Instant::now();
        let depth = newest - clock.position(now);
        match self.follower.step(depth, wanted, clock.rate) {
            steer::Steer::Keep => {}
            steer::Steer::Rate(rate) => {
                self.clock.update(|a| Anchor { at: now, pos: a.position(now), rate });
            }
            steer::Steer::Jump => {
                if logging() {
                    log!("live: {depth:.2} s behind, jumping to {wanted:.2} s{}", if idle > UNDRAWN { " (undrawn)" } else { "" });
                }
                let to = newest - wanted;
                self.clock.update(|a| Anchor { at: now, pos: to, rate: a.rate });
                self.skip(to, pipe);
            }
        }
    }

    /// After a discontinuity nobody drew: the clock starts again on the new timeline, at the
    /// target behind the stream, so the sound goes on. A frame that draws takes the discontinuity
    /// itself (`Native::show`) and shows the first picture.
    fn rejoin(&mut self, pipe: &Pipe, idle: Duration) {
        if self.origin.is_none()
            && let Some(first) = self.backlog.front()
        {
            self.seconds(first.item.pts);
        }
        let Some(newest) = self.input.newest_seconds() else { return };
        let mut s = pipe.lock();
        if !s.jump {
            self.reanchor = false;
        } else if idle > UNDRAWN {
            s.jump = false;
            drop(s);
            self.reanchor = false;
            let (now, to) = (Instant::now(), newest - self.input.target_seconds());
            self.clock.update(|_| Anchor { at: now, pos: to, rate: 1.0 });
            self.skip_until = Some(to);
            self.follower = steer::Follower::default();
            if logging() {
                log!("live: new timeline while undrawn, clock restarts at {to:.2} s");
            }
        }
    }

    /// Nobody draws the clip: no pictures are decoded (nobody would see them) and the access
    /// units kept start at the last keyframe at or before the clock. Returns whether it is at
    /// rest. Decoding restarts from that keyframe when a frame draws again, and the pictures
    /// before the clock are decoded but not kept: the first picture is the one at the clock,
    /// at most a keyframe interval of decoding after the frame, with the sound untouched.
    fn rest(&mut self, pipe: &Pipe, idle: Duration) -> bool {
        let anchor = self.clock.get();
        let Some(anchor) = anchor.filter(|_| idle >= REST) else {
            if std::mem::take(&mut self.resting) {
                if let Some(a) = anchor {
                    self.skip_until = Some(a.position(Instant::now()));
                }
                if logging() {
                    log!("live: drawn again, decoding from the keyframe before the clock");
                }
            }
            return false;
        };
        if !self.resting {
            self.resting = true;
            self.video.flush();
            self.pending.clear();
            self.synced = false;
            pipe.lock().frames.clear();
            if logging() {
                log!("live: undrawn for {idle:?}, no pictures decoded");
            }
        }
        if let Some(origin) = self.origin {
            let pos = anchor.position(Instant::now());
            let reachable = |u: &Pushed<AccessUnit>| u.item.keyframe && (u.item.pts - origin) as f64 / 90_000.0 <= pos;
            if let Some(at) = self.backlog.iter().rposition(reachable) {
                self.backlog.drain(..at);
            }
        }
        true
    }

    /// The clock jumped forward to `to` seconds: no picture before it is wanted.
    fn skip(&mut self, to: f64, pipe: &Pipe) {
        self.skip_until = Some(to);
        // A picture decoded before the jump would restart the clock at its own, old time.
        pipe.lock().frames.retain(|frame| frame.pts >= to);
        let Some(origin) = self.origin else { return };
        // Decoding can only restart at a keyframe: the last one at or before the new time.
        let reachable = |unit: &Pushed<AccessUnit>| {
            unit.item.keyframe && (unit.item.pts - origin) as f64 / 90_000.0 <= to
        };
        if let Some(first) = self.backlog.iter().rposition(reachable)
            && first > 0
        {
            self.backlog.drain(..first);
            self.video.flush();
            self.pending.clear();
            pipe.lock().frames.clear();
        }
    }

    /// Opens or closes the sound output as the player's mute and volume say.
    fn follow_sound(&mut self, sound: Option<f32>) {
        match (sound, &self.output, &self.audio) {
            (Some(_), None, Some(audio)) => {
                match Output::open(audio.sound, self.clock(), audio::BUFFER_LIVE) {
                    Ok(output) => self.output = Some(output),
                    Err(e) => {
                        // Tried again on every pass: say it once.
                        if logging() && !std::mem::replace(&mut self.sound_failed, true) {
                            log!("live: no sound output: {e}");
                        }
                    }
                }
            }
            (None, Some(_), _) => self.output = None,
            _ => {}
        }
        if let (Some(output), Some(gain)) = (&self.output, sound) {
            output.set_gain(gain);
        }
    }

    fn clock(&self) -> Arc<audio::Clock> {
        self.clock.clone()
    }

    fn seconds(&mut self, ticks: i64) -> f64 {
        let origin = *self.origin.get_or_insert(ticks);
        self.input.set_origin(Some(origin));
        (ticks - origin) as f64 / 90_000.0
    }

    /// The stream's timeline jumped: drop everything in flight and wait for a keyframe.
    fn restart(&mut self, pipe: &Pipe) {
        self.video.flush();
        if let Some(audio) = &mut self.audio {
            audio.flush();
        }
        if let Some(output) = &self.output {
            output.flush();
        }
        self.synced = false;
        self.skip_until = None;
        self.reanchor = true;
        self.origin = None;
        self.input.set_origin(None);
        self.pending.clear();
        let mut s = pipe.lock();
        s.frames.clear();
        s.jump = true;
    }

    fn sound_frame(&mut self, frame: Pushed<AacFrame>, wanted: bool) {
        if !wanted {
            return;
        }
        if self.audio.is_none() {
            let Some(header) = aac::adts(&frame.item.data) else {
                return;
            };
            match aac::Audio::new(self.lib, header) {
                Ok(audio) => self.audio = Some(audio),
                Err(e) => {
                    if logging() {
                        log!("live: no AAC decoder: {e}");
                    }
                    return;
                }
            }
        }
        let time = hundred_ns(frame.item.pts);
        let seconds = self.seconds(frame.item.pts);
        let Some(audio) = &mut self.audio else { return };
        if audio.push(&frame.item.data, time).is_err() {
            return;
        }
        while let Some((values, stamp)) = audio.pull() {
            let channels = usize::from(audio.sound.channels);
            let rate = f64::from(audio.sound.rate);
            let at = match (stamp, self.origin) {
                (Some(t), Some(origin)) => (t - hundred_ns(origin)) as f64 / 1e7,
                _ if self.sound_next > 0.0 => self.sound_next,
                _ => seconds,
            };
            self.sound_next = at + (values.len() / channels) as f64 / rate;
            if let Some(output) = &self.output {
                output.push_pcm(values, at, self.skip_until.unwrap_or(f64::NEG_INFINITY));
                output.trim(f64::NEG_INFINITY, steer::backlog_limit(self.input.target_seconds()));
            }
        }
    }

    fn video_unit(
        &mut self,
        unit: Pushed<AccessUnit>,
        pipe: &Pipe,
        job: &Job,
    ) -> Result<(), String> {
        let AccessUnit {
            data,
            pts,
            keyframe,
        } = unit.item;
        if !self.synced {
            if !keyframe {
                return Ok(());
            }
            self.synced = true;
        }
        self.seconds(pts);
        let time = hundred_ns(pts);
        self.pending.push_back((time, unit.at));
        for attempt in 0..2 {
            match self.video.push(&data, time) {
                Ok(()) => break,
                Err(false) if attempt == 0 => self.drain(pipe, job)?,
                Err(_) => {
                    // A unit the transform refuses: start over from the next keyframe.
                    self.video.flush();
                    self.synced = false;
                    self.pending.clear();
                    return Ok(());
                }
            }
        }
        self.drain(pipe, job)
    }

    /// Takes every picture the transform has.
    fn drain(&mut self, pipe: &Pipe, job: &Job) -> Result<(), String> {
        loop {
            match self.video.pull()? {
                h264::Output::Picture(sample, time) => self.picture(&sample, time, pipe, job)?,
                h264::Output::More => return Ok(()),
                h264::Output::Changed => self.changed(pipe, job)?,
            }
        }
    }

    /// The pictures' size (or the first facts of them) is known: a ring to fit, and the UI told.
    fn changed(&mut self, pipe: &Pipe, job: &Job) -> Result<(), String> {
        let picture = self
            .video
            .picture
            .ok_or("the decoder gave no picture size")?;
        if let Some(device) = &self.device {
            // Pictures already queued hold the old ring's textures; they play out.
            let mut ring = Ring::new(device.clone(), &picture, RING_LIVE)?;
            ring.follow_surface(pipe.surfaced.clone(), self.ring.as_ref());
            self.ring = Some(ring);
        }
        let frame = if picture.frame > 0.0 && picture.frame < 0.2 {
            picture.frame
        } else {
            1.0 / 60.0
        };
        let sound = self.audio.as_ref().map(|a| a.sound).or_else(|| {
            self.input.config.audio.then_some(mf::Sound {
                channels: 2,
                rate: 48_000,
            })
        });
        let facts = Facts {
            meta: Meta {
                duration: 0.0,
                size: Some(picture.natural),
                frame,
            },
            picture: Some(picture),
            sound,
            hardware: self.video.hardware,
            gpu: self.ring.is_some(),
        };
        let first = self.facts.is_none();
        self.facts = Some(facts);
        let mut s = pipe.lock();
        s.news.push(if first {
            News::Opened(facts)
        } else {
            News::Resized(facts)
        });
        drop(s);
        if logging() {
            log!(
                "live: {} {}x{} at {} us",
                if first { "opened" } else { "resized to" },
                picture.natural.0,
                picture.natural.1,
                us(job.started)
            );
        }
        (job.notify)();
        Ok(())
    }

    fn picture(
        &mut self,
        sample: &IMFSample,
        time: i64,
        pipe: &Pipe,
        job: &Job,
    ) -> Result<(), String> {
        if self.facts.is_none() {
            self.changed(pipe, job)?;
        }
        let pushed = loop {
            match self.pending.front() {
                Some(&(t, _)) if t < time => {
                    self.pending.pop_front();
                }
                Some(&(t, at)) if t == time => {
                    self.pending.pop_front();
                    break Some(at);
                }
                _ => break None,
            }
        };
        let picture = self.video.picture.ok_or("no picture size")?;
        let pts = (time - hundred_ns(self.origin.unwrap_or(0))) as f64 / 1e7;
        if self.skip_until.is_some_and(|to| pts < to) {
            return Ok(());
        }
        self.skip_until = None;
        let decoded = if let Some(ring) = &mut self.ring {
            let Some(slot) = ring.take()? else {
                if pipe.late.fetch_add(1, Ordering::Relaxed) == 0 && logging() {
                    log!(
                        "live: no free texture, dropping pictures ({} units in, {} pictures out, {} queued)",
                        self.video.inputs,
                        self.video.outputs,
                        pipe.lock().frames.len()
                    );
                }
                return Ok(());
            };
            // SAFETY: COM reads on a sample this thread owns.
            let texture = unsafe {
                sample
                    .GetBufferByIndex(0)
                    .ok()
                    .and_then(|b| b.cast::<IMFDXGIBuffer>().ok())
                    .and_then(|b| {
                        let mut raw = std::ptr::null_mut();
                        b.GetResource(&ID3D11Texture2D::IID, &mut raw).ok()?;
                        Some((
                            ID3D11Texture2D::from_raw(raw),
                            b.GetSubresourceIndex().ok()?,
                        ))
                    })
            };
            let (planes, value): (Arc<DecodedPlanes>, u64) = match texture {
                Some((texture, index)) => {
                    let mut desc = Default::default();
                    // SAFETY: a plain query on a live texture.
                    unsafe { texture.GetDesc(&mut desc) };
                    self.video.hardware = desc.BindFlags & D3D11_BIND_DECODER.0 as u32 != 0;
                    ring.copy(slot, &texture, index)
                }
                None => {
                    self.video.hardware = false;
                    ring.upload(slot, &mf::nv12_bytes(sample, &picture)?)
                }
            };
            Pic::Gpu(planes, value)
        } else {
            Pic::Cpu(mf::nv12_bytes(sample, &picture)?)
        };
        pipe.hardware
            .store(if self.video.hardware { 2 } else { 1 }, Ordering::Relaxed);
        pipe.hold
            .store(self.video.hold.unwrap_or(0) as u32, Ordering::Relaxed);
        let mut s = pipe.lock();
        let was_empty = s.frames.is_empty();
        if s.frames.len() >= MAX_QUEUE {
            s.frames.pop_front();
            if pipe.late.fetch_add(1, Ordering::Relaxed) == 0 && logging() {
                log!("live: picture queue full, dropping pictures");
            }
        }
        s.frames.push_back(Decoded {
            generation: job.generation,
            pts,
            picture: decoded,
            pushed,
        });
        drop(s);
        if was_empty {
            (job.notify)();
        }
        Ok(())
    }
}
