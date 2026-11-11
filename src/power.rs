//! Battery and idle (0.11.0), for the tray: the charge for the tooltip and the menu, a warning
//! once per discharge, and "turn off when idle". Reads only: the mouse thread, which already
//! reads every input report, feeds `Watch`; the tray looks at the statics once a second.
//! Over Bluetooth the short report has no battery byte, so the charge stays unknown until a full
//! report arrives (nothing here asks for one: that would blind plain readers).

use crate::ds::State;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Warn when the level (0-10) falls to this while running on the battery ...
const LOW: u8 = 2;
/// ... and again only after it was charging or back up to this.
const REARM: u8 = 4;
/// "Turn off when idle" acts after this long without input.
pub const IDLE_LIMIT: u32 = 600;
/// A stick or trigger must move this far (of 255) from where it rested: sticks jitter by a few.
const MOVED: u8 = 8;
/// Raw gyro units on any axis that count as "someone holds it": about 10 degrees a second
/// (0.061 deg/s per unit on the test controller; at rest it reads within a few units of 0).
const GYRO_NOTABLE: u16 = 160;
pub const NONE: u32 = u32::MAX;

/// The battery byte of the last full report (low nibble level, high nibble charging), 0xFF = unknown.
pub static BATTERY: AtomicU8 = AtomicU8::new(0xFF);
/// Whole seconds since the last input, `NONE` = no controller.
pub static IDLE_SECS: AtomicU32 = AtomicU32::new(NONE);
/// Mods connected to the port (src/port.rs), and whether the worker (src/ui.rs) has any trigger
/// effect, rumble or tone on: the controller is in use then, however still it lies.
pub static MODS: AtomicU32 = AtomicU32::new(0);
pub static EFFECT: AtomicBool = AtomicBool::new(false);
/// The tray asks to count from now (the idle switch was just ticked).
pub static RESET: AtomicBool = AtomicBool::new(false);

pub fn battery() -> Option<(u8, u8)> {
    Some(BATTERY.load(Ordering::Relaxed)).filter(|&b| b != 0xFF).map(|b| (b & 0x0F, b >> 4))
}

/// Percent as the Linux driver reports it: the middle of the level's tenth, 100 when full.
fn percent(level: u8, state: u8) -> u8 {
    if state == 2 { 100 } else { (level * 10 + 5).min(100) }
}

/// The charge in words. State nibble (Linux driver): 0 on battery, 1 charging, 2 full,
/// 0xA / 0xB not charging (temperature or voltage out of range), anything else a charging fault.
pub fn text(b: Option<(u8, u8)>) -> String {
    match b {
        None => "Battery unknown".into(),
        Some((level, state)) => {
            let p = percent(level.min(10), state);
            match state {
                0 => format!("Battery {p}%"),
                1 => format!("Battery {p}%, charging"),
                2 => "Battery full".into(),
                0xA | 0xB => format!("Battery {p}%, not charging (too hot, too cold or wrong voltage)"),
                _ => format!("Battery {p}%, charging fault"),
            }
        }
    }
}

/// The low-battery warning: once per discharge.
#[derive(Default)]
pub struct Warn(pub bool);

impl Warn {
    /// The balloon's text when this reading is the one to warn on.
    pub fn step(&mut self, b: Option<(u8, u8)>) -> Option<String> {
        let (level, state) = b?; // unknown (short report, or gone): keep what we know
        if state != 0 || level >= REARM {
            self.0 = false;
        } else if level <= LOW && !self.0 {
            self.0 = true;
            return Some(format!("Controller battery low ({}%). Plug it in soon.", percent(level, state)));
        }
        None
    }
}

/// How long the controller has gone without a button, stick, trigger, touch or notable turn.
pub struct Idle {
    rest: State,
    since: Instant,
}

impl Idle {
    pub fn new(now: Instant) -> Self {
        Idle { rest: State::default(), since: now }
    }

