//! `winsense ui [--dry|--pad]`: the trigger and rumble test screen, drawn by hand in the Windows 11
//! style (cards, segmented effect pickers, slim sliders; dark or light as Windows is set, in the
//! Windows accent colour). GDI+ draws the shapes antialiased, GDI the text (ClearType), into one
//! back buffer. Raw Win32 through windows-sys. Mouse, wheel and keyboard (Tab, arrows, Home/End).
//! Every change hands both triggers' effects and the rumble to one worker thread, which writes
//! them to every DualSense in one report. Nothing is written before the first change; closing
//! the window switches both triggers and the rumble off. `--dry` opens no controller and prints the reports.
//! With `--pad` (opt-in: by default the DualSense stays itself) it also runs the virtual Xbox 360 pad (src/pad.rs); a game's rumble
//! reaches the same worker, so it never wipes the trigger effects (every report carries both).
//! Since 0.6.0 it also runs touchpad and gyro as mouse (src/mouse.rs, reads only) unless `--dry`.
//! Since 0.7.0 the tray program (src/tray.rs) opens the same window in its own process: there the
//! worker lives as long as the tray, closing the window switches triggers and rumble off, and the
//! tray runs the mouse.

use crate::ds::{self, Effect};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::{Mutex, mpsc};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::{FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawPath, GdipFillEllipse, GdipFillPath, GdipFillRectangle, GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpGraphics, PixelOffsetModeHalf, SmoothingModeAntiAlias, UnitWorld};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::PCWSTR;
use windows_sys::w;

const NAMES: [&str; 5] = ["Off", "Resist", "Wall", "Click", "Vibrate"];
/// (label, lowest, highest, start value) of the three sliders of each effect, in NAMES order.
const KNOBS: [[Option<(&str, u8, u8, u8)>; 3]; 5] = [
    [None, None, None],
    [Some(("Start", 0, 9, 0)), Some(("Strength", 1, 8, 4)), None],
    [Some(("Point", 0, 9, 5)), None, None],
    [Some(("Start", 2, 7, 3)), Some(("End", 3, 8, 5)), Some(("Strength", 1, 8, 6))],
    [Some(("Start", 1, 9, 2)), Some(("Strength", 1, 8, 4)), Some(("Frequency (Hz)", 1, 60, 30))],
];
const MOTORS: [&str; 2] = ["Left motor (deep)", "Right motor (light)"];

/// The effect for a picked effect and its three slider values, pulled into range so
/// `Effect::bytes()` always succeeds.
pub fn effect_from(kind: usize, v: [u8; 3]) -> Effect {
    match kind {
        1 => Effect::Resist { start: v[0].min(9), strength: v[1].clamp(1, 8) },
        2 => Effect::Resist { start: v[0].min(9), strength: 8 },
        3 => {
            let start = v[0].clamp(2, 7);
            Effect::Click { start, end: v[1].clamp(start + 1, 8), strength: v[2].clamp(1, 8) }
        }
        4 => Effect::Vibrate { start: v[0].clamp(1, 9), strength: v[1].clamp(1, 8), hz: v[2].max(1) },
        _ => Effect::Off,
    }
}

pub type Cmd = (Effect, Effect, [u8; 2]); // (L2, R2, rumble [left strong, right weak])
pub const OFF: Cmd = (Effect::Off, Effect::Off, [0, 0]);

/// What reaches the worker: the window's controls, a game's rumble through the virtual pad, or a
/// mod through the port (src/port.rs): its lasting state (None = gone; only the connection that
/// set the state can clear it), a state played on top for a while, or a haptic tone in report 0x32
/// for a while (port.rs plays it; the motors rest meanwhile, see `worker`). `Light` = set the mute
/// light (src/desk.rs): it rides in the next report, beside the triggers and rumble as they are.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Msg {
    Ui(Cmd),
    Game([u8; 2]),
    Mod(u32, Option<Cmd>),
    Pulse(Cmd, std::time::Duration),
    Tone(std::time::Duration),
    Light(u8),
    /// A mod's lasting lights (all None = given back; only the mod that set them can), and lights
    /// played on top for a while (a part left None shows the lasting one).
    Leds(u32, ds::Glow),
    Blink(ds::Glow, std::time::Duration),
}

const DARK: ds::Glow = (None, None);

/// The lights a mod wants: each part from the blink while one runs, else the lasting one.
fn glow(lasting: ds::Glow, blink: Option<ds::Glow>) -> ds::Glow {
    let b = blink.unwrap_or(DARK);
    (b.0.or(lasting.0), b.1.or(lasting.1))
}

/// What the controller gets: per trigger the first effect that is not Off from the pulse, the mod,
/// the window; each motor at the strongest of all of them and the game.
fn merge(ui: Cmd, game: [u8; 2], m: Option<Cmd>, pulse: Option<Cmd>) -> Cmd {
    let layers = [pulse, m, Some(ui)];
    let pick = |f: fn(&Cmd) -> Effect| layers.iter().flatten().map(f).find(|&e| e != Effect::Off).unwrap_or(Effect::Off);
    let motor = |k: usize| layers.iter().flatten().map(|c| c.2[k]).fold(game[k], u8::max);
    (pick(|c| c.0), pick(|c| c.1), [motor(0), motor(1)])
}

/// A control: the effect picker of a trigger (0 L2, 1 R2), or slider `i` of a card (2 = rumble).
#[derive(Debug, PartialEq, Clone, Copy)]
enum Ctl {
    Seg(usize),
    Slider(usize, usize),
}

/// What the window shows: the picked effects, every slider value (row 2 = rumble [left, right]),
/// and the pointer and keyboard state the painting needs.
struct Model {
    kind: [usize; 2],
    v: [[u8; 3]; 3],
    hover: Option<(Ctl, usize)>, // control and, for a picker, the option under the pointer
    drag: Option<Ctl>,
    focus: usize,
    keys: bool, // show the focus ring (after the keyboard was used, as Windows does)
}

impl Model {
    const fn new() -> Model {
        Model { kind: [0; 2], v: [[0; 3]; 3], hover: None, drag: None, focus: 0, keys: false }
    }

    /// (label, lowest, highest) of a slider, None when the picked effect has no such slider.
    fn range(&self, t: usize, i: usize) -> Option<(&'static str, u8, u8)> {
        if t == 2 {
            return (i < 2).then(|| (MOTORS[i], 0, 255));
        }
        KNOBS[self.kind[t]][i].map(|(l, lo, hi, _)| (l, lo, hi))
    }

    /// The controls in Tab order.
    fn controls(&self) -> Vec<Ctl> {
        let mut c = Vec::new();
        for t in 0..2 {
            c.push(Ctl::Seg(t));
            c.extend((0..3).filter(|&i| self.range(t, i).is_some()).map(|i| Ctl::Slider(t, i)));
        }
        c.extend([Ctl::Slider(2, 0), Ctl::Slider(2, 1)]);
        c
    }

    fn pick(&mut self, t: usize, kind: usize) {
        self.kind[t] = kind.min(4);
        self.v[t] = KNOBS[self.kind[t]].map(|k| k.map_or(0, |k| k.3));
    }

