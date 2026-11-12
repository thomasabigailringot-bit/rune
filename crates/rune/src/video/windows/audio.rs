//! Sound for a clip: a WASAPI shared-mode stream on the default output device, fed by the
//! decoder and timed by the player's clock. The stream follows the clock rather than leading
//! it: each time the device asks for more, it writes the samples for the moment they'll be
//! heard, padding with silence or skipping ahead when the clock moved (a start, a seek, a stall,
//! the device's own drift), so picture and sound stay together without either waiting.

use super::machine::Anchor;
use super::mf::Sound;
use crate::time::Instant;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::IMFSample;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The device's buffer: long enough to ride out a busy decoder, short enough to react.
pub(super) const BUFFER: Duration = Duration::from_millis(100);
/// A live video's: its sound is only a jitter buffer ahead of the clock, so the device holds less.
pub(super) const BUFFER_LIVE: Duration = Duration::from_millis(20);
/// Sound decoded ahead of what's heard; the decoder waits beyond it.
const AHEAD: f64 = 1.0;
/// How far written sound may drift from the clock before it jumps back in line.
const SLOP: f64 = 0.04;

/// The playback clock the UI sets and the sound follows: none while paused, stalled or ended.
#[derive(Default)]
pub(super) struct Clock {
    anchor: Mutex<Option<Anchor>>,
    changed: Condvar,
    /// Device fills that had no sound queued: silence went out while the clock ran.
    gaps: AtomicU64,
    /// Frames of silence written for those gaps, and frames written in all while the clock ran.
    silent: AtomicU64,
    played: AtomicU64,
    /// The clock may run at other speeds than 1 (a live video catching up): the sound is
    /// resampled to follow it, rather than going silent.
    varispeed: AtomicBool,
    /// The media time the device is playing now, and when that was read.
    heard: Mutex<Option<(f64, Instant)>>,
}

impl Clock {
    pub(super) fn set(&self, anchor: Option<Anchor>) {
        *self.anchor.lock().unwrap() = anchor;
        self.changed.notify_all();
    }
    /// Frames of silence written for gaps, and frames written in all, while the clock ran.
    pub(super) fn silence(&self) -> (u64, u64) {
        (self.silent.load(Ordering::Relaxed), self.played.load(Ordering::Relaxed))
    }
    /// Lets the sound follow a clock that doesn't run at 1 (resampled) instead of going silent.
    pub(super) fn set_varispeed(&self, on: bool) {
        self.varispeed.store(on, Ordering::Relaxed);
    }
    /// Whether the sound can follow a clock running at `rate`.
    fn follows(&self, rate: f64) -> bool {
        if self.varispeed.load(Ordering::Relaxed) { (0.5..=2.0).contains(&rate) } else { (rate - 1.0).abs() < 1e-6 }
    }
    /// Records the media time the device plays now (None: it isn't playing).
    fn set_heard(&self, media: Option<f64>) {
        *self.heard.lock().unwrap() = media.map(|m| (m, Instant::now()));
    }
    /// The media time the sound device plays at `at`, from its own position counter.
    pub(super) fn heard_at(&self, at: Instant) -> Option<f64> {
        let (media, read) = (*self.heard.lock().unwrap())?;
        let speed = self.get().map_or(1.0, |a| a.rate);
        Some(media + speed * (at.checked_duration_since(read).map(|d| d.as_secs_f64()).unwrap_or_else(|| -read.duration_since(at).as_secs_f64())))
    }
    /// How many device fills found no sound queued while the clock ran.
    pub(super) fn gaps(&self) -> u64 {
        self.gaps.load(Ordering::Relaxed)
    }
    pub(super) fn get(&self) -> Option<Anchor> {
        *self.anchor.lock().unwrap()
    }
    /// Sleeps until the clock runs at 1x (or patience passes) while it doesn't: sound plays
    /// only at normal speed.
    fn wait_while_stopped(&self, patience: Duration) {
        let anchor = self.anchor.lock().unwrap();
        if anchor.is_none_or(|a| !self.follows(a.rate)) {
            let _ = self.changed.wait_timeout(anchor, patience);
        }
    }
}

