//! `winsense haptic`: the DualSense's real haptics are sound played on channels 3 and 4 (rear
//! left/right) of its USB sound device "Speakers (DualSense Wireless Controller)"; channels 1
//! and 2 feed its speaker and headphone jack (wine-devel 2022-07 and the DJm00n WASAPI gist;
//! read). Played through winmm (shared mode): Windows mixes the stream into the
//! device's own format, so we refuse to play unless that format is 4 channels front+rear, where
//! nothing of ours can be mixed into the speaker.

use std::ptr::{addr_of, null_mut};
use std::time::Duration;
use windows_sys::Win32::Media::Audio::*;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW};
use windows_sys::core::GUID;

const RATE: u32 = 48_000;
const QUAD: u32 = 0x33; // front left, front right, back left, back right
// Not in the features we pull in.
const DRV_QUERYFUNCTIONINSTANCEID: u32 = 2065;
const DRV_QUERYFUNCTIONINSTANCEIDSIZE: u32 = 2066;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

/// Which actuator(s) a tone goes to: rear left (channel 3), rear right (channel 4), or both.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Side {
    Left,
    Right,
    Both,
}

/// A sine of `hz` at `level` percent, scaled -1..1, `rate` samples a second, with a 10 ms fade
/// in and out so the edges do not click.
fn wave(rate: u32, hz: u32, level: u8, ms: u32) -> impl Iterator<Item = f32> {
    let n = (rate as u64 * ms as u64 / 1000) as usize;
    let fade = (rate / 100).max(1) as f32;
    let peak = level.min(100) as f32 / 100.0;
    (0..n).map(move |i| {
        let edge = (i.min(n - 1 - i) as f32 / fade).min(1.0);
        peak * edge * (std::f32::consts::TAU * hz as f32 * i as f32 / rate as f32).sin()
    })
}

/// Interleaved 4-channel 16-bit samples for the cable: the wave on the chosen rear channel(s),
/// silence on the front two (the speaker).
pub fn tone(side: Side, hz: u32, level: u8, ms: u32) -> Vec<i16> {
    wave(RATE, hz, level, ms)
        .flat_map(|w| {
            let s = (w * i16::MAX as f32) as i16;
            [0, 0, if side != Side::Right { s } else { 0 }, if side != Side::Left { s } else { 0 }]
        })
        .collect()
}

/// Stereo signed 8-bit frames at 3000 Hz for Bluetooth report 0x32 (`ds::haptic_report`).
pub fn tone_bt(side: Side, hz: u32, level: u8, ms: u32) -> Vec<[i8; 2]> {
    wave(3000, hz, level, ms)
        .map(|w| {
            let s = (w * i8::MAX as f32) as i8;
            [if side != Side::Right { s } else { 0 }, if side != Side::Left { s } else { 0 }]
        })
        .collect()
}

/// The notification tap (src/port.rs) as ONE stream: silence, a pulse, a gap, a pulse, at 80 Hz full
/// strength; (lead, pulse, gap) in ms. Felt over Bluetooth, controller resting on a leg:
/// two separate tones of 120 ms at 50 % = nothing, two of 200 ms at 100 % = only one, one of 1.5 s =
/// felt. So each new stream loses its start (guess: the controller fills a buffer first); the
/// silence in front takes that loss once, and the gap inside one stream is not lost.
const TAP: (u32, u32, u32) = (150, 150, 130);

pub fn tap_ms() -> u32 {
    TAP.0 + 2 * TAP.1 + TAP.2
}

pub fn tap_bt() -> Vec<[i8; 2]> {
    let quiet = |ms: u32| vec![[0i8; 2]; (3 * ms) as usize];
    let p = tone_bt(Side::Both, 80, 100, TAP.1);
    [quiet(TAP.0), p.clone(), quiet(TAP.2), p].concat()
}

