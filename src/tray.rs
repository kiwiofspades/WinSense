//! The tray program: `winsense` with no arguments. No console window; an icon in the
//! notification area whose menu opens the trigger test screen (src/ui.rs, same process, same
//! worker), switches the touchpad and gyro mouse, turns a Bluetooth controller off, sets
//! "start with Windows" (a per-user Run entry) and quits. The mouse (src/mouse.rs) runs the
//! whole time and reads only. Quitting closes the screen and ends the worker, which switches
//! triggers and rumble off if anything was ever sent. One tray per user session (a named mutex);
//! while it runs, the CLI commands that write or move the mouse refuse (`claim`).
//! Since 0.8.0 it also runs the mod port (src/port.rs): game mods send effects through a named pipe.
//! Since 0.10.0 it also runs the voice thread (src/voice.rs): dings and the hold-Mute voice command.
//! Since 0.9.1, when a controller connected by Bluetooth is plugged in by cable too, it drops the
//! Bluetooth link, so the controller runs on the cable alone (cable haptics are not felt while
//! Bluetooth is up; measured) without turning the PC's Bluetooth off.
//! Since 0.11.0 the tooltip and the menu show the battery, a balloon warns once per discharge when
//! it is low, and "Turn off when idle" (off unless ticked) switches a Bluetooth controller off
//! after 10 minutes without input (src/power.rs holds the rules).
//! Since 0.12.0 "Game sound as haptics" (off unless ticked) plays the low end of what Windows plays
//! in the grips (src/sound.rs).
//! Since 0.15.0 "Mute light follows PC mic" and "Buzz on notifications" (off unless ticked, src/desk.rs).

use crate::{mouse, power, ui};
use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows_sys::Win32::Devices::Bluetooth::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::PCWSTR;
use windows_sys::w;

const WM_TRAY: u32 = WM_APP + 1;
const ID_SCREEN: usize = 1;
const ID_TOUCH: usize = 2;
const ID_GYRO: usize = 3;
pub const ID_OFF: usize = 4;
const ID_AUTOSTART: usize = 5;
const ID_QUIT: usize = 6;
const ID_IDLE: usize = 7;
const ID_SOUND: usize = 8;
pub const ID_COUCH: usize = 9;
const ID_MICLIGHT: usize = 10;
const ID_BUZZ: usize = 11;
const ID_UPDATE: usize = 12;
/// WinSense's own settings (per user), DWORDs: `IdleOff` = "Turn off when idle" is ticked,
/// `SoundHaptics` = "Game sound as haptics" is ticked, `Couch` = "Couch mode" is ticked,
/// `MicLight` = "Mute light follows PC mic", `Buzz` = "Buzz on notifications".
const SETTINGS_KEY: PCWSTR = w!("Software\\WinSense");
const TIMER_WIRED: usize = 2;
/// WM_DEVICECHANGE's "a device arrived or left" (dbt.h), sent to every top-level window.
const DBT_DEVNODES_CHANGED: usize = 7;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// IOCTL_BTH_DISCONNECT_DEVICE (bthioctl.h; DS4Windows uses it the same way): input = the 8-byte address.
const IOCTL_BTH_DISCONNECT_DEVICE: u32 = 0x41000C;