struct Chunk {
    pts: f64,
    samples: Vec<f32>,
}

#[derive(Default)]
struct Feed {
    chunks: VecDeque<Chunk>,
    /// Interleaved samples buffered.
    samples: usize,
    gain: f32,
    stop: bool,
    /// Media time of the next sample the device gets, once it's writing in line with the clock.
    cursor: Option<f64>,
}

/// A clip's sound output; dropping it stops the stream.
pub(super) struct Output {
    feed: Arc<Mutex<Feed>>,
    clock: Arc<Clock>,
    sound: Sound,
}

impl Output {
    pub(super) fn open(sound: Sound, clock: Arc<Clock>, buffer: Duration) -> Result<Self, String> {
        let feed = Arc::new(Mutex::new(Feed { gain: 1.0, ..Feed::default() }));
        let (thread_feed, thread_clock) = (feed.clone(), clock.clone());
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("rune-video-sound".into())
            .spawn(move || {
                // SAFETY: COM for this thread, matched below.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                match Stream::new(sound, buffer) {
                    Ok(stream) => {
                        let _ = ready.send(Ok(()));
                        stream.run(&thread_feed, &thread_clock, sound);
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
                if com {
                    // SAFETY: matches the initialisation above.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| e.to_string())?;
        started.recv().map_err(|_| "the sound thread stopped".to_string())??;
        Ok(Self { feed, clock, sound })
    }

    /// Queues a decoded float sample at `pts` seconds; sound before `skip_before` (a seek's
    /// pre-roll) is dropped.
    pub(super) fn push(&self, sample: &IMFSample, pts: f64, skip_before: f64, _generation: u32) {
        let Some(samples) = samples(sample) else { return };
        self.push_pcm(samples, pts, skip_before);
    }

    /// Queues interleaved float sound at `pts` seconds; sound before `skip_before` is dropped.
    pub(super) fn push_pcm(&self, samples: Vec<f32>, pts: f64, skip_before: f64) {
        let channels = usize::from(self.sound.channels);
        let rate = f64::from(self.sound.rate);
        let end = pts + (samples.len() / channels) as f64 / rate;
        if end <= skip_before {
            return;
        }
        let mut feed = self.feed.lock().unwrap();
        feed.samples += samples.len();
        feed.chunks.push_back(Chunk { pts, samples });
    }

    /// Forgets queued sound (a seek).
    pub(super) fn flush(&self) {
        let mut feed = self.feed.lock().unwrap();
        feed.chunks.clear();
        feed.samples = 0;
        feed.cursor = None;
    }

    pub(super) fn set_gain(&self, gain: f32) {
        self.feed.lock().unwrap().gain = gain.clamp(0.0, 1.0);
    }

    /// Enough is queued; the decoder may wait.
    pub(super) fn full(&self) -> bool {
        let per_second = f64::from(self.sound.rate) * f64::from(self.sound.channels);
        self.feed.lock().unwrap().samples as f64 >= AHEAD * per_second
    }

    /// Nothing plays: paused, stalled or ended.
    pub(super) fn idle(&self) -> bool {
        self.clock.get().is_none()
    }

    /// The decoder reached the end; what's queued still plays out.
    pub(super) fn end(&self, _generation: u32) {}
}

impl Drop for Output {
    fn drop(&mut self) {
        self.feed.lock().unwrap().stop = true;
        self.clock.changed.notify_all();
    }
}

/// A decoded float sample's interleaved values.
fn samples(sample: &IMFSample) -> Option<Vec<f32>> {
    // SAFETY: the buffer is locked for the copy and unlocked after it.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().ok()?;
        let (mut base, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut base, None, Some(&mut len)).ok()?;
        let values = std::slice::from_raw_parts(base.cast::<f32>(), len as usize / 4).to_vec();
        let _ = buffer.Unlock();
        Some(values)
    }
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    frames: u32,
    latency: Duration,
    position: Option<IAudioClock>,
}

impl Stream {
    fn new(sound: Sound, buffer: Duration) -> Result<Self, String> {
        // SAFETY: COM calls on this thread's objects; the format describes interleaved float.
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.message())?;
            let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|_| "no sound output device".to_string())?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| e.message())?;
            let block = 4 * sound.channels;
            let format = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: sound.channels,
                nSamplesPerSec: sound.rate,
                nAvgBytesPerSec: sound.rate * u32::from(block),
                nBlockAlign: block,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer.as_nanos() as i64 / 100, 0, &format, None).map_err(|e| e.message())?;
            let event = CreateEventW(None, false, false, None).map_err(|e| e.message())?;
            client.SetEventHandle(event).map_err(|e| e.message())?;
            let render: IAudioRenderClient = client.GetService().map_err(|e| e.message())?;
            let frames = client.GetBufferSize().map_err(|e| e.message())?;
            let latency = Duration::from_nanos(client.GetStreamLatency().unwrap_or(0).max(0) as u64 * 100);
            let position = client.GetService::<IAudioClock>().ok();
            Ok(Self { client, render, event, frames, latency, position })
        }
    }

    /// The media time at the device's play position, from its own counter.
    fn heard(&self, history: &mut VecDeque<(u64, f64, f64)>, rate: f64) -> Option<f64> {
        let clock = self.position.as_ref()?;
        let (mut position, mut qpc) = (0u64, 0u64);
        // SAFETY: plain reads of the stream's clock into locals.
        let frequency = unsafe {
            clock.GetPosition(&mut position, Some(&mut qpc)).ok()?;
            clock.GetFrequency().ok()?
        };
        let played = (position as f64 / frequency as f64 * rate) as u64;
        while history.len() > 1 && history[1].0 <= played {
            history.pop_front();
        }
        let &(start, media, speed) = history.front()?;
        Some(media + played.saturating_sub(start) as f64 * speed / rate)
    }

    fn run(&self, feed: &Mutex<Feed>, clock: &Clock, sound: Sound) {
        let channels = usize::from(sound.channels);
        let rate = f64::from(sound.rate);
        let mut started = false;
        // Where each fill's sound sits in media time, to say what the device plays now.
        let mut written: u64 = 0;
        let mut history: VecDeque<(u64, f64, f64)> = VecDeque::new();
        loop {
            if !started {
                // Stopped: sleep until the clock runs, not on the device's period.
                clock.wait_while_stopped(Duration::from_secs(1));
            }
            // SAFETY: a wait on this stream's own event.
            let woke = started && unsafe { WaitForSingleObject(self.event, 20) } == WAIT_OBJECT_0;
            if feed.lock().unwrap().stop {
                break;
            }
            let anchor = clock.get().filter(|a| clock.follows(a.rate));
            let Some(anchor) = anchor else {
                if started {
                    // SAFETY: stops and empties this stream.
                    unsafe {
                        let _ = self.client.Stop();
                        let _ = self.client.Reset();
                    }
                    started = false;
                    feed.lock().unwrap().cursor = None;
                    (written, history) = (0, VecDeque::new());
                    clock.set_heard(None);
                }
                continue;
            };
            if started && !woke {
                continue;
            }
            // SAFETY: buffer calls on this stream; the device buffer holds `free` frames.
            unsafe {
                let padding = self.client.GetCurrentPadding().unwrap_or(self.frames);
                let free = self.frames.saturating_sub(padding);
                if free == 0 {
                    continue;
                }
                let Ok(data) = self.render.GetBuffer(free) else { continue };
                let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), free as usize * channels);
                let heard = Instant::now() + self.latency + Duration::from_secs_f64(f64::from(padding) / rate);
                let (silent, from) = fill(&mut feed.lock().unwrap(), out, channels, rate, anchor.position(heard), anchor.rate);
                history.push_back((written, from, anchor.rate));
                written += u64::from(free);
                clock.played.fetch_add(u64::from(free), Ordering::Relaxed);
                if silent > 0 {
                    clock.gaps.fetch_add(1, Ordering::Relaxed);
                    clock.silent.fetch_add(silent as u64, Ordering::Relaxed);
                }
                let _ = self.render.ReleaseBuffer(free, 0);
                if !started {
                    started = self.client.Start().is_ok();
                }
                if started {
                    clock.set_heard(self.heard(&mut history, rate));
                }
            }
        }
        // SAFETY: stops this stream and closes its event.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Writes `out` with the sound from media time `want` on, in line with the clock, which runs