/// `tap_bt` for the cable's rear channels.
pub fn tap() -> Vec<i16> {
    let quiet = |ms: u32| vec![0i16; (RATE / 1000 * 4 * ms) as usize];
    let p = tone(Side::Both, 80, 100, TAP.1);
    [quiet(TAP.0), p.clone(), quiet(TAP.2), p].concat()
}

/// A tone for the mod port: side, hz, level percent, ms (the ranges of `winsense haptic`).
pub type Haptic = (Side, u32, u8, u32);

/// Send `frames` to Bluetooth DualSenses, 32 per report, one report per 10.67 ms, then three
/// silent reports. A report whose time slot has fully passed is dropped, so a late stream does
/// not burst. Once `keep` says no (asked before each tone report) it ends with one silent report.
/// Returns (reports written, reports dropped as late).
pub fn play_bt(devs: &[hidapi::HidDevice], frames: &[[i8; 2]], keep: &mut dyn FnMut() -> bool) -> Result<(usize, usize), String> {
    let silence = [[0i8; 2]; 32];
    let chunks: Vec<&[[i8; 2]]> = frames.chunks(32).collect();
    paced(devs, chunks.len(), 3, keep, &|k| crate::ds::haptic_report(k as u8, k as u8, chunks.get(k).copied().unwrap_or(&silence)))
}

/// A clip for the controller's speaker over Bluetooth: 200-byte Opus frames (`ds::speaker_report`),
/// one report per 10.67 ms. Cut between reports once `keep` says no.
pub fn play_speaker(devs: &[hidapi::HidDevice], clip: &[u8], keep: &mut dyn FnMut() -> bool) -> Result<(usize, usize), String> {
    let frames: Vec<&[u8; 200]> = clip.chunks_exact(200).map(|f| f.try_into().unwrap()).collect();
    paced(devs, frames.len(), 0, keep, &|k| frames.get(k).map_or_else(|| crate::ds::haptic_report(k as u8, k as u8, &[]), |f| crate::ds::speaker_report(k as u8, k as u8, f)))
}

/// Write `make(k)` for k in 0..n + tail, one per 10.67 ms; k >= n are the closing silent reports.
fn paced(devs: &[hidapi::HidDevice], n: usize, tail: usize, keep: &mut dyn FnMut() -> bool, make: &dyn Fn(usize) -> Vec<u8>) -> Result<(usize, usize), String> {
    let period = Duration::from_nanos(1_000_000_000 * 32 / 3000);
    unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) }; // 1 ms sleeps, so the pacing holds
    let start = std::time::Instant::now();
    let (mut sent, mut late) = (0, 0);
    let mut result = Ok(());
    for k in 0..n + tail {
        if k < n && !keep() {
            let _ = devs.iter().try_for_each(|d| d.write(&make(n)).map(drop));
            break;
        }
        let due = start + period * k as u32;
        let now = std::time::Instant::now();
        match due.checked_duration_since(now) {
            Some(wait) => std::thread::sleep(wait),
            None if now - due > period => {
                late += 1;
                continue;
            }
            None => {}
        }
        let r = make(k);
        if let Some(e) = devs.iter().find_map(|d| d.write(&r).err()) {
            result = Err(format!("write failed after {sent} reports: {e}"));
            break;
        }
        sent += 1;
    }
    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
    result.map(|()| (sent, late))
}