static STOP: AtomicBool = AtomicBool::new(false);
/// The real tray: hand a plugged-in controller over to the cable, switch an idle one off, keep
/// settings in the registry (off in dry runs and tests: those act on the controller or on the user's settings).
static WIRED: AtomicBool = AtomicBool::new(false);
/// "Turn off when idle": off unless ticked.
static IDLE_ON: AtomicBool = AtomicBool::new(false);
static POWER: Mutex<(power::Warn, power::IdleOff)> = Mutex::new((power::Warn(false), power::IdleOff(false)));
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(u32::MAX);
/// What the tip shows now (`shown`: mouse::STATE and the battery byte), u32::MAX = the icon is not in the tray yet.
static SHOWN: AtomicU32 = AtomicU32::new(u32::MAX);
/// Updates (src/update.rs): look for one at all (off in tests), when the last look was (Unix
/// seconds), a look under way, its result (true = asked from the menu), the release on offer, and
/// a dialog that is open.
static UPDATES: AtomicBool = AtomicBool::new(false);
static LAST_LOOK: AtomicU32 = AtomicU32::new(0);
static LOOKING: AtomicBool = AtomicBool::new(false);
static LOOKED: Mutex<Option<(bool, Result<Option<crate::update::Release>, String>)>> = Mutex::new(None);
static OFFER: Mutex<Option<crate::update::Release>> = Mutex::new(None);
static ASKING: AtomicBool = AtomicBool::new(false);
static DRY: AtomicBool = AtomicBool::new(false);
thread_local! {
    static ICON: Cell<HICON> = const { Cell::new(null_mut()) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Take the one-per-session WinSense lock; None when the tray (or another owner) already has it.
/// Keep the handle for as long as the process should own the controller.
pub fn claim() -> Option<HANDLE> {
    unsafe {
        let h = CreateMutexW(null(), 0, w!("Local\\WinSense"));
        if h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            if !h.is_null() {
                CloseHandle(h);
            }
            return None;
        }
        Some(h)
    }
}

/// The app icon from the exe's resources (drawn by build.rs) at the size Windows wants.
pub fn icon(small: bool) -> HICON {
    let (cx, cy) = if small { (SM_CXSMICON, SM_CYSMICON) } else { (SM_CXICON, SM_CYICON) };
    unsafe { LoadImageW(GetModuleHandleW(null()), 1 as PCWSTR, IMAGE_ICON, GetSystemMetrics(cx), GetSystemMetrics(cy), LR_DEFAULTCOLOR) as HICON }
}

/// Let popup menus follow Windows' dark mode (uxtheme ordinals 135 SetPreferredAppMode and
/// 136 FlushMenuThemes: undocumented, used by Notepad++ and others since Windows 10 1903).
fn dark_menus() {
    unsafe {
        let ux = LoadLibraryW(w!("uxtheme.dll"));
        if ux.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(ux, 135 as _) {
            let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set(1); // AllowDark: follow the Windows setting
        }
        if let Some(f) = GetProcAddress(ux, 136 as _) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

fn shown() -> u32 {
    let flag = |f: &AtomicBool, bit: u32| (f.load(Ordering::Relaxed) as u32) << bit;
    mouse::STATE.load(Ordering::Relaxed) as u32 | (power::BATTERY.load(Ordering::Relaxed) as u32) << 8 | flag(&mouse::TOUCH_ON, 16) | flag(&mouse::GYRO_ON, 17)
}

/// The first line of the tip and the menu. `state` = mouse::STATE: 2 only means the full report is
/// read, so whether the mouse is on comes from the touchpad and gyro switches.
fn status(state: u8, touch: bool, gyro: bool) -> &'static str {
    match (state, touch, gyro) {
        (0, ..) => "No DualSense connected",
        (1, ..) | (_, false, false) => "Mouse off (hold the touchpad down to turn it on)",
        (_, true, true) => "Touchpad and gyro mouse on",
        (_, true, false) => "Touchpad mouse on",
        _ => "Gyro mouse on",
    }
}

/// The battery line for the tip and the menu; None without a controller.
fn battery_line(state: u8, b: Option<(u8, u8)>) -> Option<String> {
    match (state, b) {
        (0, _) => None,
        // Bluetooth before the mouse is on: the short report carries no battery byte.
        (1, None) => Some("Battery shows after you hold the touchpad down".into()),
        _ => Some(power::text(b)),
    }
}

fn tip(state: u8, touch: bool, gyro: bool, b: Option<(u8, u8)>) -> String {
    let first = format!("WinSense: {}", status(state, touch, gyro));
    battery_line(state, b).map_or(first.clone(), |b| format!("{first}\n{b}"))
}

/// Add, update or remove the tray icon; `info` = a balloon message.
fn notify(h: HWND, msg: NOTIFY_ICON_MESSAGE, info: Option<&str>) -> bool {
    let state = shown();
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = h;
    d.uID = 1;
    d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = ICON.get();
    let copy = |dst: &mut [u16], s: &str| s.encode_utf16().take(dst.len() - 1).enumerate().for_each(|(i, c)| dst[i] = c);
    copy(&mut d.szTip, &tip(state as u8, state >> 16 & 1 != 0, state >> 17 & 1 != 0, power::battery()));
    if let Some(text) = info {
        d.uFlags |= NIF_INFO;
        d.dwInfoFlags = NIIF_INFO;
        copy(&mut d.szInfoTitle, "WinSense");
        copy(&mut d.szInfo, text);
    }
    let ok = unsafe { Shell_NotifyIconW(msg, &d) } != 0;
    if ok && msg != NIM_DELETE {
        SHOWN.store(state, Ordering::Relaxed);
    }
    ok
}

/// Once a second: put the icon in the tray if it is not there yet (at logon the taskbar may
/// not be ready), keep the tip in step with the mouse and the battery, warn when the battery is
/// low, and switch an idle controller off if that is ticked.
fn tick(h: HWND) {
    let in_use = power::MODS.load(Ordering::Relaxed) > 0 || power::EFFECT.load(Ordering::Relaxed) || crate::sound::ACTIVE.load(Ordering::Relaxed);
    let (low, idle) = {
        let mut p = POWER.lock().unwrap();
        (p.0.step(power::battery()), p.1.due(IDLE_ON.load(Ordering::Relaxed) && !in_use, power::IDLE_SECS.load(Ordering::Relaxed)))
    };
    if let Some(text) = low {
        notify(h, NIM_MODIFY, Some(&text));
    }
    if idle {
        if !WIRED.load(Ordering::Relaxed) {
            println!("tray: dry, idle for {} s: would turn the controller off", power::IDLE_LIMIT);
        } else if let Some(text) = idle_off() {
            notify(h, NIM_MODIFY, Some(&text));
        }
    }
    if let Some(text) = crate::voice::NOTE.lock().unwrap().take() {
        notify(h, NIM_MODIFY, Some(text)); // why voice refused (src/voice.rs)
    }
    updates(h);
    let was = SHOWN.load(Ordering::Relaxed);
    if was == u32::MAX {
        notify(h, NIM_ADD, None);
    } else if was != shown() {
        notify(h, NIM_MODIFY, None);
    }
}

fn now() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

/// Look on GitHub in the background; the result is picked up by `updates` on the next tick.
fn look(manual: bool) {
    if LOOKING.swap(true, Ordering::Relaxed) {
        return;
    }
    LAST_LOOK.store(now(), Ordering::Relaxed);
    if WIRED.load(Ordering::Relaxed) {
        let _ = set_dword(w!("LastUpdateCheck"), now());
    }
    std::thread::spawn(move || {
        let r = crate::update::check();
        *LOOKED.lock().unwrap() = Some((manual, r));
        LOOKING.store(false, Ordering::Relaxed);
    });
}

/// From the tick: look once a day, and act on what a look found.
fn updates(h: HWND) {
    if UPDATES.load(Ordering::Relaxed) && now().saturating_sub(LAST_LOOK.load(Ordering::Relaxed)) >= 24 * 3600 {
        look(false);
    }
    if ASKING.load(Ordering::Relaxed) {
        return;
    }
    let Some((manual, found)) = LOOKED.lock().unwrap().take() else { return };
    let current = env!("CARGO_PKG_VERSION");
    match found {
        Ok(Some(r)) => {
            println!("update: {} offered (this is {current})", r.version);
            *OFFER.lock().unwrap() = Some(r.clone());
            if manual {
                offer(h, r);
            } else {
                notify(h, NIM_MODIFY, Some(&format!("WinSense {} is out (you have {current}). Click here to update.", r.version)));
            }
        }
        Ok(None) if manual => {
            notify(h, NIM_MODIFY, Some(&format!("WinSense {current} is the latest version.")));
        }
        Ok(None) => println!("update: none newer than {current}"),
        Err(e) if manual => {
            notify(h, NIM_MODIFY, Some(&format!("Could not check for updates: {e}.")));
        }
        Err(e) => println!("update: {e}"),
    }
}

/// Ask before anything is downloaded. Yes: download, check, swap, restart (src/update.rs).
fn offer(h: HWND, r: crate::update::Release) {
    use windows_sys::Win32::UI::Controls::*;
    if ASKING.swap(true, Ordering::Relaxed) {
        return;
    }
    let dry = DRY.load(Ordering::Relaxed);
    // A dry tray on a hidden desktop can be answered through the environment (self-tests).
    let answer = std::env::var("WINSENSE_UPDATE_ANSWER").ok().filter(|_| dry);
    let yes = match answer.as_deref() {
        Some(a) => a == "yes",
        None => {
            let main = wide(&format!("WinSense {} is available", r.version));
            let body = wide(&format!(
                "You have {}. Download and install it now? WinSense restarts by itself, and any trigger effect or rumble stops for a moment.

Release notes: github.com/{}/releases",
                env!("CARGO_PKG_VERSION"),
                crate::update::REPO
            ));
            let mut button = 0;
            unsafe {
                SetForegroundWindow(h);
                TaskDialog(h, null_mut(), w!("WinSense update"), main.as_ptr(), body.as_ptr(), TDCBF_YES_BUTTON | TDCBF_NO_BUTTON, TD_INFORMATION_ICON, &mut button);
            }
            button == IDYES
        }
    };
    println!("update: answer {}", if yes { "yes" } else { "no" });
    if !yes {
        ASKING.store(false, Ordering::Relaxed);
        return;
    }
    notify(h, NIM_MODIFY, Some(&format!("Downloading WinSense {}...", r.version)));
    let h = h as isize;
    std::thread::spawn(move || {
        let args: &[&str] = if dry { &["tray", "--dry"] } else { &["tray"] };
        match crate::update::install(&r, args) {
            Ok(()) => {
                println!("update: {} in place, restarting", r.version);
                unsafe { PostMessageW(h as HWND, WM_COMMAND, ID_QUIT, 0) };
            }
            Err(e) => {
                println!("update: {e}");
                notify(h as HWND, NIM_MODIFY, Some(&format!("The update was not installed: {e}.")));
                ASKING.store(false, Ordering::Relaxed);
            }
        }
    });
}

fn autostart() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, w!("WinSense"), RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == 0 }
}

