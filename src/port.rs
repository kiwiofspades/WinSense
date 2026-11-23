//! The mod port (0.8.0): a game mod writes one short text line per game event into the
//! named pipe `\\.\pipe\WinSense`, and the tray's worker plays it (src/ui.rs; the tray stays the
//! only writer). Any language that can open a file can use it, Lua's `io.open` included, so a mod
//! needs no DLL and nothing is registered. WinSense writes back only to a client that said `hello`
//! (a client that only writes would otherwise fill the pipe). Protocol: docs/modding.md.
//! A mod's effects end when it disconnects (its process ends too) and when WinSense quits.

use crate::audio::Haptic;
use crate::ds::Effect;
use crate::ui::{Msg, OFF};
use std::io::{BufRead, BufReader, Read};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::CreateEventW;

pub const PIPE: &str = r"\\.\pipe\WinSense";
/// `tray --dry` listens here, so a self-test never takes the real tray's name.
const DRY_PIPE: &str = r"\\.\pipe\WinSense-dry";
const MAX_LINE: usize = 256;
/// The protocol's version, raised when a word changes meaning; new words show up in `CAPS`.
const PROTO: u32 = 1;
const CAPS: &str = "triggers,curve,rumble,haptic,stream,lights,players,sound,release,events,ack";
/// Lines waiting for a `hello` client that does not read: past this many, events are dropped.
const BACKLOG: usize = 64;
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
    /// `release` / `reclaim` (`set_hold`).
    Hold(u32, bool),
    /// A client stream opens, brings a block, or closes (`feed`).
    Stream(u32, Sv),
}

/// `tray --dry`: there is no voice thread, so a clip is printed.
static DRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn send(m: Out) {
    let m = match m {
        Out::Hold(id, on) => return set_hold(id, on),
        Out::Stream(id, s) => return feed(id, s),
        m => m,
    };
    if let Out::Clip(id) = m {
        if crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed) {
            return; // a game holds the controller
        }
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
            Out::Clip(_) | Out::Hold(..) | Out::Stream(..) => true,
        };
    }
}

/// The haptic thread's Bluetooth controllers, kept open between tones and streams: looked for
/// again when Windows says a device came or went (`ui::DEVICES`), after a write fails, and every
/// 2 s while there is none. A running stream ends only when the set of Bluetooth controllers changed.
struct Bt {
    api: Option<hidapi::HidApi>,
    pads: Vec<hidapi::HidDevice>,
    paths: Vec<std::ffi::CString>,
    seen: u32,
    look: std::time::Instant,
}

/// The Bluetooth DualSenses Windows lists now (after `refresh_devices`).
fn bt_infos(api: &hidapi::HidApi) -> impl Iterator<Item = &hidapi::DeviceInfo> {
    api.device_list().filter(|d| d.vendor_id() == crate::ds::VID && crate::ds::PIDS.contains(&d.product_id()) && matches!(d.bus_type(), hidapi::BusType::Bluetooth))
}

impl Bt {
    fn get(&mut self) -> &[hidapi::HidDevice] {
        let now = std::time::Instant::now();
        let changes = crate::ui::DEVICES.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(api) = &mut self.api
            && (changes != self.seen || self.pads.is_empty() && now >= self.look)
        {
            let _ = api.refresh_devices();
            let api = &*api;
            // Reopened even on the same paths: a controller switched off and on keeps its path, not its handle.
            self.pads = bt_infos(api).filter_map(|d| d.open_device(api).ok()).collect();
            self.paths = bt_infos(api).map(|d| d.path().to_owned()).collect();
            (self.seen, self.look) = (changes, now + Duration::from_secs(2));
        }
        &self.pads
    }

    /// Windows said a device came or went: true if the Bluetooth controllers are still the same
    /// ones (a USB stick or a virtual pad changes nothing here).
    fn same(&mut self) -> bool {
        let changes = crate::ui::DEVICES.load(std::sync::atomic::Ordering::Relaxed);
        let Some(api) = &mut self.api else { return true };
        let _ = api.refresh_devices();
        let same = bt_infos(api).map(|d| d.path()).eq(self.paths.iter().map(|p| p.as_c_str()));
        if same {
            self.seen = changes;
        }
        same
    }

    /// A write failed: the controller went away.
    fn broke(&mut self) {
        self.pads.clear();
        self.seen = u32::MAX;
    }
}

/// 32 stereo frames at 3 kHz: one report 0x32's worth.
type Block = [[i8; 2]; 32];
const SILENT: Block = [[0; 2]; 32];
/// Blocks a client stream may queue (85 ms); older ones are dropped past that.
const QUEUE: usize = 8;

/// A client's own samples (`haptic stream`): which connection owns it, what waits, and the counts
/// told to `hello` clients once a second.
struct Feed {
    owner: u32,
    blocks: std::collections::VecDeque<Block>,
    last: std::time::Instant,
    /// Reports sent with nothing queued since the last block: an underrun if the next block comes
    /// within 100 ms, a pause if later.
    empty: u32,
    underrun: u32,
    overflow: u32,
    /// Plays nowhere (a game holds the controller, or no Bluetooth DualSense): blocks are dropped
    /// as they come and not counted.
    idle: bool,
}