    fn set(&mut self, t: usize, i: usize, v: i32) {
        if let Some((_, lo, hi)) = self.range(t, i) {
            self.v[t][i] = v.clamp(lo as i32, hi as i32) as u8;
            if t < 2 && self.kind[t] == 3 && self.v[t][1] <= self.v[t][0] {
                self.v[t][1] = self.v[t][0] + 1; // Click: the end stays past the start
            }
        }
    }

    fn cmd(&self) -> Cmd {
        (effect_from(self.kind[0], self.v[0]), effect_from(self.kind[1], self.v[1]), [self.v[2][0], self.v[2][1]])
    }
}

thread_local! {
    /// Where the window's thread sends each change. Dropped when the window is destroyed
    /// (in the tray: when the tray quits).
    static TX: RefCell<Option<mpsc::Sender<Msg>>> = const { RefCell::new(None) };
    static MODEL: RefCell<Model> = const { RefCell::new(Model::new()) };
    /// Fonts made once at the system DPI: title, card heading, body, small.
    static FONTS: [HFONT; 4] = [font(20.0, 600, true), font(14.0, 600, false), font(13.0, 400, false), font(12.0, 400, false)];
}
/// The open window (0 = none), for the worker's status line and the tray.
static SCREEN: AtomicIsize = AtomicIsize::new(0);
/// Set by the tray: closing the window then keeps the worker and the message loop.
pub static IN_TRAY: AtomicBool = AtomicBool::new(false);
/// The worker drives a rumble motor right now: game sound as haptics waits (src/sound.rs).
pub static RUMBLE: AtomicBool = AtomicBool::new(false);
/// The mute light the worker's next report carries (0 off, 1 on), `NO_LIGHT` = none: a report
/// without it leaves the light as it is, so it is sent once per change.
/// shortcut: taken by the next write even when no controller is there to get it; src/desk.rs only asks while one is connected.
static LIGHT: AtomicU8 = AtomicU8::new(NO_LIGHT);
const NO_LIGHT: u8 = 0xFF;
/// A mod's lights for the worker's next report (`DARK` = give every light back), None = none:
/// like `LIGHT`, sent once per change.
static GLOW: Mutex<Option<ds::Glow>> = Mutex::new(None);
/// This connection already got the light-bar setup report (Bluetooth holds the bar in its start-up
/// animation until then; the driver sends it first). The mouse thread clears it at each connection.
/// shortcut: one flag, so with two controllers only the first to be written gets the setup.
pub static FADED: AtomicBool = AtomicBool::new(false);
/// The worker's last result, shown in the status line.
static STATUS: Mutex<String> = Mutex::new(String::new());
const WM_STATUS: u32 = WM_APP + 2;

/// Layout units (pixels at 100 %) to pixels at the system DPI (the process is system DPI aware).
pub(crate) fn scale() -> f32 {
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

pub(crate) fn px(v: i32) -> i32 {
    (v as f32 * scale()).round() as i32
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Segoe UI Variable (Windows 11), else Segoe UI (Windows 10).
pub(crate) fn font(size: f32, weight: i32, display: bool) -> HFONT {
    let make = |face: &str| unsafe { CreateFontW(-(size * scale()).round() as i32, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, wide(face).as_ptr()) };
    let face = if display { "Segoe UI Variable Display" } else { "Segoe UI Variable Text" };
    let f = make(face);
    let mut got = [0u16; 64];
    unsafe {
        let dc = GetDC(null_mut());
        let old = SelectObject(dc, f);
        let n = GetTextFaceW(dc, 64, got.as_mut_ptr());
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        if String::from_utf16_lossy(&got[..n.max(1) as usize - 1]) == face {
            return f;
        }
        DeleteObject(f);
    }
    make("Segoe UI")
}

// ---- layout, in units at 100 %; (x, y, w, h) ----

pub(crate) type R = (f32, f32, f32, f32);
const W: f32 = 640.0;
const H: f32 = 470.0;
const M: f32 = 20.0;
const GAP: f32 = 12.0;
const CARD_Y: f32 = 76.0;
const CARD_H: f32 = 236.0;
const RUMBLE_H: f32 = 98.0;

fn card(t: usize) -> R {
    let w = (W - 2.0 * M - GAP) / 2.0;
    match t {
        0 | 1 => (M + t as f32 * (w + GAP), CARD_Y, w, CARD_H),
        _ => (M, CARD_Y + CARD_H + GAP, W - 2.0 * M, RUMBLE_H),
    }
}

fn seg(t: usize) -> R {
    let c = card(t);
    (c.0 + 14.0, c.1 + 42.0, c.2 - 28.0, 32.0)
}

fn seg_item(t: usize, k: usize) -> R {
    let s = seg(t);
    let w = (s.2 - 4.0) / 5.0;
    (s.0 + 2.0 + k as f32 * w, s.1 + 2.0, w, s.3 - 4.0)
}

/// A slider's whole row: label line on top, the track below it.
fn row(t: usize, i: usize) -> R {
    let c = card(t);
    if t < 2 {
        (c.0 + 14.0, c.1 + 88.0 + i as f32 * 48.0, c.2 - 28.0, 44.0)
    } else {
        let w = (c.2 - 28.0 - 28.0) / 2.0;
        (c.0 + 14.0 + i as f32 * (w + 28.0), c.1 + 42.0, w, 44.0)
    }
}

/// The track of a slider: (left end, right end, centre line). The thumb (radius 10) stays inside the row.
fn track(t: usize, i: usize) -> (f32, f32, f32) {
    let r = row(t, i);
    (r.0 + 10.0, r.0 + r.2 - 10.0, r.1 + 32.0)
}

fn value_at(t: usize, i: usize, x: f32, (lo, hi): (u8, u8)) -> i32 {
    let (x0, x1, _) = track(t, i);
    (lo as f32 + ((x - x0) / (x1 - x0)).clamp(0.0, 1.0) * (hi - lo) as f32).round() as i32
}

fn x_of(t: usize, i: usize, v: u8, (lo, hi): (u8, u8)) -> f32 {
    let (x0, x1, _) = track(t, i);
    x0 + (v.saturating_sub(lo)) as f32 / (hi - lo).max(1) as f32 * (x1 - x0)
}

pub(crate) fn inside(r: R, (x, y): (f32, f32)) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// The control (and picker option) at a point.
fn hit(m: &Model, p: (f32, f32)) -> Option<(Ctl, usize)> {
    for t in 0..2 {
        if let Some(k) = (0..5).find(|&k| inside(seg_item(t, k), p)) {
            return Some((Ctl::Seg(t), k));
        }
    }
    for t in 0..3 {
        for i in 0..3 {
            let r = row(t, i);
            if m.range(t, i).is_some() && inside((r.0 - 6.0, r.1 + 18.0, r.2 + 12.0, r.3 - 14.0), p) {
                return Some((Ctl::Slider(t, i), 0));
            }
        }
    }
    None
}

fn ctl_rect(c: Ctl) -> R {
    match c {
        Ctl::Seg(t) => seg(t),
        Ctl::Slider(t, i) => {
            let r = row(t, i);
            (r.0, r.1 + 20.0, r.2, 24.0)
        }
    }
}

// ---- colours ----

/// ARGB colours of one theme.
pub(crate) struct Theme {
    pub(crate) dark: bool,
    pub(crate) bg: u32,
    pub(crate) card: u32,
    pub(crate) edge: u32,
    pub(crate) text: u32,
    pub(crate) text2: u32,
    pub(crate) seg: u32,
    pub(crate) hover: u32,
    pub(crate) track: u32,
    pub(crate) thumb: u32,
    pub(crate) thumb_edge: u32,
    pub(crate) accent: u32,
    pub(crate) on_accent: u32,
    pub(crate) ok: u32,
    pub(crate) bad: u32,
}

fn reg(path: PCWSTR, name: PCWSTR, buf: &mut [u8]) -> Option<usize> {
    let mut len = buf.len() as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, path, name, RRF_RT_ANY, null_mut(), buf.as_mut_ptr() as _, &mut len) };
    (r == 0).then_some(len as usize)
}

