//! Game sound as haptics (0.12.0): while the tray's toggle is on, what Windows plays on the
//! default playback device is captured (WASAPI loopback: reads only, makes no sound), its low end
//! is kept and sent to the grips of every Bluetooth DualSense in report 0x32, so any game gets
//! haptics without a mod. The haptic thread (src/port.rs) owns the `Stream` and plays it between
//! a mod's tones. Rules: docs/guide.md.
//!
//! Filter (0.17.1): per channel a 20 Hz high-pass (4th order: DC and sub-bass the actuators
//! cannot move) and an 800 Hz low-pass (8th-order Butterworth), run at the capture rate. That
//! low-pass is also the anti-alias filter for the 3 kHz output, so the downsample picks samples.
//! Then a strength curve on the band's envelope (see LOW_DB), so loud and quiet parts of a song
//! feel different, and a gate that keeps the stream on while anything plays.

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
/// The band (0.17.1): 20 Hz to 800 Hz, bass and the tune. Felt as better than 20-200 Hz;
/// a vocals band (150-900 Hz, middle of the mix) gave "absolutely nothing".
const LOW: f64 = 20.0;
const HIGH: f64 = 800.0;
/// Strength curve (0.17.1, measured on real music: low band 0.003 to 0.17 of full scale,
/// average 0.04; loopback is taken before the Windows volume, so this holds at any volume). The
/// low band's envelope is mapped in dB: LOW_DB and below = OUT_MIN, HIGH_DB and above = full
/// scale, straight in dB between. A tone at 10 % is felt, but 10 % and 35 % feel alike, so
/// quiet and loud have to be far apart.
const LOW_DB: f64 = -50.0;
const HIGH_DB: f64 = -15.0;
const OUT_MIN: f64 = 0.12;
/// Below LOW_DB the output fades out (as the square of the level), so filter leftovers stay at 0.
const FLOOR: f64 = 0.003_162; // -50 dB
/// The envelope: rises in 3 ms (a kick drum's start), falls in 80 ms.
const ATTACK: f64 = 1.0 / (0.003 * OUT as f64);
const RELEASE: f64 = 1.0 / (0.080 * OUT as f64);
/// The gate: any sound at all (raw, before the filter) at or above -60 dB keeps the stream on...
const GATE: f32 = 0.001;
/// ...for 5 s after the last of it, so a song never restarts the stream (each new 0x32 stream
/// loses its first part, measured with the notification buzz).
const HOLD: u32 = 5 * OUT;
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
    filters: [[Biquad; 6]; 2],
    quiet: u32, // 3 kHz frames since the last one with sound at or above GATE
    loud: f32,  // raw level since the last 3 kHz frame
    env: f64,
    queue: VecDeque<[i8; 2]>,
    starved: bool,
    pub stats: Stats,
}

/// Measurement counters since the last `Stats::line` (dry tray only prints them).
#[derive(Default)]
pub struct Stats {
    pub frames: u32,
    pub peak: f64, // filtered level before the strength curve, of full scale
    pub sum: f64,  // sum of squares, for the RMS
    pub clipped: u32,
    pub open: u32, // frames with the gate open
    pub takes: u32,
    pub starved: u32,
    pub qmin: usize,
    pub qmax: usize,
    pub lulls: u32,
    pub waits: [u32; 3], // polls held back by: gate closed, rumble, mic
}

impl Feed {
    pub fn new(rate: u32) -> Self {
        let r = rate as f64;
        // A 4th-order Butterworth high-pass, then an 8th-order low-pass: steep enough at 800 Hz to
        // keep 1.5 kHz and up (they would fold back at 3 kHz) out.
        let chain = || {
            let lp = |q| Biquad::new(false, HIGH, q, r);
            [Biquad::new(true, LOW, 0.5412, r), Biquad::new(true, LOW, 1.3066, r), lp(0.5098), lp(0.6013), lp(0.9000), lp(2.5629)]
        };
        Feed { rate: rate.max(OUT), acc: 0, filters: [chain(), chain()], quiet: HOLD, loud: 0.0, env: 0.0, queue: VecDeque::new(), starved: false, stats: Stats { qmin: usize::MAX, ..Default::default() } }
    }

