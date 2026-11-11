//! The on-screen keyboard: a hand-drawn window in the look of src/ui.rs that never takes
//! the focus from the text box (WS_EX_NOACTIVATE, topmost), driven by the controller through
//! couch mode (src/couch.rs). `Board` is the pure part: the keys, where the highlight is, shift
//! and symbols, and what a press types. The window only shows what `show` hands it; it lives on
//! the tray's thread and the mouse thread (which runs couch mode) posts it a message.
//! The highlight glides from key to key (a 15 ms timer while it is under way).

use crate::ui::{self, R};
use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    /// What the key types on the letters layer and on the symbols layer.
    Ch(&'static str, &'static str),
    Shift,
    Sym,
    Back,
    Space,
    Enter,
}
use Kind::*;

const fn k(a: &'static str, b: &'static str) -> (f32, Kind) {
    (1.0, Ch(a, b))
}

/// The keys, row by row: (width in key units, key). Every row is centred in 10 units.
/// Made for search boxes and addresses: digits, letters, `.`, `/`, `.com` without leaving the
/// letters; the rest of the punctuation is on the symbols layer.
pub const ROWS: [&[(f32, Kind)]; 5] = [
    &[k("1", "1"), k("2", "2"), k("3", "3"), k("4", "4"), k("5", "5"), k("6", "6"), k("7", "7"), k("8", "8"), k("9", "9"), k("0", "0")],
    &[k("q", "!"), k("w", "@"), k("e", "#"), k("r", "$"), k("t", "%"), k("y", "^"), k("u", "&"), k("i", "*"), k("o", "("), k("p", ")")],
    &[k("a", "-"), k("s", "_"), k("d", "="), k("f", "+"), k("g", ":"), k("h", ";"), k("j", "'"), k("k", "\""), k("l", "~")],
    &[(1.5, Shift), k("z", ","), k("x", "?"), k("c", "<"), k("v", ">"), k("b", "["), k("n", "]"), k("m", "\\"), (1.5, Back)],
    &[(1.5, Sym), k("/", "/"), (3.5, Space), k(".", "."), (1.5, Ch(".com", ".com")), (1.5, Enter)],
];

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Dir {
    Up,
    Right,
    Down,
    Left,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Typed {
    Text(String),
    Back,
    Enter,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Board {
    pub row: usize,
    pub col: usize,
    /// Where the highlight was last put sideways (key units): going up and down keeps to this
    /// line, so passing through a wide key does not shift the column.
    x: f32,
    /// The next letter is a capital (one letter, then back).
    pub shift: bool,
    pub sym: bool,
}

/// Left and right edge of a key, in key units.
pub fn span(row: usize, col: usize) -> (f32, f32) {
    let keys = ROWS[row];
    let off = (10.0 - keys.iter().map(|k| k.0).sum::<f32>()) / 2.0;
    let x0 = off + keys[..col].iter().map(|k| k.0).sum::<f32>();
    (x0, x0 + keys[col].0)
}

impl Board {
    /// Opens on G, the middle of the letters.
    pub fn new() -> Board {
        Board { row: 2, col: 4, x: 5.0, shift: false, sym: false }
    }

    /// Move the highlight one key. Sideways wraps round the row; up and down wrap round the
    /// rows and land on the key under the line the highlight was last put on.
    pub fn go(&mut self, d: Dir) {
        let n = ROWS[self.row].len();
        match d {
            Dir::Left | Dir::Right => {
                self.col = if d == Dir::Left { (self.col + n - 1) % n } else { (self.col + 1) % n };
                let (a, b) = span(self.row, self.col);
                self.x = (a + b) / 2.0;
            }
            Dir::Up | Dir::Down => {
                self.row = if d == Dir::Up { (self.row + 4) % 5 } else { (self.row + 1) % 5 };
                // The key the line runs through, else (row 2's half-key margins) the nearest one.
                let off = |c: usize| {
                    let (a, b) = span(self.row, c);
                    if self.x >= a && self.x < b { 0.0 } else { (self.x - (a + b) / 2.0).abs() }
                };
                self.col = (0..ROWS[self.row].len()).min_by(|&a, &b| off(a).total_cmp(&off(b))).unwrap();
            }
        }
    }

    /// What a character key shows and types now; empty for the others (they are drawn as icons).
    pub fn label(&self, row: usize, col: usize) -> String {
        match ROWS[row][col].1 {
            Ch(_, s) if self.sym => s.into(),
            Ch(a, _) if self.shift && a.len() == 1 => a.to_uppercase(),
            Ch(a, _) => a.into(),
            _ => String::new(),
        }
    }

    /// Press the highlighted key.
    pub fn press(&mut self) -> Option<Typed> {
        match ROWS[self.row][self.col].1 {
            Ch(..) => {
                let t = self.label(self.row, self.col);
                self.shift = false;
                Some(Typed::Text(t))
            }
            Space => Some(Typed::Text(" ".into())),
            Back => Some(Typed::Back),
            Enter => Some(Typed::Enter),
            Shift => {
                self.shift = !self.shift;
                None
            }
            Sym => {
                self.sym = !self.sym;
                None
            }
        }
    }
}

// ---- the window ----

const PAD: f32 = 12.0;
const U: f32 = 52.0;
const KH: f32 = 46.0;
const G: f32 = 6.0;
const W: f32 = 2.0 * PAD + 10.0 * U + 9.0 * G;
const H: f32 = PAD + 5.0 * KH + 4.0 * G + 34.0;
const WM_SYNC: u32 = WM_APP + 3;
/// Segoe MDL2 Assets (Windows 10 and 11): shift, backspace, enter.
const ICONS: [(Kind, &str); 3] = [(Shift, "\u{E752}"), (Back, "\u{E750}"), (Enter, "\u{E751}")];

static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// What the window shows: None = hidden.
static SHOWN: Mutex<Option<Board>> = Mutex::new(None);
thread_local! {
    /// Where the highlight is drawn right now (it glides to the picked key).
    static CUR: Cell<Option<R>> = const { Cell::new(None) };
    /// Key letters, small labels, icons.
    static FONTS: [HFONT; 3] = [ui::font(17.0, 400, false), ui::font(12.0, 400, false), unsafe {
        CreateFontW(-(16.0 * ui::scale()).round() as i32, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, w!("Segoe MDL2 Assets"))
    }];
}

fn key_rect(row: usize, col: usize) -> R {
    let (a, b) = span(row, col);
    (PAD + a * (U + G), PAD + row as f32 * (KH + G), (b - a) * (U + G) - G, KH)
}

/// Show this board (None = hide). From any thread.
pub fn show(b: Option<Board>) {
    let mut shown = SHOWN.lock().unwrap();
    let h = WINDOW.load(Ordering::Relaxed) as HWND;
    if *shown != b && !h.is_null() {
        *shown = b;
        unsafe { PostMessageW(h, WM_SYNC, 0, 0) };
    }
}

fn paint(h: HWND) {
    let th = ui::theme();
    let Some(b) = *SHOWN.lock().unwrap() else {
        unsafe { ValidateRect(h, null()) };
        return;
    };
    let cur = CUR.get().unwrap_or(key_rect(b.row, b.col));
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(h, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(h, &mut rc);
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, rc.right, rc.bottom);
        let old = SelectObject(mem, bmp);
        // Modifier keys sit a shade back from the letters, as on Windows' own touch keyboard.
        let shades = if th.dark { (th.hover, th.card) } else { (th.card, th.seg) };
        let fill = |kind: Kind| if matches!(kind, Ch(..) | Space) { shades.0 } else { shades.1 };
        let on = |kind: Kind| kind == Shift && b.shift || kind == Sym && b.sym;
        {
            let p = ui::Pen::new(mem);
            p.round((0.0, 0.0, W + 50.0, H + 50.0), 0.0, th.bg, None);
            for (row, keys) in ROWS.iter().enumerate() {
                for (col, key) in keys.iter().enumerate() {
                    let r = key_rect(row, col);
                    p.round(r, 6.0, fill(key.1), Some(th.edge));
                    if on(key.1) {
                        p.round((r.0 + r.2 / 2.0 - 9.0, r.1 + r.3 - 8.0, 18.0, 3.0), 1.5, th.accent, None);
                    }
                }
            }
            p.round(cur, 6.0, th.accent, None);
            let sp = key_rect(4, 2);
            let picked = ROWS[b.row][b.col].1 == Space;
            p.round((sp.0 + sp.2 / 2.0 - 28.0, sp.1 + sp.3 / 2.0 + 5.0, 56.0, 3.0), 1.5, if picked { th.on_accent } else { th.text2 }, None);
        }
        SetBkMode(mem, TRANSPARENT as i32);
        FONTS.with(|f| {
            for (row, keys) in ROWS.iter().enumerate() {
                for (col, key) in keys.iter().enumerate() {
                    let picked = (row, col) == (b.row, b.col);
                    let color = if picked { th.on_accent } else if on(key.1) { th.accent } else { th.text };
                    let r = key_rect(row, col);
                    match key.1 {
                        Ch(a, _) => ui::text_with(mem, f[if a.len() > 1 { 1 } else { 0 }], &b.label(row, col), r, color, DT_CENTER),
                        Sym => ui::text_with(mem, f[1], if b.sym { "abc" } else { "&123" }, r, color, DT_CENTER),
                        Space => {}
                        kind => ui::text_with(mem, f[2], ICONS.iter().find(|i| i.0 == kind).unwrap().1, r, color, DT_CENTER),
                    }
                }
            }
            let hint = "Cross type  \u{00B7}  Square delete  \u{00B7}  Triangle space  \u{00B7}  L1 shift  \u{00B7}  R1 symbols  \u{00B7}  R2 enter  \u{00B7}  Circle close";
            ui::text_with(mem, f[1], hint, (PAD, H - 30.0, W - 2.0 * PAD, 22.0), th.text2, DT_CENTER);
        });
        BitBlt(dc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(h, &ps);
    }
}

/// Bottom centre of the monitor the window in front is on, above the taskbar; never activated.
fn place(h: HWND) {
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY), &mut mi);
        let (w, hh, a) = (ui::px(W as i32), ui::px(H as i32), mi.rcWork);
        SetWindowPos(h, HWND_TOPMOST, (a.left + a.right - w) / 2, a.bottom - hh - ui::px(24), w, hh, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_SYNC => unsafe {
            if SHOWN.lock().unwrap().is_some() {
                if IsWindowVisible(h) == 0 {
                    CUR.set(None); // opens with the highlight in place
                    place(h);
                }
                SetTimer(h, 1, 15, None);
                InvalidateRect(h, null(), 0);
            } else {
                KillTimer(h, 1);
                ShowWindow(h, SW_HIDE);
            }
            0
        },
        WM_TIMER => unsafe {
            let Some(b) = *SHOWN.lock().unwrap() else { return 0 };
            let to = key_rect(b.row, b.col);
            let c = CUR.get().unwrap_or(to);
            let step = |a: f32, b: f32| a + (b - a) * 0.4;
            let next = (step(c.0, to.0), step(c.1, to.1), step(c.2, to.2), step(c.3, to.3));
            if (next.0 - to.0).abs().max((next.1 - to.1).abs()) < 0.5 {
                CUR.set(Some(to));
                KillTimer(h, 1);
            } else {
                CUR.set(Some(next));
            }
            InvalidateRect(h, null(), 0);
            0
        },
        WM_SETTINGCHANGE => unsafe {
            InvalidateRect(h, null(), 0); // dark or light, or the accent colour, may have changed
            0
        },
        WM_DESTROY => {
            WINDOW.store(0, Ordering::Relaxed);
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, w, l) },
    }
}

