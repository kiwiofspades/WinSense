//! DualSense input and output report layout. Facts from the Linux kernel driver
//! hid-playstation.c and the other open projects listed in CREDITS.md (layout only, no code copied).

pub const VID: u16 = 0x054C;
pub const PIDS: [u16; 2] = [0x0CE6, 0x0DF2]; // DualSense, DualSense Edge

/// Standard CRC-32 over one seed byte followed by the report bytes.
/// Seeds: 0xA1 input, 0xA2 output, 0xA3 feature (Bluetooth only).
pub fn crc32(seed: u8, data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in std::iter::once(&seed).chain(data) {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(c & 1));
        }
    }
    !c
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    /// USB report 0x01, 64 bytes.
    Usb,
    /// Bluetooth report 0x31, 78 bytes, CRC in the last 4.
    BtFull { crc_ok: bool },
    /// Bluetooth report 0x01: the short report a plain HID reader understands.
    BtSimple,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct State {
    pub sticks: [u8; 4], // lx, ly, rx, ry
    pub l2: u8,
    pub r2: u8,
    pub buttons: [u8; 3],
    /// (level 0..=10, charging nibble). Not present in the short report.
    pub battery: Option<(u8, u8)>,
    /// Trigger feedback, full reports only (community wiki; measured on USB and Bluetooth;
    /// the state nibble's meaning is not settled):
    /// [R2, L2, active]. R2/L2: low nibble = stop zone, high nibble = state.
    /// active: low nibble R2, high nibble L2; 0 off, 1 resistance, 2 click, 3 vibration.
    pub trig: Option<[u8; 3]>,
    /// Motion, full reports only (layout from the Linux driver; seen live 2026-11-10):
    /// (gyro x y z = pitch yaw roll, accel x y z, sensor timestamp in 1/3 microsecond units).
    pub imu: Option<([i16; 3], [i16; 3], u32)>,
    /// The two touch points, full reports only: Some((finger id, x 0-1919, y 0-1079)) while touched.
    /// The controller keeps the last position after a lift, so only bit 7 says whether it is touched.
    pub touch: [Option<(u8, u16, u16)>; 2],
}

/// One touch point: contact byte (bit 7 set = not touched, low 7 bits = finger id), then 12-bit x and y.
fn touch_point(b: &[u8]) -> Option<(u8, u16, u16)> {
    (b[0] & 0x80 == 0).then(|| (b[0] & 0x7F, b[1] as u16 | (b[2] as u16 & 0x0F) << 8, (b[2] >> 4) as u16 | (b[3] as u16) << 4))
}

/// `r` is a whole report as Windows hands it over, report id first.
pub fn parse(bt: bool, r: &[u8]) -> Option<(Kind, State)> {
    let (kind, p) = match (bt, *r.first()?, r.len()) {
        (false, 0x01, 64..) => (Kind::Usb, &r[1..]),
        // While the mic is open, some 0x31 reports carry an Opus mic frame instead of the state
        // (header bit 1, "HasMic"; PadForge, vds): never read those as sticks and buttons.
        (true, 0x31, 78..) if r[1] & 0x02 != 0 => return None,
        (true, 0x31, 78..) => {
            let crc_ok = crc32(0xA1, &r[..74]) == u32::from_le_bytes(r[74..78].try_into().unwrap());
            (Kind::BtFull { crc_ok }, &r[2..])
        }
        (true, 0x01, 10..) => {
            // Short layout (Cross, PS, sticks, hat seen on the controller; triggers not yet):
            // lx ly rx ry, hat+face, shoulders, ps/touchpad+counter, l2, r2.
            let s = State {
                sticks: [r[1], r[2], r[3], r[4]],
                buttons: [r[5], r[6], r[7] & 0x03],
                l2: r[8],
                r2: r[9],
                ..State::default()
            };
            return Some((Kind::BtSimple, s));
        }
        _ => return None,
    };
    Some((kind, State {
        sticks: [p[0], p[1], p[2], p[3]],
        l2: p[4],
        r2: p[5],
        buttons: [p[7], p[8], p[9]],
        battery: Some((p[52] & 0x0F, p[52] >> 4)),
        trig: Some([p[41], p[42], p[47]]),
        imu: Some((
            std::array::from_fn(|i| i16::from_le_bytes([p[15 + 2 * i], p[16 + 2 * i]])),
            std::array::from_fn(|i| i16::from_le_bytes([p[21 + 2 * i], p[22 + 2 * i]])),
            u32::from_le_bytes(p[27..31].try_into().unwrap()),
        )),
        touch: [touch_point(&p[32..36]), touch_point(&p[36..40])],
    }))
}