static FEED: Mutex<Option<Feed>> = Mutex::new(None);

/// Tell `hello` clients a stream's underruns and overflows, when there were any.
fn counts(underrun: u32, overflow: u32) {
    for (word, n) in [("underrun", underrun), ("overflow", overflow)] {
        if n > 0 {
            event(&format!("haptic {word} {n}"));
        }
    }
}

/// What a stream connection sends (src/port.rs `serve`).
#[derive(Debug, PartialEq)]
enum Sv {
    Open,
    Block(Block),
    Close,
}

fn feed(id: u32, s: Sv) {
    let mut f = FEED.lock().unwrap();
    match s {
        Sv::Open => {
            *f = Some(Feed { owner: id, blocks: Default::default(), last: std::time::Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(std::time::Instant::now), empty: 0, underrun: 0, overflow: 0, idle: false }); // the newest stream wins; silence before its first block is no underrun
            drop(f);
            send(Out::Haptic(Some(WAKE))); // wakes the haptic thread
        }
        Sv::Block(b) => {
            if let Some(f) = f.as_mut().filter(|f| f.owner == id && !f.idle) {
                if f.blocks.len() == QUEUE {
                    f.blocks.pop_front();
                    f.overflow += 1;
                }
                f.blocks.push_back(b);
                if f.last.elapsed() < Duration::from_millis(100) {
                    f.underrun += f.empty; // the client is sending, but too slowly
                }
                (f.last, f.empty) = (std::time::Instant::now(), 0);
            }
        }
        Sv::Close if f.as_ref().is_some_and(|f| f.owner == id) => {
            let last = f.take().unwrap();
            drop(f);
            counts(last.underrun, last.overflow);
        }
        Sv::Close => {}
    }
}

/// The client stream while it is open: one report per 10.67 ms, its blocks or silence when none
/// wait (so the controller never goes cold), with any tone added on top. Game sound as haptics
/// waits meanwhile. Ends with one silent report; returns early when the controllers change.
fn client(rx: &Receiver<Option<Haptic>>, first: Option<Option<Haptic>>, bt: &mut Bt, dry: bool, owner: u32) -> bool {
    use std::cell::RefCell;
    let mut changes = crate::ui::DEVICES.load(std::sync::atomic::Ordering::Relaxed);
    bt.get();
    let devs = std::mem::take(&mut bt.pads); // put back after: `keep` looks at the device list meanwhile
    let tone: RefCell<(Vec<[i8; 2]>, usize)> = RefCell::new((Vec::new(), 0));
    let told = std::cell::Cell::new(std::time::Instant::now());
    let mut alive = true;
    let mut job = first; // a tone that came just before the stream opened plays in it
    let mut keep = || {
        loop {
            match job.take().map(Ok).unwrap_or_else(|| rx.try_recv()) {
                // Warm: no lead in front of a tone (the tap keeps its own).
                Ok(Some(WAKE)) => {}
                Ok(Some(j)) => *tone.borrow_mut() = (if j == TAP { crate::audio::tap_bt() } else { crate::audio::tone_bt(j.0, j.1, j.2, j.3) }, 0),
                Ok(None) => *tone.borrow_mut() = (Vec::new(), 0), // `stop` cuts the tone, not the stream
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    alive = false;
                    return false;
                }
            }
        }
        // On until it closes, another connection takes it over, or the Bluetooth controllers change.
        if FEED.lock().unwrap().as_ref().map(|f| f.owner) != Some(owner) {
            return false;
        }
        let now = crate::ui::DEVICES.load(std::sync::atomic::Ordering::Relaxed);
        now == changes || {
            changes = now;
            bt.same()
        }
    };
    let frames = || {
        let mut f = FEED.lock().unwrap();
        let Some(f) = f.as_mut() else { return SILENT };
        let mut b = f.blocks.pop_front().unwrap_or_else(|| {
            f.empty += 1;
            SILENT
        });
        let (t, k) = &mut *tone.borrow_mut();
        for s in b.iter_mut() {
            if let Some(x) = t.get(*k) {
                *s = [s[0].saturating_add(x[0]), s[1].saturating_add(x[1])];
                *k += 1;
            }
        }
        if told.get().elapsed() >= Duration::from_secs(1) {
            told.set(std::time::Instant::now());
            counts(std::mem::take(&mut f.underrun), std::mem::take(&mut f.overflow));
        }
        b
    };
    let r = crate::audio::stream_bt(&devs, &mut keep, &frames);
    bt.pads = devs;
    if dry {
        println!("dry client stream: {r:?} (sent, late)");
    }
    if r.is_err() {
        bt.broke();
        std::thread::sleep(Duration::from_millis(500)); // a controller that keeps failing is not retried in a loop
    }
    alive
}

