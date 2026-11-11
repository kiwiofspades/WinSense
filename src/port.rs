//! The mod port (0.8.0): a game mod writes one short text line per game event into the
//! named pipe `\\.\pipe\WinSense`, and the tray's worker plays it (src/ui.rs; the tray stays the
//! only writer). Any language that can open a file can use it, Lua's `io.open` included, so a mod
//! needs no DLL and nothing is registered. WinSense never writes back (a client that only writes
//! would otherwise fill the pipe). Protocol: docs/modding.md.
//! A mod's effects end when it disconnects (its process ends too) and when WinSense quits.

use crate::audio::Haptic;
use crate::ds::Effect;
use crate::ui::{Msg, OFF};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::*;

pub const PIPE: &str = r"\\.\pipe\WinSense";
/// `tray --dry` listens here, so a self-test never takes the real tray's name.
const DRY_PIPE: &str = r"\\.\pipe\WinSense-dry";
const MAX_LINE: usize = 256;
const FLASH: u64 = 150;
/// Words a mod can send for common game events; each stands for its line and can be mixed with
/// other words (`beam rumble 40 80`). Feel: beam and hit were tried on a real controller; the rest are guesses.
const PRESETS: [(&str, &str); 4] = [
    ("beam", "both vibrate 1 8 25"), // both triggers buzz 25 times a second while pulled past zone 1 (felt; 8 Hz with a light pull was not)
    ("hit", "rumble 220 160 for 250"), // 200/120 for 150 ms was not felt
    ("hitstop", "both resist 0 8 rumble 255 255 for 90"),
    ("fire", "rumble 90 255 for 70"),
];

/// The worker's channel and the haptic thread's, None once the tray quits. Connections only
/// borrow them per message, so a connection blocked in a read never keeps Quit waiting.
static TX: Mutex<Option<(Sender<Msg>, Sender<Option<Haptic>>)>> = Mutex::new(None);

/// A line's parts for the worker, or a tone (None = silence) for the haptic thread.
#[derive(Debug, PartialEq)]
enum Out {
    Worker(Msg),
    Haptic(Option<Haptic>),
    /// A built-in clip for the controller speaker (`voice::named`), played by the voice thread.
    Clip(u8),
}

/// `tray --dry`: there is no voice thread, so a clip is printed.
static DRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn send(m: Out) {
    if let Out::Clip(id) = m {
        match DRY.load(std::sync::atomic::Ordering::Relaxed) {
            true => println!("dry sound clip {id}"),
            false => crate::voice::SOUND.store(id, std::sync::atomic::Ordering::Relaxed),
        }
        return;
    }
    if let Some((tx, haptic)) = &*TX.lock().unwrap() {
        let _ = match m {
            Out::Worker(m) => tx.send(m).is_ok(),
            Out::Haptic(h) => haptic.send(h).is_ok(),
            Out::Clip(_) => true,
        };
    }
}

