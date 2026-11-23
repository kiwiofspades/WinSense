//! Couch mode: the controller as a desktop remote (tray toggle, off unless ticked). Reads only;
//! acts through SendInput like the touchpad mouse, and runs on the mouse thread, which already
//! reads every report. Sticks, d-pad and buttons are all in the short Bluetooth report too, so
//! it works before the mouse is woken and plain readers keep working.
//! Mapping (Steam Deck style, like the mouse): left stick = cursor, right
//! stick = scroll, R2 = left click, L2 = right click, d-pad = arrow keys, Cross = Enter,
//! Circle = Escape, Square = the on-screen keyboard (src/osk.rs), Triangle = Task View,
//! L1 / R1 = browser back / forward, Options = Start. PS is left to Windows and Steam.
//! With the keyboard open: d-pad or left stick = highlight, Cross = type, Square = backspace,
//! Triangle = space, L1 = shift, R1 = symbols, Options or R2 = Enter (and close), Circle = close.
//! It must never type into a game. Two rules (`quiet_for`, `Couch::paused`): quiet while the
//! program in front reads a controller itself, cannot be looked into, or is fullscreen (fills its
//! monitor with no title bar) and is not a browser; never for programs under the Windows folder
//! or WinSense itself. And holding Create for a second pauses or resumes it by hand (for a
//! windowed game the first rule cannot see).

use crate::ds::State;
use crate::osk::{Board, Dir, Typed};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// The tray's "Couch mode" item.
pub static ON: AtomicBool = AtomicBool::new(false);
/// What couch mode is doing, for the tray tip: 0 off or no controller, `ACTING`, `QUIET`
/// (a game is in front: `IN_FRONT` says which and why, see `reason`), `PAUSED` (by hand).
pub static WHY: AtomicU8 = AtomicU8::new(0);
pub const ACTING: u8 = 1;
pub const QUIET: u8 = 2;
pub const PAUSED: u8 = 3;
pub static IN_FRONT: Mutex<String> = Mutex::new(String::new());
/// Voice said "couch mode on" while it already was: start afresh (this lifts a pause).
pub static FRESH: AtomicBool = AtomicBool::new(false);

// All first guesses, not tried in the hand.
/// Stick travel (of 1.0) that counts as rest.
const DEAD: f32 = 0.12;
/// Cursor pixels per second at full tilt; the curve is tilt squared, so half tilt is a quarter of it.
const SPEED: f32 = 1400.0;
/// Wheel units per second at full tilt (120 = one notch). Sent in whole notches only: many
/// programs scroll by delta / 120 lines per message, so a stream of small deltas scrolled nothing.
const SCROLL: f32 = 2400.0;
/// A held direction repeats after this long, then this often.
const FIRST: f32 = 0.4;
const NEXT: f32 = 0.09;
/// Create held this long, with no game in front, pauses or resumes couch mode.
const HOLD: f32 = 1.0;

const SQUARE: u8 = 0x10; // buttons[0]; its low nibble is the d-pad
const CROSS: u8 = 0x20;
const CIRCLE: u8 = 0x40;
const TRIANGLE: u8 = 0x80;
const L1: u8 = 0x01; // buttons[1]
const R1: u8 = 0x02;
const L2: u8 = 0x04;
const R2: u8 = 0x08;
const CREATE: u8 = 0x10;
const OPTIONS: u8 = 0x20;
/// The buttons that are keys on the desktop: (buttons byte, bit, keys pressed together).
const KEYS: [(usize, u8, &[u16]); 6] = [(0, CROSS, &[VK_RETURN]), (0, CIRCLE, &[VK_ESCAPE]), (0, TRIANGLE, &[VK_LWIN, VK_TAB]), (1, L1, &[VK_BROWSER_BACK]), (1, R1, &[VK_BROWSER_FORWARD]), (1, OPTIONS, &[VK_LWIN])];

#[derive(Debug, PartialEq, Clone)]
pub enum Act {
    Move(i32, i32),
    /// Wheel units: (sideways, up).
    Scroll(i32, i32),
    Button { right: bool, down: bool },
    /// Press these keys in order, let go in reverse.
    Tap(&'static [u16]),
    Text(String),
    /// A clip for the controller's speaker.
    Sound(u8),
}

/// A held direction: at once, then again and again like a held key.
#[derive(Default)]
struct Repeat {
    dir: Option<Dir>,
    wait: f32,
}

impl Repeat {
    fn step(&mut self, dir: Option<Dir>, dt: f32) -> Option<Dir> {
        if dir != self.dir {
            (self.dir, self.wait) = (dir, FIRST);
            return dir;
        }
        self.wait -= dt;
        if dir.is_some() && self.wait <= 0.0 {
            self.wait += NEXT;
            return dir;
        }
        None
    }
}

#[derive(Default)]
pub struct Couch {
    last: [u8; 3],
    /// Sub-pixel cursor movement and sub-unit scrolling left over.
    rem: [f32; 2],
    wheel: [f32; 2],
    rep: Repeat,
    /// How long Create has been held (minus infinity: a hold that must not count).
    hold: f32,
    /// Paused by hand (hold Create); not remembered.
    pub paused: bool,
    /// Mouse buttons held by R2 (left) and L2 (right).
    down: [bool; 2],
    pub board: Option<Board>,
}

/// A stick axis byte as -1..1 (128 = rest).
fn unit(v: u8) -> f32 {
    (v as f32 - 128.0) / 127.0
}

/// A stick as (x, y) in -1..1 with the rest zone cut out and the tilt squared.
fn curve(x: u8, y: u8) -> (f32, f32) {
    let (x, y) = (unit(x), unit(y));
    let m = x.hypot(y);
    if m < DEAD {
        return (0.0, 0.0);
    }
    let t = ((m - DEAD) / (1.0 - DEAD)).min(1.0);
    (x / m * t * t, y / m * t * t)
}

/// Whole steps out of a running sum, keeping the rest.
fn whole(rem: &mut [f32; 2], add: (f32, f32)) -> (i32, i32) {
    (rem[0], rem[1]) = (rem[0] + add.0, rem[1] + add.1);
    let n = (rem[0].trunc(), rem[1].trunc());
    (rem[0], rem[1]) = (rem[0] - n.0, rem[1] - n.1);
    (n.0 as i32, n.1 as i32)
}

/// Tell Windows about every mouse button that changed: `down` = [left, right] as Windows has
/// them. Shared with the touchpad mouse.
pub(crate) fn buttons(down: &mut [bool; 2], want: [bool; 2]) -> Vec<Act> {
    let out = (0..2).filter(|&i| want[i] != down[i]).map(|i| Act::Button { right: i == 1, down: want[i] }).collect();
    *down = want;
    out
}

/// The d-pad (low nibble of buttons[0]) as a direction; rest and diagonals are none.
fn hat(b0: u8) -> Option<Dir> {
    match b0 & 0x0F {
        0 => Some(Dir::Up),
        2 => Some(Dir::Right),
        4 => Some(Dir::Down),
        6 => Some(Dir::Left),
        _ => None,
    }
}

impl Couch {
    /// A fresh start with these buttons already held: none of them fires (a held d-pad repeats
    /// only after the first delay, a held Create starts no pause count).
    fn held(b: [u8; 3]) -> Couch {
        let hold = if b[1] & CREATE != 0 { f32::NEG_INFINITY } else { 0.0 };
        Couch { last: b, hold, rep: Repeat { dir: hat(b[0]), wait: FIRST }, ..Couch::default() }
    }

