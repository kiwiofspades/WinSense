//! Game sound as haptics (0.12.0): while the tray's toggle is on, what Windows plays on the
//! default playback device is captured (WASAPI loopback: reads only, makes no sound), its low end
//! is kept and sent to the grips of every Bluetooth DualSense in report 0x32, so any game gets
//! haptics without a mod. The haptic thread (src/port.rs) owns the `Stream` and plays it between
//! a mod's tones. Rules: docs/guide.md.
//!
//! Filter (the choice): per channel a 20 Hz high-pass (2nd order, takes out DC and sub-bass the
//! actuators cannot move) and a 200 Hz low-pass (4th-order Butterworth, 24 dB per octave), run at
//! the capture rate. That low-pass is also the anti-alias filter for the 3 kHz output (its
//! Nyquist, 1500 Hz, is 70 dB down), so the downsample just picks every 16th sample.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::core::{Error, Result};

/// The tray's toggle "Game sound as haptics": off unless ticked.
pub static ON: AtomicBool = AtomicBool::new(false);
/// The stream carries signal right now (the grips are in use: not idle, src/power.rs).
pub static ACTIVE: AtomicBool = AtomicBool::new(false);

const OUT: u32 = 3000; // report 0x32's sample rate
const LOW: f64 = 20.0;
const HIGH: f64 = 200.0;
/// Strength: the filtered sound times this, clipped to full scale. A guess, not felt yet: game
/// bass sits around 0.1 to 0.3 of full scale, which this brings near the top of the s8 range.
const GAIN: f64 = 4.0;
/// The gate: a frame at or above this (of 127) counts as signal...
const GATE: u8 = 3;
/// ...and the stream stays on this many 3 kHz frames after the last one (0.5 s), so it does not flap between drum hits.
const HOLD: u32 = 1500;
/// Frames kept waiting (32 ms): the lead-in when the gate opens, and the cushion between the
/// sound card's clock and the PC's; older ones are dropped.
const KEEP: usize = 96;

/// One second-order filter section (RBJ cookbook), transposed direct form II.
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(high_pass: bool, hz: f64, q: f64, rate: f64) -> Self {
        let (s, c) = (std::f64::consts::TAU * hz / rate).sin_cos();
        let (alpha, k) = (s / (2.0 * q), if high_pass { 1.0 + c } else { 1.0 - c });
        let a0 = 1.0 + alpha;
        Biquad { b: [k / 2.0 / a0, if high_pass { -k } else { k } / a0, k / 2.0 / a0], a: [-2.0 * c / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z = [self.b[1] * x - self.a[0] * y + self.z[1], self.b[2] * x - self.a[1] * y];
        y
    }
}

/// The signal work, pure: stereo float frames at the capture rate in, gated 3 kHz s8 stereo
/// frames for report 0x32 out (left channel = left grip).
pub struct Feed {
    rate: u32,
    acc: u32,
    filters: [[Biquad; 3]; 2],
    quiet: u32, // 3 kHz frames since the last one at or above GATE
    queue: VecDeque<[i8; 2]>,
    starved: bool,
}

impl Feed {
    pub fn new(rate: u32) -> Self {
        let r = rate as f64;
        // 0.5412 and 1.3066: the two sections of a 4th-order Butterworth.
        let chain = || [Biquad::new(true, LOW, 0.7071, r), Biquad::new(false, HIGH, 0.5412, r), Biquad::new(false, HIGH, 1.3066, r)];
        Feed { rate: rate.max(OUT), acc: 0, filters: [chain(), chain()], quiet: HOLD, queue: VecDeque::new(), starved: false }
    }

    /// One captured frame.
    pub fn push(&mut self, l: f32, r: f32) {
        let mut y = [l as f64, r as f64];
        for (y, f) in y.iter_mut().zip(&mut self.filters) {
            *y = f.iter_mut().fold(*y, |x, s| s.step(x));
        }
        self.acc += OUT;
        if self.acc < self.rate {
            return;
        }
        self.acc -= self.rate; // a 3 kHz frame is due
        let f = y.map(|v| ((v * GAIN).clamp(-1.0, 1.0) * 127.0) as i8);
        self.quiet = if f.iter().any(|s| s.unsigned_abs() >= GATE) { 0 } else { self.quiet.saturating_add(1) };
        if self.queue.len() == KEEP {
            self.queue.pop_front();
        }
        self.queue.push_back(f);
    }

    /// Nothing was captured for one report's time (Windows delivers nothing while nothing plays).
    pub fn lull(&mut self) {
        self.quiet = self.quiet.saturating_add(32);
    }

    /// The gate: signal now or within the last half second.
    pub fn open(&self) -> bool {
        self.quiet < HOLD
    }

    /// The next report's frames. When the capture falls behind, silence until two reports wait
    /// again (one short gap, not a stutter in every report).
    pub fn take(&mut self) -> [[i8; 2]; 32] {
        let mut out = [[0; 2]; 32];
        self.starved = self.queue.len() < if self.starved { 64 } else { 32 };
        if !self.starved {
            out.iter_mut().zip(self.queue.drain(..32)).for_each(|(o, f)| *o = f);
        }
        out
    }
}

struct Capture {
    _client: IAudioClient, // keeps the stream running
    cap: IAudioCaptureClient,
    id: String,
    channels: usize,
}

/// The default playback device and its endpoint id.
fn default_device() -> Result<(IMMDevice, String)> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let p = dev.GetId()?;
        let id = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as _));
        Ok((dev, id))
    }
}

