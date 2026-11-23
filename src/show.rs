//! "show <n>": a live copy of monitor n, full screen on the primary monitor, and
//! the cursor moved onto monitor n, so the touchpad mouse and the couch stick drive the real
//! windows there while they are watched on the primary. Drawn by the Windows Magnification API, which
//! also draws the cursor (MS_SHOWMAGNIFIEDCURSOR). The copy takes no focus and lets clicks through.
//! Closed by voice ("close", "show main", "show one" when 1 is the primary) or Circle in couch
//! mode; closing puts the cursor back on the primary monitor. Runs on its own thread.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Magnification::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

/// A copy is on screen.
pub static OPEN: AtomicBool = AtomicBool::new(false);
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The copy's thread, joined before the next copy starts (the Magnification API is set up and
/// torn down per thread, and two at once are not documented to work).
static THREAD: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(None);

/// Windows' display number from a monitor's device name ("\\.\DISPLAY2" = 2), the number
/// Settings > Display shows in most setups.
fn number(device: &str) -> Option<u32> {
    device.strip_prefix(r"\\.\DISPLAY")?.parse().ok()
}

/// (number, monitor rect as left, top, right, bottom, primary) for every monitor.
pub(crate) fn monitors() -> Vec<(u32, [i32; 4], bool)> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> windows_sys::core::BOOL {
        unsafe {
            let mut mi: MONITORINFOEXW = std::mem::zeroed();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(m, &mut mi as *mut _ as *mut MONITORINFO) != 0 {
                let len = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
                let r = mi.monitorInfo.rcMonitor;
                if let Some(n) = number(&String::from_utf16_lossy(&mi.szDevice[..len])) {
                    (*(out as *mut Vec<(u32, [i32; 4], bool)>)).push((n, [r.left, r.top, r.right, r.bottom], mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0));
                }
            }
            1
        }
    }
    let mut all = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null_mut(), Some(each), &mut all as *mut _ as LPARAM) };
    all
}

/// Where the copy of a `src`-sized picture goes inside `dst` (left, top, width, height; aspect
/// kept, centred, black bars), and its scale.
fn fit(src: [i32; 4], dst: [i32; 4]) -> ([i32; 4], f32) {
    let (sw, sh, dw, dh) = (src[2] - src[0], src[3] - src[1], dst[2] - dst[0], dst[3] - dst[1]);
    let k = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
    let (w, h) = ((sw as f32 * k).round() as i32, (sh as f32 * k).round() as i32);
    ([(dw - w) / 2, (dh - h) / 2, w, h], k)
}

fn centre(r: [i32; 4]) -> (i32, i32) {
    ((r[0] + r[2]) / 2, (r[1] + r[3]) / 2)
}