/// Plays tones in report 0x32 on its own handle, beside the worker's 0x31 writes (Windows queues
/// writes from both; the 10.67 ms pacing would stall the worker). A newer tone, `stop` or Quit
/// cuts the running one between reports. Bluetooth first (also when the cable is in too: the cable
/// is not felt then); with the cable alone, sound on its rear channels.
///
/// Since 0.12.0 it also plays game sound as haptics (src/sound.rs) while that toggle is on: between
/// tones it takes in what Windows plays every 10 ms and streams the low end while there is any. A
/// mod's tone comes first: it cuts the stream between reports, and the stream picks up after it.
/// A client stream (`haptic stream`) comes before both: tones are mixed into it.
fn haptics(rx: Receiver<Option<Haptic>>, dry: bool) {
    use std::sync::mpsc::RecvTimeoutError;
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED); // the capture is COM
    }
    let mut bt = Bt { api: if dry { None } else { hidapi::HidApi::new().ok() }, pads: Vec::new(), paths: Vec::new(), seen: u32::MAX, look: std::time::Instant::now() };
    let mut next = None;
    let stream = std::cell::RefCell::new(crate::sound::Stream::new(dry));
    let mut told = (0, false); // the stream and path last told (`haptic stream bt|none`)
    loop {
        let owner = FEED.lock().unwrap().as_ref().map(|f| f.owner);
        if let Some(owner) = owner {
            let bt_here = dry || !bt.get().is_empty();
            if (owner, bt_here) != told {
                told = (owner, bt_here);
                event(&format!("haptic stream {}", if bt_here { "bt" } else { "none" }));
            }
            let held = crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(f) = FEED.lock().unwrap().as_mut() {
                // A game holds the controller, or no Bluetooth DualSense: the blocks are dropped unplayed (and not counted).
                f.idle = held || !bt_here;
                if f.idle {
                    (f.empty, f.underrun, f.overflow) = (0, 0, 0);
                    f.blocks.clear();
                }
            }
            if held {
                // Tones and taps that come meanwhile are dropped, not played late after reclaim.
                next = None;
                loop {
                    match rx.try_recv() {
                        Ok(_) => {}
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            if bt_here {
                if !client(&rx, next.take(), &mut bt, dry, owner) {
                    return;
                }
                continue;
            }
            // No Bluetooth DualSense: tones go to the cable below.
        }
        let job = match next.take() {
            Some(j) => j,
            None if !crate::sound::ON.load(std::sync::atomic::Ordering::Relaxed) => {
                stream.borrow_mut().close();
                match rx.recv_timeout(if owner.is_some() { Duration::from_millis(100) } else { Duration::MAX }) {
                    Ok(j) => j,
                    Err(RecvTimeoutError::Timeout) => continue, // a stream waits for a Bluetooth controller: picked up soon after it comes
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(j) => j,
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    if !stream.borrow_mut().poll() || crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed) {
                        continue; // silence, or a game holds the controller: nothing is written
                    }
                    if bt.get().is_empty() && !dry {
                        continue; // no Bluetooth DualSense
                    }
                    let mut keep = || match rx.try_recv() {
                        Ok(j) => {
                            next = Some(j);
                            false
                        }
                        Err(TryRecvError::Empty) => stream.borrow_mut().poll() && FEED.lock().unwrap().is_none(), // a client stream comes first
                        Err(TryRecvError::Disconnected) => false,
                    };
                    let r = crate::audio::stream_bt(bt.get(), &mut keep, &|| stream.borrow_mut().feed.take());
                    if r.is_err() {
                        bt.broke();
                    }
                    if dry {
                        println!("dry sound: streamed {r:?} (sent, late)");
                    }
                    continue;
                }
            },
        };
        let Some(job @ (side, hz, level, ms)) = job.filter(|&j| j != WAKE) else { continue };
        if crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed) {
            continue; // a game holds the controller: the tone is dropped, not queued
        }
        let begun = std::time::Instant::now();
        let cable = bt.get().is_empty() && !dry;
        // Cut by a newer tone, `stop`, a game taking the controller, or (over Bluetooth) a client stream opening.
        let mut keep = || match rx.try_recv() {
            Ok(j) => {
                next = Some(j);
                false
            }
            Err(TryRecvError::Empty) => !crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed) && (cable || FEED.lock().unwrap().is_none()),
            Err(TryRecvError::Disconnected) => false,
        };
        if cable {
            // No Bluetooth DualSense: the cable's sound device, rear channels (refuses unless it is 4-channel).
            let _ = crate::audio::play(&if job == TAP { crate::audio::tap() } else { crate::audio::tone(side, hz, level, ms) }, &mut keep);
            continue;
        }
        // A new stream loses its first 150-200 ms, so a tone starts after silence (the tap has its own).
        let frames = if job == TAP { crate::audio::tap_bt() } else { [vec![[0; 2]; 3 * crate::audio::LEAD as usize], crate::audio::tone_bt(side, hz, level, ms)].concat() };
        let r = crate::audio::play_bt(bt.get(), &frames, &mut keep);
        if r.is_err() {
            bt.broke();
        }
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
    /// `release` (true) / `reclaim` (false): hand the controller to a game, or take it back.
    hold: Option<bool>,
    /// `hello` (false) / `hello ack` (true): write events (and an `ok` per line) back.
    hello: Option<bool>,
    /// `haptic stream`: the rest of the connection is 64-byte blocks of samples.
    stream: bool,
}

/// Every word that can start a part, so a refusal can tell "unknown" from "bad".
const WORDS: [&str; 14] = ["stop", "release", "reclaim", "hello", "rumble", "haptic", "light", "players", "flash", "sound", "for", "l2", "r2", "both"];