/// Dark or light as Windows' "app mode", accent from Windows' accent palette (Light 2 on dark,
/// Dark 1 on light, as Windows 11's own controls use it).
pub(crate) fn theme() -> Theme {
    let mut b = [0u8; 32];
    // WINSENSE_THEME=light|dark overrides Windows' setting (screenshots of both without touching it).
    let dark = match std::env::var("WINSENSE_THEME").as_deref() {
        Ok("light") => false,
        Ok("dark") => true,
        _ => reg(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("AppsUseLightTheme"), &mut b) == Some(4) && b[0] == 0,
    };
    let palette = reg(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"), w!("AccentPalette"), &mut b) == Some(32);
    let pick = |i: usize, fallback: u32| if palette { 0xFF00_0000 | (b[i * 4] as u32) << 16 | (b[i * 4 + 1] as u32) << 8 | b[i * 4 + 2] as u32 } else { fallback };
    if dark {
        Theme { dark, bg: 0xFF202020, card: 0xFF2B2B2B, edge: 0xFF1C1C1C, text: 0xFFFFFFFF, text2: 0xFFC5C5C5, seg: 0xFF1F1F1F, hover: 0xFF383838, track: 0xFF8B8B8B, thumb: 0xFF454545, thumb_edge: 0xFF575757, accent: pick(1, 0xFF4CC2FF), on_accent: 0xFF000000, ok: 0xFF6CCB5F, bad: 0xFFFF99A4 }
    } else {
        Theme { dark, bg: 0xFFF3F3F3, card: 0xFFFFFFFF, edge: 0xFFE5E5E5, text: 0xFF1A1A1A, text2: 0xFF5F5F5F, seg: 0xFFEDEDED, hover: 0xFFDCDCDC, track: 0xFF8A8A8A, thumb: 0xFFFFFFFF, thumb_edge: 0xFFCCCCCC, accent: pick(4, 0xFF005FB8), on_accent: 0xFFFFFFFF, ok: 0xFF0F7B0F, bad: 0xFFC42B1C }
    }
}

/// ARGB to GDI's 0x00BBGGRR.
pub(crate) fn cref(c: u32) -> u32 {
    (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF)
}

/// Title bar in the same colours as the window (Windows 11; Windows 10 takes only the dark flag).
fn title_bar(h: HWND, th: &Theme) {
    let set = |attr: u32, v: u32| unsafe { DwmSetWindowAttribute(h, attr, &v as *const u32 as _, 4) };
    set(20, th.dark as u32); // DWMWA_USE_IMMERSIVE_DARK_MODE
    set(35, cref(th.bg)); // DWMWA_CAPTION_COLOR
    set(36, cref(th.text)); // DWMWA_TEXT_COLOR
}

// ---- painting ----

/// GDI+ drawing on one device context, in layout units.
pub(crate) struct Pen {
    g: *mut GpGraphics,
    s: f32,
}

impl Pen {
    pub(crate) fn new(dc: HDC) -> Pen {
        let mut g = null_mut();
        unsafe {
            GdipCreateFromHDC(dc, &mut g);
            GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
            GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
        }
        Pen { g, s: scale() }
    }

    pub(crate) fn round(&self, (x, y, w, h): R, r: f32, fill: u32, edge: Option<u32>) {
        let s = self.s;
        let (x, y, w, h, d) = (x * s, y * s, w * s, h * s, (2.0 * r * s).min(w * s).min(h * s));
        if d < 1.0 {
            // No corners (the window background): a plain rectangle; zero-size arcs make no path.
            unsafe {
                let mut brush = null_mut();
                GdipCreateSolidFill(fill, &mut brush);
                GdipFillRectangle(self.g, brush as _, x, y, w, h);
                GdipDeleteBrush(brush as _);
            }
            return;
        }
        unsafe {
            let mut path = null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
            GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(path);
            let mut brush = null_mut();
            GdipCreateSolidFill(fill, &mut brush);
            GdipFillPath(self.g, brush as _, path);
            GdipDeleteBrush(brush as _);
            if let Some(c) = edge {
                let mut pen = null_mut();
                GdipCreatePen1(c, 1.0, UnitWorld, &mut pen);
                GdipDrawPath(self.g, pen, path);
                GdipDeletePen(pen);
            }
            GdipDeletePath(path);
        }
    }

    pub(crate) fn ring(&self, r: R, radius: f32, color: u32, width: f32) {
        let s = self.s;
        let (x, y, w, h, d) = (r.0 * s, r.1 * s, r.2 * s, r.3 * s, 2.0 * radius * s);
        unsafe {
            let mut path = null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
            GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(path);
            let mut pen = null_mut();
            GdipCreatePen1(color, width * s, UnitWorld, &mut pen);
            GdipDrawPath(self.g, pen, path);
            GdipDeletePen(pen);
            GdipDeletePath(path);
        }
    }

    pub(crate) fn disc(&self, (cx, cy): (f32, f32), r: f32, fill: u32) {
        let s = self.s;
        unsafe {
            let mut brush = null_mut();
            GdipCreateSolidFill(fill, &mut brush);
            GdipFillEllipse(self.g, brush as _, (cx - r) * s, (cy - r) * s, 2.0 * r * s, 2.0 * r * s);
            GdipDeleteBrush(brush as _);
        }
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.g) };
    }
}

fn text(dc: HDC, font: usize, s: &str, r: R, color: u32, align: u32) {
    text_with(dc, FONTS.with(|f| f[font]), s, r, color, align);
}