    /// One captured frame.
    pub fn push(&mut self, l: f32, r: f32) {
        self.loud = self.loud.max(l.abs()).max(r.abs());
        let mut y = [l as f64, r as f64];
        for (y, f) in y.iter_mut().zip(&mut self.filters) {
            *y = f.iter_mut().fold(*y, |x, s| s.step(x));
        }
        self.acc += OUT;
        if self.acc < self.rate {
            return;
        }
        self.acc -= self.rate; // a 3 kHz frame is due
        let m = y[0].abs().max(y[1].abs());
        self.env += (m - self.env) * if m > self.env { ATTACK } else { RELEASE };
        let gain = strength(self.env) / self.env.max(1e-9); // one gain for both grips keeps the stereo balance
        let f = y.map(|v| ((v * gain).clamp(-1.0, 1.0) * 127.0) as i8);
        let st = &mut self.stats;
        st.frames += 1;
        st.peak = st.peak.max(m);
        st.sum += m * m;
        st.clipped += (m * gain >= 1.0) as u32;
        st.open += (self.quiet < HOLD) as u32;
        self.quiet = if self.loud >= GATE { 0 } else { self.quiet.saturating_add(1) };
        self.loud = 0.0;
        if self.queue.len() == KEEP {
            self.queue.pop_front();
        }
        self.queue.push_back(f);
    }

    /// Nothing was captured for one report's time (Windows delivers nothing while nothing plays).
    pub fn lull(&mut self) {
        self.quiet = self.quiet.saturating_add(32);
        self.stats.lulls += 1;
    }

    /// The gate: sound now or within the last 5 s.
    pub fn open(&self) -> bool {
        self.quiet < HOLD
    }

    /// The next report's frames. When the capture falls behind, silence until two reports wait
    /// again (one short gap, not a stutter in every report).
    pub fn take(&mut self) -> [[i8; 2]; 32] {
        let mut out = [[0; 2]; 32];
        self.starved = self.queue.len() < if self.starved { 64 } else { 32 };
        let st = &mut self.stats;
        st.takes += 1;
        st.starved += self.starved as u32;
        st.qmin = st.qmin.min(self.queue.len());
        st.qmax = st.qmax.max(self.queue.len());
        if !self.starved {
            out.iter_mut().zip(self.queue.drain(..32)).for_each(|(o, f)| *o = f);
        }
        out
    }
}

/// The output level (of full scale) for a low-band envelope: see LOW_DB.
fn strength(env: f64) -> f64 {
    if env < FLOOR {
        return OUT_MIN * (env / FLOOR).powi(2);
    }
    let t = ((20.0 * env.log10() - LOW_DB) / (HIGH_DB - LOW_DB)).min(1.0);
    OUT_MIN * (1.0 / OUT_MIN).powf(t)
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
    said: Instant,
    own: bool, // already said that the controller's own speaker is the default
}