/// Err (why) when anything on the line is unknown or out of range: a bad value never reaches a
/// controller. The reason goes to `hello` clients as `err <n> <why>`.
fn parse(line: &str) -> Result<Line, String> {
    let line = line.to_ascii_lowercase();
    let words: Vec<&str> = line.split_whitespace().flat_map(|w| PRESETS.iter().find(|p| p.0 == w).map_or(vec![w], |p| p.1.split(' ').collect())).collect();
    let mut out = Line::default();
    let (mut i, mut flash) = (0, false);
    while i < words.len() {
        let w = words[i];
        i = part(&words, i, &mut out, &mut flash).ok_or_else(|| format!("{} {w}", if WORDS.contains(&w) { "bad" } else { "unknown" }))?;
    }
    if flash {
        out.ms = out.ms.or(Some(FLASH)); // `flash r g b` = that colour for a moment; a `for` on the line sets how long
    }
    if out.haptic.is_some() && out.ms.is_none() {
        return Err("haptic needs for".into()); // a tone is never lasting
    }
    Ok(out)
}

/// The part that starts at word `i`, into `out`; the index after it, None when it is unknown or out of range.
fn part(words: &[&str], i: usize, out: &mut Line, flash: &mut bool) -> Option<usize> {
    let num = |k: usize, max: u64| words.get(i + k)?.parse::<u64>().ok().filter(|&n| n <= max);
    Some(match words[i] {
        "stop" => {
            out.stop = true;
            i + 1
        }
        "release" | "reclaim" => {
            out.hold = Some(words[i] == "release");
            i + 1
        }
        "hello" => {
            let ack = words.get(i + 1) == Some(&"ack");
            out.hello = Some(ack);
            i + 1 + ack as usize
        }
        "haptic" if words.get(i + 1) == Some(&"stream") && i + 2 == words.len() => {
            out.stream = true; // binary follows, so it ends the line
            i + 2
        }
        "rumble" => {
            out.rumble = Some([num(1, 255)? as u8, num(2, 255)? as u8]);
            i + 3
        }
        "haptic" => {
            let part: Vec<String> = words.get(i + 1..i + 4)?.iter().map(|s| s.to_string()).collect();
            let (side, hz, level, _) = crate::parse_haptic(&part)?; // same ranges as `winsense haptic`
            out.haptic = Some((side, hz, level));
            i + 4
        }
        "light" | "players" if words.get(i + 1) == Some(&"off") => {
            match words[i] {
                "light" => out.bar = Some(None),
                _ => out.players = Some(None),
            }
            i + 2
        }
        "light" | "flash" => {
            *flash |= words[i] == "flash";
            out.bar = Some(Some([num(1, 255)? as u8, num(2, 255)? as u8, num(3, 255)? as u8]));
            i + 4
        }
        "players" => {
            out.players = Some(Some(num(1, 31)? as u8));
            i + 2
        }
        "sound" => {
            out.sound = Some(crate::voice::named(words.get(i + 1)?)?);
            i + 2
        }
        "for" => {
            out.ms = Some(num(1, 5000).filter(|&ms| ms > 0)?);
            i + 2
        }
        "l2" | "r2" | "both" => {
            // Mods get the official effects only: no raw `rigid`, no power reduction.
            let n = match *words.get(i + 1)? {
                "off" => 0,
                "resist" => 2,
                "click" | "vibrate" => 3,
                // the start and one strength or more: every number that follows
                "curve" => words[i + 2..].iter().take_while(|w| w.parse::<u8>().is_ok()).count(),
                _ => return None,
            };
            let part: Vec<String> = words.get(i..i + 2 + n)?.iter().map(|s| s.to_string()).collect();
            let (l2, r2, _) = crate::parse_trigger(&part)?; // range-checks through Effect::bytes
            (out.l2, out.r2) = (l2.or(out.l2), r2.or(out.r2));
            i + 2 + n
        }
        _ => return None,
    })
}