    fn buttons(&mut self, want: [bool; 2]) -> Vec<Act> {
        buttons(&mut self.down, want)
    }

    /// Let go of held mouse buttons and close the keyboard (quiet, switched off, controller gone).
    pub fn release(&mut self) -> Vec<Act> {
        self.board = None;
        self.buttons([false; 2])
    }

    /// One report in, desktop actions out. `dt` = seconds since the last report, `gate` = a game
    /// is in front (`front`).
    pub fn step(&mut self, s: &State, dt: f32, gate: bool) -> Vec<Act> {
        let (b, was) = (s.buttons, self.last);
        self.last = b;
        let hit = |i: usize, m: u8| b[i] & m != 0 && was[i] & m == 0;
        let mut out = Vec::new();
        // Create held for a second: pause / resume. Not while a game is in front: games use a held
        // Create (map, scores, emulator hotkeys), and that used to pause couch mode unseen.
        if b[1] & CREATE == 0 {
            self.hold = 0.0;
        } else if gate {
            self.hold = f32::NEG_INFINITY; // a hold begun in a game never counts
        } else {
            let before = self.hold;
            self.hold += dt;
            if before < HOLD && self.hold >= HOLD {
                self.paused = !self.paused;
                out.push(Act::Sound(if self.paused { crate::voice::OFF } else { crate::voice::DING }));
            }
        }
        if gate || self.paused {
            out.extend(self.release());
            self.rep = Repeat::default();
            return out;
        }
        let scroll = curve(s.sticks[2], s.sticks[3]);
        let n = whole(&mut self.wheel, (scroll.0 * SCROLL / 120.0 * dt, -scroll.1 * SCROLL / 120.0 * dt));
        if n != (0, 0) {
            out.push(Act::Scroll(n.0 * 120, n.1 * 120));
        }
        let hat = hat(b[0]);
        let (pulled, clicked) = ([b[1] & R2 != 0, b[1] & L2 != 0], [hit(1, R2), hit(1, L2)]);
        if let Some(bd) = &mut self.board {
            // The left stick pushed past half way picks a direction, like the d-pad.
            let (x, y) = (unit(s.sticks[0]), unit(s.sticks[1]));
            let stick = (x.abs().max(y.abs()) > 0.5).then(|| match (x.abs() > y.abs(), x > 0.0, y > 0.0) {
                (true, true, _) => Dir::Right,
                (true, false, _) => Dir::Left,
                (false, _, true) => Dir::Down,
                (false, _, false) => Dir::Up,
            });
            if let Some(d) = self.rep.step(hat.or(stick), dt) {
                bd.go(d);
            }
            let mut typed = Vec::new();
            if hit(0, CROSS) {
                typed.extend(bd.press());
            }
            if hit(0, SQUARE) {
                typed.push(Typed::Back);
            }
            if hit(0, TRIANGLE) {
                typed.push(Typed::Text(" ".into()));
            }
            if hit(1, L1) {
                bd.shift = !bd.shift;
            }
            if hit(1, R1) {
                bd.sym = !bd.sym;
            }
            if hit(1, OPTIONS) || clicked[0] {
                typed.push(Typed::Enter);
            }
            // Enter also closes: search boxes and address bars are the couch case, and Square reopens it.
            let close = hit(0, CIRCLE) || typed.contains(&Typed::Enter);
            out.extend(typed.into_iter().map(|t| match t {
                Typed::Text(t) => Act::Text(t),
                Typed::Back => Act::Tap(&[VK_BACK]),
                Typed::Enter => Act::Tap(&[VK_RETURN]),
            }));
            if close {
                self.board = None;
            }
            out.extend(self.buttons([false; 2]));
            return out;
        }
        let stick = curve(s.sticks[0], s.sticks[1]);
        let n = whole(&mut self.rem, (stick.0 * SPEED * dt, stick.1 * SPEED * dt));
        if n != (0, 0) {
            out.push(Act::Move(n.0, n.1));
        }
        // A trigger clicks when pulled, and holds until let go (so dragging works).
        let want = std::array::from_fn(|i| pulled[i] && (self.down[i] || clicked[i]));
        out.extend(self.buttons(want));
        if let Some(d) = self.rep.step(hat, dt) {
            out.push(Act::Tap(match d {
                Dir::Up => &[VK_UP],
                Dir::Right => &[VK_RIGHT],
                Dir::Down => &[VK_DOWN],
                Dir::Left => &[VK_LEFT],
            }));
        }
        out.extend(KEYS.iter().filter(|k| hit(k.0, k.1)).map(|k| Act::Tap(k.2)));
        if hit(0, SQUARE) {
            self.board = Some(Board::new());
        }
        out
    }
}

/// Why couch mode keeps quiet for the program in front.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quiet {
    /// It reads a controller itself: holds a game controller open (or a child of it does), or
    /// loaded XInput, DirectInput or GameInput.
    Reads,
    /// It covers its whole monitor with no title bar: a game that reads the controller in a way
    /// that cannot be seen from outside (through Steam Input, say).
    Fullscreen,
    /// Windows will not let WinSense look inside it (run as administrator, or protected by
    /// anti-cheat). Keys could not reach an administrator's window anyway.
    Closed,
}

