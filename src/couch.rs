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
//! It must never type into a game. Two rules, both simple (`quiet_for`, `Couch::paused`):
//! quiet while the program in front is fullscreen (fills its monitor with no title bar) and is
//! not the Windows shell or a browser, or lives in a game library folder; and holding Create
//! for a second pauses or resumes it by hand (for a windowed game the first rule cannot see).

use crate::ds::State;
use crate::osk::{Board, Dir, Typed};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// The tray's "Couch mode" item.
pub static ON: AtomicBool = AtomicBool::new(false);

// All first guesses, not tried in the hand.
/// Stick travel (of 1.0) that counts as rest.
const DEAD: f32 = 0.12;
/// Cursor pixels per second at full tilt; the curve is tilt squared, so half tilt is a quarter of it.
const SPEED: f32 = 1400.0;
/// Wheel units per second at full tilt (120 = one notch).
const SCROLL: f32 = 2400.0;
/// A held direction repeats after this long, then this often.
const FIRST: f32 = 0.4;
const NEXT: f32 = 0.09;
/// Create held this long (alone) pauses or resumes couch mode.
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
const PAD_CLICK: u8 = 0x02; // buttons[2]

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
    /// The keyboard opened, changed or closed: show `Couch::board`.
    Board,
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
    /// How long Create has been held alone.
    hold: f32,
    /// Paused by hand (hold Create); not remembered.
    pub paused: bool,
    /// Mouse buttons held by R2 (left) and L2 (right).
    down: [bool; 2],
    pub board: Option<Board>,
}

