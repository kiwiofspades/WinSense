//! The PC's state on the controller (0.15.0), tray only, each OFF unless ticked:
//! "Mute light follows PC mic": the mute light is lit while the Windows default recording device
//! is muted. "Buzz on notifications": a short double tap in the grips when Windows gets a new
//! notification. One thread (`run`) looks a few times a second and only reads Windows; the light
//! goes out through the worker (`Msg::Light`), the tap through the haptic thread (`port::tap`).

use crate::ui::Msg;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::NotificationKinds;

/// The tray toggles.
pub static LIGHT_ON: AtomicBool = AtomicBool::new(false);
pub static BUZZ_ON: AtomicBool = AtomicBool::new(false);
/// How often the thread looks at the mic; notifications every `LOOK`th time (0.9 s).
const TICK: Duration = Duration::from_millis(300);
const LOOK: u32 = 3;
/// At most one buzz in this long, however many notifications arrive.
const EVERY: Duration = Duration::from_secs(3);
/// A mic's own mute button counts after this long of exact silence (`Hush`).
const HUSH: Duration = Duration::from_millis(1500);

/// The mute light as far as WinSense knows: true = lit by us.
pub struct Light(pub bool);

impl Light {
    /// What to send now: Some(1) light it, Some(0) put it out, None nothing. `muted` = the PC
    /// mic's state, None when unknown (no recording device): the light then stays as it is.
    /// A controller that connects is dark, so an unmuted mic sends nothing at all. While voice
    /// listens (`listening`, the controller's own mic is open) nothing is sent; it catches up after.
    pub fn step(&mut self, on: bool, connected: bool, muted: Option<bool>, listening: bool) -> Option<u8> {
        if !connected {
            self.0 = false; // it comes back dark
            return None;
        }
        let want = if on { muted? } else { false };
        (want != self.0 && !listening).then(|| {
            self.0 = want;
            want as u8
        })
    }
}

/// A mic's own mute button (a USB mic's, for one) does not tell Windows: the endpoint stays unmuted, but
/// the mic then sends exact digital silence. MEASURED 2026-11-11: button pressed = peak exactly 0
/// for the whole 18 s; unmuted, silent room, Voice Clarity on = never 0 in about an hour (lowest
/// 0.0009). So: muted once the level has been exactly 0 for `HUSH`. `peak` is None when no app
/// records from the mic (the level is 0 then, muted or not), which counts as not muted.
#[derive(Default)]
pub struct Hush(Option<Instant>);

impl Hush {
    pub fn step(&mut self, peak: Option<f32>, now: Instant) -> bool {
        if peak != Some(0.0) {
            self.0 = None;
            return false;
        }
        now.duration_since(*self.0.get_or_insert(now)) >= HUSH
    }
}

/// New notifications to taps.
#[derive(Default)]
pub struct Buzz {
    /// The notifications Windows held at the last look; None = not looked yet (or the toggle is off).
    seen: Option<Vec<u32>>,
    last: Option<Instant>,
}

impl Buzz {
    /// `ids` = the notifications Windows holds now. True = tap now: one of them was not there at
    /// the last look, the last tap is `EVERY` ago, and `ok` (see `run`). The first look after the
    /// toggle goes on only remembers what is there: old notifications never buzz.
    pub fn step(&mut self, ids: Option<Vec<u32>>, now: Instant, ok: bool) -> bool {
        let new = ids.as_ref().zip(self.seen.as_ref()).is_some_and(|(ids, seen)| ids.iter().any(|i| !seen.contains(i)));
        self.seen = ids;
        let tap = new && ok && self.last.is_none_or(|t| now.duration_since(t) >= EVERY);
        if tap {
            self.last = Some(now);
        }
        tap
    }
}