/// What the tray's grey line says after "Couch mode waits: ".
pub fn reason(q: Quiet, exe: &str) -> String {
    let exe = if exe.is_empty() { "the program in front" } else { exe };
    match q {
        Quiet::Reads => format!("{exe} reads the controller"),
        Quiet::Fullscreen => format!("{exe} is full screen"),
        Quiet::Closed => format!("{exe} cannot be looked into"),
    }
}

/// Browsers: a fullscreen video still wants Escape and the cursor, and a page that once used the
/// gamepad leaves XInput loaded for good, so only a controller they hold open right now counts.
const BROWSERS: &[&str] = &["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "zen.exe"];
/// Modules only a controller reader loads (prefixes, lower case). MEASURED on a whole PC
/// (2026-11-22, 388 processes): xinput and dinput8 only in steam.exe, XboxService and WinSense,
/// gameinput only in Xbox services. hid.dll, SDL and Windows.Gaming.Input are no sign: Explorer,
/// iCUE and Steam's web helper load them and read no controller.
const PAD_DLLS: &[&str] = &["xinput", "dinput8.dll", "dinput.dll", "gameinput"];

/// Whether couch mode keeps quiet for the program in front, and why. `fullscreen` = its window
/// covers its monitor with no title bar, `exe` = its full path, `reads` = whether it reads a
/// controller (None: Windows would not say). Windows' own programs (the desktop, Start, Task
/// View, TextInputHost's see-through full-screen window) and WinSense itself never count.
pub fn quiet_for(fullscreen: bool, exe: &str, reads: Option<bool>) -> Option<Quiet> {
    let exe = exe.to_lowercase();
    if !exe.is_empty() && (exe.starts_with(&dirs().0) || exe == dirs().1) {
        return None;
    }
    match reads {
        None => Some(Quiet::Closed),
        Some(true) => Some(Quiet::Reads),
        Some(false) if fullscreen && !browser(&exe) => Some(Quiet::Fullscreen),
        _ => None,
    }
}

fn file_name(exe: &str) -> &str {
    exe.rsplit('\\').next().unwrap_or("")
}

fn browser(exe: &str) -> bool {
    BROWSERS.contains(&file_name(exe).to_lowercase().as_str())
}

/// The Windows folder ("c:\windows\") and WinSense's own path, lower case.
fn dirs() -> &'static (String, String) {
    static D: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let own = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
        (win.trim_end_matches('\\').to_lowercase() + "\\", own)
    })
}

/// What was last found about the window in front.
struct Seen {
    pid: u32,
    exe: String,
    fullscreen: bool,
    reads: Option<bool>,
    /// When it came to the front, was last looked at, and was last looked inside.
    came: Instant,
    looked: Instant,
    inside: Option<Instant>,
    /// A look inside never came back: it is not tried again.
    stuck: bool,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

/// Couch mode's verdict for the window in front right now, and its program's file name.
/// A program that comes to the front is looked at at once (a few ms), so nothing is let go or
/// missed on a switch. After that its window is looked at 4 times a second and inside it 4 times
/// a second for its first 3 seconds (a game may open the controller after its window shows),
/// then every 2 seconds. Between looks this is a lock and two small calls.
pub(crate) fn front() -> (Option<Quiet>, String) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let h = unsafe { GetForegroundWindow() };
    let pid = if h.is_null() { 0 } else { look::owner(h) };
    if pid == 0 || pid == std::process::id() {
        return (None, String::new());
    }
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let same = seen.as_ref().is_some_and(|s| s.pid == pid);
    if !same || seen.as_ref().is_some_and(|s| s.looked.elapsed() >= Duration::from_millis(250)) {
        let (exe, fullscreen) = look::window(h);
        match seen.as_mut() {
            // The same program (a pid used again by a new program has another path).
            Some(s) if same && s.exe == exe => (s.fullscreen, s.looked) = (fullscreen, now),
            _ => *seen = Some(Seen { pid, exe, fullscreen, reads: Some(false), came: now, looked: now, inside: None, stuck: false }),
        }
    }
    let s = seen.as_mut().expect("set above");
    let every = Duration::from_millis(if s.came.elapsed() < Duration::from_secs(3) { 250 } else { 2000 });
    if !s.stuck && s.inside.is_none_or(|t| t.elapsed() >= every) {
        // Windows' own programs and WinSense never count: no need to look inside.
        s.reads = if quiet_for(false, &s.exe, None).is_none() {
            Some(false)
        } else {
            // On a thread of its own: a device Windows never answers about would otherwise freeze
            // the mouse thread. Measured looks take 2-17 ms; one that takes 500 ms counts as "cannot
            // be looked into" for as long as that program stays in front, and is not tried again
            // (its thread is left waiting, one per such program).
            let (tx, rx) = std::sync::mpsc::channel();
            let browser = browser(&s.exe);
            std::thread::spawn(move || tx.send(look::reads(pid, browser)));
            rx.recv_timeout(Duration::from_millis(500)).unwrap_or_else(|_| {
                s.stuck = true;
                None
            })
        };
        s.inside = Some(now);
    }
    (quiet_for(s.fullscreen, &s.exe, s.reads), file_name(&s.exe).to_string())
}

/// `front` as a yes or no: a game is in front.
pub(crate) fn gate() -> bool {
    front().0.is_some()
}