pub(crate) fn text_with(dc: HDC, font: HFONT, s: &str, (x, y, w, h): R, color: u32, align: u32) {
    let k = scale();
    let mut rc = RECT { left: (x * k) as i32, top: (y * k) as i32, right: ((x + w) * k) as i32, bottom: ((y + h) * k) as i32 };
    unsafe {
        SelectObject(dc, font);
        SetTextColor(dc, cref(color));
        let t: Vec<u16> = s.encode_utf16().collect();
        DrawTextW(dc, t.as_ptr(), t.len() as i32, &mut rc, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
}

fn paint(h: HWND) {
    let th = theme();
    let status = STATUS.lock().unwrap().clone();
    MODEL.with_borrow(|m| unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(h, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(h, &mut rc);
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, rc.right, rc.bottom);
        let old = SelectObject(mem, bmp);
        let focus = m.controls().get(m.focus).copied();
        {
            let p = Pen::new(mem);
            p.round((0.0, 0.0, W + 50.0, H + 50.0), 0.0, th.bg, None);
            for t in 0..3 {
                p.round(card(t), 8.0, th.card, Some(th.edge));
            }
            for t in 0..2 {
                p.round(seg(t), 6.0, th.seg, None);
                for k in 0..5 {
                    if k == m.kind[t] {
                        p.round(seg_item(t, k), 4.0, th.accent, None);
                    } else if m.hover == Some((Ctl::Seg(t), k)) {
                        p.round(seg_item(t, k), 4.0, th.hover, None);
                    }
                }
            }
            for t in 0..3 {
                for i in 0..3 {
                    let Some((_, lo, hi)) = m.range(t, i) else { continue };
                    let (x0, x1, y) = track(t, i);
                    let x = x_of(t, i, m.v[t][i], (lo, hi));
                    p.round((x0, y - 2.0, x1 - x0, 4.0), 2.0, th.track, None);
                    p.round((x0, y - 2.0, (x - x0).max(4.0), 4.0), 2.0, th.accent, None);
                    let c = Ctl::Slider(t, i);
                    p.disc((x, y), 10.5, th.thumb_edge);
                    p.disc((x, y), 9.5, th.thumb);
                    let inner = if m.drag == Some(c) { 5.0 } else if m.hover.map(|h| h.0) == Some(c) { 7.0 } else { 6.0 };
                    p.disc((x, y), inner, th.accent);
                }
            }
            if m.keys && let Some(c) = focus {
                let r = ctl_rect(c);
                p.ring((r.0 - 3.0, r.1 - 3.0, r.2 + 6.0, r.3 + 6.0), 7.0, th.text, 2.0);
            }
            let dot = if status.starts_with("Sent") || status.starts_with("Dry") { th.ok } else if status.is_empty() { th.text2 } else { th.bad };
            p.disc((M + 4.0, H - 22.0), 4.0, dot);
        }
        SetBkMode(mem, TRANSPARENT as i32);
        text(mem, 0, "Adaptive triggers", (M, 12.0, W - 2.0 * M, 32.0), th.text, DT_LEFT);
        text(mem, 3, "Pick an effect, then squeeze the trigger. Closing this window switches everything off.", (M, 44.0, W - 2.0 * M, 20.0), th.text2, DT_LEFT);
        for (t, (name, side)) in [("L2", "left trigger"), ("R2", "right trigger"), ("Rumble", "")].iter().enumerate() {
            let c = card(t);
            text(mem, 1, name, (c.0 + 14.0, c.1 + 10.0, 60.0, 24.0), th.text, DT_LEFT);
            if !side.is_empty() {
                text(mem, 3, &format!("\u{00B7} {side}"), (c.0 + 38.0, c.1 + 10.0, c.2 - 52.0, 24.0), th.text2, DT_LEFT);
            }
        }
        for t in 0..2 {
            for (k, name) in NAMES.iter().enumerate() {
                text(mem, 3, name, seg_item(t, k), if k == m.kind[t] { th.on_accent } else { th.text }, DT_CENTER);
            }
            if m.kind[t] == 0 {
                let c = card(t);
                text(mem, 3, "No effect: the trigger moves freely.", (c.0 + 14.0, c.1 + 90.0, c.2 - 28.0, 20.0), th.text2, DT_LEFT);
            }
        }
        for t in 0..3 {
            for i in 0..3 {
                let Some((label, ..)) = m.range(t, i) else { continue };
                let r = row(t, i);
                text(mem, 2, label, (r.0, r.1, r.2 - 40.0, 18.0), th.text, DT_LEFT);
                text(mem, 2, &m.v[t][i].to_string(), (r.0 + r.2 - 40.0, r.1, 40.0, 18.0), th.text2, DT_RIGHT);
            }
        }
        let line = if status.is_empty() { "Nothing sent yet" } else { status.as_str() };
        text(mem, 3, line, (M + 14.0, H - 32.0, W - 2.0 * M - 14.0, 20.0), th.text2, DT_LEFT);
        BitBlt(dc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(h, &ps);
    });
}

// ---- input ----

/// After any change: repaint, and hand the new state to the worker (it skips repeats).
fn changed(h: HWND) {
    let cmd = MODEL.with_borrow(|m| m.cmd());
    TX.with_borrow(|tx| tx.as_ref().map(|tx| tx.send(Msg::Ui(cmd))));
    unsafe { InvalidateRect(h, null(), 0) };
}

fn point(l: LPARAM) -> (f32, f32) {
    let s = scale();
    ((l & 0xFFFF) as i16 as f32 / s, ((l >> 16) & 0xFFFF) as i16 as f32 / s)
}

fn key(h: HWND, vk: u16) -> bool {
    let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
    let step: i32 = match vk {
        VK_LEFT | VK_DOWN => -1,
        VK_RIGHT | VK_UP => 1,
        VK_PRIOR => 10,
        VK_NEXT => -10,
        VK_HOME => -1000,
        VK_END => 1000,
        VK_TAB => 0,
        _ => return false,
    };
    MODEL.with_borrow_mut(|m| {
        m.keys = true;
        let n = m.controls().len();
        if vk == VK_TAB {
            m.focus = if shift { (m.focus + n - 1) % n } else { (m.focus + 1) % n };
            return;
        }
        match m.controls()[m.focus.min(n - 1)] {
            Ctl::Seg(t) => {
                let kind = (m.kind[t] as i32 + step.signum()).clamp(0, 4) as usize;
                if kind != m.kind[t] {
                    m.pick(t, kind);
                }
            }
            Ctl::Slider(t, i) => {
                let v = m.v[t][i] as i32;
                m.set(t, i, v.saturating_add(step));
            }
        }
    });
    changed(h);
    true
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => unsafe {
            let p = point(l);
            let hit = MODEL.with_borrow(|m| hit(m, p));
            if let Some((c, k)) = hit {
                MODEL.with_borrow_mut(|m| {
                    m.keys = false;
                    m.focus = m.controls().iter().position(|&x| x == c).unwrap_or(0);
                    match c {
                        Ctl::Seg(t) if k != m.kind[t] => m.pick(t, k),
                        Ctl::Seg(_) => {}
                        Ctl::Slider(t, i) => {
                            m.drag = Some(c);
                            let r = m.range(t, i).map(|(_, lo, hi)| (lo, hi)).unwrap();
                            m.set(t, i, value_at(t, i, p.0, r));
                        }
                    }
                });
                if matches!(c, Ctl::Slider(..)) {
                    SetCapture(h);
                }
                changed(h);
            }
            0
        },
        WM_MOUSEMOVE => unsafe {
            let p = point(l);
            let redraw = MODEL.with_borrow_mut(|m| {
                if let Some(Ctl::Slider(t, i)) = m.drag {
                    let r = m.range(t, i).map(|(_, lo, hi)| (lo, hi)).unwrap();
                    let before = m.v[t][i];
                    m.set(t, i, value_at(t, i, p.0, r));
                    return Some(before != m.v[t][i]);
                }
                let now = hit(m, p);
                let moved = now != m.hover;
                m.hover = now;
                moved.then_some(false)
            });
            let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut track);
            match redraw {
                Some(true) => changed(h),
                Some(false) => {
                    InvalidateRect(h, null(), 0);
                }
                None => {}
            }
            0
        },
        WM_MOUSELEAVE => unsafe {
            MODEL.with_borrow_mut(|m| m.hover = None);
            InvalidateRect(h, null(), 0);
            0
        },
        WM_LBUTTONUP => unsafe {
            MODEL.with_borrow_mut(|m| m.drag = None);
            ReleaseCapture();
            InvalidateRect(h, null(), 0);
            0
        },
        WM_MOUSEWHEEL => {
            let up = ((w >> 16) & 0xFFFF) as i16 > 0;
            let over = MODEL.with_borrow(|m| m.hover.map(|x| x.0));
            if let Some(Ctl::Slider(t, i)) = over {
                MODEL.with_borrow_mut(|m| {
                    let v = m.v[t][i] as i32;
                    m.set(t, i, v + if up { 1 } else { -1 });
                });
                changed(h);
            }
            0
        }
        WM_KEYDOWN if key(h, w as u16) => 0,
        WM_SETTINGCHANGE => unsafe {
            title_bar(h, &theme()); // dark or light, or the accent colour, may have changed
            InvalidateRect(h, null(), 0);
            0
        },
        WM_STATUS => unsafe {
            InvalidateRect(h, null(), 0);
            0
        },
        WM_DESTROY => unsafe {
            SCREEN.store(0, Ordering::Relaxed);
            if IN_TRAY.load(Ordering::Relaxed) {
                // The worker stays with the tray; a repeat of its last state (OFF) writes nothing.
                TX.with_borrow(|tx| tx.as_ref().map(|tx| tx.send(Msg::Ui(OFF))));
            } else {
                TX.set(None); // ends the worker, which switches the triggers off
                PostQuitMessage(0);
            }
            0
        },
        _ => unsafe { DefWindowProcW(h, msg, w, l) },
    }
}