const OWN_DEVICE: &str = "the controller's own speaker is the default sound device";

/// Start listening to what `dev` plays, in its own mix format. Returns the capture and its rate.
fn listen(dev: &IMMDevice, id: String) -> Result<(Capture, u32)> {
    if crate::audio::is_dualsense(&id) {
        return Err(Error::new(E_FAIL, OWN_DEVICE)); // never the controller's own sound (it would feed itself)
    }
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let p = client.GetMixFormat()?;
        let f = p.read_unaligned();
        // 3 = IEEE float, as the tag or as the first field of the extensible sub-format.
        let float = f.wBitsPerSample == 32 && (f.wFormatTag == 3 || f.wFormatTag == 0xFFFE && (p as *const WAVEFORMATEXTENSIBLE).read_unaligned().SubFormat.data1 == 3);
        let r = if float { client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, p, None) } else { Err(Error::new(E_FAIL, "the sound device's mix format is not 32-bit float")) };
        CoTaskMemFree(Some(p as _));
        r?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((Capture { _client: client, cap, id, channels: f.nChannels as usize }, f.nSamplesPerSec))
    }
}

/// The capture and its `Feed`, owned by the haptic thread (COM objects stay on their thread).
pub struct Stream {
    cap: Option<Capture>,
    pub feed: Feed,
    check: Instant,
    dry: bool,
    own: bool, // already said that the controller's own speaker is the default
}

impl Stream {
    pub fn new(dry: bool) -> Self {
        Stream { cap: None, feed: Feed::new(48_000), check: Instant::now(), dry, own: false }
    }

    /// Stop listening (the toggle went off).
    pub fn close(&mut self) {
        self.cap = None;
        ACTIVE.store(false, Ordering::Relaxed);
    }

    /// Take in what was played since the last call; true while there is something for the grips.
    /// Asked every 10 ms while waiting and before every report while streaming.
    pub fn poll(&mut self) -> bool {
        if !ON.load(Ordering::Relaxed) {
            self.close();
            return false;
        }
        let now = Instant::now();
        if now >= self.check {
            // Every 2 s: follow the default device (a headset switched, the cable plugged in), try again after a failure.
            self.check = now + Duration::from_secs(2);
            let dev = default_device();
            if !matches!((&dev, &self.cap), (Ok(d), Some(c)) if d.1 == c.id) {
                let r = dev.and_then(|(dev, id)| listen(&dev, id));
                let own = matches!(&r, Err(e) if e.message() == OWN_DEVICE);
                if own && !self.own {
                    *crate::voice::NOTE.lock().unwrap() = Some("Game sound as haptics is paused: the controller's own speaker is the default sound device.");
                }
                self.own = own;
                if self.dry {
                    println!("dry sound: {}", r.as_ref().map_or_else(|e| format!("not listening ({})", e.message()), |(c, rate)| format!("listening ({rate} Hz, {} channels)", c.channels)));
                }
                self.cap = r.ok().map(|(c, rate)| {
                    self.feed = Feed::new(rate);
                    c
                });
            }
        }
        let Some(c) = &self.cap else {
            ACTIVE.store(false, Ordering::Relaxed);
            return false;
        };
        match drain(c, &mut self.feed) {
            Ok(0) => self.feed.lull(),
            Ok(_) => {}
            Err(_) => self.cap = None, // the device went away: looked for again within 2 s
        }
        // Rule: rumble wins. Report 0x32 is not felt while emulated rumble is on (measured), so
        // while the worker drives a motor (test screen, mod, game through the opt-in pad) the
        // stream waits; and it waits while the mic listens (a buzz in the grips is noise in it).
        let play = self.cap.is_some() && self.feed.open() && !crate::ui::RUMBLE.load(Ordering::Relaxed) && !crate::ds::MIC.load(Ordering::Relaxed);
        ACTIVE.store(play, Ordering::Relaxed);
        play
    }
}