/// Looking at other programs. Reads only: nothing is ever written to another process or device.
/// The handle and name layouts below are the 64-bit ones.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("couch::look reads 64-bit handle lists");
mod look {
    use super::PAD_DLLS;
    use std::collections::HashSet;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::core::GUID;

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(p: HANDLE, class: u32, out: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32;
        fn NtQueryObject(h: HANDLE, class: u32, out: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32;
    }
    const HID: GUID = GUID::from_u128(0x4D1E55B2_F16F_11CF_88CB_001111000030);
    const XUSB: GUID = GUID::from_u128(0xEC87F1E3_C13B_4100_B5F7_8B84D54260CB);

    /// The process behind a window. A Store app's window belongs to ApplicationFrameHost (in the
    /// Windows folder, so it would never count): the app itself owns the CoreWindow inside it.
    pub fn owner(h: HWND) -> u32 {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let mut pid = 0;
        unsafe {
            let mut class = [0u16; 32];
            let n = GetClassNameW(h, class.as_mut_ptr(), 32) as usize;
            let inner = if String::from_utf16_lossy(&class[..n]) == "ApplicationFrameWindow" {
                FindWindowExW(h, null_mut(), windows_sys::w!("Windows.UI.Core.CoreWindow"), null())
            } else {
                null_mut()
            };
            GetWindowThreadProcessId(if inner.is_null() { h } else { inner }, &mut pid);
        }
        pid
    }

    /// The window's program's full path ("" if unreadable) and whether the window is fullscreen
    /// (covers its whole monitor with no title bar).
    pub fn window(h: HWND) -> (String, bool) {
        use windows_sys::Win32::Graphics::Gdi::*;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            let (mut r, mut mi): (RECT, MONITORINFO) = (std::mem::zeroed(), std::mem::zeroed());
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            GetWindowRect(h, &mut r);
            GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi);
            let m = mi.rcMonitor;
            // A maximised window with a title bar can fill it too, with the taskbar hidden. A maximised
            // caption-less window counts: some games run that way, and keys into a game are worse
            // than a quiet couch mode.
            let fullscreen = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom && GetWindowLongW(h, GWL_STYLE) as u32 & WS_CAPTION != WS_CAPTION;
            let (mut path, mut len) = ([0u16; 520], 520u32);
            let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, owner(h));
            if p.is_null() || QueryFullProcessImageNameW(p, 0, path.as_mut_ptr(), &mut len) == 0 {
                len = 0;
            }
            if !p.is_null() {
                CloseHandle(p);
            }
            (String::from_utf16_lossy(&path[..len as usize]), fullscreen)
        }
    }

    /// Whether the process reads a controller: a controller module loaded (not counted for
    /// browsers), or a game controller held open by it or its WebView2. None when Windows will
    /// not let us look.
    pub fn reads(pid: u32, browser: bool) -> Option<bool> {
        let m = modules(pid)?;
        if !browser && m.iter().any(|m| PAD_DLLS.iter().any(|d| m.starts_with(d))) {
            return Some(true);
        }
        let pads = pads();
        if pads.is_empty() {
            return Some(false); // no controller connected: nothing to hold open
        }
        let mut any = holds(pid, &pads)?;
        for c in webviews(pid) {
            any = any || holds(c, &pads) == Some(true);
        }
        Some(any)
    }

    /// Lower-case names of the modules loaded in the process; None when it cannot be read.
    fn modules(pid: u32) -> Option<Vec<String>> {
        use windows::Win32::System::Diagnostics::ToolHelp::*;
        let mut v = Vec::new();
        unsafe {
            // A program still loading can answer "bad length" once or twice: ask again.
            let s = (0..3).find_map(|_| CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid).ok())?;
            let mut e: MODULEENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<MODULEENTRY32W>() as u32;
            let mut ok = Module32FirstW(s, &mut e).is_ok();
            while ok {
                let n = &e.szModule;
                v.push(String::from_utf16_lossy(&n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())]).to_lowercase());
                ok = Module32NextW(s, &mut e).is_ok();
            }
            let _ = windows::Win32::Foundation::CloseHandle(s);
        }
        (!v.is_empty()).then_some(v)
    }

    /// The process's WebView2 children: an app built on WebView2 (RetroShelf is) reads the gamepad
    /// in its msedgewebview2.exe. Not every child: MEASURED, Explorer then "read the controller"
    /// through Steam and WinSense it had started, and any launcher would too.
    fn webviews(pid: u32) -> Vec<u32> {
        use windows::Win32::System::Diagnostics::ToolHelp::*;
        let mut v = Vec::new();
        unsafe {
            let Ok(s) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return v };
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(s, &mut e).is_ok();
            while ok {
                let n = &e.szExeFile;
                let name = String::from_utf16_lossy(&n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())]);
                if e.th32ParentProcessID == pid && e.th32ProcessID != pid && name.eq_ignore_ascii_case("msedgewebview2.exe") {
                    v.push(e.th32ProcessID);
                }
                ok = Process32NextW(s, &mut e).is_ok();
            }
            let _ = windows::Win32::Foundation::CloseHandle(s);
        }
        v
    }

    /// Kernel names (\Device\000000d1) of every connected game controller: HID collections
    /// Windows marks as a joystick or gamepad, and Xbox (XUSB) controllers.
    pub fn pads() -> HashSet<String> {
        let mut set = HashSet::new();
        for (g, xbox) in [(HID, false), (XUSB, true)] {
            unsafe {
                let mut len = 0u32;
                if CM_Get_Device_Interface_List_SizeW(&mut len, &g, null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || len <= 1 {
                    continue;
                }
                let mut list = vec![0u16; len as usize];
                if CM_Get_Device_Interface_ListW(&g, null(), list.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
                    continue;
                }
                for path in list.split(|&c| c == 0).filter(|p| !p.is_empty()) {
                    // \\?\HID#VID_054C&PID_0CE6#7&abc&0&0000#{guid} -> HID\VID_054C&PID_0CE6\7&abc&0&0000
                    let p = String::from_utf16_lossy(path);
                    let Some(id) = p.strip_prefix(r"\\?\").and_then(|r| r.rsplit_once('#')).map(|r| r.0.replace('#', "\\")) else { continue };
                    let wid: Vec<u16> = id.encode_utf16().chain([0]).collect();
                    let mut dn = 0u32;
                    if CM_Locate_DevNodeW(&mut dn, wid.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
                        continue;
                    }
                    let prop = |what: u32| {
                        let (mut buf, mut n) = ([0u16; 1024], 2048u32);
                        if CM_Get_DevNode_Registry_PropertyW(dn, what, null_mut(), buf.as_mut_ptr() as _, &mut n, 0) != CR_SUCCESS {
                            return String::new();
                        }
                        String::from_utf16_lossy(&buf[..(n as usize / 2).min(1024)])
                    };
                    if xbox || prop(CM_DRP_HARDWAREID).contains("HID_DEVICE_SYSTEM_GAME") {
                        let pdo = prop(CM_DRP_PHYSICAL_DEVICE_OBJECT_NAME);
                        set.insert(pdo.trim_end_matches('\0').to_string());
                    }
                }
            }
        }
        set
    }

    #[repr(C)]
    struct Entry {
        handle: usize,
        count: usize,
        pointers: usize,
        access: u32,
        kind: u32,
        attrs: u32,
        _r: u32,
    }

    /// The process's handle list (ProcessHandleInformation, Windows 8 and later).
    /// u64 words, so the entries are aligned.
    fn handles(p: HANDLE) -> Option<Vec<u64>> {
        let mut buf = vec![0u64; 1 << 13];
        loop {
            let mut ret = 0;
            let st = unsafe { NtQueryInformationProcess(p, 51, buf.as_mut_ptr() as _, (buf.len() * 8) as u32, &mut ret) };
            match st as u32 {
                0 => return Some(buf),
                0xC0000004 if buf.len() < 1 << 23 => buf.resize((ret as usize / 8 + 1).max(buf.len() * 2), 0), // too small
                _ => return None,
            }
        }
    }

    fn entries(buf: &[u64]) -> &[Entry] {
        // The list starts with its count and a reserved word.
        let n = (buf[0] as usize).min((buf.len() - 2) * 8 / size_of::<Entry>());
        unsafe { std::slice::from_raw_parts(buf.as_ptr().add(2) as *const Entry, n) }
    }

    /// The handle type number of a file on the running machine (it differs between Windows versions).
    fn file_kind() -> u32 {
        static K: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *K.get_or_init(|| {
            use std::os::windows::io::AsRawHandle;
            let Ok(f) = std::fs::File::open(std::env::current_exe().unwrap_or_default()) else { return u32::MAX };
            let h = f.as_raw_handle() as usize;
            handles(unsafe { GetCurrentProcess() }).and_then(|b| entries(&b).iter().find(|e| e.handle == h).map(|e| e.kind)).unwrap_or(u32::MAX)
        })
    }

    /// Whether the process holds one of `pads` open. None when Windows will not let us look.
    fn holds(pid: u32, pads: &HashSet<String>) -> Option<bool> {
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
        unsafe {
            let p = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE, 0, pid);
            if p.is_null() {
                return None;
            }
            let found = handles(p).map(|buf| {
                let file = file_kind();
                entries(&buf).iter().filter(|e| e.kind == file).any(|e| {
                    let mut d: HANDLE = null_mut();
                    if DuplicateHandle(p, e.handle as HANDLE, GetCurrentProcess(), &mut d, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
                        return false;
                    }
                    // Devices only. Asking a pipe its name can hang until someone writes to it (MEASURED:
                    // 12 handles of one Electron app never answered); asking its type answered at once, and a
                    // controller is "unknown" there, a pipe or a file is not.
                    let hit = GetFileType(d) == FILE_TYPE_UNKNOWN && name(d).is_some_and(|n| pads.contains(&n));
                    CloseHandle(d);
                    hit
                })
            });
            CloseHandle(p);
            found
        }
    }

    fn name(h: HANDLE) -> Option<String> {
        let mut buf = [0u64; 256]; // 8-byte aligned: a UNICODE_STRING, then its text
        let mut ret = 0;
        if unsafe { NtQueryObject(h, 1, buf.as_mut_ptr() as _, size_of_val(&buf) as u32, &mut ret) } != 0 {
            return None;
        }
        let len = (buf[0] & 0xFFFF) as usize / 2;
        let ptr = buf[1] as *const u16;
        let start = buf.as_ptr() as usize;
        // The text lies inside our buffer; refuse anything else.
        if ptr.is_null() || (ptr as usize) < start || ptr as usize + len * 2 > start + size_of_val(&buf) {
            return None;
        }
        Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }))
    }
}