/// The per-user Run entry: this exe, quoted, no arguments (= the tray).
fn set_autostart(on: bool) -> u32 {
    unsafe {
        if !on {
            return RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("WinSense"));
        }
        let Ok(exe) = std::env::current_exe() else { return ERROR_FILE_NOT_FOUND };
        let v = wide(&format!("\"{}\"", exe.display()));
        RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("WinSense"), REG_SZ, v.as_ptr() as _, (v.len() * 2) as u32)
    }
}

fn setting(name: PCWSTR) -> bool {
    let (mut v, mut size) = (0u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, SETTINGS_KEY, name, RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as _, &mut size) == 0 && v != 0 }
}

fn set_dword(name: PCWSTR, v: u32) -> u32 {
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, SETTINGS_KEY, name, REG_DWORD, &v as *const u32 as _, 4) }
}

fn dword(name: PCWSTR) -> u32 {
    let (mut v, mut size) = (0u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, SETTINGS_KEY, name, RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as _, &mut size) };
    v
}

fn set_setting(name: PCWSTR, on: bool) -> u32 {
    let v = on as u32;
    unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, SETTINGS_KEY, name, REG_DWORD, &v as *const u32 as _, 4) }
}

/// The Bluetooth addresses of the connected DualSenses: (by Bluetooth, by cable). Bluetooth: the
/// HID serial (e.g. 0123456789ab). Cable: the serial is empty, so feature report 0x09.
fn addresses(api: &hidapi::HidApi) -> (Vec<u64>, Vec<u64>) {
    let (mut bt, mut usb) = (Vec::new(), Vec::new());
    for d in api.device_list().filter(|d| d.vendor_id() == crate::ds::VID && crate::ds::PIDS.contains(&d.product_id())) {
        if matches!(d.bus_type(), hidapi::BusType::Bluetooth) {
            bt.extend(d.serial_number().and_then(|s| u64::from_str_radix(s, 16).ok()));
        } else if let Ok(dev) = d.open_device(api) {
            let mut f = [0u8; 64];
            f[0] = 0x09;
            usb.extend(dev.get_feature_report(&mut f).ok().and_then(|n| pairing_address(&f[..n])));
        }
    }
    (bt, usb)
}

