//! Touchpad and gyro as a mouse, on the real DualSense: reads only, writes nothing
//! to the controller, and moves the Windows cursor with SendInput (no driver).
//! Controls: a finger on the touchpad moves the cursor (on at start);
//! touchpad click = left, with two fingers down = right. Switches (which
//! frees Mute for voice): a long press on the touchpad (finger still) toggles the touchpad mouse,
//! hold Create + click the touchpad toggles gyro; each asks src/voice.rs for a ding from the
//! controller's speaker. Steam Deck style:
//! R2 = left click, L2 = right click, but only while gyro is on or a finger rests on the touchpad,
//! so in a game the triggers stay triggers.
//! Over Bluetooth it waits on the short report until the touchpad is held down (or a full report
//! shows someone else already switched it), because the switch blinds plain readers until the
//! controller is turned off and on (SDL also switches only on demand).

use crate::ds;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// Cursor pixels per touchpad unit (the pad is 1920 units wide).
const TOUCH_GAIN: f32 = 1.0;
/// Cursor pixels per degree the controller turns.
/// 15 felt a little too slow in the hand; about 1.5x that felt right.
const GYRO_PX_PER_DEG: f32 = 22.0;
/// Below this speed (deg/s, after the rest bias) the gyro sends nothing: hand tremor and noise.
const GYRO_DEADZONE: f32 = 0.6;
/// Between these speeds slow turns are smoothed; above, they pass straight through.
const SMOOTH_FROM: f32 = 2.0;
const SMOOTH_TO: f32 = 6.0;
const CREATE: u8 = 0x10; // buttons[1]
/// A touchpad press this long, with the finger still, toggles the touchpad mouse.
const LONG_PRESS: Duration = Duration::from_millis(600);
/// A finger that moved further than this (touchpad units) is dragging, not long-pressing.
const STILL: i32 = 60;
const PAD_CLICK: u8 = 0x02; // buttons[2]
const L2: u8 = 0x04; // buttons[1], the controller's own digital trigger bits
const R2: u8 = 0x08;

/// Shared with the tray menu: the touchpad and gyro switches (the controller's Mute and the menu
/// change the same two), `WAKE` = the menu asks to switch to the full report now, and `STATE`
/// = 0 no DualSense, 1 waiting for a touchpad click, 2 running.
pub static TOUCH_ON: AtomicBool = AtomicBool::new(true);
pub static GYRO_ON: AtomicBool = AtomicBool::new(false);
pub static WAKE: AtomicBool = AtomicBool::new(false);
pub static STATE: AtomicU8 = AtomicU8::new(0);

/// Whether to leave the short report: already full (switched by someone else), or a long press
/// on the touchpad (`hold` carries it; `hold.fired` then says the press did it).
pub fn wake(kind: &ds::Kind, s: &ds::State, hold: &mut crate::voice::Hold, now: Instant) -> bool {
    match kind {
        ds::Kind::BtSimple => hold.long(s.buttons[2] & PAD_CLICK != 0, now, LONG_PRESS),
        _ => true,
    }
}