    /// `busy` (a mod connected, an effect or tone playing) counts as input.
    pub fn step(&mut self, s: &State, busy: bool, now: Instant) -> Duration {
        let far = |a: u8, b: u8| a.abs_diff(b) > MOVED;
        let moved = s.buttons != self.rest.buttons
            || s.sticks.iter().zip(self.rest.sticks).any(|(&a, b)| far(a, b))
            || far(s.l2, self.rest.l2)
            || far(s.r2, self.rest.r2)
            || s.touch.iter().any(|t| t.is_some())
            || s.imu.is_some_and(|(g, ..)| g.iter().any(|v| v.unsigned_abs() > GYRO_NOTABLE));
        if moved || busy {
            // Only a real move becomes the new rest position, so slow creep cannot hide in small steps.
            (self.rest, self.since) = (State { battery: None, trig: None, imu: None, touch: [None; 2], ..*s }, now);
        }
        now.saturating_duration_since(self.since)
    }
}

/// The tray's side of "turn off when idle": fire once when the limit is reached, and not again
/// until there was input (or the controller came back).
#[derive(Default)]
pub struct IdleOff(pub bool);

impl IdleOff {
    pub fn due(&mut self, on: bool, idle_secs: u32) -> bool {
        let over = on && idle_secs != NONE && idle_secs >= IDLE_LIMIT;
        let fire = over && !self.0;
        self.0 = over;
        fire
    }
}

/// One connection's reader side (the mouse thread): feeds the statics the tray shows and acts on.
pub struct Watch(Idle);

impl Watch {
    pub fn new() -> Self {
        Watch(Idle::new(Instant::now()))
    }

    pub fn feed(&mut self, s: &State) {
        if let Some((level, state)) = s.battery {
            BATTERY.store(level | state << 4, Ordering::Relaxed);
        }
        let busy = MODS.load(Ordering::Relaxed) > 0 || EFFECT.load(Ordering::Relaxed) || RESET.swap(false, Ordering::Relaxed);
        IDLE_SECS.store(self.0.step(s, busy, Instant::now()).as_secs() as u32, Ordering::Relaxed);
    }
}