/// True if a stored engine format (the registry blob of PKEY_AudioEngine_DeviceFormat: an
/// 8-byte header, then a WAVEFORMATEXTENSIBLE) is 4 channels front+rear.
fn quad_format(blob: &[u8]) -> bool {
    let u16_at = |i: usize| blob.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mask = blob.get(28..32).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    u16_at(8) == Some(WAVE_FORMAT_EXTENSIBLE) && u16_at(10) == Some(4) && mask == Some(QUAD)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A stream for the grips (game sound as haptics, src/sound.rs): `frames()` gives each report's
/// 32 frames, paced like a tone, until `keep` says no; then one silent report.
pub fn stream_bt(devs: &[hidapi::HidDevice], keep: &mut dyn FnMut() -> bool, frames: &dyn Fn() -> [[i8; 2]; 32]) -> Result<(usize, usize), String> {
    const N: usize = usize::MAX / 2;
    paced(devs, N, 0, keep, &|k| crate::ds::haptic_report(k as u8, k as u8, &if k < N { frames() } else { [[0; 2]; 32] }))
}

/// The winmm index of the DualSense's playback device and its endpoint id, if plugged in.
fn find() -> Option<(u32, String)> {
    endpoints().next()
}

/// True if this playback endpoint id is a DualSense's own sound device.
pub fn is_dualsense(endpoint: &str) -> bool {
    endpoints().any(|(_, id)| id.eq_ignore_ascii_case(endpoint))
}

fn endpoints() -> impl Iterator<Item = (u32, String)> {
    (0..unsafe { waveOutGetNumDevs() }).filter_map(|i| unsafe {
        let mut caps: WAVEOUTCAPSW = std::mem::zeroed();
        waveOutGetDevCapsW(i as usize, &mut caps, size_of::<WAVEOUTCAPSW>() as u32);
        let name = caps.szPname;
        if !String::from_utf16_lossy(&name).contains("DualSense") {
            return None;
        }
        // A device index passed where a handle goes: winmm's way to ask about a closed device.
        let (mut size, h) = (0u32, i as usize as HWAVEOUT);
        waveOutMessage(h, DRV_QUERYFUNCTIONINSTANCEIDSIZE, &mut size as *mut u32 as usize, 0);
        let mut id = vec![0u16; size as usize / 2];
        waveOutMessage(h, DRV_QUERYFUNCTIONINSTANCEID, id.as_mut_ptr() as usize, size as usize);
        Some((i, String::from_utf16_lossy(&id).trim_end_matches('\0').to_string()))
    })
}

/// The stored engine format of a playback endpoint ("{0.0.0.00000000}.{guid}").
fn engine_format(endpoint: &str) -> Option<Vec<u8>> {
    let guid = endpoint.rsplit('.').next()?;
    let key = wide(&format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\Properties"));
    let value = wide("{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0");
    let (mut buf, mut len) = (vec![0u8; 256], 256u32);
    let err = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_BINARY, null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    (err == 0).then(|| buf[..len as usize].to_vec())
}

/// Play `samples` (from `tone`) on the DualSense and wait until done, or until `keep` (asked every
/// 10 ms) says no.
pub fn play(samples: &[i16], keep: &mut dyn FnMut() -> bool) -> Result<(), String> {
    let (index, endpoint) = find().ok_or("no DualSense sound device: is the controller on the USB cable?")?;
    match engine_format(&endpoint) {
        Some(f) if quad_format(&f) => {}
        f => return Err(format!("refusing to play: Windows does not treat the controller as 4-channel front+rear (stored format {f:02x?}), so the signal could reach its speaker")),
    }
    let fmt = WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: 4,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 16,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 16 },
        dwChannelMask: QUAD,
        SubFormat: PCM,
    };
    unsafe {
        let mut h: HWAVEOUT = null_mut();
        let err = waveOutOpen(&mut h, index, addr_of!(fmt).cast(), 0, 0, CALLBACK_NULL);
        if err != 0 {
            return Err(format!("could not open the DualSense sound device (winmm error {err})"));
        }
        let mut hdr: WAVEHDR = std::mem::zeroed();
        hdr.lpData = samples.as_ptr() as *mut u8;
        hdr.dwBufferLength = (samples.len() * 2) as u32;
        let size = size_of::<WAVEHDR>() as u32;
        let mut err = waveOutPrepareHeader(h, &mut hdr, size);
        if err == 0 {
            err = waveOutWrite(h, &mut hdr, size);
            while err == 0 && std::ptr::read_volatile(addr_of!(hdr.dwFlags)) & WHDR_DONE == 0 {
                if !keep() {
                    waveOutReset(h); // stops at once and marks the buffer done
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            waveOutUnprepareHeader(h, &mut hdr, size);
        }
        waveOutClose(h);
        if err != 0 { Err(format!("playing failed (winmm error {err})")) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_only_on_rear_channels() {
        let t = tone(Side::Left, 100, 50, 100);
        assert_eq!(t.len(), 4800 * 4);
        let peak = |ch: usize| t.iter().skip(ch).step_by(4).map(|s| s.unsigned_abs()).max().unwrap();
        assert_eq!((peak(0), peak(1), peak(3)), (0, 0, 0), "speaker channels and the other side stay silent");
        assert!((16_000..=16_384).contains(&peak(2)), "{}", peak(2));
        assert_eq!((t[2], t[t.len() - 2]), (0, 0), "fades from and to silence");
        let both = tone(Side::Both, 100, 100, 50);
        assert!(both.chunks(4).all(|f| f[2] == f[3] && f[0] == 0 && f[1] == 0));
        assert!(tone(Side::Right, 100, 100, 50).chunks(4).all(|f| f[2] == 0));
    }

    #[test]
    fn bt_tone() {
        let t = tone_bt(Side::Right, 100, 100, 1000);
        assert_eq!(t.len(), 3000);
        assert!(t.iter().all(|f| f[0] == 0), "the other side stays silent");
        let peak = t.iter().map(|f| f[1].unsigned_abs()).max().unwrap();
        assert!((120..=127).contains(&peak), "{peak}");
        assert!(t.iter().any(|f| f[1] < -100), "signed: swings below zero");
        assert_eq!((t[0], t[2999]), ([0, 0], [0, 0]), "fades from and to silence");
    }

    #[test]
    fn bt_stream_paces_and_stops() {
        let frames = tone_bt(Side::Both, 80, 50, 300); // 900 frames = 29 reports, then 3 silent
        let t = std::time::Instant::now();
        let (sent, late) = play_bt(&[], &frames, &mut || true).unwrap();
        assert_eq!(sent + late, 32);
        assert!((320..1500).contains(&t.elapsed().as_millis()), "paced at 10.67 ms: {:?}", t.elapsed());
        let mut asked = 0;
        let (sent, late) = play_bt(&[], &frames, &mut || { asked += 1; asked <= 3 }).unwrap();
        assert_eq!(sent + late, 3, "a newer tone cuts in between reports");
        let (sent, late) = play_speaker(&[], &[0u8; 2000], &mut || true).unwrap();
        assert_eq!(sent + late, 10, "ten 200-byte frames, no tail");
        let (mut asked, made) = (0, std::cell::Cell::new(0));
        let (sent, late) = stream_bt(&[], &mut || { asked += 1; asked <= 5 }, &|| { made.set(made.get() + 1); [[1; 2]; 32] }).unwrap();
        assert_eq!((sent + late, made.get()), (5, sent), "a stream runs until told to stop and asks for frames only for reports it sends");
    }

    #[test]
    fn only_quadraphonic_plays() {
        // Stored format of "Speakers (DualSense Wireless Controller)" on the test PC.
        let mut q = vec![0x41, 0, 0, 0, 1, 0, 0, 0, 0xfe, 0xff, 4, 0, 0x80, 0xbb, 0, 0, 0, 0xdc, 5, 0, 8, 0, 16, 0, 22, 0, 16, 0, 0x33, 0, 0, 0];
        assert!(quad_format(&q));
        q[28] = 0x03; // stereo mask
        assert!(!quad_format(&q));
        q[28] = 0x33;
        q[10] = 2; // 2 channels
        assert!(!quad_format(&q));
        assert!(!quad_format(&q[..20]));
    }
}