/// Plays tones in report 0x32 on its own handle, beside the worker's 0x31 writes (Windows queues
/// writes from both; the 10.67 ms pacing would stall the worker). A newer tone, `stop` or Quit
/// cuts the running one between reports. Bluetooth first (also when the cable is in too: the cable
/// is not felt then); with the cable alone, sound on its rear channels.
///
/// Since 0.12.0 it also plays game sound as haptics (src/sound.rs) while that toggle is on: between
/// tones it takes in what Windows plays every 10 ms and streams the low end while there is any. A
/// mod's tone comes first: it cuts the stream between reports, and the stream picks up after it.
fn haptics(rx: Receiver<Option<Haptic>>, dry: bool) {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED); // the capture is COM
    }
    let mut api = if dry { None } else { hidapi::HidApi::new().ok() };
    let mut find = move || match &mut api {
        None => Vec::new(),
        Some(api) => {
            let _ = api.refresh_devices();
            let api = &*api;
            let bt = |d: &&hidapi::DeviceInfo| d.vendor_id() == crate::ds::VID && crate::ds::PIDS.contains(&d.product_id()) && matches!(d.bus_type(), hidapi::BusType::Bluetooth);
            api.device_list().filter(bt).filter_map(|d| d.open_device(api).ok()).collect::<Vec<_>>()
        }
    };
    let mut next = None;
    let stream = std::cell::RefCell::new(crate::sound::Stream::new(dry));
    // shortcut: the stream keeps its controllers open until a write fails or the toggle goes off,
    // so a second controller that connects later joins only then.
    let (mut pads, mut look) = (Vec::new(), Instant::now());
    loop {
        let job = match next.take() {
            Some(j) => j,
            None if !crate::sound::ON.load(std::sync::atomic::Ordering::Relaxed) => {
                stream.borrow_mut().close();
                pads.clear();
                match rx.recv() {
                    Ok(j) => j,
                    Err(_) => return,
                }
            }
            None => match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(j) => j,
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    if !stream.borrow_mut().poll() {
                        continue; // silence: nothing is written
                    }
                    if pads.is_empty() && !dry {
                        if Instant::now() < look {
                            continue;
                        }
                        look = Instant::now() + Duration::from_secs(2); // no Bluetooth DualSense: look again in 2 s
                        pads = find();
                        if pads.is_empty() {
                            continue;
                        }
                    }
                    let mut keep = || match rx.try_recv() {
                        Ok(j) => {
                            next = Some(j);
                            false
                        }
                        Err(TryRecvError::Empty) => stream.borrow_mut().poll(),
                        Err(TryRecvError::Disconnected) => false,
                    };
                    let r = crate::audio::stream_bt(&pads, &mut keep, &|| stream.borrow_mut().feed.take());
                    if r.is_err() {
                        pads.clear(); // the controller went away
                    }
                    if dry {
                        println!("dry sound: streamed {r:?} (sent, late)");
                    }
                    continue;
                }
            },
        };
        let Some(job @ (side, hz, level, ms)) = job else { continue };
        // shortcut: looks for controllers again for every tone (a few ms), so a reconnect just works
        let devs = find();
        let begun = std::time::Instant::now();
        let mut keep = || match rx.try_recv() {
            Ok(j) => {
                next = Some(j);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        };
        if devs.is_empty() && !dry {
            // No Bluetooth DualSense: the cable's sound device, rear channels (refuses unless it is 4-channel).
            let _ = crate::audio::play(&if job == TAP { crate::audio::tap() } else { crate::audio::tone(side, hz, level, ms) }, &mut keep);
            continue;
        }
        let r = crate::audio::play_bt(&devs, &if job == TAP { crate::audio::tap_bt() } else { crate::audio::tone_bt(side, hz, level, ms) }, &mut keep);
        if dry {
            println!("dry haptic {side:?} {hz} Hz {level}% {ms} ms: {r:?} (sent, late) in {} ms", begun.elapsed().as_millis());
        }
    }
}

/// One line: each part None when the line does not name it.
#[derive(Debug, PartialEq, Default)]
struct Line {
    l2: Option<Effect>,
    r2: Option<Effect>,
    rumble: Option<[u8; 2]>,
    haptic: Option<(crate::audio::Side, u32, u8)>,
    ms: Option<u64>,
    stop: bool,
    /// Lights: Some(None) = `off` (given back; see-through in a `for` line, like a trigger's off).
    bar: Option<Option<[u8; 3]>>,
    players: Option<Option<u8>>,
    sound: Option<u8>,
}