impl Stream {
    pub fn new(dry: bool) -> Self {
        Stream { cap: None, feed: Feed::new(48_000), check: Instant::now(), said: Instant::now(), dry, own: false }
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
        let (open, rumble, mic) = (self.feed.open(), crate::ui::RUMBLE.load(Ordering::Relaxed), crate::ds::MIC.load(Ordering::Relaxed));
        let w = &mut self.feed.stats.waits;
        w[0] += !open as u32;
        w[1] += rumble as u32;
        w[2] += mic as u32;
        if self.dry && now >= self.said {
            self.said = now + Duration::from_secs(1);
            let s = std::mem::replace(&mut self.feed.stats, Stats { qmin: usize::MAX, ..Default::default() });
            let n = s.frames.max(1) as f64;
            println!("dry stats: frames {} peak {:.3} rms {:.3} clipped {:.0}% gate-open {:.0}% takes {} starved {} queue {}..{} lulls {} waits gate/rumble/mic {:?}", s.frames, s.peak, (s.sum / n).sqrt(), 100.0 * s.clipped as f64 / n, 100.0 * s.open as f64 / n, s.takes, s.starved, if s.takes > 0 { s.qmin } else { 0 }, s.qmax, s.lulls, s.waits);
        }
        let play = self.cap.is_some() && open && !rumble && !mic;
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

    fn peak(o: &[[i8; 2]], ch: usize) -> u8 {
        o.iter().map(|f| f[ch].unsigned_abs()).max().unwrap()
    }

    #[test]
    fn bass_comes_through_and_treble_does_not() {
        let (feed, out) = run(48_000, 60.0, 0.2, 1.0);
        assert!((2970..=3000).contains(&out.len()), "3 kHz out: {}", out.len());
        assert!(peak(&out[1500..], 0) >= 120, "loud bass reaches full scale: {}", peak(&out[1500..], 0));
        assert!(out.iter().any(|f| f[0] < -100), "signed: swings below zero");
        assert_eq!(peak(&out, 1), 0, "the right channel stays the right grip's");
        assert!(feed.open(), "the gate is open");
        // Still a 60 Hz wave: 120 sign changes a second.
        let flips = out[1500..].windows(2).filter(|w| (w[0][0] < 0) != (w[1][0] < 0)).count();
        assert!((58..=62).contains(&flips), "{flips} sign changes in half a second");

        let (_, out) = run(48_000, 2000.0, 1.0, 1.0);
        assert!(peak(&out[300..], 0) <= 3, "2 kHz at full scale is gone (well under what can be felt): {}", peak(&out[300..], 0));
        let (_, out) = run(48_000, 5.0, 0.2, 2.0);
        assert!(peak(&out[3000..], 0) < 45, "sub-bass is held well under loud bass: {}", peak(&out[3000..], 0));
        let (_, out) = run(44_100, 60.0, 0.2, 1.0);
        assert!((2970..=3000).contains(&out.len()) && peak(&out[1500..], 0) >= 120, "44.1 kHz too");
        assert_eq!(peak(&run(48_000, 60.0, 1.0, 0.5).1, 0), 127, "very loud sound stays at full scale, no wrap-around");
    }

    #[test]
    fn the_tune_comes_through() {
        assert!(peak(&run(48_000, 500.0, 0.1, 1.0).1[1500..], 0) >= 90, "a 500 Hz note is kept");
    }

    #[test]
    fn quiet_and_loud_bass_feel_different() {
        let level = |a: f32| peak(&run(48_000, 60.0, a, 1.0).1[1500..], 0) as u32;
        let (quiet, mid, loud) = (level(0.01), level(0.04), level(0.15));
        assert!(quiet >= 20, "a quiet passage is still felt (10 % is): {quiet}");
        assert!(loud >= 3 * quiet, "loud {loud} against quiet {quiet}");
        assert!(quiet < mid && mid < loud, "in order: {quiet} {mid} {loud}");
    }

    #[test]
    fn steady_music_gives_no_gaps() {
        // Kick drums: 120 ms of 55 Hz every half second, a quiet bass line between, delivered
        // the way Windows does (480 frames every 10 ms) and taken one report per 10.67 ms.
        let mut feed = Feed::new(48_000);
        let (mut i, mut silent, mut reports, mut due) = (0u32, 0, 0, 0.0f64);
        for packet in 0..1000 {
            for _ in 0..480 {
                let t = i as f32 / 48_000.0;
                let kick = if t % 0.5 < 0.12 { 0.15 } else { 0.0 };
                let s = kick * (std::f32::consts::TAU * 55.0 * t).sin() + 0.01 * (std::f32::consts::TAU * 90.0 * t).sin();
                feed.push(s, s);
                i += 1;
            }
            let now = (packet + 1) as f64 * 10.0;
            while due + 32.0 / 3.0 <= now {
                due += 32.0 / 3.0;
                let r = feed.take();
                if packet > 10 {
                    assert!(feed.open(), "the gate stays open");
                    reports += 1;
                    silent += (r == [[0; 2]; 32]) as u32;
                }
            }
        }
        assert!(reports > 900);
        assert_eq!(silent, 0, "no silent report in {reports}");
    }

    #[test]
    fn silence_makes_no_reports_and_the_gate_closes_again() {
        let mut feed = Feed::new(48_000);
        assert!(!feed.open(), "closed at the start");
        (0..96_000).for_each(|_| feed.push(0.0, 0.0));
        assert!(!feed.open(), "silence never opens it");
        (0..4800).for_each(|_| feed.push(0.0005, -0.0005)); // below -60 dB, and DC
        assert!(!feed.open());
        assert!(feed.queue.len() <= KEEP && feed.queue.iter().all(|f| *f == [0, 0]));

        let (mut feed, _) = run(48_000, 60.0, 0.2, 0.2);
        assert!(feed.open());
        (0..192_000).for_each(|_| feed.push(0.0, 0.0)); // 4 s of silence: held
        assert!(feed.open(), "held through a pause in the song");
        (0..48_000).for_each(|_| feed.push(0.0, 0.0));
        assert!(!feed.open(), "closed 5 s after the last sound");

        let (mut feed, _) = run(48_000, 60.0, 0.2, 0.2);
        (0..468).for_each(|_| feed.lull());
        assert!(feed.open());
        feed.lull(); // 469 reports' time with nothing captured = 5 s
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