/// Make the (hidden) keyboard window on this thread, which must run a message loop.
pub fn create() -> HWND {
    ui::gdiplus();
    unsafe {
        let inst = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = inst;
        wc.lpszClassName = w!("WinSenseKeys");
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        RegisterClassW(&wc);
        let h = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW, w!("WinSenseKeys"), w!("WinSense keyboard"), WS_POPUP, 0, 0, ui::px(W as i32), ui::px(H as i32), null_mut(), null_mut(), inst, null());
        DwmSetWindowAttribute(h, 33, &2u32 as *const u32 as _, 4); // DWMWA_WINDOW_CORNER_PREFERENCE: round (Windows 11)
        WINDOW.store(h as isize, Ordering::Relaxed);
        h
    }
}

/// `winsense osk [shift] [sym] [demo]`: the keyboard alone for 30 s, for screenshots. Types nothing.
pub fn demo(args: &[String]) {
    let h = create();
    let mut b = Board::new();
    (b.shift, b.sym) = (args.iter().any(|a| a == "shift"), args.iter().any(|a| a == "sym"));
    show(Some(b));
    if args.iter().any(|a| a == "demo") {
        // The highlight walks about, to look at the glide.
        std::thread::spawn(move || {
            for d in [Dir::Right, Dir::Right, Dir::Down, Dir::Down, Dir::Left, Dir::Up, Dir::Up, Dir::Up].iter().cycle() {
                std::thread::sleep(std::time::Duration::from_millis(400));
                b.go(*d);
                show(Some(b));
            }
        });
    }
    unsafe {
        SetTimer(h, 2, 30_000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 && !(msg.message == WM_TIMER && msg.wParam == 2) {
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the highlight to a key the way a thumb would (rows first, then along the row) and press it.
    fn tap(b: &mut Board, want: &str) -> Option<Typed> {
        let (row, col) = (0..5).flat_map(|r| (0..ROWS[r].len()).map(move |c| (r, c))).find(|&(r, c)| b.label(r, c) == want).unwrap_or_else(|| panic!("no key {want}"));
        while b.row != row {
            b.go(if b.row < row { Dir::Down } else { Dir::Up });
        }
        while b.col != col {
            b.go(if b.col < col { Dir::Right } else { Dir::Left });
        }
        b.press()
    }

    #[test]
    fn typing_a_word_gives_its_keys() {
        let mut b = Board::new();
        let mut typed = String::new();
        for key in ["y", "o", "u", "t", "u", "b", "e", ".com"] {
            match tap(&mut b, key) {
                Some(Typed::Text(t)) => typed += &t,
                other => panic!("{key} gave {other:?}"),
            }
        }
        assert_eq!(typed, "youtube.com");
        b.go(Dir::Right);
        assert_eq!(b.press(), Some(Typed::Enter), "Enter is right of .com");
    }

    #[test]
    fn shift_is_one_letter_and_symbols_stay() {
        let mut b = Board::new();
        assert_eq!(b.press(), Some(Typed::Text("g".into())), "opens on G");
        (b.row, b.col) = (3, 0);
        assert_eq!(b.press(), None);
        assert!(b.shift);
        assert_eq!(tap(&mut b, "H"), Some(Typed::Text("H".into())));
        assert_eq!(b.press(), Some(Typed::Text("h".into())), "shift lasts one letter");
        b.shift = true;
        assert_eq!(b.label(4, 4), ".com", "shift leaves .com alone");
        b.shift = false;
        (b.row, b.col) = (4, 0);
        assert_eq!(b.press(), None);
        assert_eq!(tap(&mut b, "@"), Some(Typed::Text("@".into())));
        assert_eq!(tap(&mut b, "?"), Some(Typed::Text("?".into())), "symbols stay until switched back");
        assert_eq!(tap(&mut b, "7"), Some(Typed::Text("7".into())), "digits are on both layers");
        (b.row, b.col) = (3, 8);
        assert_eq!(b.press(), Some(Typed::Back));
        (b.row, b.col) = (4, 2);
        assert_eq!(b.press(), Some(Typed::Text(" ".into())));
    }

    #[test]
    fn the_highlight_crosses_rows_of_different_widths() {
        for (r, keys) in ROWS.iter().enumerate() {
            let (first, last) = (span(r, 0).0, span(r, keys.len() - 1).1);
            assert!((first + last - 10.0).abs() < 1e-4 && first >= 0.0, "row {r} is centred in 10 units");
        }
        let at = |b: &Board| b.label(b.row, b.col);
        // From G straight down: V, the space bar, and back up the same line to G; wide keys do not shift the column.
        let mut b = Board::new();
        b.go(Dir::Down);
        assert_eq!(at(&b), "v");
        b.go(Dir::Down);
        assert_eq!(ROWS[b.row][b.col].1, Space);
        b.go(Dir::Up);
        b.go(Dir::Up);
        assert_eq!(at(&b), "g");
        // P (the end of a 10-key row) down: L (the end of the 9-key row), Backspace, Enter, and round to 0.
        (b.row, b.col) = (1, 9);
        b.go(Dir::Left);
        b.go(Dir::Right);
        assert_eq!(at(&b), "p");
        b.go(Dir::Down);
        assert_eq!(at(&b), "l");
        b.go(Dir::Down);
        assert_eq!(ROWS[b.row][b.col].1, Back);
        b.go(Dir::Down);
        assert_eq!(ROWS[b.row][b.col].1, Enter);
        b.go(Dir::Down);
        assert_eq!(at(&b), "0", "down from the last row wraps to the digits");
        b.go(Dir::Right);
        assert_eq!(at(&b), "1", "sideways wraps round the row");
        b.go(Dir::Up);
        assert_eq!(ROWS[b.row][b.col].1, Sym, "up from the digits wraps to the last row");
        // Every key can be reached and is inside the window.
        for r in 0..5 {
            for c in 0..ROWS[r].len() {
                let k = key_rect(r, c);
                assert!(k.0 >= PAD - 0.01 && k.0 + k.2 <= W - PAD + 0.01 && k.1 + k.3 < H - 30.0, "key {r}/{c} inside, above the hint line");
            }
        }
    }
}
