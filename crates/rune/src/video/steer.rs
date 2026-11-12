//! How a live video's clock follows the stream: the queue's depth (newest picture pushed minus
//! the clock) against the target decides whether the clock keeps its speed, runs a little faster
//! to eat a backlog, a little slower to rebuild a thin queue, or jumps. [`backlog_cut`] bounds
//! what waits for a consumer that is not running. While nothing draws, [`Follower::hidden`]
//! also runs the clock's life cycle: it starts, holds for a refill after an underrun, and
//! restarts on a new timeline.

/// Faster than this the sound would be too far from itself.
const MAX_SPEEDUP: f64 = 0.08;
/// Slower by this much when the queue is nearly empty.
const SLOWDOWN: f64 = 0.03;
/// A backlog past the target by this many seconds, at least, is too far behind to catch up by
/// speed. It grows with the target: a long jitter buffer is allowed a long stretch past it.
const HARD: f64 = 1.0;
/// Excess depth the clock ignores, in seconds at least, and as a fraction of the target.
const SLACK: f64 = 0.04;
const SLACK_PART: f64 = 0.1;
/// The queue is nearly empty under this fraction of the target, but never under more than
/// `FLOOR_MAX` seconds: media delivered in bursts leaves a long buffer drained far below its
/// target between two bursts, and that is healthy.
const FLOOR_PART: f64 = 0.4;
const FLOOR_MAX: f64 = 0.4;
/// Slower by this much while the queue grows to a raised target.
const REFILL: f64 = 0.05;
/// A speed change smaller than this isn't worth re-anchoring the clock for.
const STEP: f64 = 0.004;

/// Media waiting past the point where the clock would jump by more than this many seconds means
/// nothing consumes it: the jump comes first when the window draws.
const BACKLOG_EXTRA: f64 = 1.0;

/// Excess depth over the target past which the clock jumps.
fn hard(target: f64) -> f64 {
    HARD.max(target)
}

/// Excess depth over the target the clock ignores.
fn slack(target: f64) -> f64 {
    SLACK.max(SLACK_PART * target)
}

/// Depth under which the queue is nearly empty.
fn floor(target: f64) -> f64 {
    (FLOOR_PART * target).min(FLOOR_MAX)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Steer {
    Keep,
    Rate(f64),
    /// Too far behind: jump the clock forward to the target depth.
    Jump,
}

/// Whether a queue that grows to a raised target has got there.
pub(crate) fn refilled(depth: f64, target: f64) -> bool {
    depth >= target - slack(target)
}

/// What the clock should do for a queue `depth` seconds deep, aiming at `target`, running at
/// `rate`. `refill`: the target was raised and the queue has not reached it yet, so a queue
/// under the target is slowed down for, however full it is.
pub(crate) fn steer(depth: f64, target: f64, rate: f64, refill: bool) -> Steer {
    let excess = depth - target;
    if excess > hard(target) {
        return Steer::Jump;
    }
    let wanted = if excess > slack(target) {
        1.0 + ((excess - slack(target)) * 0.5).min(MAX_SPEEDUP)
    } else if refill {
        1.0 - REFILL
    } else if depth < floor(target) {
        1.0 - SLOWDOWN
    } else {
        1.0
    };
    // Back at the target the clock returns to exactly 1x: a small leftover would drain the
    // queue, and the sound with it, until the slowdown kicked in.
    let settle = wanted == 1.0 && rate != 1.0;
    if settle || (wanted - rate).abs() >= STEP {
        Steer::Rate(wanted)
    } else {
        Steer::Keep
    }
}

/// How many seconds of media may wait, at most, for a `target` delay: past what the clock
/// would jump over, so that a window that draws jumps before anything is dropped here.
pub(crate) fn backlog_limit(target: f64) -> f64 {
    target + hard(target) + BACKLOG_EXTRA
}

/// Where a backlog is cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Cut {
    /// Units to drop from the front: the one after them is a keyframe.
    pub drop: usize,
    /// The time (seconds) that stays live: the newest unit minus the target. Pictures before it
    /// are not wanted.
    pub floor: f64,
}

