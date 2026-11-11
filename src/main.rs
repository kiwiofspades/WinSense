//! `winsense` with no arguments: the tray program (src/tray.rs). With a command: probe and test commands.
//! Built as a Windows (not console) program so the tray opens no console window; a command
//! attaches to the console it was started from (see `console`).
//! `look` is listen-only: it never writes to a controller and never asks it
//! for a feature report (over Bluetooth that request alone switches the
//! controller from the short report to the full one). `light`, `trigger` and `rumble` write
//! (over Bluetooth that also switches the controller to the full report).

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod couch;
mod desk;
mod ds;
mod mouse;
mod osk;
mod pad;
mod port;
mod power;
mod sound;
mod tray;
mod ui;
mod update;
mod voice;

use hidapi::{BusType, HidApi};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Print to the console the command was started from. With output redirected (pipes, files)
/// the handles are already there and stay; only a bare console start needs attaching.
// shortcut: PowerShell and cmd do not wait for a Windows program, so the prompt comes back first; pipe to Out-Host (or `| more`) to wait.
fn console() {
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::Console::*;
    unsafe {
        if GetStdHandle(STD_OUTPUT_HANDLE).is_null() && AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let out = CreateFileW(windows_sys::w!("CONOUT$"), windows_sys::Win32::Foundation::GENERIC_READ | windows_sys::Win32::Foundation::GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            SetStdHandle(STD_OUTPUT_HANDLE, out);
            SetStdHandle(STD_ERROR_HANDLE, out);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };
    let secs = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let mut args: Vec<String> = std::env::args().skip(2).collect();
    // Started by an update: wait for the old tray to end first (src/update.rs).
    if cmd == "tray" && let Some(i) = args.iter().position(|a| a == "--after") && i + 1 < args.len() {
        update::after(&args[i + 1]);
        args.drain(i..i + 2);
    }
    if cmd.is_empty() || cmd == "tray" && (args.is_empty() || args == ["--dry"]) {
        tray::run(args == ["--dry"]);
        return Ok(());
    }
    console();
    // One process owns the controller: no second mouse or writer while the tray (or another of these) runs.
    // A --dry run never writes or moves the mouse, so it may run next to the tray (self-tests).
    let _owner = if ["ui", "mouse", "light", "trigger", "rumble", "haptic", "mic", "speak"].contains(&cmd.as_str()) && !args.iter().any(|a| a == "--dry") {
        let Some(lock) = tray::claim() else {
            eprintln!("WinSense is already running (the tray icon, or another ui/mouse/write command). Quit it first; `look`, `full` and `pad` work alongside.");
            std::process::exit(1);
        };
        Some(lock)
    } else {
        None
    };
    let lights = if cmd == "light" { parse_light(&args) } else { None };
    let trig = if cmd == "trigger" { parse_trigger(&args) } else { None };
    let rumble = if cmd == "rumble" { parse_rumble(&args) } else { None };
    if cmd == "haptic" && let Some((side, hz, level, ms)) = parse_haptic(&args) {
        // Bluetooth first: with the cable and Bluetooth both up, the cable's sound is not felt (measured).
        let api = HidApi::new()?;
        let bt: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()) && matches!(d.bus_type(), BusType::Bluetooth))
            .collect();
        if !bt.is_empty() {
            let frames = audio::tone_bt(side, hz, level, ms);
            println!("[BT] haptic {side:?} {hz} Hz at {level}% for {ms} ms in report 0x32 to {} controller(s)", bt.len());
            let devs: Vec<_> = bt.iter().filter_map(|info| info.open_device(&api).map_err(|e| println!("[BT] {e}")).ok()).collect();
            match audio::play_bt(&devs, &frames, &mut || true) {
                Ok((sent, late)) => println!("[BT] done, {sent} reports, {late} dropped as late"),
                Err(e) => println!("[BT] {e}"),
            }
            return Ok(());
        }
        println!("haptic {side:?} {hz} Hz at {level}% for {ms} ms on the DualSense's rear channels (its speaker channels stay silent)");
        match audio::play(&audio::tone(side, hz, level, ms), &mut || true) {
            Ok(()) => println!("done"),
            Err(e) => {
                println!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if cmd == "mouse" && args.iter().all(|a| a == "--dry" || a.parse::<u64>().is_ok()) {
        // Runs until closed, or for the given seconds (tests: the end lets go of any held button).
        let stop = std::sync::atomic::AtomicBool::new(false);
        let secs = args.iter().find_map(|a| a.parse::<u64>().ok());
        std::thread::scope(|scope| {
            scope.spawn(|| mouse::run(&stop, args.iter().any(|a| a == "--dry")));
            if let Some(secs) = secs {
                std::thread::sleep(Duration::from_secs(secs));
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        return Ok(());
    }
    if cmd == "osk" {
        osk::demo(&args); // test command: the on-screen keyboard alone for 30 s, types nothing
        return Ok(());
    }
    if cmd == "pad" {
        return pad_check(secs);
    }
    if cmd == "speak" && (args.len() == 1 || args.len() == 2 && args[1] == "setup") {
        // Test command: a file of 200-byte Opus frames to the speaker of every Bluetooth DualSense. MAKES SOUND.
        let clip = std::fs::read(&args[0])?;
        let api = HidApi::new()?;
        let devs: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()) && matches!(d.bus_type(), BusType::Bluetooth))
            .filter_map(|d| d.open_device(&api).ok())
            .collect();
        if args.len() == 2 {
            println!("speaker setup report: {:?}", devs.iter().map(|d| d.write(&ds::speaker_setup(0)).map_err(|e| e.to_string())).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("speaker: {} frames to {} Bluetooth controller(s): {:?} (sent, late)", clip.len() / 200, devs.len(), audio::play_speaker(&devs, &clip, &mut || true));
        return Ok(());
    }
    if cmd == "mic" && args.len() == 2 && let Ok(secs @ 1..=30) = args[0].parse::<u64>() {
        // Opens the controller's mic (a write), keeps the raw frames, closes it again.
        if let Err(e) = voice::capture(secs, &args[1]) {
            println!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if cmd == "ui" && (args.is_empty() || args == ["--dry"] || args == ["--pad"]) {
        ui::run(args == ["--dry"], args == ["--pad"]);
        return Ok(());
    }
    if cmd != "look" && cmd != "full" && lights.is_none() && trig.is_none() && rumble.is_none() {
        eprintln!("usage: winsense                  the tray program (no console window); `winsense tray --dry` prints instead of moving the mouse or writing
       winsense look [seconds]   listen only\n       winsense pad [seconds]    virtual Xbox 360 pad (needs ViGEmBus) fed from the DualSense, read back through XInput; sends nothing to the controller (game rumble is only printed)
       winsense mouse [seconds] [--dry]   touchpad and gyro as the Windows mouse until closed (or for seconds); reads only, sends nothing to the controller. Bluetooth: starts at the first touchpad click (that switches to the full report). Touchpad on: slide = move, click = left, two-finger click = right. Mute toggles gyro (starts off); hold Mute + click the pad toggles the touchpad. While gyro is on or a finger rests on the pad: R2 = left click, L2 = right click. --dry prints instead of moving the cursor
       winsense ui [--dry|--pad] trigger test screen; --pad also runs the virtual Xbox 360 pad (needs ViGEmBus; games then see a second pad unless HidHide hides the DualSense), with game rumble. WRITES to every DualSense on each change, switches both triggers off on close (--dry: opens no controller, prints the reports)\n       winsense trigger <l2|r2|both> off | resist <start 0-9> <strength 1-8> | click <start 2-7> <end, up to 8> <strength 1-8> | vibrate <start 1-9> <strength 1-8> <hz 1-255> | rigid <raw start 0-255> <force 0-255>  [power 0-7]\n                                 WRITES to every DualSense. Positions: 0 released, 9 fully pulled (rigid: raw, about 176 = fully pulled). power = trigger power reduction (0 full force, 7 weakest; never tried on the controller; `off` does not reset it). The effect stays until `off`\n       winsense full             ask for the calibration report (Bluetooth: switches to the full report until reconnect)\n       winsense light <r> <g> <b> [player-mask 0-31] [mute 0|1|2] [dim 0|1|2]   WRITES to every DualSense: light bar; player lights, mute light (2 = pulse) and small-light brightness (0 bright, 2 dim; dims the mute light too) only if given. Bluetooth: any write switches to the full report until reconnect\n       winsense light off        give the lights back to the controller (cable: its orange charging pulse; Bluetooth: all dark)
       winsense rumble <left 0-255> <right 0-255> [ms 1-5000]   WRITES to every DualSense: left = strong motor, right = weak; stops by itself after ms (default 1000)
       winsense rumble off       stop rumble (if a run was killed before its stop)
       light and trigger end a running rumble (each report carries the whole rumble state, and they send none)
       winsense haptic <left|right|both> <hz 1-1000> <level 1-100> [ms 1-5000]   over Bluetooth: haptic samples in report 0x32; otherwise PLAYS SOUND to the DualSense's sound device on the cable, rear channels = haptics, refused unless Windows treats it as 4-channel
       winsense mic <secs 1-30> <file>   Bluetooth: OPENS the controller's mic, keeps the raw Opus mic frames in <file>, closes it");
        std::process::exit(2);
    }
    // WINSENSE_ONLY=usb|bt: use one connection when the same controller is on both.
    let only = match std::env::var("WINSENSE_ONLY").map(|v| v.to_ascii_lowercase()) {
        Err(_) => None,
        Ok(v) if v == "bt" => Some(true),
        Ok(v) if v == "usb" => Some(false),
        Ok(v) => {
            eprintln!("WINSENSE_ONLY must be usb or bt, not {v:?}");
            std::process::exit(2);
        }
    };
    let api = HidApi::new()?;
    let pads: Vec<_> = api
        .device_list()
        .filter(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()))
        .collect();
    if pads.is_empty() {
        println!("No DualSense is connected (none visible as a HID device).");
        return Ok(());
    }
    std::thread::scope(|scope| {
        for (n, info) in pads.iter().enumerate() {
            let bt = matches!(info.bus_type(), BusType::Bluetooth);
            if only.is_some_and(|want_bt| want_bt != bt) {
                continue;
            }
            let tag = format!("[{} {}]", n + 1, if bt { "BT" } else { "USB" });
            println!(
                "{tag} {:04x}:{:04x} \"{}\" serial={} usage={:04x}:{:04x} interface={}\n{tag} path {}",
                info.vendor_id(),
                info.product_id(),
                info.product_string().unwrap_or("?"),
                info.serial_number().unwrap_or("?"),
                info.usage_page(),
                info.usage(),
                info.interface_number(),
                info.path().to_string_lossy(),
            );
            let dev = match info.open_device(&api) {
                Ok(d) => d,
                Err(e) => {
                    println!("{tag} cannot open: {e}");
                    continue;
                }
            };
            if let Some(l) = &lights {
                // Bluetooth: fade the start-up animation out first (as the driver does), then set.
                let steps: &[&ds::Lights] = if bt && !matches!(l, ds::Lights::Release) { &[&ds::Lights::Setup, l] } else { &[l] };
                for (seq, s) in steps.iter().enumerate() {
                    match dev.write(&ds::output_report(bt, seq as u8, s, ds::Rumble::default())) {
                        Ok(n) => println!("{tag} wrote {n} bytes"),
                        Err(e) => {
                            println!("{tag} write failed: {e}");
                            break; // send nothing more to this controller after a failure
                        }
                    }
                }
                continue;
            }
            if let Some((l2, r2, power)) = trig {
                match dev.write(&ds::trigger_report(bt, 0, l2, r2, power, ds::Rumble::default()).expect("ranges checked in parse_trigger")) {
                    Ok(n) => println!("{tag} wrote {n} bytes"),
                    Err(e) => {
                        println!("{tag} write failed: {e}");
                        continue;
                    }
                }
                // Read back what the controller says is active now (the newest full report wins).
                let (end, mut buf, mut t) = (Instant::now() + Duration::from_millis(600), [0u8; 128], None);
                while Instant::now() < end {
                    let len = match dev.read_timeout(&mut buf, 100) {
                        Ok(len) => len,
                        Err(e) => {
                            println!("{tag} read failed: {e}");
                            break;
                        }
                    };
                    // Only a USB report or a Bluetooth full report with a good checksum counts.
                    if let Some((ds::Kind::Usb | ds::Kind::BtFull { crc_ok: true }, s)) = ds::parse(bt, &buf[..len]) {
                        t = s.trig.or(t);
                    }
                }
                match t {
                    Some([r, l, active]) => println!(
                        "{tag} controller reports: R2 effect {} state {} stop {} | L2 effect {} state {} stop {}  (effect: 0 off, 1 resist, 2 click, 3 vibrate)",
                        active & 15, r >> 4, r & 15, active >> 4, l >> 4, l & 15
                    ),
                    None => println!("{tag} no full report came back to confirm"),
                }
                if [l2, r2].iter().any(|e| matches!(e, Some(ds::Effect::Rigid { .. }))) {
                    println!("{tag} rigid is a raw mode: the readback cannot confirm it (effect reads 0, the stop is unreliable); only feel can");
                }
                continue;
            }
            if let Some((left, right, ms)) = rumble {
                let m = ds::Rumble { left, right, v2: vibration_v2(&dev, info.product_id()) };
                let report = |seq, m| ds::trigger_report(bt, seq, None, None, None, m).expect("no effects");
                if left | right != 0 {
                    println!("{tag} rumble left {left} right {right} for {ms} ms ({} form)", if m.v2 { "v2" } else { "old" });
                    if let Err(e) = dev.write(&report(0, m)) {
                        println!("{tag} write failed: {e}");
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(ms));
                }
                match dev.write(&report(1, ds::Rumble::default())) {
                    Ok(_) => println!("{tag} rumble stopped"),
                    Err(e) => println!("{tag} STOP FAILED ({e}): rumble may still be on; run `winsense rumble off`"),
                }
                continue;
            }
            if cmd == "full" {
                for (id, name) in [(0x05, "calibration"), (0x20, "firmware")] {
                    let mut f = [0u8; 64];
                    f[0] = id;
                    match dev.get_feature_report(&mut f) {
                        // Bluetooth feature reports end in a CRC seeded with 0xA3.
                        Ok(len) if bt && len >= 5 => println!(
                            "{tag} {name} report: {len} bytes, checksum ok: {}",
                            ds::crc32(0xA3, &f[..len - 4]) == u32::from_le_bytes(f[len - 4..len].try_into().unwrap())
                        ),
                        Ok(len) => println!("{tag} {name} report: {len} bytes"),
                        Err(e) => println!("{tag} {name} report failed: {e}"),
                    }
                    if id == 0x20 {
                        println!("{tag} firmware raw: {}\n{tag} update version (bytes 44-45): {:#06x}", hex(&f), u16::from_le_bytes([f[44], f[45]]));
                    }
                }
                continue;
            }
            let mut desc = [0u8; 4096];
            match dev.get_report_descriptor(&mut desc) {
                Ok(len) => println!("{tag} report descriptor, {len} bytes: {}", hex(&desc[..len])),
                Err(e) => println!("{tag} no report descriptor: {e}"),
            }
            scope.spawn(move || listen(&tag, bt, &dev, secs));
        }
    });
    Ok(())
}

/// `winsense pad [secs]`: run the virtual pad and print what XInput (what games read) sees.
fn pad_check(secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState};
    let p = pad::Pad::start(|[large, small]| println!("game rumble: left (strong) {large}, right (weak) {small} - not sent to the controller"))?;
    println!("virtual Xbox 360 pad plugged in (bus serial {}); XInput slots for {secs} s:", p.serial);
    let mut last = Vec::new();
    for t in 0..=secs * 2 {
        let mut now = Vec::new();
        for slot in 0..4 {
            let mut x: XINPUT_STATE = unsafe { std::mem::zeroed() };
            if unsafe { XInputGetState(slot, &mut x) } == 0 {
                let g = x.Gamepad;
                now.push(format!("slot {slot}: buttons {:04x} LT {} RT {} L ({}, {}) R ({}, {})", g.wButtons, g.bLeftTrigger, g.bRightTrigger, g.sThumbLX, g.sThumbLY, g.sThumbRX, g.sThumbRY));
            }
        }
        // Act as a game at 1 s and 2 s: rumble 100/200 on the first XInput pad, then 0/0 (printed by the pad, never sent on).
        if (t == 2 || t == 4) && let Some(slot) = now.first().and_then(|l| l[5..6].parse::<u32>().ok()) {
            let v = if t == 2 { XINPUT_VIBRATION { wLeftMotorSpeed: 100 << 8, wRightMotorSpeed: 200 << 8 } } else { XINPUT_VIBRATION { wLeftMotorSpeed: 0, wRightMotorSpeed: 0 } };
            println!("XInputSetState slot {slot}: left {} right {}", v.wLeftMotorSpeed >> 8, v.wRightMotorSpeed >> 8);
            unsafe { XInputSetState(slot, &v) };
        }
        if now != last {
            println!("{:5.1}s {}", t as f32 / 2.0, if now.is_empty() { "no XInput pad".into() } else { now.join(" | ") });
            last = now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn listen(tag: &str, bt: bool, dev: &hidapi::HidDevice, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 128];
    let mut seen: BTreeMap<(u8, usize), u32> = BTreeMap::new(); // (report id, length) -> count
    let (mut last, mut bad_crc, mut shown_raw) = (None, 0u32, false);
    while Instant::now() < end {
        let len = match dev.read_timeout(&mut buf, 200) {
            Ok(0) => continue,
            Ok(len) => len,
            Err(e) => {
                println!("{tag} read failed: {e}");
                break;
            }
        };
        let r = &buf[..len];
        *seen.entry((r[0], len)).or_default() += 1;
        if !shown_raw {
            println!("{tag} first report raw: {}", hex(r));
            shown_raw = true;
        }
        let Some((kind, state)) = ds::parse(bt, r) else { continue };
        bad_crc += (kind == ds::Kind::BtFull { crc_ok: false }) as u32;
        if last.map(|(k, s): (ds::Kind, ds::State)| (k, s.buttons)) != Some((kind, state.buttons)) {
            println!(
                "{tag} {:5.1}s {kind:?}: {}  sticks={:?} L2={} R2={} battery={:?} trig={:02x?}",
                (secs as f32) - (end - Instant::now()).as_secs_f32(),
                ds::button_names(state.buttons),
                state.sticks,
                state.l2,
                state.r2,
                state.battery,
                state.trig,
            );
        }
        last = Some((kind, state));
    }
    println!("{tag} reports in {secs}s by (id, length): {seen:02x?}  bad checksums: {bad_crc}");
    if seen.is_empty() {
        println!("{tag} nothing arrived: another program may hold the controller, or it is asleep.");
    }
}

/// `light off` or `light r g b [mask] [mute] [dim]`; None if the arguments are not valid.
fn parse_light(a: &[String]) -> Option<ds::Lights> {
    if a == ["off"] {
        return Some(ds::Lights::Release);
    }
    let n: Vec<u8> = a.iter().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let over = |i: usize, max: u8| n.get(i).is_some_and(|&v| v > max);
    if !(3..=6).contains(&n.len()) || over(3, 31) || over(4, 2) || over(5, 2) {
        return None;
    }
    // Arguments left out are not sent, so `light r g b` changes the light bar only.
    Some(ds::Lights::Set { rgb: [n[0], n[1], n[2]], players: n.get(3).copied(), mute: n.get(4).copied(), dim: n.get(5).copied() })
}

/// The rumble form for one controller, from its firmware feature report (0x20).
fn vibration_v2(dev: &hidapi::HidDevice, pid: u16) -> bool {
    let mut f = [0u8; 64];
    f[0] = 0x20;
    let n = dev.get_feature_report(&mut f).unwrap_or(0);
    ds::vibration_v2(pid, (n >= 46).then_some(&f[..]))
}

/// `haptic <left|right|both> <hz> <level %> [ms]`; None if not valid.
fn parse_haptic(a: &[String]) -> Option<(audio::Side, u32, u8, u32)> {
    let side = match a.first()?.as_str() {
        "left" => audio::Side::Left,
        "right" => audio::Side::Right,
        "both" => audio::Side::Both,
        _ => return None,
    };
    let n: Vec<u32> = a[1..].iter().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let (hz, level, ms) = match n[..] {
        [hz, level] => (hz, level, 1000),
        [hz, level, ms] => (hz, level, ms),
        _ => return None,
    };
    ((1..=1000).contains(&hz) && (1..=100).contains(&level) && (1..=5000).contains(&ms)).then_some((side, hz, level as u8, ms))
}

/// `rumble off` or `rumble <left> <right> [ms]` as (left, right, ms); None if not valid.
/// `off` is (0, 0, 0): only the stop is sent.
fn parse_rumble(a: &[String]) -> Option<(u8, u8, u64)> {
    if a == ["off"] {
        return Some((0, 0, 0));
    }
    let n: Vec<u64> = a.iter().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    match n[..] {
        [l, r] if l <= 255 && r <= 255 => Some((l as u8, r as u8, 1000)),
        [l, r, ms] if l <= 255 && r <= 255 && (1..=5000).contains(&ms) => Some((l as u8, r as u8, ms)),
        _ => None,
    }
}

/// `trigger <l2|r2|both> <effect> <numbers> [power 0-7]` as (L2, R2, power); None if anything
/// is not valid, so a bad value never reaches a controller.
fn parse_trigger(a: &[String]) -> Option<(Option<ds::Effect>, Option<ds::Effect>, Option<u8>)> {
    let (a, power) = match a {
        [rest @ .., p, v] if p == "power" => (rest, Some(v.parse::<u8>().ok().filter(|&v| v <= 7)?)),
        _ => (a, None),
    };
    let n: Vec<u8> = a.get(2..)?.iter().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let e = match (a[1].as_str(), &n[..]) {
        ("off", []) => ds::Effect::Off,
        ("resist", &[start, strength]) => ds::Effect::Resist { start, strength },
        ("click", &[start, end, strength]) => ds::Effect::Click { start, end, strength },
        ("vibrate", &[start, strength, hz]) => ds::Effect::Vibrate { start, strength, hz },
        ("rigid", &[start, force]) => ds::Effect::Rigid { start, force },
        _ => return None,
    };
    e.bytes()?; // range check
    let (l2, r2) = match a[0].as_str() {
        "l2" => (Some(e), None),
        "r2" => (None, Some(e)),
        "both" => (Some(e), Some(e)),
        _ => return None,
    };
    Some((l2, r2, power))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_arguments() {
        let p = |s: &str| parse_trigger(&s.split_whitespace().map(String::from).collect::<Vec<_>>());
        for bad in ["", "r2", "r2 off 1", "x2 off", "r2 resist 0", "r2 resist 0 9", "r2 resist 10 4", "r2 click 1 5 4", "r2 click 5 5 4", "r2 vibrate 0 4 30", "r2 vibrate 2 4 0", "r2 vibrate 2 4 256", "r2 raw 252", "both resist a b", "r2 rigid 0 256", "r2 rigid 0", "r2 resist 0 8 power 8", "r2 resist 0 8 power", "power 0"] {
            assert!(p(bad).is_none(), "{bad}");
        }
        use ds::Effect::*;
        assert_eq!(p("both off"), Some((Some(Off), Some(Off), None)));
        assert_eq!(p("r2 resist 4 2"), Some((None, Some(Resist { start: 4, strength: 2 }), None)));
        assert_eq!(p("l2 click 3 5 6"), Some((Some(Click { start: 3, end: 5, strength: 6 }), None, None)));
        assert_eq!(p("r2 vibrate 3 2 25"), Some((None, Some(Vibrate { start: 3, strength: 2, hz: 25 }), None)));
        assert_eq!(p("l2 rigid 0 110"), Some((Some(Rigid { start: 0, force: 110 }), None, None)));
        assert_eq!(p("r2 resist 0 8 power 7"), Some((None, Some(Resist { start: 0, strength: 8 }), Some(7))));
        assert_eq!(p("both off power 0"), Some((Some(Off), Some(Off), Some(0))), "the power cleanup form");
    }

    #[test]
    fn rumble_arguments() {
        let p = |s: &str| parse_rumble(&s.split_whitespace().map(String::from).collect::<Vec<_>>());
        for bad in ["", "80", "256 0", "0 256", "80 0 0", "80 0 5001", "80 0 100 1", "off 1", "-1 0", "a b"] {
            assert!(p(bad).is_none(), "{bad}");
        }
        assert_eq!(p("off"), Some((0, 0, 0)));
        assert_eq!(p("80 0"), Some((80, 0, 1000)));
        assert_eq!(p("0 255 1500"), Some((0, 255, 1500)));
    }

    #[test]
    fn haptic_arguments() {
        let p = |s: &str| parse_haptic(&s.split_whitespace().map(String::from).collect::<Vec<_>>());
        for bad in ["", "left", "left 100", "up 100 20", "left 0 20", "left 1001 20", "left 100 0", "left 100 101", "left 100 20 0", "left 100 20 5001", "left 100 20 9 9"] {
            assert!(p(bad).is_none(), "{bad}");
        }
        assert_eq!(p("both 80 20"), Some((audio::Side::Both, 80, 20, 1000)));
        assert_eq!(p("right 150 5 300"), Some((audio::Side::Right, 150, 5, 300)));
    }

    #[test]
    fn light_arguments() {
        // The only guard between typed input and a write to the controller.
        let p = |s: &str| parse_light(&s.split_whitespace().map(String::from).collect::<Vec<_>>());
        for bad in ["", "1 2", "1 2 3 4 5 6 7", "1 2 256", "1 2 x", "-1 2 3", "1 2 3 32", "1 2 3 4 3", "1 2 3 4 2 3", "off 1"] {
            assert!(p(bad).is_none(), "{bad}");
        }
        assert!(matches!(p("off"), Some(ds::Lights::Release)));
        assert!(matches!(p("1 2 3 4"), Some(ds::Lights::Set { rgb: [1, 2, 3], players: Some(4), mute: None, dim: None })));
    }
}