/// None when anything on the line is unknown or out of range: a bad value never reaches a controller.
fn parse(line: &str) -> Option<Line> {
    let line = line.to_ascii_lowercase();
    let words: Vec<&str> = line.split_whitespace().flat_map(|w| PRESETS.iter().find(|p| p.0 == w).map_or(vec![w], |p| p.1.split(' ').collect())).collect();
    let mut out = Line::default();
    let (mut i, mut flash) = (0, false);
    while i < words.len() {
        let num = |k: usize, max: u64| words.get(i + k)?.parse::<u64>().ok().filter(|&n| n <= max);
        match words[i] {
            "stop" => (out.stop, i) = (true, i + 1),
            "rumble" => (out.rumble, i) = (Some([num(1, 255)? as u8, num(2, 255)? as u8]), i + 3),
            "haptic" => {
                let part: Vec<String> = words.get(i + 1..i + 4)?.iter().map(|s| s.to_string()).collect();
                let (side, hz, level, _) = crate::parse_haptic(&part)?; // same ranges as `winsense haptic`
                (out.haptic, i) = (Some((side, hz, level)), i + 4);
            }
            "light" | "players" if words.get(i + 1) == Some(&"off") => {
                match words[i] {
                    "light" => out.bar = Some(None),
                    _ => out.players = Some(None),
                }
                i += 2;
            }
            "light" | "flash" => {
                flash |= words[i] == "flash";
                (out.bar, i) = (Some(Some([num(1, 255)? as u8, num(2, 255)? as u8, num(3, 255)? as u8])), i + 4);
            }
            "players" => (out.players, i) = (Some(Some(num(1, 31)? as u8)), i + 2),
            "sound" => (out.sound, i) = (Some(crate::voice::named(words.get(i + 1)?)?), i + 2),
            "for" => (out.ms, i) = (Some(num(1, 5000).filter(|&ms| ms > 0)?), i + 2),
            "l2" | "r2" | "both" => {
                // Mods get the official effects only: no raw `rigid`, no power reduction.
                let n = match *words.get(i + 1)? {
                    "off" => 0,
                    "resist" => 2,
                    "click" | "vibrate" => 3,
                    _ => return None,
                };
                let part: Vec<String> = words.get(i..i + 2 + n)?.iter().map(|s| s.to_string()).collect();
                let (l2, r2, _) = crate::parse_trigger(&part)?; // range-checks through Effect::bytes
                (out.l2, out.r2) = (l2.or(out.l2), r2.or(out.r2));
                i += 2 + n;
            }
            _ => return None,
        }
    }
    if flash {
        out.ms = out.ms.or(Some(FLASH)); // `flash r g b` = that colour for a moment; a `for` on the line sets how long
    }
    if out.haptic.is_some() && out.ms.is_none() {
        return None; // a tone is never lasting: it needs `for`
    }
    Some(out)
}

/// One mod's connection until it disconnects. Its lasting state lives here; a line with `for`
/// plays on top of it for that long. Ends with this mod's effects off.
fn serve(id: u32, mut r: impl BufRead, out: &impl Fn(Out)) {
    let send = |m: Msg| out(Out::Worker(m));
    let (mut steady, mut owns) = (OFF, false);
    let (mut glow, mut lit): (crate::ds::Glow, bool) = ((None, None), false); // lasting lights, and whether this mod ever set any
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut r).take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break, // disconnected
            Ok(_) => {}
        }
        if buf.len() == MAX_LINE && buf.last() != Some(&b'\n') {
            let _ = r.skip_until(b'\n'); // too long: drop the whole line
            continue;
        }
        let Some(l) = parse(&String::from_utf8_lossy(&buf)) else { continue };
        if l.stop || l.haptic.is_some() {
            let tone = l.haptic.zip(l.ms).map(|((side, hz, level), ms)| (side, hz, level, ms as u32));
            out(Out::Haptic(tone));
            send(Msg::Tone(Duration::from_millis(tone.map_or(0, |t| t.3 as u64)))); // the motors rest meanwhile
        }
        if l.stop {
            steady = OFF;
            send(Msg::Pulse(OFF, Duration::ZERO)); // ends a running `for` at once
            if lit {
                glow = (None, None);
                send(Msg::Blink(glow, Duration::ZERO));
            }
        }
        if let Some(id) = l.sound {
            out(Out::Clip(id)); // once, whatever else the line says
        }
        let named = l.l2.is_some() || l.r2.is_some() || l.rumble.is_some();
        if l.bar.is_some() || l.players.is_some() {
            lit = true;
            match l.ms {
                Some(ms) => send(Msg::Blink((l.bar.flatten(), l.players.flatten()), Duration::from_millis(ms))),
                None => glow = (l.bar.unwrap_or(glow.0), l.players.unwrap_or(glow.1)),
            }
        }
        if lit && (l.stop || l.ms.is_none() && (l.bar.is_some() || l.players.is_some())) {
            send(Msg::Leds(id, glow));
        }
        let part = (l.l2.unwrap_or(Effect::Off), l.r2.unwrap_or(Effect::Off), l.rumble.unwrap_or([0, 0]));
        match l.ms {
            // A line with only a tone leaves a running pulse alone.
            Some(ms) if named => send(Msg::Pulse(part, Duration::from_millis(ms))),
            Some(_) => {}
            None => steady = (l.l2.unwrap_or(steady.0), l.r2.unwrap_or(steady.1), l.rumble.unwrap_or(steady.2)),
        }
        if l.stop || l.ms.is_none() && named {
            owns = true;
            send(Msg::Mod(id, Some(steady)));
        }
    }
    if owns {
        send(Msg::Mod(id, None));
    }
    if lit {
        send(Msg::Leds(id, (None, None)));
    }
}