/// `speed` media seconds a second (the sound is resampled when that isn't 1). Returns how many
/// frames of `out` had no sound to take (silence for a gap, not for a sound that starts later)
/// and the media time of its first frame.
fn fill(feed: &mut Feed, out: &mut [f32], channels: usize, rate: f64, want: f64, speed: f64) -> (usize, f64) {
    let mut cursor = match feed.cursor {
        Some(c) if (c - want).abs() < SLOP => c,
        _ => want,
    };
    let from = cursor;
    let frames = out.len() / channels;
    if (speed - 1.0).abs() > 1e-6 {
        let step = speed / rate;
        let mut gap = 0;
        for i in 0..frames {
            while let Some(c) = feed.chunks.front() {
                if c.pts + (c.samples.len() / channels) as f64 / rate > cursor {
                    break;
                }
                feed.samples -= c.samples.len();
                feed.chunks.pop_front();
            }
            let Some(chunk) = feed.chunks.front() else {
                out[i * channels..].fill(0.0);
                cursor += (frames - i) as f64 * step;
                gap = frames - i;
                break;
            };
            let at = (cursor - chunk.pts) * rate;
            if at < 0.0 {
                out[i * channels..(i + 1) * channels].fill(0.0);
            } else {
                let (i0, frac) = (at as usize, (at - at.floor()) as f32);
                let n = chunk.samples.len() / channels;
                let next = if i0 + 1 < n { Some(&chunk.samples[(i0 + 1) * channels..]) } else { feed.chunks.get(1).map(|c| &c.samples[..]) };
                for ch in 0..channels {
                    let a = chunk.samples[i0 * channels + ch];
                    let b = next.and_then(|s| s.get(ch)).copied().unwrap_or(a);
                    out[i * channels + ch] = (a + (b - a) * frac) * feed.gain;
                }
            }
            cursor += step;
        }
        feed.cursor = Some(cursor);
        return (gap, from);
    }
    let mut written = 0;
    let mut gap = 0;
    while written < frames {
        while let Some(c) = feed.chunks.front() {
            let end = c.pts + (c.samples.len() / channels) as f64 / rate;
            if end > cursor + 0.5 / rate {
                break;
            }
            feed.samples -= c.samples.len();
            feed.chunks.pop_front();
        }
        let Some(chunk) = feed.chunks.front() else {
            out[written * channels..].fill(0.0);
            cursor += (frames - written) as f64 / rate;
            gap = frames - written;
            break;
        };
        let start = ((cursor - chunk.pts) * rate).round() as i64;
        let n = if start < 0 {
            let n = ((-start) as usize).min(frames - written);
            out[written * channels..(written + n) * channels].fill(0.0);
            n
        } else {
            let start = start as usize;
            let n = (chunk.samples.len() / channels - start).min(frames - written);
            let gain = feed.gain;
            for (o, s) in out[written * channels..(written + n) * channels].iter_mut().zip(&chunk.samples[start * channels..]) {
                *o = s * gain;
            }
            n
        };
        written += n;
        cursor += n as f64 / rate;
    }
    feed.cursor = Some(cursor);
    (gap, from)
}