/// One mod's connection until it disconnects. Its lasting state lives here; a line with `for`
/// plays on top of it for that long. Ends with this mod's effects off. After `hello`, `talk`
/// writes lines back to this client (Some); None at the end.
fn serve(id: u32, mut r: impl BufRead, out: &impl Fn(Out), talk: &impl Fn(Option<String>)) {
    let send = |m: Msg| out(Out::Worker(m));
    // When this connection's pulse, blink and tone end (`TIMED` order), to cut them if it goes first.
    let mut ends: [Option<std::time::Instant>; 3] = [None; 3];
    // Recorded and sent under one lock, so the newest starter and what runs never disagree.
    let timed = |ends: &mut [Option<std::time::Instant>; 3], k: usize, ms: u64, go: &dyn Fn()| {
        let mut t = TIMED.lock().unwrap();
        ends[k] = Some(std::time::Instant::now() + Duration::from_millis(ms));
        t[k] = id;
        go();
    };
    let (mut steady, mut owns) = (OFF, false);
    let (mut glow, mut lit): (crate::ds::Glow, bool) = ((None, None), false); // lasting lights, and whether this mod ever set any
    let mut holds = false; // this connection holds the controller for a game (`release`)
    let (mut hello, mut ack, mut n) = (false, false, 0u32); // writes back only after `hello`; line count
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut r).take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break, // disconnected
            Ok(_) => {}
        }
        n += 1;
        let parsed = if buf.len() == MAX_LINE && buf.last() != Some(&b'\n') {
            let _ = r.skip_until(b'\n'); // too long: drop the whole line
            Err("too long".to_string())
        } else {
            parse(&String::from_utf8_lossy(&buf))
        };
        let l = match parsed {
            Ok(l) => l,
            Err(why) => {
                if hello {
                    talk(Some(format!("err {n} {why}")));
                }
                continue;
            }
        };
        match l.hello {
            Some(a) if !hello => {
                (hello, ack) = (true, a);
                talk(Some(format!("winsense {} proto {PROTO} caps {CAPS}", env!("CARGO_PKG_VERSION"))));
                let pads = crate::ui::PADS.lock().unwrap().clone();
                pads.into_iter().for_each(|p| talk(Some(p)));
            }
            _ if ack => talk(Some(format!("ok {n}"))),
            _ => {}
        }
        if let Some(on) = l.hold.filter(|&on| on != holds) {
            holds = on;
            out(Out::Hold(id, on));
        }
        if l.stop || l.haptic.is_some() && !crate::ui::RELEASED.load(std::sync::atomic::Ordering::Relaxed) {
            let tone = l.haptic.zip(l.ms).map(|((side, hz, level), ms)| (side, hz, level, ms as u32));
            // shortcut: the motors rest for the lead-in too, also where none is played (a warm client stream, the cable): 150 ms more rest.
            let ms = tone.map_or(0, |t| (t.3 + crate::audio::LEAD) as u64);
            timed(&mut ends, 2, ms, &|| {
                out(Out::Haptic(tone));
                send(Msg::Tone(Duration::from_millis(ms))); // the motors rest meanwhile
            });
        }
        if l.stop {
            (steady, ends[0], ends[1]) = (OFF, None, None); // `stop` cuts them itself (a tone on its line is recorded above)
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
                Some(ms) => {
                    timed(&mut ends, 1, ms, &|| send(Msg::Blink((l.bar.flatten(), l.players.flatten()), Duration::from_millis(ms))));
                }
                None => glow = (l.bar.unwrap_or(glow.0), l.players.unwrap_or(glow.1)),
            }
        }
        if lit && (l.stop || l.ms.is_none() && (l.bar.is_some() || l.players.is_some())) {
            send(Msg::Leds(id, glow));
        }
        let part = (l.l2.unwrap_or(Effect::Off), l.r2.unwrap_or(Effect::Off), l.rumble.unwrap_or([0, 0]));
        match l.ms {
            // A line with only a tone leaves a running pulse alone.
            Some(ms) if named => {
                timed(&mut ends, 0, ms, &|| send(Msg::Pulse(part, Duration::from_millis(ms))));
            }
            Some(_) => {}
            None => steady = (l.l2.unwrap_or(steady.0), l.r2.unwrap_or(steady.1), l.rumble.unwrap_or(steady.2)),
        }
        if l.stop || l.ms.is_none() && named {
            owns = true;
            send(Msg::Mod(id, Some(steady)));
        }
        if l.stream {
            out(Out::Stream(id, Sv::Open));
            let mut b = [0u8; 64];
            while r.read_exact(&mut b).is_ok() {
                out(Out::Stream(id, Sv::Block(std::array::from_fn(|k| [b[2 * k] as i8, b[2 * k + 1] as i8]))));
            }
            out(Out::Stream(id, Sv::Close));
            break;
        }
    }
    // Gone: what it started and still runs ends now, unless another connection started a newer one.
    let now = std::time::Instant::now();
    let t = TIMED.lock().unwrap();
    let mine = |k: usize| ends[k].is_some_and(|e| e > now) && t[k] == id;
    if mine(0) {
        send(Msg::Pulse(OFF, Duration::ZERO));
    }
    if mine(1) {
        send(Msg::Blink((None, None), Duration::ZERO));
    }
    if mine(2) {
        out(Out::Haptic(None));
        send(Msg::Tone(Duration::ZERO));
    }
    drop(t);
    if owns {
        send(Msg::Mod(id, None));
    }
    if lit {
        send(Msg::Leds(id, (None, None)));
    }
    if holds {
        out(Out::Hold(id, false)); // a game that quits or crashes gives the controller back
    }
    talk(None);
}

/// One client's pipe, opened overlapped, so its reader and its writer thread can use it at once (a
/// plain handle lets a waiting read hold up every write).
struct Pipe {
    h: HANDLE,
    /// One event per direction (read, write); each direction has one thread.
    ev: [HANDLE; 2],
}