/// The Windows default recording device: its Windows mute, and its level (peak, 0-1) while an app
/// records from it (None when none does). None when there is no recording device.
/// DECISION: WinSense never records itself (the taskbar's "mic in use" icon would stay on), so a
/// mute button is seen only while another app (Steam, Discord, a game) has the mic open.
fn mic() -> Option<(bool, Option<f32>)> {
    use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
    use windows::Win32::Media::Audio::{AudioSessionStateActive, IAudioSessionManager2, eCapture};
    use windows::Win32::System::Com::CLSCTX_ALL;
    unsafe {
        let dev = crate::voice::default_device(eCapture).ok()?;
        let muted = dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).and_then(|v| v.GetMute()).ok()?.as_bool();
        let all = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).and_then(|m| m.GetSessionEnumerator()).ok()?;
        let recording = (0..all.GetCount().unwrap_or(0)).any(|i| all.GetSession(i).and_then(|s| s.GetState()).is_ok_and(|st| st == AudioSessionStateActive));
        let peak = recording.then(|| dev.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None).and_then(|m| m.GetPeakValue()).ok()).flatten();
        Some((muted, peak))
    }
}

/// The ids of the notifications Windows holds; None when Windows does not let us see them.
/// DECISION: listed about once a second, not the NotificationChanged event (reported to fail
/// without package identity, and it could not be tried without raising a notification).
fn listed(l: &UserNotificationListener) -> Option<Vec<u32>> {
    if l.GetAccessStatus().ok()? != UserNotificationListenerAccessStatus::Allowed {
        return None; // never RequestAccessAsync: it may put a consent prompt on screen unasked
    }
    let all = l.GetNotificationsAsync(NotificationKinds::Toast).ok()?.join().ok()?;
    Some(all.into_iter().filter_map(|n| n.Id().ok()).collect())
}