/// Listen for mods on the pipe (a thread per connection), handing their lines to the worker.
pub fn start(tx: Sender<Msg>, dry: bool) {
    let (haptic, rx) = channel();
    std::thread::spawn(move || haptics(rx, dry));
    *TX.lock().unwrap() = Some((tx, haptic));
    DRY.store(dry, std::sync::atomic::Ordering::Relaxed);
    let name: Vec<u16> = if dry { DRY_PIPE } else { PIPE }.encode_utf16().chain([0]).collect();
    std::thread::spawn(move || {
        let mut first = FILE_FLAG_FIRST_PIPE_INSTANCE; // nobody else may have made our pipe first
        for id in 1.. {
            // Duplex so any open mode works; WinSense still never writes. Local clients only.
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
            let h = unsafe { CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | first, mode, PIPE_UNLIMITED_INSTANCES, 0, 4096, 0, null()) };
            if h == INVALID_HANDLE_VALUE {
                return; // the name is taken: no port this run
            }
            first = 0;
            if unsafe { ConnectNamedPipe(h, null_mut()) } == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(h) };
                continue;
            }
            let pipe = unsafe { File::from_raw_handle(h) };
            std::thread::spawn(move || {
                // A connected mod keeps "turn off when idle" away (src/power.rs), whatever it sends.
                crate::power::MODS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                serve(id, BufReader::new(pipe), &send);
                crate::power::MODS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });
}

/// "Buzz on notifications" (src/desk.rs): a double tap in the grips, like a phone, played as one
/// stream (`audio::tap_bt`, why: there). The motors rest meanwhile. Length 0 marks it: no mod line
/// can carry 0 ms.
/// shortcut: it cuts a mod's tone that happens to play; game sound as haptics picks up after it.
const TAP: Haptic = (crate::audio::Side::Both, 80, 100, 0);

fn taps(out: &impl Fn(Out)) {
    out(Out::Worker(Msg::Tone(Duration::from_millis(crate::audio::tap_ms() as u64))));
    out(Out::Haptic(Some(TAP)));
}

pub fn tap() {
    taps(&send);
}

/// Wake the haptic thread so it sees the "Game sound as haptics" toggle.
/// shortcut: sent as "silence", so it also cuts a mod's tone that happens to play at that click.
pub fn poke() {
    send(Out::Haptic(None));
}