/// What to drop from the units waiting to be decoded, given as `(seconds, keyframe)` in push
/// order. More than [`backlog_limit`] of video waiting means the consumer is not running
/// (the window is covered, say): decoding can only restart at a keyframe, so drop up to the latest
/// one at or before `newest - target`, which keeps about the target. A cut that would keep less
/// (no such keyframe after the first unit) goes to the next keyframe; with none, nothing is cut.
pub(crate) fn backlog_cut(
    units: impl Iterator<Item = (f64, bool)> + Clone,
    target: f64,
) -> Option<Cut> {
    let oldest = units.clone().next()?.0;
    let newest = units.clone().map(|u| u.0).fold(oldest, f64::max);
    if newest - oldest <= backlog_limit(target) {
        return None;
    }
    let floor = newest - target;
    let (mut before, mut after) = (None, None);
    for (i, (seconds, keyframe)) in units.enumerate() {
        if !keyframe {
            continue;
        }
        if seconds <= floor {
            before = Some(i);
        } else if after.is_none() {
            after = Some(i);
        }
    }
    let at = before.filter(|&i| i > 0).or(after)?;
    Some(Cut { drop: at, floor })
}

/// A running clock, as the follower sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct View {
    /// Where it is now, in seconds.
    pub pos: f64,
    pub rate: f64,
}

/// What the thread that owns the clock of a stream nobody draws should do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Life {
    Steer(Steer),
    /// The queue ran dry (the clock passed the newest media): stop it at `at` and wait for media.
    Hold { at: f64 },
    /// Enough media waits: start the clock at `pos`, at 1x.
    Start { pos: f64 },
}

/// The shortest queue a stopped clock starts on, in seconds, whatever the target: under it the
/// clock would run dry again at once.
const MIN_START: f64 = 0.02;

/// The memory of a clock that follows a stream: the last target it steered to, and whether the
/// queue still grows to a target raised since. One follower steers one clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Follower {
    seen: Option<f64>,
    refill: bool,
    /// Where the clock stopped for want of media, and starts again from.
    held: Option<f64>,
}

impl Follower {
    /// [`steer`] for the clock at `rate`. A raised target is reached by slowing the clock down; a
    /// lowered one by the usual speedup.
    pub(crate) fn step(&mut self, depth: f64, target: f64, rate: f64) -> Steer {
        match self.seen.replace(target) {
            Some(was) if target > was => self.refill = true,
            Some(was) if target < was => self.refill = false,
            _ => {}
        }
        if self.refill && refilled(depth, target) {
            self.refill = false;
        }
        steer(depth, target, rate, self.refill)
    }

    /// One step of a clock nobody draws, whose life cycle this thread owns. `clock` is the
    /// clock now (None: stopped), `newest` the newest media time pushed (None: no media yet),
    /// `play` whether the app wants the stream to play. A running clock is steered as [`step`]
    /// does, and held when it passes the newest media. A stopped clock starts once the media
    /// after its start (the hold, else the timeline's start) reaches the target, so sound resumes
    /// when the queue is rebuilt, not when the stream's gap has been played out in silence.
    pub(crate) fn hidden(&mut self, clock: Option<View>, newest: Option<f64>, target: f64, play: bool) -> Life {
        let Some(newest) = newest else { return Life::Steer(Steer::Keep) };
        if let Some(view) = clock {
            self.held = None;
            let depth = newest - view.pos;
            if depth < 0.0 {
                self.held = Some(newest);
                self.refill = false;
                return Life::Hold { at: newest };
            }
            return Life::Steer(self.step(depth, target, view.rate));
        }
        let from = self.held.unwrap_or(0.0).min(newest);
        let waiting = newest - from;
        if !play || waiting < (target - slack(target)).max(MIN_START) {
            return Life::Steer(Steer::Keep);
        }
        self.held = None;
        self.refill = false;
        self.seen = Some(target);
        // Far more than the target waits (a long stall, then a burst): start at the target
        // behind the stream rather than at the hold, which the clock would jump from at once.
        let pos = if waiting > target + hard(target) { newest - target } else { from };
        Life::Start { pos }
    }

    /// The window draws: the UI owns the clock's start and stops, so a hold is forgotten.
    pub(crate) fn drawn(&mut self) {
        self.held = None;
    }

