//! How a live video's clock follows the stream: the queue's depth (newest picture pushed minus
//! the clock) against the target decides whether the clock keeps its speed, runs a little faster
//! to eat a backlog, a little slower to rebuild a thin queue, or jumps. [`backlog_cut`] bounds
//! what waits for a consumer that is not running.

/// Faster than this the sound would be too far from itself.
const MAX_SPEEDUP: f64 = 0.08;
/// Slower by this much when the queue is nearly empty.
const SLOWDOWN: f64 = 0.03;
/// A backlog past the target by this many seconds is too far behind to catch up by speed.
const HARD: f64 = 1.0;
/// Excess depth, in seconds, the clock ignores.
const SLACK: f64 = 0.04;
/// A speed change smaller than this isn't worth re-anchoring the clock for.
const STEP: f64 = 0.004;

/// Media waiting past the target by more than this many seconds means nothing consumes it.
const BACKLOG_EXTRA: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Steer {
    Keep,
    Rate(f64),
    /// Too far behind: jump the clock forward to the target depth.
    Jump,
}

/// What the clock should do for a queue `depth` seconds deep, aiming at `target`, running at `rate`.
pub(crate) fn steer(depth: f64, target: f64, rate: f64) -> Steer {
    let excess = depth - target;
    if excess > HARD {
        return Steer::Jump;
    }
    let wanted = if excess > SLACK {
        1.0 + ((excess - SLACK) * 0.5).min(MAX_SPEEDUP)
    } else if depth < target * 0.4 {
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

/// How many seconds of media may wait, at most, for a `target` delay.
pub(crate) fn backlog_limit(target: f64) -> f64 {
    target + BACKLOG_EXTRA
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_speed_near_the_target() {
        assert_eq!(steer(0.1, 0.1, 1.0), Steer::Keep);
        assert_eq!(steer(0.13, 0.1, 1.0), Steer::Keep);
    }

    #[test]
    fn speeds_up_for_a_backlog_and_caps() {
        assert!(matches!(steer(0.3, 0.1, 1.0), Steer::Rate(r) if r > 1.0 && r < 1.1));
        assert_eq!(steer(1.0, 0.1, 1.0), Steer::Rate(1.0 + MAX_SPEEDUP));
    }

    #[test]
    fn slows_down_for_a_thin_queue_and_recovers() {
        assert_eq!(steer(0.02, 0.1, 1.0), Steer::Rate(1.0 - SLOWDOWN));
        assert_eq!(steer(0.1, 0.1, 1.0 - SLOWDOWN), Steer::Rate(1.0));
    }

    #[test]
    fn returns_to_exactly_1x_at_the_target() {
        assert_eq!(steer(0.1, 0.1, 1.002), Steer::Rate(1.0));
        assert_eq!(steer(0.1, 0.1, 0.999), Steer::Rate(1.0));
    }

    #[test]
    fn jumps_when_too_far_behind() {
        assert_eq!(steer(3.0, 0.1, 1.0), Steer::Jump);
    }

    #[test]
    fn small_changes_keep_the_anchor() {
        assert_eq!(steer(0.2, 0.1, 1.0 + (0.06 * 0.5)), Steer::Keep);
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
}