/// (degrees per second for one raw gyro unit, raw rest bias) on each axis (pitch, yaw, roll),
/// from feature report 0x05 (id first). The scale as the Linux driver computes it:
/// speed_2x / (|plus - bias| + |minus - bias|). The driver ignores the bias; we start from it
/// (it matched the desk readings: 0, -1, 4 against about 0, -1, 3) and refine it while still.
/// An axis with no usable data gets the driver's fallback, 2048 deg/s over the i16 range, bias 0.
pub fn calibration(f: &[u8]) -> ([f32; 3], [f32; 3]) {
    let v = |i: usize| f.get(i..i + 2).map_or(0, |b| i16::from_le_bytes([b[0], b[1]])) as f32;
    let speed_2x = v(19) + v(21);
    let bias: [f32; 3] = std::array::from_fn(|a| v(1 + 2 * a));
    let scale = std::array::from_fn(|a| {
        let denom = (v(7 + 4 * a) - bias[a]).abs() + (v(9 + 4 * a) - bias[a]).abs();
        if denom == 0.0 || speed_2x == 0.0 { 2048.0 / 32767.0 } else { speed_2x / denom }
    });
    (scale, bias)
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Out {
    Move(i32, i32),
    Button { right: bool, down: bool },
    /// A clip for the controller's speaker (voice::DING, DING2, OFF).
    Sound(u8),
}

pub struct Mouse {
    pub touch_on: bool,
    pub gyro_on: bool,
    scale: [f32; 3],
    /// Raw gyro reading at rest, tracked while the controller is still.
    bias: [f32; 3],
    still: f32,
    smooth: [f32; 2],
    /// Sub-pixel movement left over, so slow movement is not lost.
    rem: [f32; 2],
    finger: Option<(u8, u16, u16)>,
    /// The mouse button a touchpad click holds: Some(true) = right.
    pad: Option<bool>,
    /// Mouse buttons held by R2 (left) and L2 (right), and the triggers in the last report.
    trig: [bool; 2],
    trig_last: [bool; 2],
    /// What Windows has been told: [left, right] down.
    down: [bool; 2],
    /// A touchpad press that may become a long press: sensor time and finger position at its start.
    press: Option<(u32, Option<(u16, u16)>)>,
    /// This press was a switch (long press, or with Create): it is not a mouse click.
    switched: bool,
    click: bool,
    ts: Option<u32>,
    /// Couch mode is acting (src/couch.rs): the triggers are its clicks, not ours.
    pub couch: bool,
}

impl Mouse {
    pub fn new((scale, bias): ([f32; 3], [f32; 3])) -> Mouse {
        Mouse { touch_on: true, gyro_on: false, scale, bias, still: 0.0, smooth: [0.0; 2], rem: [0.0; 2], finger: None, pad: None, trig: [false; 2], trig_last: [false; 2], down: [false; 2], press: None, switched: false, click: false, ts: None, couch: false }
    }

    /// Tell Windows about every mouse button that changed.
    fn buttons(&mut self, want: [bool; 2]) -> Vec<Out> {
        let out = (0..2).filter(|&i| want[i] != self.down[i]).map(|i| Out::Button { right: i == 1, down: want[i] }).collect();
        self.down = want;
        out
    }

    /// Let go of every held mouse button (controller gone, program ending).
    pub fn release(&mut self) -> Vec<Out> {
        (self.pad, self.trig) = (None, [false; 2]);
        self.buttons([false; 2])
    }

    /// One full report in, mouse actions out.
    pub fn step(&mut self, s: &ds::State) -> Vec<Out> {
        let mut out = Vec::new();
        let (create, click) = (s.buttons[1] & CREATE != 0, s.buttons[2] & PAD_CLICK != 0);
        let (ts, at) = (s.imu.map(|i| i.2), s.touch[0].map(|(_, x, y)| (x, y)));
        if !click {
            (self.press, self.switched) = (None, false);
        } else if !self.click {
            if create {
                // Create + click: gyro on/off.
                self.gyro_on = !self.gyro_on;
                self.smooth = [0.0; 2];
                self.switched = true;
                out.push(Out::Sound(if self.gyro_on { crate::voice::DING2 } else { crate::voice::OFF }));
            } else {
                self.press = ts.map(|t| (t, at));
            }
        } else if let (Some((t0, from)), Some(t)) = (self.press, ts) {
            let far = |a: (u16, u16), b: (u16, u16)| (a.0 as i32 - b.0 as i32).abs().max((a.1 as i32 - b.1 as i32).abs()) > STILL;
            if from.zip(at).is_some_and(|(a, b)| far(a, b)) {
                self.press = None; // dragging
            } else if t.wrapping_sub(t0) as u64 >= LONG_PRESS.as_micros() as u64 * 3 {
                // Long press: touchpad mouse on/off; the click it began with is let go below.
                self.touch_on = !self.touch_on;
                (self.press, self.switched) = (None, true);
                out.push(Out::Sound(if self.touch_on { crate::voice::DING } else { crate::voice::OFF }));
            }
        }

        let mut moved = [0.0f32; 2];
        if !click || !self.touch_on || self.switched {
            self.pad = None;
        } else if !self.click {
            self.pad = Some(s.touch.iter().all(Option::is_some)); // two fingers down = right
        }
        // A trigger clicks if pulled while armed, and holds until let go (so dragging works).
        let armed = !self.couch && (self.gyro_on || (self.touch_on && s.touch.iter().any(Option::is_some)));
        let pulled = [s.buttons[1] & R2 != 0, s.buttons[1] & L2 != 0];
        for i in 0..2 {
            self.trig[i] = pulled[i] && (self.trig[i] || (!self.trig_last[i] && armed));
        }
        self.trig_last = pulled;
        out.extend(self.buttons([self.pad == Some(false) || self.trig[0], self.pad == Some(true) || self.trig[1]]));
        if self.touch_on {
            // Move only while the same finger stays down; a lift or a new finger starts over.
            if let (Some((id, x, y)), Some((last, lx, ly))) = (s.touch[0], self.finger)
                && id == last
            {
                moved = [(x as f32 - lx as f32) * TOUCH_GAIN, (y as f32 - ly as f32) * TOUCH_GAIN];
            }
        }
        self.click = click;
        self.finger = s.touch[0];

        if let Some((gyro, _, ts)) = s.imu {
            // Sensor timestamp ticks are 1/3 microsecond (driver); a gap (reconnect, lost reports) is capped.
            let dt = self.ts.map_or(0.0, |t| (ts.wrapping_sub(t) as f32 / 3e6).min(0.05));
            self.ts = Some(ts);
            let rate: [f32; 3] = std::array::from_fn(|a| (gyro[a] as f32 - self.bias[a]) * self.scale[a]);
            // Refine the bias only when truly still (under the deadzone on every axis for 1 s), and slowly:
            // a looser, faster version soaked up slow real tilts and then drifted the cursor.
            if rate.iter().all(|r| r.abs() < 0.5) {
                self.still += dt;
                if self.still > 1.0 {
                    for a in 0..3 {
                        self.bias[a] += (gyro[a] as f32 - self.bias[a]) * 0.002;
                    }
                }
            } else {
                self.still = 0.0;
            }
            if self.gyro_on {
                // Turning left (yaw +) moves left, tilting the top up (pitch +) moves up.
                let v = [-rate[1], -rate[0]];
                let speed = v[0].hypot(v[1]);
                let w = ((speed - SMOOTH_FROM) / (SMOOTH_TO - SMOOTH_FROM)).clamp(0.0, 1.0);
                for i in 0..2 {
                    self.smooth[i] += (v[i] - self.smooth[i]) * 0.25;
                    let r = if speed < GYRO_DEADZONE { 0.0 } else { v[i] * w + self.smooth[i] * (1.0 - w) };
                    moved[i] += r * dt * GYRO_PX_PER_DEG;
                }
            }
        }

        let mut px = [0i32; 2];
        for i in 0..2 {
            self.rem[i] += moved[i];
            px[i] = self.rem[i].trunc() as i32;
            self.rem[i] -= px[i] as f32;
        }
        if px != [0, 0] {
            out.push(Out::Move(px[0], px[1]));
        }
        out
    }
}

fn onoff(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Hand mouse actions to Windows.
fn send(out: &[Out]) {
    let inputs: Vec<INPUT> = out
        .iter()
        .map(|o| {
            let (dx, dy, flags) = match *o {
                Out::Move(x, y) => (x, y, MOUSEEVENTF_MOVE),
                Out::Button { right: false, down: true } => (0, 0, MOUSEEVENTF_LEFTDOWN),
                Out::Button { right: false, down: false } => (0, 0, MOUSEEVENTF_LEFTUP),
                Out::Button { right: true, down: true } => (0, 0, MOUSEEVENTF_RIGHTDOWN),
                Out::Button { right: true, down: false } => (0, 0, MOUSEEVENTF_RIGHTUP),
                Out::Sound(id) => {
                    crate::voice::SOUND.store(id, Ordering::Relaxed); // the tray's voice thread plays it
                    (0, 0, 0)
                }
            };
            INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
        })
        .collect();
    if !inputs.is_empty() {
        unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    }
}

/// Read the first DualSense and drive the cursor until `stop`; looks again every second when
/// none is there or it went away. `dry` prints instead of moving the cursor, plus the gyro's
/// turned angle and the sensor clock against the wall clock once a second (for checks).
// shortcut: the first DualSense found only; a second controller would need its own Mouse.
pub fn run(stop: &AtomicBool, dry: bool) {
    while !stop.load(Ordering::Relaxed) {
        crate::power::gone(); // battery and idle time (src/power.rs) start over with each connection
        let found = hidapi::HidApi::new().ok().and_then(|api| {
            let info = api.device_list().find(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()))?.clone();
            Some((matches!(info.bus_type(), hidapi::BusType::Bluetooth), info.open_device(&api).ok()?))
        });
        let Some((bt, dev)) = found else {
            STATE.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        crate::ui::FADED.store(false, Ordering::Relaxed); // a new connection: its light bar needs the setup report again
        STATE.store(1, Ordering::Relaxed);
        let mut hold = crate::voice::Hold::default();
        let mut watch = crate::power::Watch::new();
        let mut couch = crate::couch::Runner::new(dry); // does nothing unless the tray's "Couch mode" is ticked
        if bt {
            println!("mouse: Bluetooth DualSense, waiting for a long press on the touchpad (until then plain readers keep working)");
            let (mut buf, mut go) = ([0u8; 128], false);
            while !go && !stop.load(Ordering::Relaxed) {
                let Ok(len) = dev.read_timeout(&mut buf, 100) else { break };
                let report = ds::parse(bt, &buf[..len]).filter(|(k, _)| *k != ds::Kind::BtFull { crc_ok: false });
                if let Some((_, s)) = &report {
                    watch.feed(s); // idle time from the short report too; the battery only once a full one comes
                    couch.feed(s); // sticks and buttons are in the short report: no switch needed
                }
                go = WAKE.swap(false, Ordering::Relaxed) || report.is_some_and(|(k, s)| wake(&k, &s, &mut hold, Instant::now()));
            }
            if !go {
                couch.stop();
                continue; // stopped, or the controller went away
            }
        }
        STATE.store(2, Ordering::Relaxed);
        // The calibration request (not a write) also switches Bluetooth to the full report.
        let mut f = [0u8; 64];
        f[0] = 0x05;
        let cal = dev.get_feature_report(&mut f).ok().filter(|&len| !bt || (len >= 41 && ds::crc32(0xA3, &f[..len - 4]) == u32::from_le_bytes(f[len - 4..len].try_into().unwrap())));
        let (scale, bias) = if cal.is_some() { calibration(&f[..41]) } else { calibration(&[]) };
        let mut m = Mouse::new((scale, bias));
        if hold.fired {
            // Woken by the long press: that is "mouse on" (with its ding), and the press still held is no click.
            TOUCH_ON.store(true, Ordering::Relaxed);
            (m.click, m.switched) = (true, true);
            if !dry {
                crate::voice::SOUND.store(crate::voice::DING, Ordering::Relaxed);
            }
        }
        if dry {
            GYRO_ON.store(true, Ordering::Relaxed); // dry: gyro from the start, to see drift at rest
        }
        println!("mouse: {} DualSense, calibration {} (deg/s per unit {scale:.5?}, bias {bias:?}); touchpad {}, gyro {} (a long press on the touchpad toggles it, Create + pad click toggles gyro; R2/L2 = left/right click while gyro is on or a finger is on the pad)", if bt { "Bluetooth" } else { "USB" }, if cal.is_some() { "read" } else { "NOT read, driver fallback" }, onoff(TOUCH_ON.load(Ordering::Relaxed)), onoff(GYRO_ON.load(Ordering::Relaxed)));
        if dry {
            println!("mouse: calibration raw {}", crate::hex(&f[..41]));
        }
        let (mut buf, mut modes) = ([0u8; 128], (TOUCH_ON.load(Ordering::Relaxed), GYRO_ON.load(Ordering::Relaxed)));
        let (mut turned, mut first_ts, mut last_ts, mut tick, start) = ([0f32; 3], None, None, Instant::now(), Instant::now());
        while !stop.load(Ordering::Relaxed) {
            let Ok(len) = dev.read_timeout(&mut buf, 100) else { break };
            let s = match ds::parse(bt, &buf[..len]) {
                Some((ds::Kind::Usb | ds::Kind::BtFull { crc_ok: true }, s)) => s,
                _ => continue,
            };
            watch.feed(&s);
            (m.touch_on, m.gyro_on) = (TOUCH_ON.load(Ordering::Relaxed), GYRO_ON.load(Ordering::Relaxed));
            let before = (m.touch_on, m.gyro_on);
            m.couch = couch.feed(&s);
            let out = m.step(&s);
            // Store only what the controller changed, so a menu click in between is not undone.
            if m.touch_on != before.0 {
                TOUCH_ON.store(m.touch_on, Ordering::Relaxed);
            }
            if m.gyro_on != before.1 {
                GYRO_ON.store(m.gyro_on, Ordering::Relaxed);
            }
            if (m.touch_on, m.gyro_on) != modes {
                modes = (m.touch_on, m.gyro_on);
                println!("mouse: touchpad {}, gyro {}", onoff(modes.0), onoff(modes.1));
            }
            if dry {
                if !out.is_empty() {
                    println!("mouse: dry {out:?}");
                }
                if let Some((g, _, ts)) = s.imu {
                    let t0 = *first_ts.get_or_insert(ts);
                    let dt = last_ts.map_or(0.0, |t: u32| ts.wrapping_sub(t) as f32 / 3e6);
                    last_ts = Some(ts);
                    for a in 0..3 {
                        turned[a] += (g[a] as f32 - bias[a]) * scale[a] * dt;
                    }
                    if tick.elapsed() >= Duration::from_secs(1) {
                        tick = Instant::now();
                        println!("mouse: turned pitch {:+.0} yaw {:+.0} roll {:+.0} deg (report bias) | sensor clock {:.2} s vs wall {:.2} s | gyro raw {g:?}", turned[0], turned[1], turned[2], ts.wrapping_sub(t0) as f32 / 3e6, start.elapsed().as_secs_f32());
                    }
                }
            } else {
                send(&out);
            }
        }
        STATE.store(0, Ordering::Relaxed);
        couch.stop();
        let out = m.release();
        if !dry {
            send(&out);
        }
    }
    crate::power::gone();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(buttons2: u8, touch: [Option<(u8, u16, u16)>; 2], gyro: [i16; 3], ts: u32) -> ds::State {
        ds::State { buttons: [8, 0, buttons2], touch, imu: Some((gyro, [0, 8192, 0], ts)), ..Default::default() }
    }
    const TICK: u32 = 12_000; // 4 ms in sensor ticks

    #[test]
    fn touch_moves_only_while_the_same_finger_stays() {
        let mut m = Mouse::new(([0.0625; 3], [0.0; 3]));
        assert_eq!(m.step(&st(0, [Some((1, 100, 100)), None], [0; 3], 0)), vec![]);
        assert_eq!(m.step(&st(0, [Some((1, 110, 95)), None], [0; 3], TICK)), vec![Out::Move(10, -5)]);
        // Lifted: nothing, and the next touch somewhere else does not jump.
        assert_eq!(m.step(&st(0, [None, None], [0; 3], 2 * TICK)), vec![]);
        assert_eq!(m.step(&st(0, [Some((2, 900, 500)), None], [0; 3], 3 * TICK)), vec![]);
        // A new finger id without a lift in between does not jump either.
        assert_eq!(m.step(&st(0, [Some((3, 100, 100)), None], [0; 3], 4 * TICK)), vec![]);
    }

    #[test]
    fn clicks_and_mode_switches() {
        let mut m = Mouse::new(([0.0625; 3], [0.0; 3]));
        let one = [Some((1, 500, 500)), None];
        let two = [Some((1, 500, 500)), Some((2, 900, 500))];
        assert_eq!(m.step(&st(PAD_CLICK, one, [0; 3], 0)), vec![Out::Button { right: false, down: true }]);
        assert_eq!(m.step(&st(0, one, [0; 3], TICK)), vec![Out::Button { right: false, down: false }]);
        assert_eq!(m.step(&st(PAD_CLICK, two, [0; 3], 2 * TICK)), vec![Out::Button { right: true, down: true }]);
        assert_eq!(m.step(&st(0, two, [0; 3], 3 * TICK)), vec![Out::Button { right: true, down: false }]);
        // Create + click: gyro on with the double ding, no mouse click; again: off.
        let with = |b1: u8, b2: u8, i: u32| ds::State { buttons: [8, b1, b2], touch: one, imu: Some(([0; 3], [0, 8192, 0], i * TICK)), ..Default::default() };
        assert_eq!(m.step(&with(CREATE, PAD_CLICK, 4)), vec![Out::Sound(crate::voice::DING2)]);
        assert!(m.gyro_on);
        assert_eq!(m.step(&with(CREATE, PAD_CLICK, 5)), vec![], "still held: once");
        assert_eq!(m.step(&with(CREATE, 0, 6)), vec![]);
        assert_eq!(m.step(&with(CREATE, PAD_CLICK, 7)), vec![Out::Sound(crate::voice::OFF)]);
        assert!(!m.gyro_on);
        m.step(&with(0, 0, 8));
        // Long press with the finger still: the click it began with is let go, the touchpad mouse goes off.
        assert_eq!(m.step(&with(0, PAD_CLICK, 10)), vec![Out::Button { right: false, down: true }]);
        assert_eq!(m.step(&with(0, PAD_CLICK, 10 + 149)), vec![], "596 ms: not yet");
        assert_eq!(m.step(&with(0, PAD_CLICK, 10 + 150)), vec![Out::Sound(crate::voice::OFF), Out::Button { right: false, down: false }]);
        assert!(!m.touch_on);
        assert_eq!(m.step(&with(0, PAD_CLICK, 400)), vec![], "still held: once");
        m.step(&with(0, 0, 401));
        assert_eq!(m.step(&with(0, PAD_CLICK, 402)), vec![], "touchpad off: no click");
        // Long press again: on, with the ding, and that press is not a click.
        assert_eq!(m.step(&with(0, PAD_CLICK, 402 + 150)), vec![Out::Sound(crate::voice::DING)]);
        assert!(m.touch_on);
        assert_eq!(m.step(&with(0, PAD_CLICK, 600)), vec![]);
        m.step(&with(0, 0, 601));
        // A press that drags (the finger moves) never becomes a long press.
        let drag = |x: u16, b2: u8, i: u32| st(b2, [Some((1, x, 500)), None], [0; 3], i * TICK);
        assert_eq!(m.step(&drag(500, PAD_CLICK, 700)), vec![Out::Button { right: false, down: true }]);
        assert_eq!(m.step(&drag(600, PAD_CLICK, 701)), vec![Out::Move(100, 0)]);
        assert_eq!(m.step(&drag(600, PAD_CLICK, 900)), vec![]);
        assert!(m.touch_on);
    }

    #[test]
    fn triggers_click_only_when_armed() {
        let mut m = Mouse::new(([0.0625; 3], [0.0; 3]));
        let st1 = |b1: u8, touch, i: u32| ds::State { buttons: [8, b1, 0], touch, imu: Some(([0; 3], [0; 3], i * TICK)), ..Default::default() };
        let finger = [Some((1, 500, 500)), None];
        // No finger, gyro off: R2 is the game's.
        assert_eq!(m.step(&st1(R2, [None, None], 0)), vec![]);
        // Still held when a finger lands: no click (it was not pulled while armed).
        assert_eq!(m.step(&st1(R2, finger, 1)), vec![]);
        assert_eq!(m.step(&st1(0, finger, 2)), vec![]);
        // Finger on the pad: R2 = left; it holds after the finger lifts, until R2 lets go.
        assert_eq!(m.step(&st1(R2, finger, 3)), vec![Out::Button { right: false, down: true }]);
        assert_eq!(m.step(&st1(R2, [None, None], 4)), vec![]);
        assert_eq!(m.step(&st1(0, [None, None], 5)), vec![Out::Button { right: false, down: false }]);
        // Gyro on, no finger: L2 = right.
        m.gyro_on = true;
        assert_eq!(m.step(&st1(L2, [None, None], 6)), vec![Out::Button { right: true, down: true }]);
        assert_eq!(m.release(), vec![Out::Button { right: true, down: false }]);
    }

    #[test]
    fn gyro_rests_still_and_keeps_slow_turns() {
        let mut m = Mouse::new(([0.0625; 3], [0.0; 3]));
        m.gyro_on = true;
        // Noise at rest (under 1 deg/s): no movement over 2 s.
        let mut out = Vec::new();
        for i in 0..500u32 {
            out.extend(m.step(&st(0, [None, None], [(i % 3) as i16 * 8 - 8, 5, -3], i * TICK)));
        }
        assert_eq!(out, vec![]);
        // A steady slow turn left at 4 deg/s (yaw raw 64 over the rest value) for 1 s: about 4 deg, nothing lost to rounding.
        let mut dx = 0;
        for i in 500..750u32 {
            for o in m.step(&st(0, [None, None], [0, 5 + 64, 0], i * TICK)) {
                if let Out::Move(x, _) = o {
                    dx += x;
                }
            }
        }
        let deg = -dx as f32 / GYRO_PX_PER_DEG; // 4 deg/s, less what the slowly learned bias (up to 5 raw) takes
        assert!((3.6..=4.4).contains(&deg), "turned {deg} deg");
    }

    #[test]
    fn a_slow_tilt_does_not_leave_drift() {
        // Tilting down slowly (1.5 deg/s) for 3 s, then holding still: the still part must not move.
        let mut m = Mouse::new(([0.0625; 3], [0.0; 3]));
        m.gyro_on = true;
        for i in 0..750u32 {
            m.step(&st(0, [None, None], [-24, 0, 0], i * TICK));
        }
        let mut out = Vec::new();
        for i in 750..1500u32 {
            out.extend(m.step(&st(0, [None, None], [(i % 3) as i16 * 4 - 4, 0, 0], i * TICK)));
        }
        let moved: i32 = out.iter().map(|o| if let Out::Move(_, y) = o { y.abs() } else { 0 }).sum();
        assert!(moved <= 1, "drifted {moved} px");
    }

    #[test]
    fn waits_for_a_long_press() {
        let short = |b2| ds::State { buttons: [0, 0, b2], ..ds::State::default() };
        let (mut hold, t) = (crate::voice::Hold::default(), Instant::now());
        let ms = Duration::from_millis;
        assert!(!wake(&ds::Kind::BtSimple, &short(0x01), &mut hold, t), "PS is not the touchpad");
        assert!(!wake(&ds::Kind::BtSimple, &short(PAD_CLICK), &mut hold, t));
        assert!(!wake(&ds::Kind::BtSimple, &short(0), &mut hold, t + ms(300)), "a short click: plain readers keep the controller");
        assert!(!wake(&ds::Kind::BtSimple, &short(PAD_CLICK | 0x01), &mut hold, t + ms(400)));
        assert!(wake(&ds::Kind::BtSimple, &short(PAD_CLICK), &mut hold, t + ms(1000)), "held 600 ms: switch now");
        assert!(hold.fired);
        assert!(wake(&ds::Kind::BtFull { crc_ok: true }, &short(0), &mut crate::voice::Hold::default(), t), "already full: nothing to keep");
    }

    #[test]
    fn calibration_scale() {
        // Made-up report: bias 0, plus/minus ±8000 on every axis, speed 540 + 540.
        let mut f = [0u8; 41];
        for a in 0..3 {
            f[7 + 4 * a..9 + 4 * a].copy_from_slice(&8000i16.to_le_bytes());
            f[9 + 4 * a..11 + 4 * a].copy_from_slice(&(-8000i16).to_le_bytes());
        }
        f[19..21].copy_from_slice(&540i16.to_le_bytes());
        f[21..23].copy_from_slice(&540i16.to_le_bytes());
        assert_eq!(calibration(&f), ([1080.0 / 16000.0; 3], [0.0; 3]));
        assert_eq!(calibration(&[]), ([2048.0 / 32767.0; 3], [0.0; 3]));
        // A real controller over Bluetooth: biases 0, -1, 4; about ±8860; speed 540 + 540.
        let hex = "05 00 00 ff ff 04 00 9f 22 5f dd a4 22 57 dd f2 22 19 dd 1c 02 1c 02 fc 1f 23 e0 50 20 5b e0 bb 1f bf df 02 00 4f d6 eb 5c";
        let f: Vec<u8> = hex.split(' ').map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
        assert_eq!(ds::crc32(0xA3, &f[..37]), u32::from_le_bytes(f[37..41].try_into().unwrap()));
        let (s, b) = calibration(&f);
        assert_eq!(b, [0.0, -1.0, 4.0]);
        assert!((s[0] - 1080.0 / (8863.0 + 8865.0)).abs() < 1e-6, "{s:?}");
    }
}