pub(crate) fn gdiplus() {
    static GDIPLUS: std::sync::Once = std::sync::Once::new();
    GDIPLUS.call_once(|| unsafe {
        let mut token = 0;
        GdiplusStartup(&mut token, &GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 }, null_mut());
    });
}

/// Make the window, with every control at its start (all off).
fn create(visible: bool) -> HWND {
    gdiplus();
    MODEL.set(Model::new());
    STATUS.lock().unwrap().clear();
    unsafe {
        let inst = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = inst;
        wc.lpszClassName = w!("WinSense");
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        RegisterClassW(&wc); // fails harmlessly when the class is already there (tests)
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: px(W as i32), bottom: px(H as i32) };
        AdjustWindowRectEx(&mut r, style, 0, 0);
        let h = CreateWindowExW(0, w!("WinSense"), w!("WinSense"), style, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top, null_mut(), null_mut(), inst, null());
        SCREEN.store(h as isize, Ordering::Relaxed);
        SendMessageW(h, WM_SETICON, ICON_SMALL as usize, crate::tray::icon(true) as isize);
        SendMessageW(h, WM_SETICON, ICON_BIG as usize, crate::tray::icon(false) as isize);
        title_bar(h, &theme());
        if visible {
            ShowWindow(h, SW_SHOW);
        }
        h
    }
}

/// Write one command to every DualSense, or print it when there is no `api` (dry run).
/// Returns the line for the status bar.
fn write_all(api: Option<&mut hidapi::HidApi>, seq: u8, (l2, r2, [left, right]): Cmd) -> String {
    let light = LIGHT.swap(NO_LIGHT, Ordering::Relaxed);
    let glow = GLOW.lock().unwrap().take();
    let report = |bt, v2| {
        let r = ds::trigger_report(bt, seq, Some(l2), Some(r2), None, ds::Rumble { left, right, v2 }).expect("effect_from keeps every value in range");
        let r = if light == NO_LIGHT { r } else { ds::with_mute_light(bt, r, light) };
        glow.map_or(r.clone(), |g| ds::with_glow(bt, r, g))
    };
    let fade = glow.is_some_and(|g| g.0.is_some()) && !FADED.load(Ordering::Relaxed);
    let setup = |v2| ds::output_report(true, seq, &ds::Lights::Setup, ds::Rumble { left, right, v2 }); // carries the rumble, so it does not stop it
    let Some(api) = api else {
        if fade {
            FADED.store(true, Ordering::Relaxed);
            println!("dry BT  {} (light bar setup, once per connection)", crate::hex(&setup(true)));
        }
        println!("dry USB {}\ndry BT  {}", crate::hex(&report(false, true)), crate::hex(&report(true, true)));
        return "Dry run: reports printed, nothing sent".into();
    };
    // shortcut: looks for controllers again on every write, so a reconnect just works; keep them open if writes must get faster
    let _ = api.refresh_devices();
    let (mut sent, mut failed) = (Vec::new(), Vec::new());
    for info in api.device_list().filter(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id())) {
        let bt = matches!(info.bus_type(), hidapi::BusType::Bluetooth);
        let link = if bt { "Bluetooth" } else { "USB" };
        // The rumble form is only looked up while rumbling: one feature report per write.
        let write = |d: hidapi::HidDevice| {
            let v2 = left | right != 0 && crate::vibration_v2(&d, info.product_id());
            if bt && fade {
                d.write(&setup(v2))?;
                FADED.store(true, Ordering::Relaxed);
            }
            d.write(&report(bt, v2))
        };
        match info.open_device(api).and_then(write) {
            Ok(_) => sent.push(link.to_string()),
            Err(e) => failed.push(format!("{link} ({e})")),
        }
    }
    match (sent.is_empty(), failed.is_empty()) {
        (true, true) => "No DualSense is connected".into(),
        (_, true) => format!("Sent to your DualSense ({})", sent.join(" and ")),
        _ => format!("Could not send by {}", failed.join(", ")),
    }
}

/// The function that carries out one command: it owns the controllers (none when `dry`).
fn sender(dry: bool) -> impl FnMut(Cmd) -> String {
    let mut api = if dry { None } else { Some(hidapi::HidApi::new()) };
    let mut seq = 0u8;
    move |cmd| {
        seq = (seq + 1) & 15;
        match &mut api {
            None => write_all(None, seq, cmd),
            Some(Ok(api)) => write_all(Some(api), seq, cmd),
            Some(Err(e)) => format!("Cannot reach HID devices: {e}"),
        }
    }
}