/// An empty output report (USB 0x02, 48 bytes; Bluetooth 0x31, 78 bytes) and the index
/// where its common block starts. `seq` is the Bluetooth sequence number; ignored on USB.
fn frame(bt: bool, seq: u8) -> (Vec<u8>, usize) {
    if !bt {
        let mut r = vec![0u8; 48]; // the length Windows expects; SDL sends the same
        r[0] = 0x02;
        return (r, 1);
    }
    let mut r = vec![0u8; 78];
    r[0] = 0x31;
    r[1] = (seq & 0x0F) << 4; // sequence in the high nibble, tag 0 in the low
    r[2] = 0x10; // the driver's fixed tag (works; other projects use another form)
    (r, 3)
}

/// Bluetooth: route audio to the built-in speaker and set its volume (LinuxAudio4Dualsense5 and
/// XephyLon's notes, layout only): valid0 0x20 speaker volume + 0x80 audio control, valid1 0x80
/// audio control 2; common 5 = volume 0x64, 7 = 0x30 (speaker on), 37 = 2 (pre-gain).
/// Carries no rumble bits, so it ends a running rumble.
pub fn speaker_setup(seq: u8) -> Vec<u8> {
    let (mut r, c) = frame(true, seq);
    (r[c], r[c + 1], r[c + 5], r[c + 7], r[c + 37]) = (0xA0, 0x80, 0x64, 0x30, 2);
    let crc = crc32(0xA2, &r[..74]);
    r[74..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Classic rumble (SDL and the Linux driver). `left` is the strong motor,
/// `right` the weak one. `v2` = the newer form (valid2 bit 2), chosen by `vibration_v2`.
/// Both motors 0 sends no rumble bits at all: SDL's stop, which also hands audio haptics back.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct Rumble {
    pub left: u8,
    pub right: u8,
    pub v2: bool,
}

/// Whether a controller takes the v2 rumble form: the Edge always; a DualSense from update
/// version 0x0215 (the driver's cutoff; SDL's is 0x0224) in feature report 0x20, bytes 44-45.
/// `firmware` is that report, id first; None (could not be read) means v2, as SDL assumes.
pub fn vibration_v2(pid: u16, firmware: Option<&[u8]>) -> bool {
    pid == 0x0DF2 || firmware.and_then(|f| f.get(44..46)).is_none_or(|v| u16::from_le_bytes([v[0], v[1]]) >= 0x0215)
}

/// Write the rumble bits into a report whose common block starts at `c`. Flags are OR-ed in,
/// so they never wipe light or trigger flags.
fn put_rumble(r: &mut [u8], c: usize, m: Rumble) {
    if m.left == 0 && m.right == 0 {
        return;
    }
    r[c] |= 0x02; // valid0: haptics select (classic rumble instead of audio haptics)
    if m.v2 {
        r[c + 38] |= 0x04; // valid2: v2 rumble
    } else {
        r[c] |= 0x01; // valid0: old rumble
    }
    r[c + 2] = m.right;
    r[c + 3] = m.left;
}

/// Bluetooth reports end in a CRC-32 seeded with 0xA2.
fn seal(bt: bool, mut r: Vec<u8>) -> Vec<u8> {
    if bt {
        let crc = crc32(0xA2, &r[..74]);
        r[74..].copy_from_slice(&crc.to_le_bytes());
    }
    r
}

/// One adaptive-trigger effect. Positions are zones 0 (released) to 9 (fully pulled),
/// strengths 1 to 8. Layout from the Nielk1 trigger notes (MIT) and dualsensectl, layout
/// only.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Effect {
    Off,
    /// Resistance from `start` to the end of the pull. Strength 8 from a point = a wall.
    Resist { start: u8, strength: u8 },
    /// Resistance from `start` that gives way at `end`: a gun-trigger click.
    Click { start: u8, end: u8, strength: u8 },
    /// Vibrates while the trigger is held past `start`, `hz` times a second.
    Vibrate { start: u8, strength: u8, hz: u8 },
    /// Old raw resistance (mode 0x01) that most PC tools use for "rigid": raw position
    /// (0 released, about 0xB0 fully pulled) and force 0-255. Not an official mode: the
    /// controller's effect nibble reads 0 for it, only the stop nibble moves.
    Rigid { start: u8, force: u8 },
}