/// Feature report 0x09 (pairing info): the controller's address at bytes 1-6, lowest byte first
/// (the Linux driver and SDL match the cable and Bluetooth instances by it).
fn pairing_address(f: &[u8]) -> Option<u64> {
    let a = f.get(1..7).filter(|_| f[0] == 0x09)?;
    Some(a.iter().rev().fold(0, |n, &b| n << 8 | b as u64)).filter(|&n| n != 0)
}

/// Drop one Bluetooth device's link through the PC's radio; the PC's Bluetooth stays on.
fn bt_disconnect(addr: u64) -> Result<(), String> {
    unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS { dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32 };
        let mut radio = null_mut();
        let find = BluetoothFindFirstRadio(&params, &mut radio);
        if find.is_null() {
            return Err("No Bluetooth radio found.".into());
        }
        BluetoothFindRadioClose(find);
        let mut got = 0;
        let ok = DeviceIoControl(radio, IOCTL_BTH_DISCONNECT_DEVICE, &addr as *const u64 as _, 8, null_mut(), 0, &mut got, null_mut());
        let err = GetLastError();
        CloseHandle(radio);
        if ok != 0 { Ok(()) } else { Err(format!("error {err:#x}")) }
    }
}

/// Turn a Bluetooth DualSense off through the PC's radio. Pressing PS brings it back on the
/// short report, so plain readers work again. Returns the line for the balloon.
fn controller_off() -> String {
    let Ok(api) = hidapi::HidApi::new() else { return "Cannot reach HID devices.".into() };
    let Some(&addr) = addresses(&api).0.first() else { return "No DualSense is connected by Bluetooth.".into() };
    match bt_disconnect(addr) {
        Ok(()) => "Controller turned off. Press PS to turn it back on.".into(),
        Err(e) => format!("Could not turn the controller off ({e})."),
    }
}