// One reader and one writer thread, each with its own OVERLAPPED and event.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    fn new(h: HANDLE) -> Pipe {
        Pipe { h, ev: [0; 2].map(|_| unsafe { CreateEventW(null(), 1, 0, null()) }) }
    }

    /// Wait for a client; false when the connection failed.
    fn connect(&self) -> bool {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.ev[0];
        let mut n = 0;
        unsafe {
            ConnectNamedPipe(self.h, &mut ov) != 0
                || match GetLastError() {
                    ERROR_PIPE_CONNECTED => true,
                    ERROR_IO_PENDING => GetOverlappedResult(self.h, &ov, &mut n, 1) != 0,
                    _ => false,
                }
        }
    }

    /// One read or write, waited for; a closed pipe reads as the end (Ok(0)).
    fn io(&self, write: bool, buf: *mut u8, len: usize) -> std::io::Result<usize> {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.ev[write as usize];
        let (len, mut n) = (len.min(u32::MAX as usize) as u32, 0u32);
        unsafe {
            let ok = if write { WriteFile(self.h, buf, len, null_mut(), &mut ov) } else { ReadFile(self.h, buf, len, null_mut(), &mut ov) };
            if (ok != 0 || GetLastError() == ERROR_IO_PENDING) && GetOverlappedResult(self.h, &ov, &mut n, 1) != 0 {
                return Ok(n as usize);
            }
        }
        match std::io::Error::last_os_error() {
            e if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            e => Err(e),
        }
    }

    fn write_all(&self, mut b: &[u8]) -> bool {
        while !b.is_empty() {
            match self.io(true, b.as_ptr() as *mut u8, b.len()) {
                Ok(0) | Err(_) => return false,
                Ok(k) => b = &b[k..],
            }
        }
        true
    }
}

impl Read for &Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.io(false, buf.as_mut_ptr(), buf.len())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.h);
            for &e in &self.ev {
                CloseHandle(e);
            }
        }
    }
}

/// The connection that started the newest pulse, blink and tone (`serve`).
static TIMED: Mutex<[u32; 3]> = Mutex::new([0; 3]);

/// Why the pipe could not be made, for the tray to show once (`trouble`).
static TROUBLE: Mutex<Option<String>> = Mutex::new(None);

/// For the tray: a reason to show, once, when the pipe could not be made (WinSense keeps trying).
pub fn trouble() -> Option<String> {
    TROUBLE.lock().unwrap().take()
}

/// Clients that said `hello`: their id and the queue of their writer thread.
static CLIENTS: Mutex<Vec<(u32, SyncSender<String>)>> = Mutex::new(Vec::new());

/// Write a line to every `hello` client (src/ui.rs: `pad <id> connected|gone|full`, `writefail`).
/// Never waits: a client that does not read loses lines past `BACKLOG`.
pub fn event(line: &str) {
    CLIENTS.lock().unwrap().iter().for_each(|(_, tx)| drop(tx.try_send(line.to_string())));
}