/// For the tray's Quit: let go of the worker's channel so the worker can end (and switch off),
/// and of the haptic thread's, which cuts a running tone.
pub fn close() {
    *TX.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let vib = Effect::Vibrate { start: 1, strength: 8, hz: 25 };
        assert_eq!(parse("beam"), Some(Line { l2: Some(vib), r2: Some(vib), ..Default::default() }));
        assert_eq!(parse("Beam rumble 40 80\r\n"), Some(Line { l2: Some(vib), r2: Some(vib), rumble: Some([40, 80]), ..Default::default() }));
        assert_eq!(parse("hit"), Some(Line { rumble: Some([220, 160]), ms: Some(250), ..Default::default() }));
        let wall = Effect::Resist { start: 5, strength: 8 };
        assert_eq!(parse("r2 resist 5 8 l2 off"), Some(Line { l2: Some(Effect::Off), r2: Some(wall), ..Default::default() }));
        assert_eq!(parse("stop"), Some(Line { stop: true, ..Default::default() }));
        assert_eq!(parse(""), Some(Line::default()));
        use crate::audio::Side;
        assert_eq!(parse("haptic both 80 50 for 300"), Some(Line { haptic: Some((Side::Both, 80, 50)), ms: Some(300), ..Default::default() }));
        assert_eq!(parse("haptic left 1000 100 rumble 0 9 for 5000"), Some(Line { haptic: Some((Side::Left, 1000, 100)), rumble: Some([0, 9]), ms: Some(5000), ..Default::default() }));
        for bad in ["haptic both 80 50", "haptic both 0 50 for 9", "haptic both 80 101 for 9", "haptic up 80 50 for 9", "haptic both 80 for 9", "r2 rigid 0 255", "both resist 0 8 power 7", "r2 resist 10 8", "both vibrate 0 8 8", "rumble 256 0", "rumble 1", "hit for 0", "for 5001", "jump", "r2 click 3"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn light_and_sound_lines() {
        let red = Some(Some([255, 0, 0]));
        assert_eq!(parse("light 255 0 0"), Some(Line { bar: red, ..Default::default() }));
        assert_eq!(parse("Light 255 0 0 players 4 for 200"), Some(Line { bar: red, players: Some(Some(4)), ms: Some(200), ..Default::default() }));
        assert_eq!(parse("light off players off"), Some(Line { bar: Some(None), players: Some(None), ..Default::default() }));
        assert_eq!(parse("players 31 light off"), Some(Line { bar: Some(None), players: Some(Some(31)), ..Default::default() }));
        assert_eq!(parse("flash 255 0 0"), Some(Line { bar: red, ms: Some(FLASH), ..Default::default() }));
        assert_eq!(parse("flash 255 0 0 for 400"), parse("for 400 flash 255 0 0"), "a `for` on the line sets the flash's length");
        assert_eq!(parse("hit flash 255 0 0").map(|l| (l.rumble, l.ms)), Some((Some([220, 160]), Some(250))), "a flash on a hit lasts as long as the hit");
        assert_eq!(parse("sound ding"), Some(Line { sound: crate::voice::named("ding"), ..Default::default() }));
        assert_eq!(parse("sound levelup light 0 255 0").map(|l| l.sound), Some(crate::voice::named("levelup")));
        for name in ["ding", "ding2", "off", "listen", "pickup", "alert", "radio", "levelup"] {
            assert!(crate::voice::named(name).is_some(), "{name}");
        }
        for bad in ["light 256 0 0", "light 1 2", "light", "light on", "players 32", "players", "players -1", "flash off", "flash 1 2", "sound", "sound opening", "sound sorry", "sound 1"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_connection_ends_with_its_lights_given_back() {
        let run = |input: &str| {
            let sent = std::cell::RefCell::new(Vec::new());
            serve(7, input.as_bytes(), &|m| sent.borrow_mut().push(m));
            sent.into_inner()
        };
        let w = Out::Worker;
        let (blue, red, ms) = (Some([0, 0, 255]), Some([255, 0, 0]), Duration::from_millis);
        assert_eq!(run("light 0 0 255\nplayers 4\nflash 255 0 0\nlight off\n"), [
            w(Msg::Leds(7, (blue, None))),
            w(Msg::Leds(7, (blue, Some(4)))), // the players join, the colour stays
            w(Msg::Blink((red, None), ms(150))),
            w(Msg::Leds(7, (None, Some(4)))), // the bar is given back, the players stay
            w(Msg::Leds(7, (None, None))),    // disconnected
        ], "lights alone never touch the mod's triggers and rumble");
        assert_eq!(run("light 0 0 255\nstop"), [
            w(Msg::Leds(7, (blue, None))),
            Out::Haptic(None),
            w(Msg::Tone(Duration::ZERO)),
            w(Msg::Pulse(OFF, Duration::ZERO)),
            w(Msg::Blink((None, None), Duration::ZERO)),
            w(Msg::Leds(7, (None, None))),
            w(Msg::Mod(7, Some(OFF))),
            w(Msg::Mod(7, None)),
            w(Msg::Leds(7, (None, None))),
        ]);
        assert_eq!(run("light off players 4 for 300"), [w(Msg::Blink((None, Some(4)), ms(300))), w(Msg::Leds(7, (None, None)))], "`off` in a `for` line is see-through");
        let clip = |n| Out::Clip(crate::voice::named(n).unwrap());
        assert_eq!(run("sound pickup\nsound radio for 300\n\n"), [clip("pickup"), clip("radio")], "a clip plays once, owns nothing and writes nothing");
        assert_eq!(run("sound alert hit"), [clip("alert"), w(Msg::Pulse((Effect::Off, Effect::Off, [220, 160]), ms(250)))]);
    }

    #[test]
    fn a_notification_is_two_taps() {
        let sent = std::cell::RefCell::new(Vec::new());
        taps(&|m| sent.borrow_mut().push(m));
        assert_eq!(sent.into_inner(), [Out::Worker(Msg::Tone(Duration::from_millis(580))), Out::Haptic(Some(TAP))], "one stream; the motors rest for all of it");
        assert!(parse("haptic both 80 100 for 0").is_none(), "no mod line can look like the tap");
        // One stream: silence in front, then two pulses with silence between them.
        let f = crate::audio::tap_bt();
        assert_eq!(f.len(), 3 * 580);
        let loud = |r: std::ops::Range<usize>| f[r].iter().any(|s| s[0] != 0 && s[1] != 0);
        assert!(!loud(0..450) && loud(450..900) && !loud(905..1290) && loud(1290..1740));
        assert_eq!(crate::audio::tap().len(), 48 * 4 * 580, "the cable's version is as long");
    }

    #[test]
    fn a_connection_ends_with_its_effects_off() {
        let run = |input: &str| {
            let sent = std::cell::RefCell::new(Vec::new());
            serve(7, input.as_bytes(), &|m| sent.borrow_mut().push(m));
            sent.into_inner()
        };
        let vib = Effect::Vibrate { start: 1, strength: 8, hz: 25 };
        let w = Out::Worker;
        let hit = || w(Msg::Pulse((Effect::Off, Effect::Off, [220, 160]), Duration::from_millis(250)));
        assert_eq!(run("beam\nrumble 0 60\nhit\nstop\n"), [
            w(Msg::Mod(7, Some((vib, vib, [0, 0])))),
            w(Msg::Mod(7, Some((vib, vib, [0, 60])))), // rumble added, the beam stays
            hit(),
            Out::Haptic(None), // stop also cuts a running tone
            w(Msg::Tone(Duration::ZERO)),
            w(Msg::Pulse(OFF, Duration::ZERO)),
            w(Msg::Mod(7, Some(OFF))),
            w(Msg::Mod(7, None)),
        ]);
        assert_eq!(run("jump\nhit"), [hit()], "bad lines are skipped; a pulse alone owns nothing");
        let long = format!("{}beam\nbeam", "x".repeat(300));
        assert_eq!(run(&long), [w(Msg::Mod(7, Some((vib, vib, [0, 0])))), w(Msg::Mod(7, None))], "an over-long line is dropped whole");
        use crate::audio::Side;
        assert_eq!(run("hit\nhaptic right 80 50 for 300"), [hit(), Out::Haptic(Some((Side::Right, 80, 50, 300))), w(Msg::Tone(Duration::from_millis(300)))], "a tone alone leaves the hit's pulse in place (its motors rest during the tone)");
        assert_eq!(run("haptic both 80 50 rumble 90 0 for 300"), [Out::Haptic(Some((Side::Both, 80, 50, 300))), w(Msg::Tone(Duration::from_millis(300))), w(Msg::Pulse((Effect::Off, Effect::Off, [90, 0]), Duration::from_millis(300)))]);
    }
}
