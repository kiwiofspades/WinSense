//! Builds the Windows resources in plain Rust (no rc.exe, no crate): the app icon (drawn here,
//! several sizes), a manifest (common controls 6, Windows 10/11, asInvoker) and version info
//! (the name Task Manager and its Startup list show). Linked into the exe as a .res file.

/// Signed distance to a rounded box centred at c with half size h and corner radius r.
fn rbox(p: (f32, f32), c: (f32, f32), h: (f32, f32), r: f32) -> f32 {
    let (qx, qy) = ((p.0 - c.0).abs() - h.0 + r, (p.1 - c.1).abs() - h.1 + r);
    (qx.max(0.0).hypot(qy.max(0.0))) + qx.max(qy).min(0.0) - r
}

fn capsule(p: (f32, f32), a: (f32, f32), b: (f32, f32), r: f32) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let t = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    (pa.0 - ba.0 * t).hypot(pa.1 - ba.1 * t) - r
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b + (a - b) * h - k * h * (1.0 - h)
}

/// Colour (straight RGBA, 0..1) of one point of the icon in unit coordinates; `detail` 0 = glyph
/// only (16-20 px), 1 = with sticks (24 px), 2 = sticks and touchpad (32 px and up).
fn sample(p: (f32, f32), detail: u8) -> [f32; 4] {
    // Tile: a Windows 11 style rounded square with a blue gradient (light at the top).
    let tile = rbox(p, (0.5, 0.5), (0.46, 0.46), 0.2);
    if tile > 0.0 {
        return [0.0; 4];
    }
    let t = p.1;
    let blue = [0.30 + (0.10 - 0.30) * t, 0.58 + (0.30 - 0.58) * t, 1.0 + (0.86 - 1.0) * t];
    // Controller: body plus two grips, softly joined.
    let body = rbox(p, (0.5, 0.47), (0.27, 0.115), 0.1);
    // Mirror x so one capsule makes both grips.
    let grip = capsule((0.5 - (p.0 - 0.5).abs(), p.1), (0.315, 0.48), (0.255, 0.68), 0.088);
    let mut pad = smin(body, grip, 0.06);
    if detail >= 1 {
        let stick = |x: f32| (p.0 - x).hypot(p.1 - 0.535) - 0.048;
        pad = pad.max(-stick(0.405)).max(-stick(0.595));
    }
    let mut rgb = if pad <= 0.0 { [1.0; 3] } else { blue };
    if detail >= 2 && pad <= 0.0 && rbox(p, (0.5, 0.425), (0.085, 0.042), 0.02) <= 0.0 {
        rgb = [0.84, 0.90, 1.0]; // touchpad: a faint blue tint
    }
    [rgb[0], rgb[1], rgb[2], 1.0]
}