/// "Turn off when idle": the same switch-off, by itself. None (no balloon) when no DualSense is
/// on Bluetooth: one on the cable stays on, it is charging.
fn idle_off() -> Option<String> {
    let api = hidapi::HidApi::new().ok()?;
    let addr = *addresses(&api).0.first()?;
    Some(match bt_disconnect(addr) {
        Ok(()) => format!("Controller turned off after {} minutes without input. Press PS to turn it back on.", power::IDLE_LIMIT / 60),
        Err(e) => format!("The idle controller could not be turned off ({e})."),
    })
}

/// A controller on the cable that is also connected by Bluetooth: drop the Bluetooth link, so
/// it runs on the cable alone. The balloon's line, None when there was nothing to do.
fn wired_handover() -> Option<String> {
    let api = hidapi::HidApi::new().ok()?;
    let (bt, usb) = addresses(&api);
    let addr = *bt.iter().find(|a| usb.contains(a))?;
    Some(match bt_disconnect(addr) {
        Ok(()) => "Cable in: the controller now runs on the cable (its Bluetooth link was dropped; the PC's Bluetooth stays on).".into(),
        Err(e) => format!("Cable in, but the Bluetooth link could not be dropped ({e})."),
    })
}

fn show_menu(h: HWND) {
    unsafe {
        let m = CreatePopupMenu();
        let item = |flags: u32, id: usize, text: &str| AppendMenuW(m, flags, id, wide(text).as_ptr());
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let state = mouse::STATE.load(Ordering::Relaxed).min(2);
        item(MF_STRING | MF_GRAYED, 0, status(state, mouse::TOUCH_ON.load(Ordering::Relaxed), mouse::GYRO_ON.load(Ordering::Relaxed)));
        if let Some(b) = battery_line(state, power::battery()) {
            item(MF_STRING | MF_GRAYED, 0, &b);
        }
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        item(MF_STRING, ID_SCREEN, "Trigger test screen");
        item(MF_STRING | check(mouse::TOUCH_ON.load(Ordering::Relaxed)), ID_TOUCH, "Touchpad mouse");
        item(MF_STRING | check(mouse::GYRO_ON.load(Ordering::Relaxed)), ID_GYRO, "Gyro mouse");
        item(MF_STRING | check(crate::couch::ON.load(Ordering::Relaxed)), ID_COUCH, "Couch mode");
        item(MF_STRING | check(crate::sound::ON.load(Ordering::Relaxed)), ID_SOUND, "Game sound as haptics");
        item(MF_STRING | check(crate::desk::LIGHT_ON.load(Ordering::Relaxed)), ID_MICLIGHT, "Mute light follows PC mic");
        item(MF_STRING | check(crate::desk::BUZZ_ON.load(Ordering::Relaxed)), ID_BUZZ, "Buzz on notifications");
        item(MF_STRING, ID_OFF, "Turn controller off");
        item(MF_STRING | check(IDLE_ON.load(Ordering::Relaxed)), ID_IDLE, &format!("Turn off when idle ({} minutes)", power::IDLE_LIMIT / 60));
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        item(MF_STRING | check(autostart()), ID_AUTOSTART, "Start with Windows");
        item(MF_STRING, ID_UPDATE, "Check for updates");
        item(MF_STRING, ID_QUIT, "Quit");
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        SetForegroundWindow(h); // without it the menu does not close when clicking elsewhere
        TrackPopupMenu(m, TPM_RIGHTBUTTON, p.x, p.y, 0, h, null());
        PostMessageW(h, WM_NULL, 0, 0);
        DestroyMenu(m);
    }
}