/// No controller (or a new connection begins): nothing known.
pub fn gone() {
    BATTERY.store(0xFF, Ordering::Relaxed);
    IDLE_SECS.store(NONE, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_becomes_text() {
        assert_eq!(text(None), "Battery unknown");
        assert_eq!(text(Some((8, 0))), "Battery 85%");
        assert_eq!(text(Some((0, 0))), "Battery 5%");
        assert_eq!(text(Some((10, 0))), "Battery 100%");
        assert_eq!(text(Some((3, 1))), "Battery 35%, charging");
        assert_eq!(text(Some((9, 2))), "Battery full");
        assert_eq!(text(Some((15, 0))), "Battery 100%", "a level past 10 is capped");
        assert!(text(Some((4, 0xB))).starts_with("Battery 45%, not charging"));
        assert_eq!(text(Some((4, 0xF))), "Battery 45%, charging fault");
    }

    #[test]
    fn warns_once_per_discharge() {
        let mut w = Warn::default();
        let warned = |w: &mut Warn, readings: &[(u8, u8)]| readings.iter().filter(|&&b| w.step(Some(b)).is_some()).count();
        assert_eq!(warned(&mut w, &[(5, 0), (4, 0), (3, 0)]), 0);
        assert_eq!(w.step(Some((2, 0))).as_deref(), Some("Controller battery low (25%). Plug it in soon."));
        assert_eq!(warned(&mut w, &[(2, 0), (1, 0), (3, 0), (2, 0), (0, 0)]), 0, "once, also when the reading wobbles");
        assert_eq!(w.step(None), None, "the short report (unknown) changes nothing");
        assert_eq!(warned(&mut w, &[(1, 0)]), 0, "nor does a reconnect at the same charge");
        assert_eq!(warned(&mut w, &[(1, 1), (2, 1)]), 0, "never while charging");
        assert_eq!(warned(&mut w, &[(2, 0)]), 1, "unplugged while still low: a new discharge");
        assert_eq!(warned(&mut w, &[(6, 0), (2, 0), (1, 0)]), 1, "and after the level was back up");
        assert_eq!(Warn::default().step(Some((1, 0xA))), None, "a charger that refuses is still a charger");
    }

    #[test]
    fn idle_counts_from_the_last_input() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let rest = State { sticks: [128, 127, 129, 128], imu: Some(([3, -2, 4], [0, 8339, 0], 0)), ..State::default() };
        let mut i = Idle::new(t0);
        assert_eq!(i.step(&rest, false, at(1)).as_secs(), 0, "the first report sets the rest position");
        assert_eq!(i.step(&rest, false, at(100)).as_secs(), 99);
        let jitter = State { sticks: [131, 125, 127, 130], l2: 3, imu: Some(([40, -60, 100], [0, 8339, 0], 0)), ..rest };
        assert_eq!(i.step(&jitter, false, at(200)).as_secs(), 199, "stick jitter and a nudge of the desk are not input");
        for (what, s) in [
            ("a button", State { buttons: [0x28, 0, 0], ..rest }),
            ("a stick", State { sticks: [128, 127, 129, 160], ..rest }),
            ("a trigger", State { r2: 40, ..rest }),
            ("a finger on the touchpad", State { touch: [Some((1, 900, 500)), None], ..rest }),
            ("picking it up", State { imu: Some(([0, 400, 0], [0, 8339, 0], 0)), ..rest }),
        ] {
            let mut i = Idle::new(t0);
            i.step(&rest, false, at(1));
            assert_eq!(i.step(&s, false, at(500)).as_secs(), 0, "{what} is input");
            assert_eq!(i.step(&s, false, at(501)).as_secs() <= 1, true);
        }
        // A stick pushed slowly: each report is near the last one, but far from where it rested.
        let mut i = Idle::new(t0);
        i.step(&rest, false, at(1));
        let slow = (0..20).map(|k| i.step(&State { sticks: [128 + k * 2, 127, 129, 128], ..rest }, false, at(300 + k as u64)).as_secs()).min();
        assert_eq!(slow, Some(0), "a slow push is seen");
        // A held button is one input, not endless input; in use by a mod or an effect is.
        let held = State { buttons: [0x28, 0, 0], ..rest };
        let mut i = Idle::new(t0);
        i.step(&held, false, at(1));
        assert_eq!(i.step(&held, false, at(700)).as_secs(), 699);
        assert_eq!(i.step(&held, true, at(701)).as_secs(), 0, "busy counts as input");
        assert_eq!(i.step(&held, false, at(711)).as_secs(), 10);
        // The short report (no gyro, no touch, no battery) is enough.
        let short = State { sticks: [128, 128, 128, 128], ..State::default() };
        let mut i = Idle::new(t0);
        i.step(&short, false, at(0));
        assert_eq!(i.step(&short, false, at(IDLE_LIMIT as u64 + 5)).as_secs(), IDLE_LIMIT as u64 + 5);
    }

    #[test]
    fn idle_off_fires_once() {
        let mut o = IdleOff::default();
        assert!(!o.due(false, 9999), "off by default: never");
        assert!(!o.due(true, IDLE_LIMIT - 1));
        assert!(!o.due(true, NONE), "no controller");
        assert!(o.due(true, IDLE_LIMIT));
        assert!(!o.due(true, IDLE_LIMIT + 1) && !o.due(true, IDLE_LIMIT + 500), "once, even if switching off failed");
        assert!(!o.due(true, 3), "input again");
        assert!(o.due(true, IDLE_LIMIT + 2), "and the next idle stretch fires again");
        assert!(!o.due(true, NONE) && o.due(true, IDLE_LIMIT), "back after being off");
    }
}