/// Carries out the newest state, reports to the status line, and switches both
/// triggers and the rumble off once the window, the pad and the port are gone (an effect outlives the program).
fn worker(rx: mpsc::Receiver<Msg>, mut go: impl FnMut(Cmd) -> String) {
    use std::time::Instant;
    let (mut ui, mut game, mut sent) = (OFF, [0u8; 2], false);
    let (mut m, mut pulse): (Option<(u32, Cmd)>, Option<(Cmd, Instant)>) = (None, None);
    let mut tone: Option<Instant> = None;
    // A mod's lights: lasting (with its owner), on top for a while, and what the controller shows now.
    let (mut leds, mut blink, mut lit): ((u32, ds::Glow), Option<(ds::Glow, Instant)>, ds::Glow) = ((0, DARK), None, DARK);
    let mut mute = false; // the mute light is lit (src/desk.rs)
    loop {
        // Wait for a message, or until a pulse or a tone ends.
        let first = match pulse.map(|p| p.1).into_iter().chain(tone).chain(blink.map(|b| b.1)).min() {
            Some(end) => match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => break,
            },
        };
        let mut changed = first.is_none();
        if first.is_none() {
            let now = Instant::now();
            (pulse, tone, blink) = (pulse.filter(|p| p.1 > now), tone.filter(|&t| t > now), blink.filter(|b| b.1 > now));
        }
        // A slider drag or a game queues many; only the newest of each matters.
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::Ui(c) if c != ui => (ui, changed) = (c, true),
                Msg::Ui(_) => {}
                // ViGEm answers with 0, 0 at once and repeats unchanged values (LED updates): no write for those.
                Msg::Game(g) if g != game => (game, changed) = (g, true),
                Msg::Game(_) => {}
                Msg::Mod(id, Some(c)) if m != Some((id, c)) => (m, changed) = (Some((id, c)), true),
                Msg::Mod(id, None) if m.is_some_and(|(owner, _)| owner == id) => (m, changed) = (None, true),
                Msg::Mod(..) => {}
                Msg::Pulse(c, d) => (pulse, changed) = (Some((c, Instant::now() + d)), true),
                Msg::Tone(d) => (tone, changed) = ((!d.is_zero()).then(|| Instant::now() + d), true),
                Msg::Light(v) => {
                    LIGHT.store(v, Ordering::Relaxed);
                    (mute, changed) = (v == 1, true);
                }
                Msg::Leds(id, g) if g != DARK => (leds, changed) = ((id, g), true),
                Msg::Leds(id, _) if leds.0 == id && leds.1 != DARK => (leds, changed) = ((id, DARK), true),
                Msg::Leds(..) => {}
                Msg::Blink(g, d) => (blink, changed) = (Some((g, Instant::now() + d)), true),
            }
        }
        if !changed {
            continue;
        }
        sent = true;
        let mut c = merge(ui, game, m.map(|m| m.1), pulse.map(|p| p.0));
        crate::power::EFFECT.store(c != OFF || tone.is_some(), Ordering::Relaxed); // in use: not idle (src/power.rs)
        if tone.is_some() {
            // Felt: a tone over a rumble is lost (the rumble only stumbles). A report
            // without rumble bits hands the grips back to the tone; the rumble returns after it.
            c.2 = [0, 0];
        }
        RUMBLE.store(c.2 != [0, 0], Ordering::Relaxed);
        let want = glow(leds.1, blink.map(|b| b.0));
        if want != lit {
            if lit.0.is_some() && want.0.is_none() || lit.1.is_some() && want.1.is_none() {
                // A light is given back. The release bit hands back every light, so it goes alone
                // and what stays lit (the other light, the mute light) follows in the next report.
                *GLOW.lock().unwrap() = Some(DARK);
                go(c);
                if mute {
                    LIGHT.store(1, Ordering::Relaxed);
                }
            }
            if want != DARK {
                *GLOW.lock().unwrap() = Some(want);
            }
            lit = want;
        }
        *STATUS.lock().unwrap() = go(c);
        unsafe { PostMessageW(SCREEN.load(Ordering::Relaxed) as HWND, WM_STATUS, 0, 0) };
    }
    crate::power::EFFECT.store(false, Ordering::Relaxed);
    RUMBLE.store(false, Ordering::Relaxed);
    if lit != DARK {
        *GLOW.lock().unwrap() = Some(DARK); // a mod's lights are still on: the closing report hands them back
    }
    if sent {
        go(OFF);
    }
}

/// Start the worker that owns every write to the controllers; this thread's window feeds it.
/// Returns a sender for others (the virtual pad's game rumble). The worker ends once every sender is gone.
pub fn start(dry: bool) -> (mpsc::Sender<Msg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    TX.set(Some(tx.clone()));
    (tx, std::thread::spawn(move || worker(rx, sender(dry))))
}

/// For the tray: show the window (made if not open).
pub fn open() -> HWND {
    let h = SCREEN.load(Ordering::Relaxed) as HWND;
    if h.is_null() {
        return create(true);
    }
    unsafe {
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
    }
    h
}

/// For the tray's Quit: close the window, then end the worker, which switches everything off.
pub fn stop(worker: std::thread::JoinHandle<()>) {
    let h = SCREEN.load(Ordering::Relaxed) as HWND;
    if !h.is_null() {
        unsafe { DestroyWindow(h) };
    }
    TX.set(None);
    let _ = worker.join();
}