impl Effect {
    /// The 11-byte effect block (mode, then 10 parameters); None if a value is out of range.
    /// Only modes 0x05, 0x21, 0x25, 0x26 and 0x01 can come out of here. Never 0xFC-0xFE:
    /// those are said to corrupt the trigger until the controller's reset button is pressed.
    pub fn bytes(self) -> Option<[u8; 11]> {
        // Zone form: a mask of zones start..=9, then ten 3-bit fields holding level - 1.
        fn zones(mode: u8, start: u8, level: u8) -> [u8; 11] {
            let (mut mask, mut fields) = (0u16, 0u32);
            for z in start..10 {
                mask |= 1 << z;
                fields |= (level as u32 - 1) << (3 * z);
            }
            let mut b = [0u8; 11];
            b[0] = mode;
            b[1..3].copy_from_slice(&mask.to_le_bytes());
            b[3..7].copy_from_slice(&fields.to_le_bytes());
            b
        }
        let level = |s: u8| (1..=8).contains(&s);
        Some(match self {
            Effect::Off => [0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Effect::Resist { start, strength } if start <= 9 && level(strength) => zones(0x21, start, strength),
            Effect::Click { start, end, strength } if (2..=7).contains(&start) && start < end && end <= 8 && level(strength) => {
                let mask = (1u16 << start | 1 << end).to_le_bytes();
                [0x25, mask[0], mask[1], strength - 1, 0, 0, 0, 0, 0, 0, 0]
            }
            // start 0 is refused: zone 0 is the released trigger, so it could buzz at rest.
            Effect::Vibrate { start, strength, hz } if (1..=9).contains(&start) && level(strength) && hz > 0 => {
                let mut b = zones(0x26, start, strength);
                b[9] = hz;
                b
            }
            Effect::Rigid { start, force } => [0x01, start, force, 0, 0, 0, 0, 0, 0, 0, 0],
            _ => return None,
        })
    }
}

/// Output report that sets trigger effects and rumble and nothing else (lights and audio flags
/// stay 0). With both sides None it is a rumble-only report. A side given None is left as it is. `power` 0-7 also sends the trigger power
/// reduction (0 = full, 7 = most reduced; rumble reduction 0); None leaves it as it is.
/// None if an effect or `power` is out of range.
/// `rumble` is the whole current rumble state: a zero one stops any rumble.
pub fn trigger_report(bt: bool, seq: u8, l2: Option<Effect>, r2: Option<Effect>, power: Option<u8>, rumble: Rumble) -> Option<Vec<u8>> {
    let (mut r, c) = frame(bt, seq);
    put_rumble(&mut r, c, rumble);
    if let Some(p) = power {
        if p > 7 {
            return None;
        }
        r[c + 1] = 0x40; // valid1: motor power reduction
        r[c + 36] = p << 4; // high nibble triggers, low rumble (dualsensectl, DS4Windows; never sent yet)
    }
    // (effect, valid0 bit, start of its block in the common block)
    for (e, flag, at) in [(r2, 0x04, 10), (l2, 0x08, 21)] {
        if let Some(e) = e {
            r[c] |= flag;
            r[c + at..c + at + 11].copy_from_slice(&e.bytes()?);
        }
    }
    Some(seal(bt, r))
}

/// What a lights output report asks for. Trigger bits stay 0 in every variant, so triggers
/// are not touched. Rumble is passed alongside: a valid0 = 0 report stops rumble (SDL).
pub enum Lights {
    /// Light bar r,g,b. The rest is only touched when given: player lights (low 5 bits,
    /// 4 = centre), mute light (1 on, 2 pulse seen; 0 off read), brightness of the small lights
    /// including the mute light (0 bright, 2 dim seen; 1 medium read).
    Set { rgb: [u8; 3], players: Option<u8>, mute: Option<u8>, dim: Option<u8> },
    /// Fade the Bluetooth start-up animation out before Set, as the driver does (never tried without it).
    Setup,
    /// The driver's "release LEDs" bit: gives the lights back to the controller. Seen on the cable:
    /// the bar went to the controller's own orange charging pulse. Seen on Bluetooth: every light went dark.
    Release,
}

/// Whole output report: USB 0x02, 48 bytes (the length Windows expects; SDL sends the same);
/// Bluetooth 0x31, 78 bytes with CRC.
/// `seq` is the Bluetooth sequence number (0..=15, raised by one per report); ignored on USB.
/// `rumble` is the whole current rumble state: a zero one stops any rumble.
pub fn output_report(bt: bool, seq: u8, l: &Lights, rumble: Rumble) -> Vec<u8> {
    let (mut r, c) = frame(bt, seq); // c = start of the common block
    match *l {
        Lights::Set { rgb, players, mute, dim } => {
            r[c + 1] = 0x04; // valid1: light bar
            r[c + 44..c + 47].copy_from_slice(&rgb);
            if let Some(m) = mute {
                r[c + 1] |= 0x01; // valid1: mute LED
                r[c + 8] = m;
            }
            if let Some(p) = players {
                r[c + 1] |= 0x10; // valid1: player lights
                r[c + 43] = p & 0x1F;
            }
            if let Some(d) = dim {
                r[c + 38] = 0x01; // valid2: player-light brightness (from dualsensectl and pydualsense, not the driver)
                r[c + 42] = d;
            }
        }
        Lights::Setup => {
            r[c + 38] = 0x02; // valid2: light bar setup
            r[c + 41] = 0x02; // setup: fade light out
        }
        Lights::Release => r[c + 1] = 0x08, // valid1: release LEDs
    }
    put_rumble(&mut r, c, rumble); // after the lights: valid2 is shared and OR-ed
    seal(bt, r)
}

/// Add the mute light (0 off, 1 on) to a finished output report: valid1 bit 0 and common 8, the
/// Bluetooth CRC redone. Every other flag stays, so its triggers and rumble are untouched.
pub fn with_mute_light(bt: bool, mut r: Vec<u8>, v: u8) -> Vec<u8> {
    let c = if bt { 3 } else { 1 };
    r[c + 1] |= 0x01;
    r[c + 8] = v;
    seal(bt, r)
}

/// A mod's lights (src/port.rs): the light bar's r,g,b and the player lights (low 5 bits), each
/// None when not set.
pub type Glow = (Option<[u8; 3]>, Option<u8>);

/// Add a mod's lights to a finished output report (flags OR-ed in, Bluetooth CRC redone, so its
/// triggers, rumble and mute light stay). Nothing set = the release bit: the lights go back to the
/// controller (seen: the charging look on the cable, dark over Bluetooth).
pub fn with_glow(bt: bool, mut r: Vec<u8>, (bar, players): Glow) -> Vec<u8> {
    let c = if bt { 3 } else { 1 };
    if let Some(rgb) = bar {
        r[c + 1] |= 0x04;
        r[c + 44..c + 47].copy_from_slice(&rgb);
    }
    if let Some(p) = players {
        r[c + 1] |= 0x10;
        r[c + 43] = p & 0x1F;
    }
    if bar.is_none() && players.is_none() {
        r[c + 1] |= 0x08;
    }
    seal(bt, r)
}

/// Bluetooth haptics: report 0x32, 142 bytes (this controller's Bluetooth descriptor; SAxense
/// on Linux sends 141). Packet 0x11 (session header: mic closed, `counter`) then packet 0x12:
/// 32 stereo frames of signed 8-bit samples at 3000 Hz (left, right), so one report per
/// 10.67 ms. Layout from SAxense (egormanga, https://apps.sdore.me/SAxense) and PadForge,
/// layout only.
/// The controller's mic is wanted open (src/voice.rs); read by every audio report (0x32).
pub static MIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn haptic_report(seq: u8, counter: u8, frames: &[[i8; 2]]) -> Vec<u8> {
    let mut r = vec![0u8; 142];
    r[0] = 0x32;
    r[1] = (seq & 0x0F) << 4;
    // 0x11: 0xFE = mic session closed, 0xFF = open. Every audio report sets it, and a 0xFE closes an
    // open mic (PadForge, measured), so it follows the one shared MIC flag.
    let mic = if MIC.load(std::sync::atomic::Ordering::Relaxed) { 0xFF } else { 0xFE };
    r[2..11].copy_from_slice(&[0x91, 7, mic, 0, 0, 0, 0, 0xFF, counter]);
    r[11..13].copy_from_slice(&[0x92, 64]);
    for (i, f) in frames.iter().take(32).enumerate() {
        r[13 + 2 * i] = f[0] as u8;
        r[14 + 2 * i] = f[1] as u8;
    }
    let crc = crc32(0xA2, &r[..138]);
    r[138..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Bluetooth speaker: report 0x35, 334 bytes (PadForge and awalol's tools, layout only): the same
/// 0x11 header as the haptics (buffer length 48 as they send it), then packet 0x13 = one 200-byte
/// Opus frame (48 kHz stereo, 10 ms, constant bitrate; the speaker plays the right channel).
/// One report per 10.67 ms, like the haptics.
pub fn speaker_report(seq: u8, counter: u8, opus: &[u8; 200]) -> Vec<u8> {
    let mut r = vec![0u8; 334];
    r[0] = 0x35;
    r[1] = (seq & 0x0F) << 4;
    let mic = if MIC.load(std::sync::atomic::Ordering::Relaxed) { 0xFF } else { 0xFE };
    r[2..11].copy_from_slice(&[0x91, 7, mic, 0, 0, 0, 0, 48, counter]);
    r[11..13].copy_from_slice(&[0x93, 200]);
    r[13..213].copy_from_slice(opus);
    let crc = crc32(0xA2, &r[..330]);
    r[330..].copy_from_slice(&crc.to_le_bytes());
    r
}

pub fn button_names(b: [u8; 3]) -> String {
    const HAT: [&str; 8] = ["Up", "Up+Right", "Right", "Down+Right", "Down", "Down+Left", "Left", "Up+Left"];
    const BITS: [[&str; 8]; 3] = [
        ["", "", "", "", "Square", "Cross", "Circle", "Triangle"],
        ["L1", "R1", "L2", "R2", "Create", "Options", "L3", "R3"],
        ["PS", "Touchpad", "Mute", "", "FnL", "FnR", "BackL", "BackR"], // last four: Edge only
    ];
    let mut v: Vec<&str> = HAT.get((b[0] & 0x0F) as usize).copied().into_iter().collect();
    for (byte, names) in b.iter().zip(BITS) {
        v.extend((0..8).filter(|i| byte >> i & 1 == 1).map(|i| names[i]).filter(|n| !n.is_empty()));
    }
    if v.is_empty() { "(nothing held)".into() } else { v.join(" ") }
}

#[cfg(test)]
mod tests {
    use super::*;
    const R0: Rumble = Rumble { left: 0, right: 0, v2: false };

    #[test]
    fn crc_known_vector() {
        // CRC-32 of "123456789" is 0xCBF43926; here the leading 1 plays the seed byte.
        assert_eq!(crc32(0x31, b"23456789"), 0xCBF4_3926);
    }

    #[test]
    fn usb_report() {
        let mut r = [0u8; 64];
        r[0] = 0x01;
        r[5] = 200; // l2
        r[8] = 0x08 | 0x20; // hat neutral + Cross
        r[9] = 0x04; // L2 button
        r[53] = 0x18; // battery 8, charging
        (r[42], r[43], r[48]) = (0x14, 0x09, 0x01); // R2 resisting from zone 4, L2 off
        let (k, s) = parse(false, &r).unwrap();
        assert_eq!(k, Kind::Usb);
        assert_eq!(s.trig, Some([0x14, 0x09, 0x01]));
        assert_eq!((s.l2, s.battery), (200, Some((8, 1))));
        assert_eq!(button_names(s.buttons), "Cross L2");
    }

    #[test]
    fn bt_full_report_checks_crc() {
        let mut r = [0u8; 78];
        r[0] = 0x31;
        r[9] = 0x80; // hat Up + Triangle
        (r[43], r[44], r[49]) = (0x14, 0x09, 0x01);
        let crc = crc32(0xA1, &r[..74]);
        r[74..].copy_from_slice(&crc.to_le_bytes());
        let (k, s) = parse(true, &r).unwrap();
        assert_eq!(k, Kind::BtFull { crc_ok: true });
        assert_eq!(button_names(s.buttons), "Up Triangle");
        assert_eq!(s.trig, Some([0x14, 0x09, 0x01]));
        r[20] ^= 1;
        assert_eq!(parse(true, &r).unwrap().0, Kind::BtFull { crc_ok: false });
    }

    #[test]
    fn bt_motion_and_touch() {
        // A real report from a controller at rest over Bluetooth (2026-11-10).
        let hex = "31 11 81 7f 81 7f 00 00 01 08 00 00 00 31 13 4b 37 fd ff fe ff 0a 00 72 01 93 20 68 fe 51 88 9f 44 09 86 c8 23 03 80 00 00 00 58 09 09 00 00 00 00 00 4c 90 9f 44 06 00 00 83 22 f0 df 88 01 59 bb 00 00 00 00 00 00 00 00 00 4f a7 be da";
        let mut r: Vec<u8> = hex.split(' ').map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
        let (k, s) = parse(true, &r).unwrap();
        assert_eq!(k, Kind::BtFull { crc_ok: true });
        let (gyro, accel, ts) = s.imu.unwrap();
        assert_eq!((gyro, accel[1], ts), ([-3, -2, 10], 8339, 0x449F_8851));
        assert_eq!(s.touch, [None, None]); // both lifted (contact 0x86, 0x80)
        r[34] = 0x06; // the same point touched: finger 6 at x 968, y 50
        r[38] = 0x07;
        (r[39], r[40], r[41]) = (0x7F, 0x77, 0x43); // x 1919, y 1079: the middle byte holds x high and y low
        let (_, s) = parse(true, &r).unwrap();
        assert_eq!(s.touch, [Some((6, 968, 50)), Some((7, 1919, 1079))]);
        assert_eq!(parse(true, &[0x01, 0x80, 0x80, 0x80, 0x80, 8, 0, 0, 0, 0]).unwrap().1.imu, None);
        r[1] = 0x12; // header with HasMic: bytes 3..74 are an Opus mic frame, not state
        assert_eq!(parse(true, &r), None);
    }

    #[test]
    fn output_usb() {
        let r = output_report(false, 5, &Lights::Set { rgb: [1, 2, 3], players: Some(0xFF), mute: Some(1), dim: Some(2) }, R0);
        assert_eq!((r.len(), r[0], r[1], r[2]), (48, 0x02, 0, 0x15));
        assert_eq!((r[9], r[39], r[43], r[44], r[45], r[46], r[47]), (1, 1, 2, 0x1F, 1, 2, 3));
        assert_eq!(r.iter().filter(|&&b| b != 0).count(), 9); // nothing else touched
        // Colour alone claims the light bar only: id, flag, green.
        let g = output_report(false, 0, &Lights::Set { rgb: [0, 255, 0], players: None, mute: None, dim: None }, R0);
        assert_eq!((g[2], g[46], g.iter().filter(|&&b| b != 0).count()), (0x04, 255, 3));
        let s = output_report(false, 0, &Lights::Setup, R0);
        assert_eq!((s[39], s[42], s.iter().filter(|&&b| b != 0).count()), (2, 2, 3));
        let o = output_report(false, 0, &Lights::Release, R0);
        assert_eq!((o[2], o.iter().filter(|&&b| b != 0).count()), (0x08, 2));
    }

    #[test]
    fn output_bt() {
        let r = output_report(true, 5, &Lights::Set { rgb: [1, 2, 3], players: Some(4), mute: Some(1), dim: Some(2) }, R0);
        assert_eq!((r.len(), r[0], r[1], r[2], r[3], r[4]), (78, 0x31, 0x50, 0x10, 0, 0x15));
        assert_eq!((r[11], r[41], r[45], r[46], r[47], r[48], r[49]), (1, 1, 2, 4, 1, 2, 3));
        // Colour alone: id, tag, flag, green and nothing else before the CRC.
        let g = output_report(true, 0, &Lights::Set { rgb: [0, 255, 0], players: None, mute: None, dim: None }, R0);
        assert_eq!((g[4], g[48], g[..74].iter().filter(|&&b| b != 0).count()), (0x04, 255, 4));
        assert_eq!(&r[74..], &crc32(0xA2, &r[..74]).to_le_bytes());
        assert_eq!(output_report(true, 17, &Lights::Release, R0)[1], 0x10); // sequence wraps to 1
        let s = output_report(true, 0, &Lights::Setup, R0);
        assert_eq!((s[41], s[44]), (2, 2));
    }

    #[test]
    fn effect_bytes() {
        // Worked examples from the Nielk1 notes and dualsensectl, recomputed by hand.
        let b = |e: Effect| e.bytes().unwrap();
        assert_eq!(b(Effect::Off), [0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Resist { start: 4, strength: 6 }), [0x21, 0xF0, 0x03, 0x00, 0xD0, 0xB6, 0x2D, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Resist { start: 0, strength: 8 }), [0x21, 0xFF, 0x03, 0xFF, 0xFF, 0xFF, 0x3F, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Resist { start: 4, strength: 2 }), [0x21, 0xF0, 0x03, 0x00, 0x90, 0x24, 0x09, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Click { start: 3, end: 5, strength: 6 }), [0x25, 0x28, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Click { start: 2, end: 6, strength: 8 }), [0x25, 0x44, 0x00, 0x07, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(b(Effect::Vibrate { start: 2, strength: 4, hz: 30 }), [0x26, 0xFC, 0x03, 0xC0, 0xB6, 0x6D, 0x1B, 0, 0, 0x1E, 0]);
        assert_eq!(b(Effect::Vibrate { start: 3, strength: 2, hz: 25 }), [0x26, 0xF8, 0x03, 0x00, 0x92, 0x24, 0x09, 0, 0, 0x19, 0]);
        for bad in [
            Effect::Resist { start: 10, strength: 1 },
            Effect::Resist { start: 0, strength: 0 },
            Effect::Resist { start: 0, strength: 9 },
            Effect::Click { start: 1, end: 5, strength: 4 },
            Effect::Click { start: 5, end: 5, strength: 4 },
            Effect::Click { start: 7, end: 9, strength: 4 },
            Effect::Vibrate { start: 0, strength: 4, hz: 30 },
            Effect::Vibrate { start: 2, strength: 4, hz: 0 },
        ] {
            assert_eq!(bad.bytes(), None, "{bad:?}");
        }
    }

    #[test]
    fn trigger_reports() {
        let e = Effect::Click { start: 3, end: 5, strength: 6 };
        // USB, right trigger only: id, valid0 = 0x04, block at 11, nothing else.
        let r = trigger_report(false, 0, None, Some(e), None, R0).unwrap();
        assert_eq!((r.len(), r[0], r[1], &r[11..15]), (48, 0x02, 0x04, &[0x25, 0x28, 0x00, 0x05][..]));
        assert_eq!(r.iter().filter(|&&b| b != 0).count(), 5);
        // USB, left trigger only: valid0 = 0x08, block at 22, right block untouched.
        let l = trigger_report(false, 0, Some(e), None, None, R0).unwrap();
        assert_eq!((l[1], &l[22..26], l[11]), (0x08, &[0x25, 0x28, 0x00, 0x05][..], 0));
        // Bluetooth, both off: valid0 = 0x0C, 05 at 13 and 24, CRC over the first 74 bytes.
        let o = trigger_report(true, 0, Some(Effect::Off), Some(Effect::Off), None, R0).unwrap();
        assert_eq!((o.len(), o[0], o[2], o[3], o[13], o[24]), (78, 0x31, 0x10, 0x0C, 0x05, 0x05));
        assert_eq!(o[..74].iter().filter(|&&b| b != 0).count(), 5);
        assert_eq!(&o[74..], &crc32(0xA2, &o[..74]).to_le_bytes());
        // An effect out of range never becomes a report.
        assert_eq!(trigger_report(true, 0, Some(Effect::Resist { start: 0, strength: 9 }), None, None, R0), None);
        // Power reduction: valid1 0x40, trigger nibble high in common 36 (USB 37, Bluetooth 39).
        let wall = Effect::Resist { start: 0, strength: 8 };
        let p = trigger_report(false, 0, None, Some(wall), Some(7), R0).unwrap();
        assert_eq!((p[1], p[2], p[37]), (0x04, 0x40, 0x70));
        let pb = trigger_report(true, 0, None, Some(wall), Some(0), R0).unwrap();
        assert_eq!((pb[4], pb[39], pb[3]), (0x40, 0x00, 0x04));
        assert_eq!(trigger_report(false, 0, None, Some(wall), Some(8), R0), None);
        // Raw rigid: mode 0x01, raw start and force, nothing else (SDL's test block).
        assert_eq!(Effect::Rigid { start: 0, force: 0x6E }.bytes(), Some([0x01, 0x00, 0x6E, 0, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn rumble_bits() {
        let m = |left, right, v2| Rumble { left, right, v2 };
        // USB, v2: valid0 = 0x02 only, valid2 0x04 at 39, right motor at 3, left at 4.
        let r = trigger_report(false, 0, None, None, None, m(80, 20, true)).unwrap();
        assert_eq!((r[1], r[3], r[4], r[39]), (0x02, 20, 80, 0x04));
        assert_eq!(r.iter().filter(|&&b| b != 0).count(), 5);
        // Old form: valid0 = 0x03, valid2 untouched.
        let o = trigger_report(false, 0, None, None, None, m(80, 0, false)).unwrap();
        assert_eq!((o[1], o[3], o[4], o[39]), (0x03, 0, 80, 0));
        // Zero motors = SDL's stop: no rumble bits at all, an empty report.
        let z = trigger_report(true, 3, None, None, None, m(0, 0, true)).unwrap();
        assert_eq!(z[..74].iter().filter(|&&b| b != 0).count(), 3); // id, sequence, tag
        // Shared bytes are OR-ed: dim (valid2 0x01) + v2 (0x04); trigger flag + rumble flag.
        let d = output_report(true, 0, &Lights::Set { rgb: [0, 0, 9], players: None, mute: None, dim: Some(2) }, m(1, 1, true));
        assert_eq!((d[3], d[4], d[41], d[45], d[49]), (0x02, 0x04, 0x05, 2, 9));
        assert_eq!(&d[74..], &crc32(0xA2, &d[..74]).to_le_bytes());
        let t = trigger_report(true, 0, None, Some(Effect::Off), None, m(0, 5, false)).unwrap();
        assert_eq!((t[3], t[5], t[6], t[13]), (0x07, 5, 0, 0x05));
        // Setup (valid2 0x02) keeps its bit too.
        assert_eq!(output_report(false, 0, &Lights::Setup, m(1, 0, true))[39], 0x06);
    }

    #[test]
    fn bt_haptic_report() {
        let mut frames = [[0i8; 2]; 32];
        frames[0] = [-1, 5];
        frames[31] = [127, -128];
        let r = haptic_report(17, 9, &frames);
        assert_eq!((r.len(), r[0], r[1]), (142, 0x32, 0x10)); // sequence wraps to 1
        assert_eq!(&r[2..13], &[0x91, 7, 0xFE, 0, 0, 0, 0, 0xFF, 9, 0x92, 64]);
        assert_eq!((r[13], r[14], r[75], r[76]), (0xFF, 5, 0x7F, 0x80));
        assert!(r[77..138].iter().all(|&b| b == 0));
        assert_eq!(&r[138..], &crc32(0xA2, &r[..138]).to_le_bytes());
        let su = speaker_setup(1);
        assert_eq!((su.len(), su[0], su[1], su[2], su[3], su[4], su[8], su[10], su[40]), (78, 0x31, 0x10, 0x10, 0xA0, 0x80, 0x64, 0x30, 2));
        assert_eq!(&su[74..], &crc32(0xA2, &su[..74]).to_le_bytes());
        let mut opus = [0u8; 200];
        (opus[0], opus[199]) = (0xF4, 7);
        let sp = speaker_report(18, 3, &opus);
        assert_eq!((sp.len(), sp[0], sp[1]), (334, 0x35, 0x20));
        assert_eq!(&sp[2..15], &[0x91, 7, 0xFE, 0, 0, 0, 0, 48, 3, 0x93, 200, 0xF4, 0]);
        assert_eq!(sp[212], 7);
        assert!(sp[213..330].iter().all(|&b| b == 0));
        assert_eq!(&sp[330..], &crc32(0xA2, &sp[..330]).to_le_bytes());
        // The only test that touches MIC (others build audio reports in parallel and never check byte 4).
        MIC.store(true, std::sync::atomic::Ordering::Relaxed);
        let open = haptic_report(0, 0, &[]);
        MIC.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(&open[2..5], &[0x91, 7, 0xFF], "mic open");
        assert_eq!(&open[138..], &crc32(0xA2, &open[..138]).to_le_bytes());
    }

    #[test]
    fn rumble_form() {
        let mut f = [0u8; 64];
        f[44..46].copy_from_slice(&0x0633u16.to_le_bytes()); // a real controller
        assert!(vibration_v2(0x0CE6, Some(&f)));
        f[44..46].copy_from_slice(&0x0214u16.to_le_bytes());
        assert!(!vibration_v2(0x0CE6, Some(&f)));
        assert!(vibration_v2(0x0DF2, Some(&f)), "the Edge always takes v2");
        assert!(vibration_v2(0x0CE6, None), "unreadable: v2, as SDL assumes");
    }

    #[test]
    fn never_a_corrupting_mode() {
        // Modes 0xFC-0xFE are said to jam the trigger until the reset button is pressed.
        for a in [0u8, 1, 2, 5, 9, 128, 255] {
            for b in [0u8, 1, 4, 8, 9, 255] {
                for e in [
                    Effect::Off,
                    Effect::Resist { start: a, strength: b },
                    Effect::Click { start: a, end: b, strength: b },
                    Effect::Vibrate { start: a, strength: b, hz: b },
                    Effect::Rigid { start: a, force: b },
                ] {
                    if let Some(x) = e.bytes() {
                        assert!(matches!(x[0], 0x01 | 0x05 | 0x21 | 0x25 | 0x26), "{e:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn bt_short_report() {
        let mut r = [0u8; 78]; // Windows pads every report to the longest one
        r[0] = 0x01;
        r[5] = 0x08;
        r[7] = 0x01 | 0xFC; // PS + counter bits
        r[43] = 0x14; // stale full-report bytes in the padding must not count as trigger feedback
        let (k, s) = parse(true, &r).unwrap();
        assert_eq!(k, Kind::BtSimple);
        assert_eq!(s.trig, None);
        assert_eq!(button_names(s.buttons), "PS");
    }
}