fn command(h: HWND, id: usize) {
    let toggle = |flag: &AtomicBool| {
        if !flag.fetch_xor(true, Ordering::Relaxed) {
            mouse::WAKE.store(true, Ordering::Relaxed); // switching one on from the menu = use it now
        }
    };
    // A toggle kept in the registry (the real tray only): flip it, say so if it cannot be remembered.
    let remembered = |flag: &AtomicBool, name: PCWSTR, label: &str| {
        let on = !flag.fetch_xor(true, Ordering::Relaxed);
        let err = if WIRED.load(Ordering::Relaxed) { set_setting(name, on) } else { 0 };
        if err != 0 {
            notify(h, NIM_MODIFY, Some(&format!("\"{label}\" is {} until WinSense quits, but could not be remembered (error {err}).", if on { "on" } else { "off" })));
        }
    };
    match id {
        ID_MICLIGHT => remembered(&crate::desk::LIGHT_ON, w!("MicLight"), "Mute light follows PC mic"),
        ID_BUZZ => remembered(&crate::desk::BUZZ_ON, w!("Buzz"), "Buzz on notifications"),
        ID_SCREEN => {
            ui::open();
        }
        ID_UPDATE => look(true),
        ID_TOUCH => toggle(&mouse::TOUCH_ON),
        ID_GYRO => toggle(&mouse::GYRO_ON),
        ID_OFF => {
            notify(h, NIM_MODIFY, Some(&controller_off()));
        }
        ID_AUTOSTART => {
            let err = set_autostart(!autostart());
            if err != 0 {
                notify(h, NIM_MODIFY, Some(&format!("Could not change \"Start with Windows\" (error {err}).")));
            }
        }
        ID_IDLE => {
            let on = !IDLE_ON.fetch_xor(true, Ordering::Relaxed);
            if on {
                // Count from the tick, not from the last input: ticking it never switches off at once.
                power::RESET.store(true, Ordering::Relaxed);
                let _ = power::IDLE_SECS.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| (v != power::NONE).then_some(0));
            }
            let err = if WIRED.load(Ordering::Relaxed) { set_setting(w!("IdleOff"), on) } else { 0 };
            if err != 0 {
                notify(h, NIM_MODIFY, Some(&format!("\"Turn off when idle\" is {} until WinSense quits, but could not be remembered (error {err}).", if on { "on" } else { "off" })));
            }
        }
        ID_SOUND => {
            let on = !crate::sound::ON.fetch_xor(true, Ordering::Relaxed);
            crate::port::poke(); // the haptic thread starts or stops listening
            let err = if WIRED.load(Ordering::Relaxed) { set_setting(w!("SoundHaptics"), on) } else { 0 };
            if err != 0 {
                notify(h, NIM_MODIFY, Some(&format!("\"Game sound as haptics\" is {} until WinSense quits, but could not be remembered (error {err}).", if on { "on" } else { "off" })));
            }
        }
        ID_COUCH => {
            let on = !crate::couch::ON.fetch_xor(true, Ordering::Relaxed);
            let err = if WIRED.load(Ordering::Relaxed) { set_setting(w!("Couch"), on) } else { 0 };
            if err != 0 {
                notify(h, NIM_MODIFY, Some(&format!("\"Couch mode\" is {} until WinSense quits, but could not be remembered (error {err}).", if on { "on" } else { "off" })));
            }
        }
        ID_QUIT => unsafe {
            DestroyWindow(h);
        },
        _ => {}
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_TRAY => {
            match l as u32 & 0xFFFF {
                WM_LBUTTONUP | WM_RBUTTONUP => show_menu(h),
                NIN_BALLOONUSERCLICK => {
                    let r = OFFER.lock().unwrap().clone();
                    if let Some(r) = r {
                        offer(h, r);
                    }
                }
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            command(h, w & 0xFFFF);
            0
        }
        WM_TIMER if w == TIMER_WIRED => {
            unsafe { KillTimer(h, TIMER_WIRED) };
            let h = h as isize;
            std::thread::spawn(move || {
                if let Some(text) = wired_handover() {
                    notify(h as HWND, NIM_MODIFY, Some(&text));
                }
            });
            0
        }
        WM_TIMER => {
            tick(h);
            0
        }
        // A device came or went: look 1.5 s after the last change (plugging in sends a burst).
        WM_DEVICECHANGE if w == DBT_DEVNODES_CHANGED && WIRED.load(Ordering::Relaxed) => {
            unsafe { SetTimer(h, TIMER_WIRED, 1500, None) };
            1
        }
        WM_DESTROY => unsafe {
            notify(h, NIM_DELETE, None);
            PostQuitMessage(0);
            0
        },
        // Explorer restarted (or started after us): the icon must be added again.
        _ if m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            SHOWN.store(u32::MAX, Ordering::Relaxed);
            tick(h);
            0
        }
        _ => unsafe { DefWindowProcW(h, m, w, l) },
    }
}

/// The tray's own window: never shown, it only receives the icon's and the menu's messages.
fn window() -> HWND {
    unsafe {
        let inst = GetModuleHandleW(null());
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = inst;
        wc.lpszClassName = w!("WinSenseTray");
        RegisterClassW(&wc);
        CreateWindowExW(0, w!("WinSenseTray"), w!("WinSense"), WS_OVERLAPPED, 0, 0, 0, 0, null_mut(), null_mut(), inst, null())
    }
}