/// The tray's thread for both toggles; costs two sleeps a second while both are off. Ends by
/// putting out a light it lit. `dry`: prints what it reads, and the buzz does not wait for a controller.
pub fn run(stop: &AtomicBool, tx: Sender<Msg>, dry: bool) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let connected = || crate::mouse::STATE.load(Relaxed) != 0;
    let (mut light, mut buzz, mut hush) = (Light(false), Buzz::default(), Hush::default());
    let (mut listener, mut told, mut was) = (None, false, None);
    for n in 0.. {
        if stop.load(Relaxed) {
            break;
        }
        let on = LIGHT_ON.load(Relaxed);
        let muted = if on { mic().map(|(muted, peak)| hush.step(peak, Instant::now()) || muted) } else { None };
        if dry && on && muted != was {
            println!("dry mic light: PC mic muted = {muted:?}");
            was = muted;
        }
        if let Some(v) = light.step(on, connected(), muted, crate::ds::MIC.load(Relaxed)) {
            let _ = tx.send(Msg::Light(v));
        }
        if !BUZZ_ON.load(Relaxed) {
            (buzz.seen, told) = (None, false);
        } else if n % LOOK == 0 {
            if listener.is_none() {
                listener = UserNotificationListener::Current().ok(); // only once ticked: it loads Windows' notification parts
            }
            let begun = Instant::now();
            let ids = listener.as_ref().and_then(listed);
            if dry && buzz.seen.is_none() {
                println!("dry buzz: {:?} notifications listed in {} ms", ids.as_ref().map(Vec::len), begun.elapsed().as_millis());
            }
            if ids.is_none() && !std::mem::replace(&mut told, true) {
                *crate::voice::NOTE.lock().unwrap() = Some("Buzz on notifications: Windows does not let WinSense see notifications (Settings > Privacy & security > Notifications).");
            }
            // Only a controller already on the full report (the battery is known then; the cable
            // always is): the tap would switch a Bluetooth one and blind plain readers. Not while
            // voice listens (the grips sit next to the mic), not while a game is in front.
            let ok = (dry || crate::power::battery().is_some()) && !crate::ds::MIC.load(Relaxed) && !crate::couch::gate();
            if buzz.step(ids, Instant::now(), ok) {
                if dry {
                    println!("dry buzz: tap");
                }
                crate::port::tap();
            }
        }
        std::thread::sleep(TICK);
    }
    // Quit: hand the light back if we lit it (the worker is still there: the tray joins us first).
    if let Some(v) = light.step(false, connected(), None, false) {
        let _ = tx.send(Msg::Light(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_mute_becomes_light_commands() {
        let mut l = Light(false);
        assert_eq!(l.step(false, true, Some(true), false), None, "off unless ticked");
        assert_eq!(l.step(true, true, Some(false), false), None, "not muted and the light is dark: nothing is sent");
        assert_eq!(l.step(true, true, None, false), None, "state unknown: nothing is sent");
        assert_eq!(l.step(true, true, Some(true), false), Some(1));
        assert_eq!(l.step(true, true, Some(true), false), None, "once per change");
        assert_eq!(l.step(true, true, None, false), None, "the recording device went away: left as it is");
        assert_eq!(l.step(true, true, Some(false), true), None, "voice is listening: wait");
        assert_eq!(l.step(true, true, Some(false), false), Some(0), "caught up after listening");
        assert_eq!(l.step(true, true, Some(true), false), Some(1));
        assert_eq!(l.step(true, false, Some(true), false), None, "controller gone");
        assert_eq!(l.step(true, true, Some(true), false), Some(1), "it came back dark: lit again");
        assert_eq!(l.step(false, true, None, false), Some(0), "unticked (or Quit): handed back");
        assert_eq!(l.step(false, true, None, false), None, "and only once");
    }

    #[test]
    fn new_notifications_become_rate_limited_taps() {
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        let mut b = Buzz::default();
        assert!(!b.step(Some(vec![1, 2]), at(0), true), "what was already there never buzzes");
        assert!(!b.step(Some(vec![1, 2]), at(900), true));
        assert!(b.step(Some(vec![1, 2, 3]), at(1800), true), "a new one");
        assert!(!b.step(Some(vec![1, 2, 3, 4]), at(2700), true), "another within 3 s: no second buzz");
        assert!(!b.step(Some(vec![2, 3, 4]), at(9000), true), "one dismissed is not news");
        assert!(!b.step(Some(vec![2, 3, 4, 5]), at(9900), false), "a game in front, voice listening or no full report: no buzz");
        assert!(!b.step(Some(vec![2, 3, 4, 5]), at(10800), true), "and not later for that one either");
        assert!(b.step(Some(vec![2, 3, 4, 5, 6, 7]), at(11700), true), "two at once: one buzz");
        assert!(!b.step(None, at(20000), true), "Windows refused, or the toggle went off");
        assert!(!b.step(Some(vec![9]), at(21000), true), "the first look after that only remembers");
        assert!(b.step(Some(vec![9, 10]), at(22000), true));
    }

    #[test]
    fn a_silent_mic_counts_as_muted() {
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        let mut h = Hush::default();
        assert!(!h.step(Some(0.0), at(0)), "exact silence starts the count");
        assert!(!h.step(Some(0.0), at(1200)));
        assert!(h.step(Some(0.0), at(1500)), "1.5 s of exact silence: the mic's own mute button");
        assert!(!h.step(Some(0.0009), at(1800)), "the quietest unmuted room measured: not muted");
        assert!(!h.step(Some(0.0), at(2100)), "the count starts over");
        assert!(!h.step(None, at(5000)), "no app records: unknown, not muted");
        assert!(!h.step(Some(0.0), at(5300)), "and starts over after that too");
    }

    /// Reads only: the real default recording device (None on a PC without one) and Windows'
    /// notification list (ids only), the way the tray's thread does.
    #[test]
    fn windows_answers() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        println!("PC mic (muted, level while an app records): {:?}", mic());
        let l = UserNotificationListener::Current().unwrap();
        println!("access {:?}, {:?} notifications", l.GetAccessStatus().map(|s| s.0), listed(&l).map(|v| v.len()));
    }
}