/// Feed every waiting packet; the number of frames taken in.
fn drain(c: &Capture, feed: &mut Feed) -> Result<u32> {
    let mut total = 0;
    unsafe {
        while c.cap.GetNextPacketSize()? > 0 {
            let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            c.cap.GetBuffer(&mut data, &mut n, &mut flags, None, None)?;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                (0..n).for_each(|_| feed.push(0.0, 0.0));
            } else {
                let s = std::slice::from_raw_parts(data as *const f32, n as usize * c.channels);
                s.chunks_exact(c.channels).for_each(|f| feed.push(f[0], f[(c.channels > 1) as usize]));
            }
            c.cap.ReleaseBuffer(n)?;
            total += n;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` of a sine on the left channel only, then everything the feed made of it.
    fn run(rate: u32, hz: f32, level: f32, secs: f32) -> (Feed, Vec<[i8; 2]>) {
        let mut feed = Feed::new(rate);
        let mut out = Vec::new();
        for i in 0..(rate as f32 * secs) as u32 {
            feed.push(level * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin(), 0.0);
            if feed.queue.len() >= 32 {
                out.extend(feed.take());
            }
        }
        (feed, out)
    }

    #[test]
    fn bass_comes_through_and_treble_does_not() {
        let (feed, out) = run(48_000, 60.0, 0.2, 1.0);
        assert!((2970..=3000).contains(&out.len()), "3 kHz out: {}", out.len());
        let peak = |o: &[[i8; 2]], ch: usize| o.iter().map(|f| f[ch].unsigned_abs()).max().unwrap();
        assert!((90..=105).contains(&peak(&out[1500..], 0)), "0.2 x GAIN 4 of 127: {}", peak(&out[1500..], 0));
        assert!(out.iter().any(|f| f[0] < -80), "signed: swings below zero");
        assert_eq!(peak(&out, 1), 0, "the right channel stays the right grip's");
        assert!(feed.open(), "the gate is open");
        // Still a 60 Hz wave: 120 sign changes a second.
        let flips = out[1500..].windows(2).filter(|w| (w[0][0] < 0) != (w[1][0] < 0)).count();
        assert!((58..=62).contains(&flips), "{flips} sign changes in half a second");

        let (feed, out) = run(48_000, 2000.0, 1.0, 1.0);
        assert!(peak(&out[300..], 0) <= 1, "2 kHz at full scale is gone: {}", peak(&out[300..], 0));
        assert!(!feed.open(), "and does not open the gate");
        let (feed, out) = run(48_000, 5.0, 0.2, 2.0);
        assert!(peak(&out[3000..], 0) < 15, "sub-bass is cut: {}", peak(&out[3000..], 0));
        drop(feed);
        let (_, out) = run(44_100, 60.0, 0.2, 1.0);
        assert!((2970..=3000).contains(&out.len()) && (90..=105).contains(&peak(&out[1500..], 0)), "44.1 kHz too");
        assert_eq!(peak(&run(48_000, 60.0, 1.0, 0.5).1, 0), 127, "loud sound clips at full scale, no wrap-around");
    }

    #[test]
    fn silence_makes_no_reports_and_the_gate_closes_again() {
        let mut feed = Feed::new(48_000);
        assert!(!feed.open(), "closed at the start");
        (0..96_000).for_each(|_| feed.push(0.0, 0.0));
        assert!(!feed.open(), "silence never opens it");
        (0..4800).for_each(|_| feed.push(0.0005, -0.0005)); // far below the gate, and DC
        assert!(!feed.open());
        assert!(feed.queue.len() <= KEEP && feed.queue.iter().all(|f| *f == [0, 0]));

        let (mut feed, _) = run(48_000, 60.0, 0.2, 0.2);
        assert!(feed.open());
        (0..12_000).for_each(|_| feed.push(0.0, 0.0)); // 0.25 s of silence: held
        assert!(feed.open(), "held between hits");
        (0..16_000).for_each(|_| feed.push(0.0, 0.0));
        assert!(!feed.open(), "closed half a second after the last signal");

        let (mut feed, _) = run(48_000, 60.0, 0.2, 0.2);
        (0..46).for_each(|_| feed.lull());
        assert!(feed.open());
        feed.lull(); // 47 reports' time with nothing captured = half a second
        assert!(!feed.open(), "nothing playing at all closes it too");
    }

    #[test]
    fn a_starved_stream_waits_for_two_reports() {
        let mut feed = Feed::new(3000);
        let mut i = 0.0f32;
        let mut push = |feed: &mut Feed, n: usize| {
            (0..n).for_each(|_| {
                i += 1.0;
                let s = 0.2 * (std::f32::consts::TAU * 60.0 * i / 3000.0).sin();
                feed.push(s, s);
            })
        };
        push(&mut feed, 200);
        assert_eq!(feed.queue.len(), KEEP, "only the newest 32 ms wait");
        assert!((0..3).all(|_| feed.take() != [[0; 2]; 32]));
        assert_eq!(feed.take(), [[0; 2]; 32], "empty: silence");
        push(&mut feed, 40);
        assert_eq!(feed.take(), [[0; 2]; 32], "still filling");
        push(&mut feed, 24);
        assert_ne!(feed.take(), [[0; 2]; 32], "two reports wait: plays again");
    }
}