/// One icon size as bottom-up BGRA rows (the order an icon resource stores them), 4x4 supersampled.
fn render(n: usize) -> Vec<u8> {
    let detail = match n {
        ..=20 => 0,
        ..=24 => 1,
        _ => 2,
    };
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0f32; 4];
            for s in 0..16 {
                let p = ((x as f32 + (s % 4) as f32 / 4.0 + 0.125) / n as f32, (y as f32 + (s / 4) as f32 / 4.0 + 0.125) / n as f32);
                let c = sample(p, detail);
                for i in 0..3 {
                    acc[i] += c[i] * c[3];
                }
                acc[3] += c[3];
            }
            let a = acc[3] / 16.0;
            let at = ((n - 1 - y) * n + x) * 4;
            if a > 0.0 {
                for i in 0..3 {
                    out[at + 2 - i] = (acc[i] / acc[3] * 255.0).round() as u8; // RGB -> BGR
                }
                out[at + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    out
}

/// The RT_ICON data: BITMAPINFOHEADER (double height), pixels, an empty AND mask.
fn icon_image(n: usize) -> Vec<u8> {
    let mask = n.div_ceil(32) * 4 * n;
    let mut d = Vec::new();
    for v in [40u32, n as u32, 2 * n as u32] {
        d.extend(v.to_le_bytes());
    }
    d.extend(1u16.to_le_bytes());
    d.extend(32u16.to_le_bytes());
    d.extend(0u32.to_le_bytes());
    d.extend(((n * n * 4 + mask) as u32).to_le_bytes());
    d.extend([0u8; 16]);
    d.extend(render(n));
    d.extend(vec![0u8; mask]);
    d
}

/// One entry of a .res file (type and name as numeric ids).
fn res(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes());
    for v in [0xFFFF, kind, 0xFFFF, id] {
        out.extend((v as u16).to_le_bytes());
    }
    out.extend(0u32.to_le_bytes());
    out.extend(flags.to_le_bytes());
    out.extend(0x0409u16.to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(data);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// A version-info node: key, value (text or binary), children; lengths and padding as Windows wants.
fn node(key: &str, value: Option<(bool, Vec<u8>)>, children: &[Vec<u8>]) -> Vec<u8> {
    let mut d = vec![0u8; 6];
    d.extend(utf16z(key));
    d.resize(d.len().div_ceil(4) * 4, 0);
    let (text, v) = value.unwrap_or((false, Vec::new()));
    let vlen = if text { v.len() / 2 } else { v.len() };
    d.extend(&v);
    d.resize(d.len().div_ceil(4) * 4, 0);
    for c in children {
        d.extend(c);
        d.resize(d.len().div_ceil(4) * 4, 0);
    }
    let len = d.len() as u16;
    d[0..2].copy_from_slice(&len.to_le_bytes());
    d[2..4].copy_from_slice(&(vlen as u16).to_le_bytes());
    d[4..6].copy_from_slice(&(text as u16).to_le_bytes());
    d
}

fn version_info() -> Vec<u8> {
    let v = |k| std::env::var(k).unwrap().parse::<u32>().unwrap();
    let (major, minor, patch) = (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let mut fixed = Vec::new();
    for x in [0xFEEF04BDu32, 0x10000, major << 16 | minor, patch << 16, major << 16 | minor, patch << 16, 0x3F, 0, 0x40004, 1, 0, 0, 0] {
        fixed.extend(x.to_le_bytes());
    }
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let s = |k: &str, val: &str| node(k, Some((true, utf16z(val))), &[]);
    let strings = [s("FileDescription", "WinSense"), s("ProductName", "WinSense"), s("FileVersion", &ver), s("ProductVersion", &ver), s("OriginalFilename", "winsense.exe"), s("InternalName", "winsense")];
    let table = node("040904B0", None, &strings);
    let translation = node("Translation", Some((false, [0x0409u16, 0x04B0].iter().flat_map(|x| x.to_le_bytes()).collect())), &[]);
    node("VS_VERSION_INFO", Some((false, fixed)), &[node("StringFileInfo", None, &[table]), node("VarFileInfo", None, &[translation])])
}

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="WinSense" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application></compatibility>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
</assembly>
"#;

/// Icon sizes: the small ones for the tray at 100-200 % (16, 20, 24, 32), the rest for Explorer.
const SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 128];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut r = vec![0u8; 32];
    r[4] = 32;
    r[8..12].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    r[12..16].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    let (mut group, mut ico, mut images) = (Vec::new(), Vec::new(), Vec::new());
    for (i, &n) in SIZES.iter().enumerate() {
        let img = icon_image(n);
        res(&mut r, 3, i as u16 + 1, 0x1010, &img);
        let entry = |tail: &[u8]| [&[n as u8, n as u8, 0, 0][..], &1u16.to_le_bytes(), &32u16.to_le_bytes(), &(img.len() as u32).to_le_bytes(), tail].concat();
        group.extend(entry(&(i as u16 + 1).to_le_bytes()));
        ico.push(entry(&[]));
        images.push(img);
    }
    let dir = |count: usize| [0u16, 1, count as u16].iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    res(&mut r, 14, 1, 0x1030, &[dir(SIZES.len()), group].concat());
    res(&mut r, 24, 1, 0x0030, MANIFEST.as_bytes());
    res(&mut r, 16, 1, 0x0030, &version_info());
    std::fs::write(out.join("winsense.res"), &r).unwrap();
    // The same icon as a .ico file, to look at the drawing (not shipped).
    let mut f = dir(SIZES.len());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (e, img) in ico.iter().zip(&images) {
        f.extend(e);
        f.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    images.iter().for_each(|i| f.extend(i));
    std::fs::write(out.join("winsense.ico"), f).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.join("winsense.res").display());
}