/// Hand the actions to Windows (sounds and the keyboard are not input: skipped).
pub(crate) fn send(out: &[Act]) {
    let key = |vk: u16, scan: u16, flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
    let mouse = |dx, dy, data: i32, flags| INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
    // Arrow and Windows keys are "extended" keys; without the flag an arrow can arrive as a number-pad key.
    let ext = |vk: u16| if matches!(vk, VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_LWIN) { KEYEVENTF_EXTENDEDKEY } else { 0 };
    let mut v = Vec::new();
    for a in out {
        match a {
            Act::Move(x, y) => v.push(mouse(*x, *y, 0, MOUSEEVENTF_MOVE)),
            Act::Scroll(h, up) => {
                v.extend((*h != 0).then(|| mouse(0, 0, *h, MOUSEEVENTF_HWHEEL)));
                v.extend((*up != 0).then(|| mouse(0, 0, *up, MOUSEEVENTF_WHEEL)));
            }
            Act::Button { right, down } => v.push(mouse(0, 0, 0, match (right, down) {
                (false, true) => MOUSEEVENTF_LEFTDOWN,
                (false, false) => MOUSEEVENTF_LEFTUP,
                (true, true) => MOUSEEVENTF_RIGHTDOWN,
                (true, false) => MOUSEEVENTF_RIGHTUP,
            })),
            Act::Tap(keys) => {
                v.extend(keys.iter().map(|&k| key(k, 0, ext(k))));
                v.extend(keys.iter().rev().map(|&k| key(k, 0, ext(k) | KEYEVENTF_KEYUP)));
            }
            // As Unicode, so it does not depend on the keyboard layout.
            Act::Text(t) => v.extend(t.encode_utf16().flat_map(|u| [key(0, u, KEYEVENTF_UNICODE), key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)])),
            Act::Sound(_) => {}
        }
    }
    if !v.is_empty() {
        unsafe { SendInput(v.len() as u32, v.as_ptr(), size_of::<INPUT>() as i32) };
    }
}

/// Couch mode on the mouse thread: feeds `Couch` with every report while the tray item is ticked.
pub struct Runner {
    c: Couch,
    t: Instant,
    dry: bool,
    live: bool,
}

impl Runner {
    pub fn new(dry: bool) -> Runner {
        Runner { c: Couch::default(), t: Instant::now(), dry, live: false }
    }

    /// One report. True while couch mode is acting: the mouse then leaves the triggers to it.
    pub fn feed(&mut self, s: &State) -> bool {
        self.run(s, ON.load(Ordering::Relaxed))
    }