/// `serve`'s `talk` for a real client: the first line starts its writer thread.
fn talk(id: u32, pipe: &Arc<Pipe>, line: Option<String>) {
    let mut c = CLIENTS.lock().unwrap();
    let Some(line) = line else {
        c.retain(|x| x.0 != id); // the writer thread ends with its queue
        return;
    };
    if !c.iter().any(|x| x.0 == id) {
        let (tx, rx) = sync_channel::<String>(BACKLOG);
        let pipe = pipe.clone();
        std::thread::spawn(move || {
            for line in rx {
                if !pipe.write_all(format!("{line}\n").as_bytes()) {
                    break;
                }
            }
        });
        c.push((id, tx));
    }
    let _ = c.iter().find(|x| x.0 == id).map(|x| x.1.try_send(line));
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
        let mut wait = 0; // seconds before the next try after a failure
        for id in 1.. {
            // Duplex: any open mode works, and `hello` clients get lines back. Local clients only.
            let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
            let h = unsafe { CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first, mode, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, null()) };
            if h == INVALID_HANDLE_VALUE {
                let e = unsafe { GetLastError() };
                if wait == 0 {
                    let why = if first != 0 && e == 5 { "another program holds its name" } else { "Windows refused it" };
                    let text = format!("The mod port is not available ({why}, error {e}). WinSense keeps trying.");
                    if dry {
                        println!("dry port: {text}");
                    }
                    *TROUBLE.lock().unwrap() = Some(text);
                }
                wait = (wait * 2).clamp(1, 30);
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
            (first, wait) = (0, 0);
            let pipe = Arc::new(Pipe::new(h));
            if !pipe.connect() {
                continue;
            }
            std::thread::spawn(move || {
                // A connected mod keeps "turn off when idle" away (src/power.rs), whatever it sends.
                crate::power::MODS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                serve(id, BufReader::new(&*pipe), &send, &|line| talk(id, &pipe, line));
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
/// Wakes the haptic thread for a client stream and cuts nothing: no line can carry 0 Hz.
const WAKE: Haptic = (crate::audio::Side::Both, 0, 0, 0);

fn taps(out: &impl Fn(Out)) {
    TIMED.lock().unwrap()[2] = 0; // nobody's: a client that leaves does not cut it
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

/// The worker looks for controllers again (src/ui.rs `linked`).
pub fn rescan() {
    send(Out::Worker(Msg::Devices));
}

/// The tray's "Hands off the controller while a game is in front", once a second: tells the
/// worker when that changes (id 0; connections count from 1).
pub fn hold(on: bool) {
    static LAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LAST.swap(on, std::sync::atomic::Ordering::Relaxed) != on {
        set_hold(0, on);
    }
}

/// Who holds the controller for a game: connection ids, 0 = the tray. `ui::RELEASED` follows it
/// here, on the holder's thread, before the worker hears of it.
fn set_hold(id: u32, on: bool) {
    static HOLDERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let mut h = HOLDERS.lock().unwrap();
    h.retain(|&x| x != id);
    if on {
        h.push(id);
    }
    crate::ui::RELEASED.store(!h.is_empty(), std::sync::atomic::Ordering::Relaxed);
    send(Out::Worker(Msg::Hold(id, on)));
}

/// For the tray's Quit: let go of the worker's channel so the worker can end (and switch off),
/// and of the haptic thread's, which cuts a running tone.
pub fn close() {
    *TX.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line's parts, None when refused (why: `refusals`).
    fn parse(line: &str) -> Option<Line> {
        super::parse(line).ok()
    }

    #[test]
    fn lines() {
        let vib = Effect::Vibrate { start: 1, strength: 8, hz: 25 };
        assert_eq!(parse("beam"), Some(Line { l2: Some(vib), r2: Some(vib), ..Default::default() }));
        assert_eq!(parse("Beam rumble 40 80\r\n"), Some(Line { l2: Some(vib), r2: Some(vib), rumble: Some([40, 80]), ..Default::default() }));
        assert_eq!(parse("hit"), Some(Line { rumble: Some([220, 160]), ms: Some(250), ..Default::default() }));
        let wall = Effect::Resist { start: 5, strength: 8 };
        let pull = Effect::curve(3, &[1, 1, 5]);
        assert_eq!(parse("r2 curve 3 1 1 5 rumble 9 9").map(|l| (l.r2, l.rumble)), Some((pull, Some([9, 9]))));
        assert_eq!(parse("both curve 0 4").map(|l| (l.l2, l.r2)), Some((Effect::curve(0, &[4]), Effect::curve(0, &[4]))));
        for bad in ["r2 curve", "r2 curve 3", "r2 curve 3 9", "r2 curve 9 1 1", "r2 curve 3 1 for"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
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
        let _one = SERVES.lock().unwrap();
        let run = |input: &str| {
            let sent = std::cell::RefCell::new(Vec::new());
            serve(7, input.as_bytes(), &|m| sent.borrow_mut().push(m), &|_| {});
            sent.into_inner()
        };
        let w = Out::Worker;
        let (blue, red, ms) = (Some([0, 0, 255]), Some([255, 0, 0]), Duration::from_millis);
        assert_eq!(run("light 0 0 255\nplayers 4\nflash 255 0 0\nlight off\n"), [
            w(Msg::Leds(7, (blue, None))),
            w(Msg::Leds(7, (blue, Some(4)))), // the players join, the colour stays
            w(Msg::Blink((red, None), ms(150))),
            w(Msg::Leds(7, (None, Some(4)))), // the bar is given back, the players stay
            w(Msg::Blink((None, None), Duration::ZERO)), // disconnected: its flash ends with it
            w(Msg::Leds(7, (None, None))),
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
        assert_eq!(run("light off players 4 for 300"), [w(Msg::Blink((None, Some(4)), ms(300))), w(Msg::Blink((None, None), Duration::ZERO)), w(Msg::Leds(7, (None, None)))], "`off` in a `for` line is see-through");
        let clip = |n| Out::Clip(crate::voice::named(n).unwrap());
        assert_eq!(run("sound pickup\nsound radio for 300\n\n"), [clip("pickup"), clip("radio")], "a clip plays once, owns nothing and writes nothing");
        assert_eq!(run("sound alert hit"), [clip("alert"), w(Msg::Pulse((Effect::Off, Effect::Off, [220, 160]), ms(250))), w(Msg::Pulse(OFF, Duration::ZERO))]);
    }

    /// After `haptic stream` the connection is 64-byte blocks until it closes; a part block is dropped.
    #[test]
    fn a_stream_connection_sends_blocks() {
        let sent = std::cell::RefCell::new(Vec::new());
        let mut input = b"r2 off haptic stream\n".to_vec();
        input.extend((0..138u32).map(|k| (k % 64) as u8 | 0x80 * (k >= 64) as u8));
        serve(3, &input[..], &|m| sent.borrow_mut().push(m), &|_| {});
        let s = sent.into_inner();
        let block = |hi: u8| Sv::Block(std::array::from_fn(|k| [(2 * k as u8 | hi) as i8, (2 * k as u8 + 1 | hi) as i8]));
        assert_eq!(&s[1..], [Out::Stream(3, Sv::Open), Out::Stream(3, block(0)), Out::Stream(3, block(0x80)), Out::Stream(3, Sv::Close), Out::Worker(Msg::Mod(3, None))]);
        assert!(super::parse("haptic stream beam").is_err(), "nothing may follow it on the line");
    }

    /// Tests that start timed effects (`TIMED`) run one at a time.
    static SERVES: Mutex<()> = Mutex::new(());

    /// Blocks for a stream that plays nowhere are dropped as they come and never counted.
    #[test]
    fn blocks_that_play_nowhere_are_not_counted() {
        let _one = SERVES.lock().unwrap();
        feed(5, Sv::Open);
        let state = || FEED.lock().unwrap().as_ref().map(|f| (f.blocks.len(), f.overflow));
        FEED.lock().unwrap().as_mut().unwrap().idle = true;
        (0..QUEUE + 3).for_each(|_| feed(5, Sv::Block(SILENT)));
        assert_eq!(state(), Some((0, 0)));
        FEED.lock().unwrap().as_mut().unwrap().idle = false;
        (0..QUEUE + 1).for_each(|_| feed(5, Sv::Block(SILENT)));
        assert_eq!(state(), Some((QUEUE, 1)), "playing: the oldest goes and is counted");
        feed(5, Sv::Close);
        assert_eq!(state(), None);
    }

    #[test]
    fn refusals_say_why() {
        assert_eq!(super::parse("jump"), Err("unknown jump".into()));
        assert_eq!(super::parse("r2 resist 10 8"), Err("bad r2".into()));
        assert_eq!(super::parse("rumble 1"), Err("bad rumble".into()));
        assert_eq!(super::parse("haptic both 80 50"), Err("haptic needs for".into()));
        assert_eq!(super::parse("hello ack").map(|l| l.hello), Ok(Some(true)));
        assert_eq!(super::parse("hello beam").map(|l| (l.hello, l.l2.is_some())), Ok((Some(false), true)));
    }

    /// Nothing is written back before `hello`; after it the greeting, then `err` and (with `ack`) `ok`.
    #[test]
    fn hello_clients_hear_back() {
        let run = |input: &str| {
            let said = std::cell::RefCell::new(Vec::new());
            serve(7, input.as_bytes(), &|_| {}, &|l: Option<String>| said.borrow_mut().push(l.unwrap_or("(end)".into())));
            said.into_inner()
        };
        let hi = format!("winsense {} proto {PROTO} caps {CAPS}", env!("CARGO_PKG_VERSION"));
        assert_eq!(run("jump
beam
"), ["(end)"], "no hello: nothing written");
        let long = "x".repeat(300);
        assert_eq!(run(&format!("jump
hello
beam
jump
{long}
haptic both 80 50
hello
")), [&hi, "err 4 unknown jump", "err 5 too long", "err 6 haptic needs for", "(end)"]);
        assert_eq!(run("hello ack
beam
r2 off
nope
stop"), [&hi, "ok 2", "ok 3", "err 4 unknown nope", "ok 5", "(end)"]);
    }

    #[test]
    fn a_notification_is_two_taps() {
        let _one = SERVES.lock().unwrap(); // taps() writes TIMED
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
        let _one = SERVES.lock().unwrap();
        let run = |input: &str| {
            let sent = std::cell::RefCell::new(Vec::new());
            serve(7, input.as_bytes(), &|m| sent.borrow_mut().push(m), &|_| {});
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
        assert_eq!(run("jump\nhit"), [hit(), w(Msg::Pulse(OFF, Duration::ZERO))], "bad lines are skipped; a pulse alone owns nothing, and ends when its client goes");
        let long = format!("{}beam\nbeam", "x".repeat(300));
        assert_eq!(run(&long), [w(Msg::Mod(7, Some((vib, vib, [0, 0])))), w(Msg::Mod(7, None))], "an over-long line is dropped whole");
        use crate::audio::Side;
        assert_eq!(run("hit\nhaptic right 80 50 for 300"), [hit(), Out::Haptic(Some((Side::Right, 80, 50, 300))), w(Msg::Tone(Duration::from_millis(450))), w(Msg::Pulse(OFF, Duration::ZERO)), Out::Haptic(None), w(Msg::Tone(Duration::ZERO))], "a tone alone leaves the hit's pulse in place (its motors rest during the tone)");
        assert_eq!(run("release\nrelease\nreclaim\nrelease hit"), [Out::Hold(7, true), Out::Hold(7, false), Out::Hold(7, true), hit(), w(Msg::Pulse(OFF, Duration::ZERO)), Out::Hold(7, false)], "a client holds once, and gives the controller back when it goes");
        assert_eq!(parse("Release"), Some(Line { hold: Some(true), ..Default::default() }));
        assert_eq!(parse("reclaim beam").map(|l| l.hold), Some(Some(false)));
        assert_eq!(run("haptic both 80 50 rumble 90 0 for 300"), [Out::Haptic(Some((Side::Both, 80, 50, 300))), w(Msg::Tone(Duration::from_millis(450))), w(Msg::Pulse((Effect::Off, Effect::Off, [90, 0]), Duration::from_millis(300))), w(Msg::Pulse(OFF, Duration::ZERO)), Out::Haptic(None), w(Msg::Tone(Duration::ZERO))]);
        // Another connection's newer pulse (it started while this one ran) is not cut when this one goes.
        let sent = std::cell::RefCell::new(Vec::new());
        let newer = |m: Out| {
            if matches!(m, Out::Clip(_)) {
                TIMED.lock().unwrap()[0] = 99; // the other connection's pulse starts here
            }
            sent.borrow_mut().push(m)
        };
        serve(7, "hit\nsound ding".as_bytes(), &newer, &|_| {});
        assert_eq!(sent.into_inner(), [hit(), Out::Clip(crate::voice::named("ding").unwrap())]);
        assert_eq!(run("stop haptic right 80 50 for 3000").iter().filter(|m| **m == Out::Haptic(None)).count(), 1, "a tone on a `stop` line is cut when its client goes");
    }
}