/// `dry`: the mouse prints instead of moving the cursor and the worker prints instead of writing.
pub fn run(dry: bool) {
    let lock = if dry { Some(null_mut()) } else { claim() }; // a dry tray never writes: no lock (self-tests next to the real one)
    let Some(lock) = lock else {
        unsafe { MessageBoxW(null_mut(), w!("WinSense is already running. Its icon is in the tray (maybe behind the ^ arrow)."), w!("WinSense"), MB_OK | MB_ICONINFORMATION) };
        return;
    };
    ui::IN_TRAY.store(true, Ordering::Relaxed);
    crate::update::tidy();
    DRY.store(dry, Ordering::Relaxed);
    if !dry {
        LAST_LOOK.store(dword(w!("LastUpdateCheck")), Ordering::Relaxed);
    }
    UPDATES.store(true, Ordering::Relaxed);
    if !dry {
        crate::sound::ON.store(setting(w!("SoundHaptics")), Ordering::Relaxed); // before the haptic thread starts
    }
    let (tx, worker) = ui::start(dry);
    let desk = tx.clone();
    crate::port::start(tx, dry); // mods' lines reach the worker (src/port.rs); Quit lets go of it
    let mouse = std::thread::spawn(move || mouse::run(&STOP, dry));
    let voice = std::thread::spawn(move || crate::voice::run(&STOP, dry)); // dings and the hold-Mute command
    if !dry {
        crate::desk::LIGHT_ON.store(setting(w!("MicLight")), Ordering::Relaxed);
        crate::desk::BUZZ_ON.store(setting(w!("Buzz")), Ordering::Relaxed);
    }
    let desk = std::thread::spawn(move || crate::desk::run(&STOP, desk, dry)); // mic light and notification buzz
    ICON.set(icon(true));
    dark_menus();
    let h = window();
    let keys = crate::osk::create(); // the on-screen keyboard, hidden until couch mode opens it
    tick(h);
    unsafe {
        SetTimer(h, 1, 1000, None);
        if !dry {
            IDLE_ON.store(setting(w!("IdleOff")), Ordering::Relaxed);
            crate::couch::ON.store(setting(w!("Couch")), Ordering::Relaxed);
            WIRED.store(true, Ordering::Relaxed);
            SetTimer(h, TIMER_WIRED, 1500, None); // already on both when the tray starts
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    STOP.store(true, Ordering::Relaxed);
    let _ = mouse.join(); // lets go of a held mouse button
    unsafe { DestroyWindow(keys) };
    let _ = voice.join(); // closes an open mic
    let _ = desk.join(); // puts out a mute light it lit, while the worker is still there
    crate::port::close();
    ui::stop(worker); // closes the screen; triggers and rumble off if anything was sent
    if !lock.is_null() {
        unsafe { CloseHandle(lock) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cable_and_bluetooth_match_by_address() {
        let mut f = [0u8; 20];
        f[0] = 0x09;
        f[1..7].copy_from_slice(&[0xab, 0x89, 0x67, 0x45, 0x23, 0x01]); // lowest byte first
        assert_eq!(pairing_address(&f), Some(0x0123456789ab), "the same number as its Bluetooth serial 0123456789ab");
        assert_eq!(pairing_address(&[0x09, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(pairing_address(&f[..5]), None);
        f[0] = 0x05;
        assert_eq!(pairing_address(&f), None);
    }

    #[test]
    fn tip_and_menu_show_the_battery() {
        assert_eq!(tip(0, true, false, Some((8, 0))), "WinSense: No DualSense connected", "nothing stale once the controller is gone");
        assert_eq!(tip(1, true, false, None), "WinSense: Mouse off (hold the touchpad down to turn it on)\nBattery shows after you hold the touchpad down");
        assert_eq!(tip(1, true, false, Some((8, 0))), "WinSense: Mouse off (hold the touchpad down to turn it on)\nBattery 85%", "the cable, or someone else switched the report");
        assert_eq!(tip(2, true, false, Some((3, 1))), "WinSense: Touchpad mouse on\nBattery 35%, charging");
        // It once kept saying the mouse was on after it was turned off.
        assert_eq!(tip(2, false, false, Some((3, 0))), "WinSense: Mouse off (hold the touchpad down to turn it on)\nBattery 35%", "mouse off again: the battery stays");
        assert_eq!(status(2, false, true), "Gyro mouse on");
        assert_eq!(status(2, true, true), "Touchpad and gyro mouse on");
        assert_eq!(battery_line(2, None).as_deref(), Some("Battery unknown"));
        for b in (0..=10).flat_map(|l| [0, 1, 2, 0xA, 0xF].map(|s| Some((l, s)))) {
            assert!(tip(1, true, true, b).encode_utf16().count() < 128, "fits the tooltip: {}", tip(1, true, true, b));
        }
    }

    /// The idle switch is off unless ticked, and ticking it starts the count afresh. No controller
    /// and no registry are touched (WIRED is off in tests).
    #[test]
    fn idle_switch_is_off_until_ticked() {
        let h = window();
        assert!(!IDLE_ON.load(Ordering::Relaxed), "off by default");
        power::RESET.store(false, Ordering::Relaxed);
        unsafe { SendMessageW(h, WM_COMMAND, ID_IDLE, 0) };
        assert!(IDLE_ON.load(Ordering::Relaxed) && power::RESET.load(Ordering::Relaxed));
        unsafe { SendMessageW(h, WM_COMMAND, ID_IDLE, 0) };
        assert!(!IDLE_ON.load(Ordering::Relaxed));
        power::RESET.store(false, Ordering::Relaxed);
        unsafe { DestroyWindow(h) };
    }

    /// Couch mode is off unless ticked; no registry (WIRED is off in tests).
    #[test]
    fn couch_switch_is_off_until_ticked() {
        let h = window();
        assert!(!crate::couch::ON.load(Ordering::Relaxed), "off by default");
        unsafe { SendMessageW(h, WM_COMMAND, ID_COUCH, 0) };
        assert!(crate::couch::ON.load(Ordering::Relaxed));
        unsafe { SendMessageW(h, WM_COMMAND, ID_COUCH, 0) };
        assert!(!crate::couch::ON.load(Ordering::Relaxed));
        unsafe { DestroyWindow(h) };
    }

    /// The mic light and the notification buzz are off unless ticked; no registry (WIRED is off in tests).
    #[test]
    fn desk_switches_are_off_until_ticked() {
        let h = window();
        for (id, flag) in [(ID_MICLIGHT, &crate::desk::LIGHT_ON), (ID_BUZZ, &crate::desk::BUZZ_ON)] {
            assert!(!flag.load(Ordering::Relaxed), "off by default");
            unsafe { SendMessageW(h, WM_COMMAND, id, 0) };
            assert!(flag.load(Ordering::Relaxed));
            unsafe { SendMessageW(h, WM_COMMAND, id, 0) };
            assert!(!flag.load(Ordering::Relaxed));
        }
        unsafe { DestroyWindow(h) };
    }

    /// Game sound as haptics is off unless ticked; no registry (WIRED is off in tests), no port.
    #[test]
    fn sound_switch_is_off_until_ticked() {
        let h = window();
        assert!(!crate::sound::ON.load(Ordering::Relaxed), "off by default");
        unsafe { SendMessageW(h, WM_COMMAND, ID_SOUND, 0) };
        assert!(crate::sound::ON.load(Ordering::Relaxed));
        unsafe { SendMessageW(h, WM_COMMAND, ID_SOUND, 0) };
        assert!(!crate::sound::ON.load(Ordering::Relaxed));
        unsafe { DestroyWindow(h) };
    }

    /// The real tray window (no icon added), driven by the menu's WM_COMMAND.
    #[test]
    fn menu_switches_the_mouse() {
        let h = window();
        assert!(!h.is_null());
        let (touch, gyro) = (mouse::TOUCH_ON.load(Ordering::Relaxed), mouse::GYRO_ON.load(Ordering::Relaxed));
        mouse::WAKE.store(false, Ordering::Relaxed);
        unsafe { SendMessageW(h, WM_COMMAND, ID_GYRO, 0) };
        assert_eq!(mouse::GYRO_ON.load(Ordering::Relaxed), !gyro);
        assert_eq!(mouse::WAKE.load(Ordering::Relaxed), !gyro, "switching on asks for the full report");
        unsafe { SendMessageW(h, WM_COMMAND, ID_GYRO, 0) };
        unsafe { SendMessageW(h, WM_COMMAND, ID_TOUCH, 0) };
        assert_eq!(mouse::TOUCH_ON.load(Ordering::Relaxed), !touch);
        unsafe { SendMessageW(h, WM_COMMAND, ID_TOUCH, 0) };
        assert_eq!((mouse::TOUCH_ON.load(Ordering::Relaxed), mouse::GYRO_ON.load(Ordering::Relaxed)), (touch, gyro));
        unsafe { DestroyWindow(h) };
    }
}
