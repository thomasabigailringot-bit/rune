//! How a live video's clock follows the stream: the queue's depth (newest picture pushed minus
//! the clock) against the target decides whether the clock keeps its speed, runs a little faster
//! to eat a backlog, a little slower to rebuild a thin queue, or jumps.

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
}