/// A stick as (x, y) in -1..1 with the rest zone cut out and the tilt squared.
fn curve(x: u8, y: u8) -> (f32, f32) {
    let (x, y) = ((x as f32 - 128.0) / 127.0, (y as f32 - 128.0) / 127.0);
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

impl Couch {
    fn buttons(&mut self, want: [bool; 2]) -> Vec<Act> {
        let out = (0..2).filter(|&i| want[i] != self.down[i]).map(|i| Act::Button { right: i == 1, down: want[i] }).collect();
        self.down = want;
        out
    }

    /// Let go of held mouse buttons and close the keyboard (quiet, switched off, controller gone).
    pub fn release(&mut self) -> Vec<Act> {
        let mut out = self.buttons([false; 2]);
        if self.board.take().is_some() {
            out.push(Act::Board);
        }
        out
    }

    /// One report in, desktop actions out. `dt` = seconds since the last report, `gate` = a game
    /// is in front (`quiet_for`).
    pub fn step(&mut self, s: &State, dt: f32, gate: bool) -> Vec<Act> {
        let (b, was) = (s.buttons, self.last);
        self.last = b;
        let hit = |i: usize, m: u8| b[i] & m != 0 && was[i] & m == 0;
        let mut out = Vec::new();
        // Create held alone for a second: pause / resume. With a touchpad click it is the gyro switch.
        if b[1] & CREATE == 0 {
            self.hold = 0.0;
        } else if b[2] & PAD_CLICK != 0 {
            self.hold = f32::NEG_INFINITY;
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
        let n = whole(&mut self.wheel, (scroll.0 * SCROLL * dt, -scroll.1 * SCROLL * dt));
        if n != (0, 0) {
            out.push(Act::Scroll(n.0, n.1));
        }
        let hat = match b[0] & 0x0F {
            0 => Some(Dir::Up),
            2 => Some(Dir::Right),
            4 => Some(Dir::Down),
            6 => Some(Dir::Left),
            _ => None, // rest, or a diagonal
        };
        let (pulled, clicked) = ([b[1] & R2 != 0, b[1] & L2 != 0], [hit(1, R2), hit(1, L2)]);
        if let Some(bd) = &mut self.board {
            let (x, y) = ((s.sticks[0] as f32 - 128.0) / 127.0, (s.sticks[1] as f32 - 128.0) / 127.0);
            let stick = match (x.abs() > y.abs(), x.abs().max(y.abs()) > 0.5) {
                (_, false) => None,
                (true, _) => Some(if x > 0.0 { Dir::Right } else { Dir::Left }),
                (false, _) => Some(if y > 0.0 { Dir::Down } else { Dir::Up }),
            };
            if let Some(d) = self.rep.step(hat.or(stick), dt) {
                bd.go(d);
                out.push(Act::Board);
            }
            let mut typed = Vec::new();
            if hit(0, CROSS) {
                typed.extend(bd.press());
                out.push(Act::Board); // shift or symbols may have changed
            }
            if hit(0, SQUARE) {
                typed.push(Typed::Back);
            }
            if hit(0, TRIANGLE) {
                typed.push(Typed::Text(" ".into()));
            }
            if hit(1, L1) {
                bd.shift = !bd.shift;
                out.push(Act::Board);
            }
            if hit(1, R1) {
                bd.sym = !bd.sym;
                out.push(Act::Board);
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
                out.push(Act::Board);
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
        let keys: [(usize, u8, &'static [u16]); 6] = [(0, CROSS, &[VK_RETURN]), (0, CIRCLE, &[VK_ESCAPE]), (0, TRIANGLE, &[VK_LWIN, VK_TAB]), (1, L1, &[VK_BROWSER_BACK]), (1, R1, &[VK_BROWSER_FORWARD]), (1, OPTIONS, &[VK_LWIN])];
        out.extend(keys.iter().filter(|k| hit(k.0, k.1)).map(|k| Act::Tap(k.2)));
        if hit(0, SQUARE) {
            self.board = Some(Board::new());
            out.push(Act::Board);
        }
        out
    }
}

/// Programs that fill the screen without being a game: the Windows shell (desktop, Start,
/// Task View, search) and browsers (a fullscreen video still wants Escape and the cursor).
const NOT_GAMES: [&str; 11] = ["explorer.exe", "startmenuexperiencehost.exe", "searchhost.exe", "shellexperiencehost.exe", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "zen.exe"];
/// Folders games live in (lower case).
const GAME_FOLDERS: [&str; 6] = ["\\steamapps\\", "\\epic games\\", "\\gog games\\", "\\xboxgames\\", "\\emulation\\", "\\roms\\"];

/// Whether couch mode keeps quiet for the program in front. `fullscreen` = its window fills
/// its monitor and has no title bar; `exe` = its full path ("" when it cannot be read: then a
/// fullscreen window counts as a game).
pub fn quiet_for(fullscreen: bool, exe: &str) -> bool {
    let exe = exe.to_lowercase();
    let name = exe.rsplit('\\').next().unwrap_or("");
    GAME_FOLDERS.iter().any(|f| exe.contains(f)) || fullscreen && !NOT_GAMES.contains(&name)
}

/// `quiet_for` for the window in front right now.
pub(crate) fn gate() -> bool {
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let h = GetForegroundWindow();
        if h.is_null() {
            return false;
        }
        let (mut r, mut mi): (RECT, MONITORINFO) = (std::mem::zeroed(), std::mem::zeroed());
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetWindowRect(h, &mut r);
        GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi);
        let m = mi.rcMonitor;
        // A maximised window can fill the monitor too (hidden taskbar): it still has its title bar.
        let fullscreen = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom && GetWindowLongW(h, GWL_STYLE) as u32 & WS_CAPTION != WS_CAPTION;
        let (mut pid, mut path, mut len) = (0, [0u16; 520], 520u32);
        GetWindowThreadProcessId(h, &mut pid);
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() || QueryFullProcessImageNameW(p, 0, path.as_mut_ptr(), &mut len) == 0 {
            len = 0;
        }
        if !p.is_null() {
            CloseHandle(p);
        }
        quiet_for(fullscreen, &String::from_utf16_lossy(&path[..len as usize]))
    }
}

/// Hand the actions to Windows.
fn send(out: &[Act]) {
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
            Act::Sound(_) | Act::Board => {}
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
    /// When to look at the window in front again, and what it said.
    gate: (Instant, bool),
    dry: bool,
    live: bool,
}

impl Runner {
    pub fn new(dry: bool) -> Runner {
        Runner { c: Couch::default(), t: Instant::now(), gate: (Instant::now(), false), dry, live: false }
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
        self.live = true;
        if now >= self.gate.0 {
            self.gate = (now + Duration::from_millis(250), !self.dry && gate()); // a dry run acts on nothing, so it asks nothing
        }
        let out = self.c.step(s, dt, self.gate.1);
        self.act(&out);
        !(self.gate.1 || self.c.paused)
    }

    /// Switched off, or the controller went away: let go of everything.
    pub fn stop(&mut self) {
        if std::mem::take(&mut self.live) {
            let out = self.c.release();
            self.act(&out);
        }
    }

    fn act(&self, out: &[Act]) {
        if out.is_empty() {
            return;
        }
        if out.contains(&Act::Board) {
            crate::osk::show(self.c.board);
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
        assert!(mv == (0, 0) && sc.0 == 0 && (sc.1 - SCROLL as i32).abs() <= 2, "{sc:?}");
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
        let hold = |c: &mut Couch, b2: u8, n: usize| (0..n).flat_map(|_| c.step(&State { sticks: REST, buttons: [NONE, CREATE, b2], ..State::default() }, DT, false)).collect::<Vec<_>>();
        assert_eq!(hold(&mut c, 0, 240), vec![], "0.96 s: not yet");
        assert_eq!(hold(&mut c, 0, 500), vec![Act::Sound(crate::voice::OFF)]);
        assert!(c.paused);
        assert_eq!(c.step(&st([255, 128, 128, 128], NONE | CROSS, 0), DT, false), vec![], "paused: silent");
        assert_eq!(hold(&mut c, 0, 300), vec![Act::Sound(crate::voice::DING)], "again: back on");
        assert!(!c.paused);
        c.step(&st(REST, NONE, 0), DT, false);
        // Create + touchpad click is the gyro switch, however long it is held.
        assert_eq!(hold(&mut c, PAD_CLICK, 400), vec![]);
        assert_eq!(hold(&mut c, 0, 400), vec![], "letting go of the pad does not start the count");
        assert!(!c.paused);
    }

    #[test]
    fn the_keyboard_types_through_the_controller() {
        let mut c = Couch::default();
        let go = |c: &mut Couch, sticks: [u8; 4], b0: u8, b1: u8| {
            let out = c.step(&st(sticks, b0, b1), DT, false);
            c.step(&st(REST, NONE, 0), DT, false);
            out
        };
        assert_eq!(go(&mut c, REST, NONE | SQUARE, 0), vec![Act::Board]);
        assert_eq!(c.board, Some(Board::new()));
        // Cross types the highlighted key; the d-pad and the left stick move the highlight, not the cursor.
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Board, Act::Text("g".into())]);
        assert_eq!(go(&mut c, REST, 2, 0), vec![Act::Board]);
        assert_eq!(go(&mut c, [128, 0, 128, 128], NONE, 0), vec![Act::Board]);
        assert_eq!(go(&mut c, REST, NONE, L1), vec![Act::Board]);
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Board, Act::Text("U".into())], "right of G is H, above it U; L1 = shift");
        assert_eq!(go(&mut c, REST, NONE, R1), vec![Act::Board]);
        assert_eq!(go(&mut c, REST, NONE | CROSS, 0), vec![Act::Board, Act::Text("&".into())], "R1 = symbols");
        assert_eq!(go(&mut c, REST, NONE | TRIANGLE, 0), vec![Act::Text(" ".into())]);
        assert_eq!(go(&mut c, REST, NONE | SQUARE, 0), vec![Act::Tap(&[VK_BACK])]);
        assert_eq!(go(&mut c, [128, 128, 128, 0], NONE, 0), vec![Act::Scroll(0, 9)], "the right stick still scrolls");
        // R2 = Enter, and the keyboard closes; it is not a click.
        assert_eq!(go(&mut c, REST, NONE, R2), vec![Act::Tap(&[VK_RETURN]), Act::Board]);
        assert_eq!(c.board, None);
        // Open again: Options = Enter too; Circle closes without a key; a game in front closes it.
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(go(&mut c, REST, NONE, OPTIONS), vec![Act::Tap(&[VK_RETURN]), Act::Board]);
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(go(&mut c, REST, NONE | CIRCLE, 0), vec![Act::Board]);
        assert_eq!(c.board, None);
        go(&mut c, REST, NONE | SQUARE, 0);
        assert_eq!(c.step(&st(REST, NONE, 0), DT, true), vec![Act::Board]);
        assert_eq!(c.board, None);
    }

    #[test]
    fn only_games_make_it_quiet() {
        let q = quiet_for;
        assert!(!q(false, r"C:\Windows\System32\notepad.exe"));
        assert!(!q(true, r"C:\Windows\explorer.exe"), "the desktop and Task View fill the screen");
        assert!(!q(true, r"C:\Windows\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe"));
        assert!(!q(true, r"C:\Program Files\Google\Chrome\Application\chrome.exe"), "a fullscreen video");
        assert!(q(true, r"C:\Program Files (x86)\Steam\steam.exe"), "Big Picture");
        assert!(q(true, r"D:\Games\Some Game\game.exe"), "fullscreen and unknown: a game");
        assert!(q(true, ""), "fullscreen and unreadable: a game");
        assert!(q(false, r"D:\SteamLibrary\steamapps\common\Hades\Hades.exe"), "windowed, but from a game library");
        assert!(q(false, r"C:\Games\Emulation\Dolphin\Dolphin.exe"));
        assert!(!q(false, ""));
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