    fn run(&mut self, s: &State, on: bool) -> bool {
        let now = Instant::now();
        let dt = (now - self.t).as_secs_f32().min(0.05);
        self.t = now;
        if !on {
            self.stop();
            return false;
        }
        let fresh = FRESH.swap(false, Ordering::Relaxed); // always taken, so it cannot linger
        let was_live = std::mem::replace(&mut self.live, true);
        if fresh && was_live {
            let out = self.c.release(); // a drag in progress lets go before the fresh start
            self.act(&out);
        }
        if !was_live || fresh {
            // Just switched on (or a new connection): start from nothing, so a pause or anything else
            // left from before cannot outlive the switch. Buttons already held do not fire.
            self.c = Couch::held(s.buttons);
        }
        // A dry run acts on nothing, so it asks nothing.
        let (quiet, exe) = if self.dry { (None, String::new()) } else { front() };
        if let Some(q) = quiet {
            let why = reason(q, &exe);
            let mut f = IN_FRONT.lock().unwrap_or_else(|e| e.into_inner());
            if *f != why {
                *f = why;
            }
        }
        let out = self.c.step(s, dt, quiet.is_some());
        self.act(&out);
        crate::osk::show(self.c.board); // the keyboard window follows the board (no-op when unchanged)
        WHY.store(if self.c.paused { PAUSED } else if quiet.is_some() { QUIET } else { ACTING }, Ordering::Relaxed);
        !(quiet.is_some() || self.c.paused)
    }

    /// Switched off, or the controller went away: let go of everything.
    pub fn stop(&mut self) {
        WHY.store(0, Ordering::Relaxed);
        if std::mem::take(&mut self.live) {
            let out = self.c.release();
            self.act(&out);
            crate::osk::show(None);
        }
    }