    /// The stream's timeline restarted: nothing of the old one is held.
    pub(crate) fn restart(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_follower_slows_for_a_raised_target_until_it_is_reached() {
        let mut f = Follower::default();
        assert_eq!(f.step(0.1, 0.1, 1.0), Steer::Keep);
        assert_eq!(f.step(0.1, 0.5, 1.0), Steer::Rate(1.0 - REFILL));
        assert_eq!(f.step(0.3, 0.5, 1.0 - REFILL), Steer::Keep);
        assert_eq!(f.step(0.5, 0.5, 1.0 - REFILL), Steer::Rate(1.0));
        assert_eq!(f.step(0.5, 0.5, 1.0), Steer::Keep);
    }

    #[test]
    fn a_follower_drops_the_refill_when_the_target_is_lowered() {
        let mut f = Follower::default();
        f.step(0.1, 0.1, 1.0);
        f.step(0.1, 0.5, 1.0);
        assert!(matches!(f.step(0.4, 0.2, 1.0 - REFILL), Steer::Rate(r) if r > 1.0));
    }

    #[test]
    fn a_follower_settles_a_leftover_rate_with_nobody_drawing() {
        // The rate left at 1.012 when the window stopped drawing returns to exactly 1.
        let mut f = Follower::default();
        assert_eq!(f.step(0.5, 0.5, 1.012), Steer::Rate(1.0));
    }

    const RUN: f64 = 1.0;

    fn view(pos: f64) -> Option<View> {
        Some(View { pos, rate: RUN })
    }

    #[test]
    fn a_stopped_clock_starts_once_the_target_is_queued_while_hidden() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, None, 0.5, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(0.2), 0.5, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(0.46), 0.5, true), Life::Start { pos: 0.0 });
        // A stream that is not to play does not start.
        assert_eq!(Follower::default().hidden(None, Some(3.0), 0.5, false), Life::Steer(Steer::Keep));
    }

    #[test]
    fn a_start_on_a_burst_begins_at_the_target_behind_the_stream() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, Some(0.9), 0.5, true), Life::Start { pos: 0.0 });
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, Some(5.0), 0.5, true), Life::Start { pos: 4.5 });
    }

    #[test]
    fn a_tiny_target_still_waits_for_some_media() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, Some(0.0), 0.0, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(0.03), 0.0, true), Life::Start { pos: 0.0 });
    }

    #[test]
    fn an_underrun_holds_the_clock_and_a_refill_restarts_it() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(view(1.0), Some(1.3), 0.3, true), Life::Steer(Steer::Keep));
        // The stream stops at 1.3 s while the clock runs on past it.
        assert_eq!(f.hidden(view(1.35), Some(1.3), 0.3, true), Life::Hold { at: 1.3 });
        // Stopped: nothing starts until the target is queued after the hold.
        assert_eq!(f.hidden(None, Some(1.3), 0.3, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(1.5), 0.3, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(1.58), 0.3, true), Life::Start { pos: 1.3 });
        // Running again, at the target: nothing to do, and no leftover refill.
        assert_eq!(f.hidden(view(1.3), Some(1.58), 0.3, true), Life::Steer(Steer::Keep));
    }

    #[test]
    fn a_long_stall_then_a_burst_restarts_at_the_target_behind_the_stream() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(view(2.0), Some(1.9), 0.3, true), Life::Hold { at: 1.9 });
        // 30 s of media arrive at once: it starts at the target behind the newest, not at the hold.
        assert_eq!(f.hidden(None, Some(32.0), 0.5, true), Life::Start { pos: 31.5 });
        assert_eq!(f.hidden(view(31.5), Some(32.0), 0.5, true), Life::Steer(Steer::Keep));
    }

    #[test]
    fn a_burst_short_of_a_jump_starts_at_the_hold() {
        let mut f = Follower::default();
        f.hidden(view(2.0), Some(1.9), 0.3, true);
        assert_eq!(f.hidden(None, Some(2.9), 0.3, true), Life::Start { pos: 1.9 });
    }

    #[test]
    fn a_running_clock_far_behind_jumps_while_hidden() {
        let mut f = Follower::default();
        assert_eq!(f.hidden(view(0.0), Some(6.0), 0.5, true), Life::Steer(Steer::Jump));
    }

    #[test]
    fn a_discontinuity_while_hidden_starts_over_on_the_new_timeline() {
        let mut f = Follower::default();
        f.hidden(view(9.0), Some(8.9), 0.3, true);
        assert!(f.held.is_some());
        f.restart();
        // The new timeline counts from 0: the old hold does not carry over.
        assert_eq!(f.hidden(None, Some(0.1), 0.3, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(0.27), 0.3, true), Life::Start { pos: 0.0 });
    }

    #[test]
    fn a_target_change_while_hidden_moves_the_start_and_the_steering() {
        // Stopped: a raised target waits for more media, a lowered one for less.
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, Some(0.3), 0.3, true), Life::Start { pos: 0.0 });
        let mut f = Follower::default();
        assert_eq!(f.hidden(None, Some(0.3), 1.0, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(None, Some(0.3), 0.2, true), Life::Start { pos: 0.0 });
        // Running: a raised target slows the clock down, as when drawn.
        let mut f = Follower::default();
        assert_eq!(f.hidden(view(0.0), Some(0.3), 0.3, true), Life::Steer(Steer::Keep));
        assert_eq!(f.hidden(view(0.0), Some(0.3), 1.0, true), Life::Steer(Steer::Rate(1.0 - REFILL)));
    }

    #[test]
    fn a_drawn_window_forgets_the_hold() {
        let mut f = Follower::default();
        f.hidden(view(2.0), Some(1.9), 0.3, true);
        f.drawn();
        assert_eq!(f.hidden(None, Some(0.31), 0.3, true), Life::Start { pos: 0.0 });
    }

    #[test]
    fn keeps_speed_near_the_target() {
        assert_eq!(steer(0.1, 0.1, 1.0, false), Steer::Keep);
        assert_eq!(steer(0.13, 0.1, 1.0, false), Steer::Keep);
    }

    #[test]
    fn speeds_up_for_a_backlog_and_caps() {
        assert!(matches!(steer(0.3, 0.1, 1.0, false), Steer::Rate(r) if r > 1.0 && r < 1.1));
        assert_eq!(steer(1.0, 0.1, 1.0, false), Steer::Rate(1.0 + MAX_SPEEDUP));
    }

    #[test]
    fn slows_down_for_a_thin_queue_and_recovers() {
        assert_eq!(steer(0.02, 0.1, 1.0, false), Steer::Rate(1.0 - SLOWDOWN));
        assert_eq!(steer(0.1, 0.1, 1.0 - SLOWDOWN, false), Steer::Rate(1.0));
    }

    #[test]
    fn returns_to_exactly_1x_at_the_target() {
        assert_eq!(steer(0.1, 0.1, 1.002, false), Steer::Rate(1.0));
        assert_eq!(steer(0.1, 0.1, 0.999, false), Steer::Rate(1.0));
    }

    #[test]
    fn jumps_when_too_far_behind() {
        assert_eq!(steer(3.0, 0.1, 1.0, false), Steer::Jump);
    }

    #[test]
    fn small_changes_keep_the_anchor() {
        assert_eq!(steer(0.2, 0.1, 1.0 + (0.06 * 0.5), false), Steer::Keep);
    }

    /// 30 pictures a second from `from` for `seconds`, a keyframe every `gop` seconds.
    fn stream(from: f64, seconds: f64, gop: f64) -> Vec<(f64, bool)> {
        let n = (seconds * 30.0) as usize;
        let every = (gop * 30.0) as usize;
        (0..n).map(|i| (from + i as f64 / 30.0, i % every == 0)).collect()
    }

    #[test]
    fn a_short_backlog_is_kept() {
        let units = stream(0.0, 2.0, 1.0);
        assert_eq!(backlog_cut(units.iter().copied(), 0.08), None);
        assert_eq!(backlog_cut(std::iter::empty(), 0.08), None);
        // Right at the limit is still kept.
        let units = [(0.0, true), (backlog_limit(0.5), false)];
        assert_eq!(backlog_cut(units.iter().copied(), 0.5), None);
    }

    #[test]
    fn a_long_backlog_is_cut_at_the_keyframe_that_keeps_the_target() {
        // 10 s waiting, keyframes every 2 s (every 60 units); newest is 9.967 s.
        let units = stream(0.0, 10.0, 2.0);
        let cut = backlog_cut(units.iter().copied(), 0.5).unwrap();
        // newest - 0.5 = 9.467: the keyframe at 8 s is the latest at or before it.
        assert_eq!(cut.drop, 240);
        assert!(units[cut.drop].1 && (units[cut.drop].0 - 8.0).abs() < 1e-9);
        assert!((cut.floor - (units[299].0 - 0.5)).abs() < 1e-9);
        // What stays holds the target and a keyframe's lead-in at most.
        assert!(units[299].0 - units[cut.drop].0 >= 0.5);
    }

    #[test]
    fn a_backlog_with_one_distant_keyframe_cuts_to_the_next() {
        // Keyframes at 0 and 9.9 only: nothing in between to start from.
        let mut units = stream(0.0, 10.0, 100.0);
        units[297].1 = true;
        let cut = backlog_cut(units.iter().copied(), 0.08).unwrap();
        assert_eq!(cut.drop, 297);
    }

    #[test]
    fn a_backlog_without_a_second_keyframe_is_kept() {
        let units = stream(0.0, 10.0, 100.0);
        assert_eq!(backlog_cut(units.iter().copied(), 0.08), None);
    }

    #[test]
    fn a_backlog_cut_is_stable_when_applied_again() {
        let units = stream(0.0, 20.0, 1.0);
        let cut = backlog_cut(units.iter().copied(), 0.08).unwrap();
        assert_eq!(backlog_cut(units[cut.drop..].iter().copied(), 0.08), None);
    }

    /// The window's loop on a live stream at 1x: every `dt` seconds the clock follows `rate`, the
    /// target changes to `to` at the start, and the result is the time to converge.
    fn retarget(mut depth: f64, to: f64) -> (f64, f64) {
        let (dt, mut rate, mut t) = (0.05, 1.0, 0.0);
        let mut refill = to > depth;
        while t < 300.0 {
            if refill && refilled(depth, to) {
                refill = false;
            }
            match steer(depth, to, rate, refill) {
                Steer::Keep => {}
                Steer::Rate(r) => rate = r,
                Steer::Jump => panic!("jumped at {t} s, depth {depth}"),
            }
            if rate == 1.0 && (depth - to).abs() <= slack(to) {
                return (t, depth);
            }
            depth += (1.0 - rate) * dt;
            t += dt;
        }
        panic!("no convergence: depth {depth} for target {to}");
    }

    #[test]
    fn a_raised_target_fills_by_slowing_down() {
        assert_eq!(steer(0.5, 1.0, 1.0, true), Steer::Rate(1.0 - REFILL));
        assert!(!refilled(0.5, 1.0) && refilled(0.99, 1.0));
        let (secs, depth) = retarget(0.08, 0.5);
        assert!(secs < 15.0 && depth >= 0.5 - slack(0.5), "{secs} {depth}");
    }

    #[test]
    fn a_lowered_target_drains_by_speeding_up_without_a_jump() {
        let (secs, depth) = retarget(0.5, 0.08);
        assert!(secs < 20.0 && depth <= 0.08 + slack(0.08), "{secs} {depth}");
    }

    /// A stream delivered in bursts of `burst` seconds every `burst` seconds, followed at 1x from
    /// a queue that starts at the target: the queue swings between `target - burst` and `target`.
    fn bursty(target: f64, burst: f64) {
        let (dt, rate) = (0.01, 1.0);
        let mut depth = target;
        let mut next = burst;
        let mut t = 0.0;
        while t < 60.0 {
            if t >= next {
                depth += burst;
                next += burst;
            }
            assert_eq!(
                steer(depth, target, rate, false),
                Steer::Keep,
                "target {target} burst {burst}: t {t:.2} depth {depth:.3}"
            );
            depth -= rate * dt;
            t += dt;
        }
    }

    #[test]
    fn bursts_do_not_move_the_clock_at_any_target() {
        bursty(0.08, 0.03);
        bursty(0.5, 0.25);
        bursty(3.0, 2.0);
        bursty(3.0, 2.5);
    }

    #[test]
    fn thresholds_scale_with_the_target() {
        // 0.08 s: as tuned.
        assert_eq!(steer(1.07, 0.08, 1.0, false), Steer::Rate(1.0 + MAX_SPEEDUP));
        assert_eq!(steer(1.09, 0.08, 1.0, false), Steer::Jump);
        assert_eq!(steer(0.03, 0.08, 1.0, false), Steer::Rate(1.0 - SLOWDOWN));
        assert_eq!(steer(0.05, 0.08, 1.0, false), Steer::Keep);
        // 0.5 s.
        assert_eq!(steer(1.49, 0.5, 1.0, false), Steer::Rate(1.0 + MAX_SPEEDUP));
        assert_eq!(steer(1.51, 0.5, 1.0, false), Steer::Jump);
        assert_eq!(steer(0.19, 0.5, 1.0, false), Steer::Rate(1.0 - SLOWDOWN));
        assert_eq!(steer(0.21, 0.5, 1.0, false), Steer::Keep);
        assert_eq!(steer(0.53, 0.5, 1.0, false), Steer::Keep);
        // 3 s: 1.2 s of buffer is healthy, 0.3 s is nearly empty, 4 s of excess is a jump.
        assert_eq!(steer(1.2, 3.0, 1.0, false), Steer::Keep);
        assert_eq!(steer(1.0, 3.0, 1.0, false), Steer::Keep);
        assert_eq!(steer(0.3, 3.0, 1.0, false), Steer::Rate(1.0 - SLOWDOWN));
        assert_eq!(steer(3.25, 3.0, 1.0, false), Steer::Keep);
        assert_eq!(steer(5.9, 3.0, 1.0, false), Steer::Rate(1.0 + MAX_SPEEDUP));
        assert_eq!(steer(6.1, 3.0, 1.0, false), Steer::Jump);
    }

    #[test]
    fn the_backlog_limit_is_past_the_jump() {
        for target in [0.08, 0.5, 1.0, 3.0] {
            assert!(backlog_limit(target) > target + hard(target));
        }
        assert!((backlog_limit(0.08) - 2.08).abs() < 1e-9);
    }
}