/// While alive, this thread sees real pixels. WinSense is only system DPI aware, so a side monitor
/// at another scale gets stretched coordinates, but the Magnification API copies real pixels
/// (measured on mixed 150 %/100 % monitors: a 1920x1080 side monitor read as 2880x1620).
pub(crate) struct Physical(DPI_AWARENESS_CONTEXT);
impl Physical {
    pub(crate) fn new() -> Self {
        Physical(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for Physical {
    fn drop(&mut self) {
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

/// Show monitor `n` on the primary one and put the cursor on it; the primary itself = close.
/// False when there is no monitor `n` or the copy could not be made.
pub fn open(n: u32) -> bool {
    let _dpi = Physical::new();
    let all = monitors();
    let (Some(src), Some(dst)) = (all.iter().find(|m| m.0 == n), all.iter().find(|m| m.2)) else { return false };
    if src.2 {
        close();
        return true;
    }
    close();
    let mut thread = THREAD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = thread.take() {
        let _ = old.join(); // closed just above: its loop is ending
    }
    let (src, dst) = (src.1, dst.1);
    let (tx, rx) = std::sync::mpsc::channel();
    *thread = Some(std::thread::spawn(move || run(src, dst, tx)));
    let ok = rx.recv().unwrap_or(false);
    if ok {
        let (x, y) = centre(src);
        unsafe { SetCursorPos(x, y) };
    }
    ok
}

/// Close the copy (if one is open) and put the cursor back on the primary monitor.
pub fn close() -> bool {
    let _dpi = Physical::new();
    let h = WINDOW.swap(0, Ordering::Relaxed) as HWND;
    if h.is_null() {
        return false;
    }
    unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
    if let Some(p) = monitors().into_iter().find(|m| m.2) {
        let (x, y) = centre(p.1);
        unsafe { SetCursorPos(x, y) };
    }
    true
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

/// The copy's thread: makes the windows, says whether that worked, refreshes about 60 times a second.
fn run(src: [i32; 4], dst: [i32; 4], made: std::sync::mpsc::Sender<bool>) {
    let _dpi = Physical::new();
    unsafe {
        if MagInitialize() == 0 {
            let _ = made.send(false);
            return;
        }
        let inst = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = inst;
        wc.lpszClassName = w!("WinSenseShow");
        wc.hbrBackground = GetStockObject(BLACK_BRUSH) as HBRUSH;
        RegisterClassW(&wc); // fails harmlessly when already registered by an earlier copy
        // Layered + transparent: every click goes to the windows below; no-activate: never the focus.
        let ex = WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let host = CreateWindowExW(ex, w!("WinSenseShow"), w!("WinSense monitor copy"), WS_POPUP, dst[0], dst[1], dst[2] - dst[0], dst[3] - dst[1], null_mut(), null_mut(), inst, std::ptr::null());
        let (c, k) = fit(src, dst);
        let mag = if host.is_null() { null_mut() } else { CreateWindowExW(0, WC_MAGNIFIER, w!(""), WS_CHILD | WS_VISIBLE | MS_SHOWMAGNIFIEDCURSOR as u32, c[0], c[1], c[2], c[3], host, null_mut(), inst, std::ptr::null()) };
        let mut t = MAGTRANSFORM { v: [k, 0.0, 0.0, 0.0, k, 0.0, 0.0, 0.0, 1.0] };
        let ok = !mag.is_null() && MagSetWindowTransform(mag, &mut t) != 0;
        if !ok {
            if !host.is_null() {
                DestroyWindow(host);
            }
            MagUninitialize();
            let _ = made.send(false);
            return;
        }
        SetLayeredWindowAttributes(host, 0, 255, LWA_ALPHA);
        let source = RECT { left: src[0], top: src[1], right: src[2], bottom: src[3] };
        MagSetWindowSource(mag, source);
        WINDOW.store(host as isize, Ordering::Relaxed);
        OPEN.store(true, Ordering::Relaxed);
        let _ = made.send(true);
        ShowWindow(host, SW_SHOWNOACTIVATE);
        SetTimer(host, 1, 16, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER {
                MagSetWindowSource(mag, source);
                InvalidateRect(mag, std::ptr::null(), 0);
            }
            DispatchMessageW(&msg);
        }
        // A newer copy may already be in WINDOW; only this one's own entry is cleared.
        let _ = WINDOW.compare_exchange(host as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
        OPEN.store(WINDOW.load(Ordering::Relaxed) != 0, Ordering::Relaxed);
        MagUninitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_fitting() {
        assert_eq!(number(r"\\.\DISPLAY2"), Some(2));
        assert_eq!(number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(number("DISPLAY2"), None);
        // A 1920x1080 side monitor on a 2560x1440 primary: fills it, same aspect.
        assert_eq!(fit([-1920, 0, 0, 1080], [0, 0, 2560, 1440]), ([0, 0, 2560, 1440], 2560.0 / 1920.0));
        // A portrait 1080x1920 monitor: centred, black bars left and right.
        let (r, k) = fit([2560, 0, 3640, 1920], [0, 0, 2560, 1440]);
        assert_eq!((r, k), ([(2560 - 810) / 2, 0, 810, 1440], 0.75));
        // This PC: the primary is there, every number once.
        let all = monitors();
        assert_eq!(all.iter().filter(|m| m.2).count(), 1, "{all:?}");
        assert!(all.iter().enumerate().all(|(i, m)| all[..i].iter().all(|o| o.0 != m.0)));
    }

    /// This PC's monitor rects as each DPI mode sees them:
    /// `cargo test monitor_rects -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn monitor_rects() {
        for (name, ctx) in [("unaware", DPI_AWARENESS_CONTEXT_UNAWARE), ("system", DPI_AWARENESS_CONTEXT_SYSTEM_AWARE), ("per-monitor v2", DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)] {
            unsafe { SetThreadDpiAwarenessContext(ctx) };
            println!("{name}: {:?}", monitors());
        }
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };
        let real = { let _dpi = Physical::new(); monitors() };
        println!("open/close use: {real:?}");
        for m in real.iter().filter(|m| !m.2) {
            println!("  show {}: cursor to {:?}, copy {:?}", m.0, centre(m.1), fit(m.1, real.iter().find(|p| p.2).unwrap().1));
        }
    }

    /// The copy's windows for real, on a hidden desktop of its own (nothing on the screen, the
    /// cursor untouched): made, never the focus, clicks pass through, closed by `close`.
    #[test]
    fn the_copy_opens_hidden_and_closes() {
        use windows_sys::Win32::System::StationsAndDesktops::*;
        let all = { let _dpi = Physical::new(); monitors() };
        assert!(!open(99), "no such monitor");
        let (Some(src), Some(dst)) = (all.iter().find(|m| !m.2).map(|m| m.1), all.iter().find(|m| m.2).map(|m| m.1)) else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || unsafe {
            let d = CreateDesktopW(w!("WinSenseShowTest"), std::ptr::null(), std::ptr::null(), 0, 0x10000000, std::ptr::null());
            assert!(!d.is_null() && SetThreadDesktop(d) != 0);
            run(src, dst, tx);
            CloseDesktop(d);
        });
        assert!(rx.recv().unwrap(), "made");
        let h = WINDOW.load(Ordering::Relaxed) as HWND;
        let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
        assert!(OPEN.load(Ordering::Relaxed) && ex & WS_EX_NOACTIVATE != 0 && ex & WS_EX_TRANSPARENT != 0 && ex & WS_EX_TOPMOST != 0);
        std::thread::sleep(std::time::Duration::from_millis(200)); // a few refreshes
        // `close` would also move the cursor; this posts the same close message without it.
        unsafe { PostMessageW(WINDOW.swap(0, Ordering::Relaxed) as HWND, WM_CLOSE, 0, 0) };
        t.join().unwrap();
        assert!(!OPEN.load(Ordering::Relaxed));
        assert!(!close(), "nothing open: nothing done, the cursor stays");
        assert!(!open(99), "no such monitor");
    }
}