    fn act(&self, out: &[Act]) {
        if out.is_empty() {
            return;
        }
        if self.dry {
            println!("couch: dry {out:?}");
            return;
        }
        // The pause ding only when the controller is already on the full report: playing a clip
        // switches a Bluetooth controller to it, and plain readers would go blind for a ding.
        if let Some(Act::Sound(id)) = out.iter().find(|a| matches!(a, Act::Sound(_))) && crate::mouse::STATE.load(Ordering::Relaxed) == 2 {
            crate::voice::SOUND.store(*id, Ordering::Relaxed);
        }
        // Circle while a monitor copy is open closes the copy instead of pressing Escape.
        let esc = Act::Tap(&[VK_ESCAPE]);
        if out.contains(&esc) && crate::show::OPEN.load(Ordering::Relaxed) && crate::show::close() {
            send(&out.iter().filter(|a| **a != esc).cloned().collect::<Vec<_>>());
            return;
        }
        send(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REST: [u8; 4] = [128; 4];
    fn st(sticks: [u8; 4], b0: u8, b1: u8) -> State {
        State { sticks, buttons: [b0, b1, 0], ..State::default() }
    }
    const NONE: u8 = 8; // the d-pad at rest
    const DT: f32 = 0.004;

    #[test]
    fn sticks_move_and_scroll_with_a_curve() {
        let mut c = Couch::default();
        assert_eq!(c.step(&st(REST, NONE, 0), DT, false), vec![]);
        assert_eq!(c.step(&st([136, 122, 120, 133], NONE, 0), DT, false), vec![], "a worn stick near the middle is rest");
        // Full right for a second: SPEED pixels, nothing lost to rounding.
        let run = |c: &mut Couch, sticks, n: usize| {
            let (mut mv, mut sc) = ((0, 0), (0, 0));
            for a in (0..n).flat_map(|_| c.step(&st(sticks, NONE, 0), DT, false)) {
                match a {
                    Act::Move(x, y) => mv = (mv.0 + x, mv.1 + y),
                    Act::Scroll(x, y) => sc = (sc.0 + x, sc.1 + y),
                    other => panic!("{other:?}"),
                }
            }
            (mv, sc)
        };
        let (mv, sc) = run(&mut c, [255, 128, 128, 128], 250);
        assert!((mv.0 - SPEED as i32).abs() <= 2 && mv.1 == 0 && sc == (0, 0), "{mv:?}");
        // Half tilt is far slower than half speed (fine aiming), and up is up.
        let (mv, _) = run(&mut c, [128, 64, 128, 128], 250);
        assert!(mv.0 == 0 && mv.1 < 0 && -mv.1 < SPEED as i32 / 4, "{mv:?}");
        // Right stick: up scrolls up (positive wheel), right scrolls right; the cursor stays.
        let (mv, sc) = run(&mut c, [128, 128, 128, 0], 250);
        assert!(mv == (0, 0) && sc.0 == 0 && (sc.1 - SCROLL as i32).abs() <= 120 && sc.1 % 120 == 0, "{sc:?}");
        let (_, sc) = run(&mut c, [128, 128, 255, 128], 250);
        assert!(sc.0 > 0 && sc.1 == 0, "{sc:?}");
    }

    #[test]
    fn buttons_become_keys_and_clicks() {
        let mut c = Couch::default();
        let mut press = |b0: u8, b1: u8| {
            let out = c.step(&st(REST, b0, b1), DT, false);
            assert_eq!(c.step(&st(REST, b0, b1), DT, false), vec![], "held: once");
            assert_eq!(c.step(&st(REST, NONE, 0), DT, false).iter().filter(|a| !matches!(a, Act::Button { down: false, .. })).count(), 0);
            out
        };
        let tap = |keys: &'static [u16]| vec![Act::Tap(keys)];
        assert_eq!(press(NONE | CROSS, 0), tap(&[VK_RETURN]));
        assert_eq!(press(NONE | CIRCLE, 0), tap(&[VK_ESCAPE]));
        assert_eq!(press(NONE | TRIANGLE, 0), tap(&[VK_LWIN, VK_TAB]));
        assert_eq!(press(NONE, L1), tap(&[VK_BROWSER_BACK]));
        assert_eq!(press(NONE, R1), tap(&[VK_BROWSER_FORWARD]));
        assert_eq!(press(NONE, OPTIONS), tap(&[VK_LWIN]));
        assert_eq!(press(NONE, R2), vec![Act::Button { right: false, down: true }]);
        assert_eq!(press(NONE, L2), vec![Act::Button { right: true, down: true }]);
        assert_eq!(press(NONE, 0x40 | 0x80), vec![], "L3 and R3 do nothing");
        assert_eq!(press(0, 0), tap(&[VK_UP]));
        assert_eq!(press(1, 0), vec![], "a diagonal is no arrow");
        // A held d-pad repeats like a held key: at once, after 0.4 s, then every 90 ms.
        let mut c = Couch::default();
        let at: Vec<usize> = (0..175).filter(|_| c.step(&st(REST, 4, 0), DT, false) == tap(&[VK_DOWN])).collect::<Vec<_>>();
        let n = (0..175).map(|_| c.step(&st(REST, 4, 0), DT, false)).filter(|o| !o.is_empty()).count();
        assert_eq!(at.len(), 1 + 4, "0.7 s held: the press, then 4 repeats ({at:?})");
        assert!((7..=8).contains(&n), "the next 0.7 s: {n} repeats");
        // R2 holds the left button until let go, so dragging works.
        let mut c = Couch::default();
        assert_eq!(c.step(&st(REST, NONE, R2), DT, false), vec![Act::Button { right: false, down: true }]);
        assert_eq!(c.step(&st([255, 128, 128, 128], NONE, R2), 0.0125, false), vec![Act::Move(17, 0)]);
        assert_eq!(c.step(&st(REST, NONE, 0), DT, false), vec![Act::Button { right: false, down: false }]);
    }

    #[test]
    fn quiet_in_a_game_and_paused_by_hand() {
        let mut c = Couch::default();
        // A game in front: nothing, whatever is pressed; a held trigger is let go first.
        assert_eq!(c.step(&st(REST, NONE, R2), DT, false), vec![Act::Button { right: false, down: true }]);
        assert_eq!(c.step(&st([255, 0, 0, 255], CROSS, R2 | L1), DT, true), vec![Act::Button { right: false, down: false }]);
        assert_eq!(c.step(&st([255, 0, 0, 255], 2 | TRIANGLE, R2 | OPTIONS), DT, true), vec![]);
        // Back on the desktop with the buttons still held: they do not fire now.
        assert_eq!(c.step(&st(REST, 2 | TRIANGLE, R2 | OPTIONS), DT, false), vec![Act::Tap(&[VK_RIGHT])], "only the d-pad, which is a held direction");
        c.step(&st(REST, NONE, 0), DT, false);
        // Create held for a second: paused, with the "off" clip, once.
        let hold = |c: &mut Couch, gate: bool, n: usize| (0..n).flat_map(|_| c.step(&st(REST, NONE, CREATE), DT, gate)).collect::<Vec<_>>();
        assert_eq!(hold(&mut c, false, 240), vec![], "0.96 s: not yet");
        assert_eq!(hold(&mut c, false, 500), vec![Act::Sound(crate::voice::OFF)]);
        assert!(c.paused);
        assert_eq!(c.step(&st([255, 128, 128, 128], NONE | CROSS, 0), DT, false), vec![], "paused: silent");
        assert_eq!(hold(&mut c, false, 300), vec![Act::Sound(crate::voice::DING)], "again: back on");
        assert!(!c.paused);
        c.step(&st(REST, NONE, 0), DT, false);
    }

    /// The cause in the code: a game in front, Create held there for a second (a map, a
    /// scoreboard, an emulator hotkey) paused couch mode with no sound (no clip on the short
    /// report), so the sticks stayed dead on the desktop afterwards.
    #[test]
    fn create_held_in_a_game_does_not_pause() {
        let mut c = Couch::default();
        let held: Vec<Act> = (0..300).flat_map(|_| c.step(&st(REST, NONE, CREATE), DT, true)).collect();
        assert_eq!(held, vec![], "1.2 s of Create in a game: nothing");
        // Still held when the desktop comes back: the hold begun in the game does not count.
        let held: Vec<Act> = (0..300).flat_map(|_| c.step(&st(REST, NONE, CREATE), DT, false)).collect();
        assert_eq!(held, vec![]);
        assert!(!c.paused);
        assert!(matches!(c.step(&st([255, 128, 128, 128], NONE, 0), 0.0125, false)[..], [Act::Move(17, 0)]), "the stick moves the cursor");
    }

    /// Switching couch mode off and on starts it afresh, whatever was left (here a pause);
    /// buttons held at that moment do not fire.
    #[test]
    fn off_and_on_starts_afresh() {
        let mut r = Runner::new(true);
        r.run(&st(REST, NONE, 0), true);
        r.c.paused = true;
        assert!(!r.run(&st(REST, NONE, 0), true), "paused");
        r.run(&st(REST, NONE, 0), false);
        assert!(r.run(&st(REST, NONE | CROSS, 0), true), "on again: acting, not paused");
        assert!(!r.c.paused);
        assert_eq!(r.c.step(&st(REST, NONE | CROSS, 0), DT, false), vec![], "Cross held while switching on: no Enter");
        // Voice "couch mode on" while on and paused: afresh, not paused.
        r.c.paused = true;
        FRESH.store(true, Ordering::Relaxed);
        assert!(r.run(&st(REST, NONE, 0), true) && !r.c.paused);
        // The d-pad and Create held at a fresh start: no arrow at once, no pause count.
        r.run(&st(REST, NONE, 0), false);
        r.run(&st(REST, 4, CREATE), true);
        let held: Vec<Act> = (0..300).flat_map(|_| r.c.step(&st(REST, 4, CREATE), DT, false)).collect();
        assert!(!r.c.paused && held.iter().all(|a| *a == Act::Tap(&[VK_DOWN])) && held.len() >= 5, "only the held arrow's repeats: {held:?}");
        r.run(&st(REST, NONE, 0), false);
    }

    #[test]
    fn the_keyboard_types_through_the_controller() {
        let mut c = Couch::default();
        let go = |c: &mut Couch, sticks: [u8; 4], b0: u8, b1: u8| {
            let out = c.step(&st(sticks, b0, b1), DT, false);
            c.step(&st(REST, NONE, 0), DT, false);
            out
        };
        assert_eq!(go(&mut c, REST, NONE | SQUARE, 0), vec![]);
        assert_eq!(c.board, Some(Board::new()));
        // Cross types the highlighted key; the d-pad and the left stick move the highlight, not the cursor.
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Text("g".into())]);
        let before = c.board;
        assert_eq!(go(&mut c, REST, 2, 0), vec![]);
        assert_ne!(c.board, before, "the d-pad moved the highlight");
        assert_eq!(go(&mut c, [128, 0, 128, 128], NONE, 0), vec![]);
        assert_eq!(go(&mut c, REST, NONE, L1), vec![]);
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Text("U".into())], "right of G is H, above it U; L1 = shift");
        assert_eq!(go(&mut c, REST, NONE, R1), vec![]);
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Text("&".into())], "R1 = symbols");
        assert_eq!(go(&mut c, REST, NONE | TRIANGLE, 0), vec![Act::Text(" ".into())]);
        assert_eq!(go(&mut c, REST, NONE | SQUARE, 0), vec![Act::Tap(&[VK_BACK])]);
        let wheel: Vec<_> = (0..20).flat_map(|_| go(&mut c, [128, 128, 128, 0], NONE, 0)).collect();
        assert_eq!(wheel, vec![Act::Scroll(0, 120)], "the right stick still scrolls, a whole notch at a time");
        // R2 = Enter, and the keyboard closes; it is not a click.
        assert_eq!(go(&mut c, REST, NONE, R2), vec![Act::Tap(&[VK_RETURN])]);
        assert_eq!(c.board, None);
        // Open again: Options = Enter too; Circle closes without a key; a game in front closes it.
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(go(&mut c, REST, NONE, OPTIONS), vec![Act::Tap(&[VK_RETURN])]);
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(go(&mut c, REST, NONE | CIRCLE, 0), vec![]);
        assert_eq!(c.board, None);
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(c.step(&st(REST, NONE, 0), DT, true), vec![]);
        assert_eq!(c.board, None);
    }

    #[test]
    fn only_controller_readers_make_it_quiet() {
        use Quiet::*;
        let q = quiet_for;
        let own = std::env::current_exe().unwrap().to_string_lossy().to_string();
        assert_eq!(q(true, r"C:\Windows\explorer.exe", None), None, "the desktop and Task View fill the screen");
        assert_eq!(q(true, r"C:\WINDOWS\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\TextInputHost.exe", Some(false)), None, "its see-through window covers the monitor");
        assert_eq!(q(true, &own, Some(true)), None, "WinSense itself (the monitor copy, the test screen) holds the controller");
        assert_eq!(q(false, r"D:\Games\Some Game\game.exe", Some(true)), Some(Reads), "a windowed game that reads the pad");
        assert_eq!(q(false, r"D:\Apps\Launcher\Launcher.exe", Some(false)), None, "a launcher that leaves the pad alone, whatever folder");
        assert_eq!(q(false, r"D:\Apps\Launcher\Launcher.exe", Some(true)), Some(Reads), "the same launcher once it reads the pad");
        assert_eq!(q(true, r"D:\Games\Some Game\game.exe", Some(false)), Some(Fullscreen), "fullscreen: it may read through Steam");
        assert_eq!(q(true, r"C:\Program Files (x86)\Steam\bin\cef\cef.win64\steamwebhelper.exe", Some(false)), Some(Fullscreen), "Big Picture");
        assert_eq!(q(false, r"C:\Program Files (x86)\Steam\bin\cef\cef.win64\steamwebhelper.exe", Some(false)), None, "the Steam window");
        assert_eq!(q(true, r"C:\Program Files\Google\Chrome\Application\chrome.exe", Some(false)), None, "a fullscreen video");
        assert_eq!(q(false, r"C:\Program Files\Google\Chrome\Application\chrome.exe", Some(true)), Some(Reads), "a page holding the gamepad");
        assert_eq!(q(false, r"C:\Tools\elevated.exe", None), Some(Closed), "run as administrator: keys could not reach it anyway");
        assert_eq!(q(false, "", None), Some(Closed));
        assert_eq!(q(false, r"C:\Program Files\Notepad++\notepad++.exe", Some(false)), None);
        assert_eq!(reason(Reads, "game.exe"), "game.exe reads the controller");
        assert_eq!(reason(Closed, ""), "the program in front cannot be looked into");
    }

    /// The inside look on real processes: this test reads, System cannot be looked into.
    #[test]
    fn looks_inside_real_processes() {
        assert!(look::reads(std::process::id(), false).is_some());
        assert_eq!(look::reads(4, false), None, "System");
        for p in look::pads() {
            assert!(p.starts_with(r"\Device\"), "{p}");
        }
    }

    /// Reads only: every visible top-level window on the running machine and what couch mode makes of it
    /// (how TextInputHost was found, and how the rule was measured).
    /// `cargo test windows_in_front -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn windows_in_front() {
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        unsafe extern "system" fn each(h: HWND, _: LPARAM) -> windows_sys::core::BOOL {
            unsafe {
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut r);
                if IsWindowVisible(h) == 0 || r.right - r.left < 200 {
                    return 1;
                }
                let (pid, (exe, full)) = (look::owner(h), look::window(h));
                let t = Instant::now();
                let reads = look::reads(pid, browser(&exe));
                println!("{pid:6} {:28} full {full:5} reads {reads:?} in {:?}: {:?}", file_name(&exe), t.elapsed(), quiet_for(full, &exe, reads));
            }
            1
        }
        let t = Instant::now();
        println!("controllers: {:?} in {:?}", look::pads(), t.elapsed());
        unsafe { EnumWindows(Some(each), 0) };
        // Processes with no window of their own (steam.exe, the tray): PROBE_PID=1,2,3
        for pid in std::env::var("PROBE_PID").unwrap_or_default().split(',').filter_map(|p| p.parse().ok()) {
            println!("{pid:6} reads {:?}, by handles alone {:?}", look::reads(pid, false), look::reads(pid, true));
            let cpu = || unsafe {
                use windows_sys::Win32::System::Threading::*;
                let (mut a, mut b, mut k, mut u) = std::mem::zeroed();
                GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u);
                ((k.dwHighDateTime as u64) << 32 | k.dwLowDateTime as u64) + ((u.dwHighDateTime as u64) << 32 | u.dwLowDateTime as u64)
            };
            let c = cpu();
            for _ in 0..20 {
                look::reads(pid, true);
            }
            println!("       one look inside: {} ms of CPU", (cpu() - c) as f64 / 20.0 / 10_000.0);
        }
    }

    /// The runner end to end, dry (prints, sends nothing): off until ticked, shows the keyboard, lets go when switched off.
    #[test]
    fn the_runner_is_off_until_ticked() {
        let mut r = Runner::new(true);
        assert!(!r.feed(&st(REST, NONE | SQUARE, 0)), "not ticked: nothing");
        assert_eq!(r.c.board, None);
        r.run(&st(REST, NONE, 0), true);
        assert!(r.run(&st(REST, NONE | SQUARE, 0), true), "acting: the mouse leaves the triggers to it");
        assert!(r.c.board.is_some(), "Square opened the keyboard");
        assert!(!r.run(&st(REST, NONE, 0), false));
        assert_eq!(r.c.board, None, "switched off: the keyboard closes");
    }
}
