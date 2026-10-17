use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub const HOVER_IN: f32 = 0.12;
pub const HOVER_OUT: f32 = 0.20;
pub const TAB_RAIL: f32 = 0.30;
pub const OVERLAY: f32 = 0.18;
pub const STARTUP: f32 = 0.22;
pub const FOCUS: f32 = 0.16;
pub const SIGNAL_DRAW: f32 = 0.40;
pub const SIGNAL_HOLD: f32 = 2.0;
pub const SIGNAL_SETTLE: f32 = 0.30;
pub const RUNNING_PULSE: f32 = 1.2;
pub const BELL: f32 = 0.60;
pub const CURSOR_PERIOD: f32 = 1.06;

static REDUCED: AtomicBool = AtomicBool::new(false);

/// the os asked for less motion: positional animation jumps to its end, fades stay
pub fn set_reduced(on: bool) {
    REDUCED.store(on, Ordering::Relaxed);
}

pub fn reduced() -> bool {
    REDUCED.load(Ordering::Relaxed)
}

/// linear 0..1 progress of a timer of `dur` seconds started at `since`
pub fn progress(since: Instant, dur: f32) -> f32 {
    (since.elapsed().as_secs_f32() / dur).clamp(0.0, 1.0)
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// eased 0..1 for a fade, None once it has settled
pub fn fade(since: Option<Instant>, dur: f32) -> Option<f32> {
    let t = progress(since?, dur);
    (t < 1.0).then(|| ease_out(t))
}

/// critically damped spring from 0 to 1, settled within `TAB_RAIL`
pub fn spring(since: Instant) -> f32 {
    if reduced() {
        return 1.0;
    }
    let t = since.elapsed().as_secs_f32();
    if t >= TAB_RAIL {
        return 1.0;
    }
    let w = 2.0 * std::f32::consts::PI / 0.28;
    (1.0 - (1.0 + w * t) * (-w * t).exp()).min(1.0)
}

/// motion for a positional change: the full curve, or an instant jump when
/// reduced motion is on
pub fn travel(t: f32) -> f32 {
    if reduced() { 1.0 } else { ease_out(t) }
}

/// soft blink: full for most of the period, a quick eased dip through zero
pub fn cursor_blink(elapsed: f32) -> f32 {
    let p = (elapsed % CURSOR_PERIOD) / CURSOR_PERIOD;
    if reduced() {
        return if p < 0.57 { 1.0 } else { 0.0 };
    }
    let fade = |x: f32| x * x * (3.0 - 2.0 * x);
    match p {
        p if p < 0.50 => 1.0,
        p if p < 0.62 => 1.0 - fade((p - 0.50) / 0.12),
        p if p < 0.88 => 0.0,
        p => fade((p - 0.88) / 0.12),
    }
}

/// 0.4..0.9 breathing alpha for a running command's dot
pub fn breathe(elapsed: f32) -> f32 {
    if reduced() {
        return 0.7;
    }
    let p = (elapsed % RUNNING_PULSE) / RUNNING_PULSE;
    0.65 - 0.25 * (p * std::f32::consts::TAU).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_at_one() {
        let w = 2.0 * std::f32::consts::PI / 0.28;
        let at = |t: f32| 1.0 - (1.0 + w * t) * (-w * t).exp();
        assert!(at(0.0).abs() < 1e-6);
        assert!(at(TAB_RAIL) > 0.98);
        assert!((0..30).map(|i| at(i as f32 * 0.01)).collect::<Vec<_>>().windows(2).all(|p| p[1] >= p[0]));
    }

    #[test]
    fn cursor_blink_is_continuous_and_bounded() {
        let mut prev = cursor_blink(0.0);
        for i in 1..=212 {
            let v = cursor_blink(i as f32 * 0.005);
            assert!((0.0..=1.0).contains(&v));
            assert!((v - prev).abs() < 0.2, "jump at step {i}");
            prev = v;
        }
    }

    #[test]
    fn breathe_stays_in_range() {
        for i in 0..120 {
            let a = breathe(i as f32 * 0.01);
            assert!((0.39..=0.91).contains(&a));
        }
    }
}
