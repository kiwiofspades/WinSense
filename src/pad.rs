//! Virtual Xbox 360 pad through ViGEmBus 1.22.0 (retired 2023, still signed),
//! fed from the real DualSense. The bus protocol (IOCTL codes, struct layouts, plug-in then
//! wait-ready) is from ViGEmClient's BusShared.h and ViGEmClient.cpp (MIT), layout only.
//! Reading the DualSense here never writes to it: only a feature-report request, which
//! switches a Bluetooth controller to the full report.

use crate::ds;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::core::GUID;

const BUS: GUID = GUID::from_u128(0x96E42B22_F5E9_42F8_B043_ED0F932F014F);
// CTL_CODE(FILE_DEVICE_BUS_EXTENDER 0x2A, 0x801 + n, METHOD_BUFFERED, access): write 0xA000, read+write 0xE000.
const PLUGIN: u32 = 0x2AA004;
const CHECK_VERSION: u32 = 0x2AA00C;
const WAIT_READY: u32 = 0x2AA010;
const XUSB_NOTIFY: u32 = 0x2AE804;
const XUSB_SUBMIT: u32 = 0x2AA808;

/// The ViGEm bus, opened for overlapped I/O. Closing it unplugs every pad it plugged in.
pub struct Bus(isize);

impl Drop for Bus {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}

/// Little-endian u32 words, the way every bus request starts (Size, SerialNo, ...).
fn words(w: &[u32], len: usize) -> Vec<u8> {
    let mut b: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    b.resize(len, 0);
    b
}