pub fn run(dry: bool, with_pad: bool) {
    let (game, worker) = start(dry);
    let h = create(true);
    // Touchpad and gyro as mouse: reads only, so it needs nothing from the worker (src/mouse.rs).
    let stop_mouse = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mouse = (!dry).then(|| {
        let stop = stop_mouse.clone();
        std::thread::spawn(move || crate::mouse::run(&stop, false))
    });
    let pad = (with_pad && !dry).then(|| crate::pad::Pad::start(move |m| { let _ = game.send(Msg::Game(m)); }));
    let title = match &pad {
        None => "WinSense".to_string(),
        Some(Ok(_)) => "WinSense - virtual Xbox 360 pad on".to_string(),
        Some(Err(e)) => format!("WinSense - no virtual pad ({e})"),
    };
    unsafe { SetWindowTextW(h, wide(&title).as_ptr()) };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    stop_mouse.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(m) = mouse {
        let _ = m.join(); // lets go of a held mouse button
    }
    drop(pad); // unplugs the virtual pad and drops its channel end, so the worker can end
    let _ = worker.join(); // it switches the triggers off before it ends
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects_stay_in_range() {
        for kind in 0..5 {
            let at = |f: fn((&str, u8, u8, u8)) -> u8| KNOBS[kind].map(|k| k.map_or(0, f));
            for v in [at(|k| k.1), at(|k| k.2), at(|k| k.3), [0; 3], [255; 3]] {
                assert!(effect_from(kind, v).bytes().is_some(), "{} {v:?}", NAMES[kind]);
            }
        }
        let start = |kind: usize| effect_from(kind, KNOBS[kind].map(|k| k.map_or(0, |k| k.3)));
        assert_eq!(start(0), Effect::Off);
        assert_eq!(start(1), Effect::Resist { start: 0, strength: 4 });
        assert_eq!(start(2), Effect::Resist { start: 5, strength: 8 });
        assert_eq!(start(3), Effect::Click { start: 3, end: 5, strength: 6 });
        assert_eq!(start(4), Effect::Vibrate { start: 2, strength: 4, hz: 30 });
    }

    #[test]
    fn layout_fits_and_does_not_overlap() {
        for t in 0..3 {
            let c = card(t);
            assert!(c.0 >= M && c.0 + c.2 <= W - M + 0.01 && c.1 + c.3 <= H - 36.0, "card {t} inside, above the status line");
            for i in 0..3 {
                let r = row(t, i);
                if t < 2 || i < 2 {
                    assert!(r.1 + r.3 <= c.1 + c.3, "row {t}/{i} inside its card");
                }
            }
        }
        assert!(seg(0).1 + seg(0).3 < row(0, 0).1, "picker above the first slider");
    }

    #[test]
    fn worker_switches_off_when_the_window_goes() {
        let run = |cmds: &[Msg]| {
            let (tx, rx) = mpsc::channel();
            cmds.iter().for_each(|&c| tx.send(c).unwrap());
            drop(tx); // the window is gone
            let mut done = Vec::new();
            worker(rx, |c| {
                done.push(c);
                String::new()
            });
            done
        };
        let wall = Effect::Resist { start: 5, strength: 8 };
        // Queued commands collapse to the newest, then both triggers are switched off.
        let ui = Msg::Ui;
        assert_eq!(run(&[ui(OFF), ui((wall, Effect::Off, [0, 0])), ui((wall, wall, [90, 0]))]), [(wall, wall, [90, 0]), OFF]);
        assert_eq!(run(&[]), [], "nothing was sent, so nothing is written on close");
        assert_eq!(run(&[ui(OFF)]), [], "the tray's close with nothing sent writes nothing");
        assert_eq!(run(&[ui((wall, wall, [0, 0])), ui(OFF)]), [OFF, OFF], "closing after a change switches off (and the end repeats it)");
        // Game rumble keeps the window's triggers; each motor takes the stronger value.
        assert_eq!(run(&[ui((wall, wall, [90, 0])), Msg::Game([40, 200])]), [(wall, wall, [90, 200]), OFF]);
        assert_eq!(run(&[Msg::Game([40, 0])]), [(Effect::Off, Effect::Off, [40, 0]), OFF]);
        assert_eq!(run(&[Msg::Game([0, 0]), Msg::Game([0, 0])]), [], "the pad's opening 0, 0 answers write nothing");
        // A mod's triggers win over the window's while set; motors take the strongest.
        let vib = Effect::Vibrate { start: 1, strength: 8, hz: 8 };
        let beam = Msg::Mod(1, Some((vib, Effect::Off, [0, 60])));
        assert_eq!(run(&[ui((wall, wall, [90, 0])), beam]), [(vib, wall, [90, 60]), OFF]);
        assert_eq!(run(&[ui((wall, wall, [0, 0])), beam, Msg::Mod(1, None)]), [(wall, wall, [0, 0]), OFF], "a mod that leaves gives the triggers back");
        assert_eq!(run(&[beam, Msg::Mod(2, None)]), [(vib, Effect::Off, [0, 60]), OFF], "another mod leaving does not end this one's beam");
        assert_eq!(run(&[beam]), [(vib, Effect::Off, [0, 60]), OFF], "the port closing (Quit) switches off");
    }

    /// Tests that read the statics `LIGHT` and `GLOW` run one at a time.
    static LIGHTS: Mutex<()> = Mutex::new(());

    /// The mute light rides in a report that carries the triggers and rumble as they are.
    #[test]
    fn the_mute_light_goes_out_with_the_current_state() {
        let _one = LIGHTS.lock().unwrap();
        let wall = Effect::Resist { start: 5, strength: 8 };
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Ui((wall, Effect::Off, [90, 0]))).unwrap();
        tx.send(Msg::Light(1)).unwrap();
        drop(tx);
        let mut done = Vec::new();
        worker(rx, |c| {
            done.push((c, LIGHT.swap(NO_LIGHT, Ordering::Relaxed))); // what write_all does
            String::new()
        });
        assert_eq!(done, [((wall, Effect::Off, [90, 0]), 1), (OFF, NO_LIGHT)], "one report with the light and the wall; the closing one leaves the light alone");
        for bt in [false, true] {
            let c = if bt { 3 } else { 1 };
            let plain = ds::trigger_report(bt, 1, Some(wall), Some(Effect::Off), None, ds::Rumble { left: 90, right: 0, v2: true }).unwrap();
            let lit = ds::with_mute_light(bt, plain.clone(), 1);
            assert_eq!((lit[c + 1], lit[c + 8]), (0x01, 1), "valid1 bit 0 and the mute light byte");
            let (mut same, end) = (lit.clone(), if bt { 74 } else { 48 });
            (same[c + 1], same[c + 8]) = (0, 0);
            assert_eq!(same[..end], plain[..end], "nothing else changed");
            if bt {
                assert_eq!(lit[74..], ds::crc32(0xA2, &lit[..74]).to_le_bytes());
            }
        }
    }

    /// A mod's lights: the blink shows on top, a light given back goes out with the release bit
    /// alone, what stays lit follows, and the closing report hands back what is still on.
    #[test]
    fn mod_lights_layer_and_are_given_back() {
        let _one = LIGHTS.lock().unwrap();
        let (blue, red) = (Some([0, 0, 255]), Some([255, 0, 0]));
        assert_eq!(glow((blue, Some(4)), Some((red, None))), (red, Some(4)), "the blink's colour over the lasting one, the lasting players show through");
        assert_eq!(glow((blue, None), None), (blue, None));
        assert_eq!(glow(DARK, Some((None, Some(31)))), (None, Some(31)));
        let run = |msgs: &[Msg], wait: u64| {
            let (tx, rx) = mpsc::channel();
            msgs.iter().for_each(|&m| tx.send(m).unwrap());
            let t = std::thread::spawn(move || {
                let mut done = Vec::new();
                worker(rx, |c| {
                    done.push((c, GLOW.lock().unwrap().take(), LIGHT.swap(NO_LIGHT, Ordering::Relaxed))); // what write_all does
                    String::new()
                });
                done
            });
            std::thread::sleep(std::time::Duration::from_millis(wait));
            drop(tx);
            t.join().unwrap()
        };
        let wall = Effect::Resist { start: 5, strength: 8 };
        let ui = (wall, Effect::Off, [90, 0]);
        let ms = std::time::Duration::from_millis;
        // Lasting blue with a red flash on top, then the port closes (Quit).
        assert_eq!(run(&[Msg::Ui(ui), Msg::Leds(1, (blue, None)), Msg::Blink((red, None), ms(60))], 400), [
            (ui, Some((red, None)), NO_LIGHT),
            (ui, Some((blue, None)), NO_LIGHT), // the flash is over: the lasting colour is back; the wall and rumble never left
            (OFF, Some(DARK), NO_LIGHT),        // Quit hands the lights back
        ]);
        // A flash with no lasting colour ends with the release; without any light there is none.
        assert_eq!(run(&[Msg::Ui(ui), Msg::Blink((red, None), ms(60))], 400), [(ui, Some((red, None)), NO_LIGHT), (ui, Some(DARK), NO_LIGHT), (ui, None, NO_LIGHT), (OFF, None, NO_LIGHT)]);
        assert_eq!(run(&[Msg::Ui(ui)], 50), [(ui, None, NO_LIGHT), (OFF, None, NO_LIGHT)]);
        // The mod disconnects: only its own lights go, and the lit mute light is sent again after the release.
        let both = (blue, Some(4));
        assert_eq!(run(&[Msg::Light(1), Msg::Leds(1, both)], 50)[0], (OFF, Some(both), 1));
        let gone = run(&[Msg::Leds(1, both), Msg::Light(1), Msg::Leds(2, DARK)], 50);
        assert_eq!(gone, [(OFF, Some(both), 1), (OFF, Some(DARK), NO_LIGHT)], "another mod leaving changes nothing; Quit hands back");
        // The bar is given back while the player lights stay: the release, then the players (and the mute light) again.
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Leds(1, both)).unwrap();
        tx.send(Msg::Light(1)).unwrap();
        let (up, wrote) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut done = Vec::new();
            worker(rx, |_| {
                done.push((GLOW.lock().unwrap().take(), LIGHT.swap(NO_LIGHT, Ordering::Relaxed)));
                let _ = up.send(());
                String::new()
            });
            done
        });
        wrote.recv().unwrap();
        tx.send(Msg::Leds(1, (None, Some(4)))).unwrap();
        wrote.recv().unwrap();
        wrote.recv().unwrap();
        tx.send(Msg::Leds(1, DARK)).unwrap();
        wrote.recv().unwrap();
        wrote.recv().unwrap();
        drop(tx);
        assert_eq!(t.join().unwrap(), [(Some(both), 1), (Some(DARK), NO_LIGHT), (Some((None, Some(4))), 1), (Some(DARK), NO_LIGHT), (None, 1), (None, NO_LIGHT)]);
        for bt in [false, true] {
            let c = if bt { 3 } else { 1 };
            let plain = ds::trigger_report(bt, 1, Some(wall), Some(Effect::Off), None, ds::Rumble { left: 90, right: 0, v2: true }).unwrap();
            let lit = ds::with_glow(bt, plain.clone(), (red, Some(4)));
            assert_eq!((lit[c + 1], lit[c + 43], &lit[c + 44..c + 47]), (0x14, 4, &[255, 0, 0][..]), "valid1 light bar + player lights, their bytes");
            let back = ds::with_glow(bt, plain.clone(), DARK);
            assert_eq!(back[c + 1], 0x08, "the release bit alone");
            assert_eq!((lit[c], &lit[c + 2..c + 43]), (plain[c], &plain[c + 2..c + 43]), "triggers and rumble untouched");
            assert_eq!((back[c], &back[c + 2..c + 47]), (plain[c], &plain[c + 2..c + 47]));
            if bt {
                assert_eq!(lit[74..], ds::crc32(0xA2, &lit[..74]).to_le_bytes());
            }
        }
    }

    #[test]
    fn a_pulse_plays_on_top_then_ends() {
        let (tx, rx) = mpsc::channel();
        let vib = Effect::Vibrate { start: 1, strength: 8, hz: 8 };
        let wall = Effect::Resist { start: 0, strength: 8 };
        tx.send(Msg::Mod(1, Some((vib, vib, [0, 0])))).unwrap();
        tx.send(Msg::Pulse((wall, Effect::Off, [255, 255]), std::time::Duration::from_millis(60))).unwrap();
        let t = std::time::Instant::now();
        let (up, started) = mpsc::channel(); // a busy PC can start the thread late: the clock runs from its first write
        let w = std::thread::spawn(move || {
            let mut done = Vec::new();
            worker(rx, |c| {
                done.push((c, t.elapsed().as_millis()));
                let _ = up.send(());
                String::new()
            });
            done
        });
        started.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(tx);
        let done = w.join().unwrap();
        let cmds: Vec<Cmd> = done.iter().map(|d| d.0).collect();
        assert_eq!(cmds, [(wall, vib, [255, 255]), (vib, vib, [0, 0]), OFF]);
        let ms = done[1].1 - done[0].1;
        assert!((55..190).contains(&ms), "the pulse ended after its 60 ms ({ms} ms)");
    }

    #[test]
    fn a_tone_rests_the_motors_then_gives_them_back() {
        let (tx, rx) = mpsc::channel();
        let wall = Effect::Resist { start: 5, strength: 8 };
        tx.send(Msg::Mod(1, Some((wall, Effect::Off, [150, 150])))).unwrap();
        tx.send(Msg::Tone(std::time::Duration::from_millis(60))).unwrap();
        let (up, started) = mpsc::channel(); // as above: wait for the worker's first write
        let w = std::thread::spawn(move || {
            let mut done = Vec::new();
            worker(rx, |c| {
                done.push(c);
                let _ = up.send(());
                String::new()
            });
            done
        });
        started.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        tx.send(Msg::Tone(std::time::Duration::from_secs(5))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        tx.send(Msg::Tone(std::time::Duration::ZERO)).unwrap(); // `stop` cuts the tone
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(tx);
        let quiet = (wall, Effect::Off, [0, 0]);
        let loud = (wall, Effect::Off, [150, 150]);
        assert_eq!(w.join().unwrap(), [quiet, loud, quiet, loud, OFF], "the trigger stays; the motors rest only while a tone plays");
    }

    /// The real window, never shown, driven by mouse and key messages at the controls' places.
    #[test]
    fn clicks_and_keys_become_commands() {
        let (tx, rx) = mpsc::channel();
        TX.set(Some(tx));
        let h = create(false);
        assert!(!h.is_null());
        assert!(rx.try_recv().is_err(), "nothing may be sent before the first change");
        let s = scale();
        let at = |x: f32, y: f32| ((x * s).round() as isize & 0xFFFF) | (((y * s).round() as isize & 0xFFFF) << 16);
        let click = |x: f32, y: f32| unsafe {
            SendMessageW(h, WM_LBUTTONDOWN, 1, at(x, y));
            SendMessageW(h, WM_LBUTTONUP, 0, at(x, y));
        };
        let pick = |t: usize, k: usize| {
            let r = seg_item(t, k);
            click(r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
        };
        // A value's place on the track; past the ends for values out of range.
        let slide = |t: usize, i: usize, v: i32| {
            let (lo, hi) = MODEL.with_borrow(|m| m.range(t, i).map(|(_, lo, hi)| (lo, hi)).unwrap());
            let (x0, x1, y) = track(t, i);
            let x = if v > hi as i32 { x1 + 5.0 } else { x_of(t, i, v as u8, (lo, hi)).min(x1).max(x0) };
            click(x, y);
        };
        let click3 = |start, end, strength| Effect::Click { start, end, strength };
        pick(1, 3); // R2: Click, with its start values
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((Effect::Off, click3(3, 5, 6), [0, 0]))));
        slide(1, 2, 8); // strength
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((Effect::Off, click3(3, 5, 8), [0, 0]))));
        slide(1, 0, 6); // start moves past the end: the end follows
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((Effect::Off, click3(6, 7, 8), [0, 0]))));
        slide(1, 1, 99); // past the track's end stops at 8
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((Effect::Off, click3(6, 8, 8), [0, 0]))));
        pick(0, 4); // L2: Vibration; R2 keeps its effect
        let vib = Effect::Vibrate { start: 2, strength: 4, hz: 30 };
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((vib, click3(6, 8, 8), [0, 0]))));
        slide(2, 0, 120); // rumble left; the triggers keep their effects
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((vib, click3(6, 8, 8), [120, 0]))));
        slide(2, 1, 999); // rumble right, stopped at 255
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((vib, click3(6, 8, 8), [120, 255]))));
        pick(0, 2); // L2: Wall; the rumble stays
        let wall = Effect::Resist { start: 5, strength: 8 };
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((wall, click3(6, 8, 8), [120, 255]))));
        // Keyboard: the picker clicked last has the focus; Right picks the next effect, Tab moves on.
        unsafe { SendMessageW(h, WM_KEYDOWN, VK_RIGHT as usize, 0) };
        assert_eq!(rx.try_recv(), Ok(Msg::Ui((click3(3, 5, 6), click3(6, 8, 8), [120, 255]))), "Right on L2's picker: Wall -> Click");
        unsafe { SendMessageW(h, WM_KEYDOWN, VK_TAB as usize, 0) }; // to L2's first slider (Start)
        unsafe { SendMessageW(h, WM_KEYDOWN, VK_END as usize, 0) };
        assert_eq!(MODEL.with_borrow(|m| m.v[0][0]), 7, "End = the slider's highest value");
        unsafe { DestroyWindow(h) };
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected), "closing must end the worker's channel");
    }
}