impl Bus {
    pub fn open() -> Result<Bus, String> {
        let not_found = "ViGEmBus is not installed (no bus device found)".to_string();
        let mut len = 0u32;
        let mut list;
        unsafe {
            if CM_Get_Device_Interface_List_SizeW(&mut len, &BUS, null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || len <= 1 {
                return Err(not_found);
            }
            list = vec![0u16; len as usize];
            if CM_Get_Device_Interface_ListW(&BUS, null(), list.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
                return Err(not_found);
            }
        }
        list.truncate(list.iter().position(|&c| c == 0).unwrap_or(list.len()) + 1); // first path of the list
        let h = unsafe {
            CreateFileW(list.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED, null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("cannot open ViGEmBus (error {})", unsafe { GetLastError() }));
        }
        let bus = Bus(h as isize);
        bus.ioctl(CHECK_VERSION, &mut words(&[8, 1], 8), false).map_err(|e| format!("ViGEmBus refused the version check (error {e})"))?;
        Ok(bus)
    }

    /// One request; with `reply` the driver writes its answer back into `buf`. Err = Win32 error.
    fn ioctl(&self, code: u32, buf: &mut [u8], reply: bool) -> Result<(), u32> {
        unsafe {
            let h = self.0 as HANDLE;
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = CreateEventW(null(), 1, 0, null());
            let (mut n, p) = (0u32, buf.as_mut_ptr());
            let ok = DeviceIoControl(h, code, p as _, buf.len() as u32, if reply { p as _ } else { null_mut() }, if reply { buf.len() as u32 } else { 0 }, &mut n, &mut ov);
            let r = if (ok != 0 || GetLastError() == ERROR_IO_PENDING) && GetOverlappedResult(h, &ov, &mut n, 1) != 0 { Ok(()) } else { Err(GetLastError()) };
            CloseHandle(ov.hEvent);
            r
        }
    }

    /// Plug in an Xbox 360 pad (045E:028E) on the first free serial (1-16, never 0) and wait
    /// until it can take reports. Returns its serial.
    pub fn plug_x360(&self) -> Result<u32, String> {
        for serial in 1..=16 {
            // VIGEM_PLUGIN_TARGET: Size 16, SerialNo, TargetType 0 (Xbox360Wired), VendorId, ProductId.
            let mut p = words(&[16, serial, 0, 0x028E_045E], 16);
            if self.ioctl(PLUGIN, &mut p, false).is_ok() {
                return match self.ioctl(WAIT_READY, &mut words(&[8, serial], 8), false) {
                    Ok(()) => Ok(serial),
                    Err(e) => Err(format!("virtual pad plugged in but never became ready (error {e})")),
                };
            }
        }
        Err("ViGEmBus has no free slot".into())
    }

    /// XUSB_SUBMIT_REPORT: Size 20, SerialNo, then the 12-byte XINPUT_GAMEPAD-shaped report.
    pub fn submit(&self, serial: u32, report: &[u8; 12]) -> Result<(), u32> {
        let mut b = words(&[20, serial], 8);
        b.extend_from_slice(report);
        self.ioctl(XUSB_SUBMIT, &mut b, false)
    }

    /// Blocks until a game sets the pad's rumble: (large = left/strong, small = right/weak).
    /// XUSB_REQUEST_NOTIFICATION: Size 12 (11 bytes padded), SerialNo, LargeMotor, SmallMotor, LedNumber.
    pub fn rumble(&self, serial: u32) -> Result<[u8; 2], u32> {
        let mut b = words(&[12, serial], 12);
        self.ioctl(XUSB_NOTIFY, &mut b, true)?;
        Ok([b[8], b[9]])
    }
}

/// The Xbox 360 report for one DualSense state: wButtons, LT, RT, LX, LY, RX, RY, little-endian.
pub fn xusb(s: &ds::State) -> [u8; 12] {
    let [b0, b1, b2] = s.buttons;
    // Hat 0-7 clockwise from Up, 8 = released -> XUSB up 1, down 2, left 4, right 8.
    let mut w: u16 = [0x1, 0x9, 0x8, 0xA, 0x2, 0x6, 0x4, 0x5].get((b0 & 0x0F) as usize).copied().unwrap_or(0);
    let mut map = |byte: u8, bit: u8, x: u16| w |= if byte >> bit & 1 == 1 { x } else { 0 };
    map(b0, 4, 0x4000); // Square -> X
    map(b0, 5, 0x1000); // Cross -> A
    map(b0, 6, 0x2000); // Circle -> B
    map(b0, 7, 0x8000); // Triangle -> Y
    map(b1, 0, 0x0100); // L1 -> LB
    map(b1, 1, 0x0200); // R1 -> RB
    map(b1, 4, 0x0020); // Create -> Back
    map(b1, 5, 0x0010); // Options -> Start
    map(b1, 6, 0x0040); // L3
    map(b1, 7, 0x0080); // R3
    map(b2, 0, 0x0400); // PS -> Guide
    map(b2, 1, 0x0020); // Touchpad click -> Back too (what DS4Windows does by default)
    // 0..255 -> -32768..32767 (128 is about 0, inside any dead zone); DualSense Y grows downward, XInput's upward.
    let axis = |v: u8| (v as i32 * 257 - 32768) as i16;
    let [lx, ly, rx, ry] = s.sticks;
    let mut r = [0u8; 12];
    r[0..2].copy_from_slice(&w.to_le_bytes());
    (r[2], r[3]) = (s.l2, s.r2);
    for (i, v) in [axis(lx), !axis(ly), axis(rx), !axis(ry)].into_iter().enumerate() {
        r[4 + 2 * i..6 + 2 * i].copy_from_slice(&v.to_le_bytes());
    }
    r
}

/// A running virtual pad: one thread copies the DualSense into it, one waits for game rumble.
pub struct Pad {
    bus: Arc<Bus>,
    pub serial: u32,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Pad {
    /// Plug in the pad and start feeding it. `on_rumble` gets every rumble change a game makes.
    pub fn start(on_rumble: impl FnMut([u8; 2]) + Send + 'static) -> Result<Pad, String> {
        let bus = Arc::new(Bus::open()?);
        let serial = bus.plug_x360()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (b, s) = (bus.clone(), stop.clone());
        let feeder = std::thread::spawn(move || feed(&b, serial, &s));
        let b = bus.clone();
        let mut on_rumble = on_rumble;
        let rumbler = std::thread::spawn(move || {
            // Ends when Drop cancels the pending request (or the bus goes away).
            while let Ok(m) = b.rumble(serial) {
                on_rumble(m);
            }
        });
        Ok(Pad { bus, serial, stop, threads: vec![feeder, rumbler] })
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        unsafe { CancelIoEx(self.bus.0 as HANDLE, null()) }; // frees the waiting rumble request
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // The last Arc<Bus> drops here: closing the bus unplugs the pad.
    }
}

/// Copy the first DualSense found into the pad until `stop`; looks again every second when
/// none is there or it went away (then the pad is set to rest).
// shortcut: one pad for the first DualSense found; a second controller needs a pad of its own (never paired yet).
fn feed(bus: &Bus, serial: u32, stop: &AtomicBool) {
    let rest = xusb(&ds::State { sticks: [128; 4], buttons: [8, 0, 0], ..Default::default() });
    let _ = bus.submit(serial, &rest); // a fresh pad read a stray stick offset until its first report (seen)
    let mut last = rest;
    while !stop.load(Ordering::Relaxed) {
        let found = hidapi::HidApi::new().ok().and_then(|api| {
            let info = api.device_list().find(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()))?.clone();
            Some((matches!(info.bus_type(), hidapi::BusType::Bluetooth), info.open_device(&api).ok()?))
        });
        let Some((bt, dev)) = found else {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        };
        if bt {
            // A feature request (not a write) switches Bluetooth to the full report: triggers, lights and rumble stay as they are.
            let mut f = [0u8; 64];
            f[0] = 0x05;
            let _ = dev.get_feature_report(&mut f);
        }
        let mut buf = [0u8; 128];
        while !stop.load(Ordering::Relaxed) {
            let Ok(len) = dev.read_timeout(&mut buf, 100) else { break };
            match ds::parse(bt, &buf[..len]) {
                Some((ds::Kind::BtFull { crc_ok: false }, _)) | None => {}
                Some((_, s)) => {
                    let r = xusb(&s);
                    if r != last && bus.submit(serial, &r).is_ok() {
                        last = r;
                    }
                }
            }
        }
        if bus.submit(serial, &rest).is_ok() {
            last = rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(sticks: [u8; 4], buttons: [u8; 3], l2: u8, r2: u8) -> ds::State {
        ds::State { sticks, buttons, l2, r2, ..Default::default() }
    }

    #[test]
    fn rest_is_neutral() {
        let r = xusb(&st([128; 4], [8, 0, 0], 0, 0));
        assert_eq!(&r[..4], &[0, 0, 0, 0]);
        for i in 0..4 {
            assert!(i16::from_le_bytes([r[4 + 2 * i], r[5 + 2 * i]]).abs() <= 129, "axis {i}" /* Y flip: !128 = -129 */);
        }
    }

    #[test]
    fn sticks_triggers_and_y_flip() {
        let r = xusb(&st([0, 0, 255, 255], [8, 0, 0], 17, 255));
        let ax = |i: usize| i16::from_le_bytes([r[4 + 2 * i], r[5 + 2 * i]]);
        assert_eq!((r[2], r[3]), (17, 255));
        assert_eq!((ax(0), ax(1), ax(2), ax(3)), (-32768, 32767, 32767, -32768)); // stick up = +Y
    }

    #[test]
    fn buttons_and_hat() {
        let w = |b: [u8; 3]| u16::from_le_bytes(xusb(&st([128; 4], b, 0, 0))[..2].try_into().unwrap());
        assert_eq!(w([8 | 0x20, 0, 0]), 0x1000, "Cross = A");
        assert_eq!(w([8 | 0xF0, 0, 0]), 0xF000, "all four face buttons");
        assert_eq!(w([8, 0xF3, 0]), 0x03F0, "L1 R1 Create Options L3 R3; L2/R2 bits are not buttons on a 360 pad");
        assert_eq!(w([8, 0, 0x01]), 0x0400, "PS = Guide");
        assert_eq!(w([8, 0, 0x02]), 0x0020, "touchpad click = Back");
        let hats: Vec<u16> = (0..9).map(|h| w([h, 0, 0])).collect();
        assert_eq!(hats, [0x1, 0x9, 0x8, 0xA, 0x2, 0x6, 0x4, 0x5, 0]); // Up, Up+Right ... Up+Left, released
    }

    #[test]
    fn request_layouts() {
        // Size fields match the C structs: plugin 16, check/wait 8, submit 20, notification 12.
        let p = words(&[16, 1, 0, 0x028E_045E], 16);
        assert_eq!(p, [16, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0x5E, 0x04, 0x8E, 0x02]);
        assert_eq!(words(&[12, 3], 12).len(), 12);
        // CTL_CODE(0x2A, 0x801 + n, 0, access) recomputed.
        let ctl = |f: u32, access: u32| 0x2A << 16 | access << 14 | f << 2;
        assert_eq!([ctl(0x801, 2), ctl(0x803, 2), ctl(0x804, 2), ctl(0xA01, 3), ctl(0xA02, 2)], [PLUGIN, CHECK_VERSION, WAIT_READY, XUSB_NOTIFY, XUSB_SUBMIT]);
    }
}
