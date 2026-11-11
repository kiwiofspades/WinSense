//! Voice commands and sounds, over Bluetooth, in the tray: hold Mute until
//! the ding, say "Computer, open <application>" (the name in front is optional, see NAMES) (or "focus" / "switch to" / "close <application>",
//! or "go to my <Downloads|Documents|Desktop|Pictures|Music|Videos> folder", or "go to <page> in
//! Retro Shelf" for the pages Retro Shelf itself lists, or one of `FIXED`: volume, media keys,
//! battery, controller off, mouse and gyro on / off), let go; the
//! controller's own speaker answers "Opening.", "Switching.", "Closing.", "Okay.", the battery
//! in words or "Didn't quite catch that.". Also plays the dings the mouse asks for.
//! Layout from PadForge (layout only, CC BY-NC-SA), vds and DS5Dongle (MIT): the mic opens with
//! packet 0x11 byte 0xFF in an audio report (0x32), and then some input reports 0x31 carry, in place
//! of the state, header bit 1 ("HasMic"), a frame counter at byte 2 and one 71-byte Opus frame
//! (48 kHz, 10 ms, decoded as mono) at bytes 3..74. A 0xFE in any later audio report closes it.
//! The mic stays open after the program ends until something closes it, and Steam (SDL) reads the
//! mic frames as buttons (measured: its PS chords opened the keyboard, Magnifier, and switched the
//! controller off), so the mic is refused while Steam runs and every path out of here closes it.
//! Speech: Windows' own Opus decoder (MSOpusDecoder) and offline recogniser (SAPI) with a closed
//! grammar of the Start menu's app names; nothing leaves the PC. Speech never becomes a command
//! line: only an item of that list is opened, with the "open" verb.

use crate::ds;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Speech::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::UI::Shell::*;
use windows::core::{GUID, HSTRING, Interface, PCWSTR, PWSTR, w};

/// One mic frame as kept: the controller's frame counter, then the 71 Opus bytes.
pub const FRAME: usize = 72;
/// Mute in the full report's third button byte (data 9, report byte 11 over Bluetooth).
const MUTE: u8 = 0x04;
/// Mute held this long starts listening (a tap does nothing).
const HOLD: Duration = Duration::from_millis(400);
/// Listening ends here even if Mute is still held.
const MAX_LISTEN: Duration = Duration::from_secs(8);
/// Below this the recogniser's match is refused. A real "computer open notepad" scored 0.99
/// against 260 names; a near miss ("stream" for "steam") scored 0.66 in a trial.
const CONFIDENCE: f32 = 0.8;
const STEAM: &str = "Voice could not pause Steam, and a running Steam reads the controller's microphone sound as button presses.";
/// What may be said in front of a command; none is needed.
const NAMES: [&str; 5] = ["computer", "jarvis", "yo", "hey", "okay"];
const SPDFID_WAVEFORMATEX: GUID = GUID::from_u128(0xC31ADBAE_527F_4ff5_A230_F62BB61FF70C);

/// Clips for the controller's speaker: 200-byte Opus frames, made by ffmpeg once (docs/modding.md, "Making clips").
pub const DING: u8 = 1; // touchpad mouse on
pub const DING2: u8 = 2; // gyro on
pub const OFF: u8 = 3; // either off, or voice refused
const LISTEN: u8 = 4;
const OPENING: u8 = 5;
const SORRY: u8 = 6;
const SWITCHING: u8 = 7;
const CLOSING: u8 = 8;
const OKAY: u8 = 9;
const BAT_FULL: u8 = 10;
const BAT_HIGH: u8 = 11;
const BAT_HALF: u8 = 12;
const BAT_LOW: u8 = 13;
const CHARGING: u8 = 14;
/// Game sounds for mods (`sound <name>` on the port), synthesised: assets/clips.py.
const PICKUP: u8 = 15;
const ALERT: u8 = 16; // Konami's Metal Gear Solid alert, not from clips.py (see CREDITS.md).
const RADIO: u8 = 17;
const LEVELUP: u8 = 18;
/// The clips a mod may ask for by name.
pub fn named(name: &str) -> Option<u8> {
    [("ding", DING), ("ding2", DING2), ("off", OFF), ("listen", LISTEN), ("pickup", PICKUP), ("alert", ALERT), ("radio", RADIO), ("levelup", LEVELUP)].iter().find(|n| n.0 == name).map(|n| n.1)
}
/// Not a clip: in a reply, "now turn the controller off" (after the clips before it were heard).
const POWER: u8 = 255;
fn clip(id: u8) -> &'static [u8] {
    match id {
        OKAY => include_bytes!("../assets/okay.bin"),
        BAT_FULL => include_bytes!("../assets/battery_full.bin"),
        BAT_HIGH => include_bytes!("../assets/battery_high.bin"),
        BAT_HALF => include_bytes!("../assets/battery_half.bin"),
        BAT_LOW => include_bytes!("../assets/battery_low.bin"),
        CHARGING => include_bytes!("../assets/charging.bin"),
        DING => include_bytes!("../assets/ding.bin"),
        DING2 => include_bytes!("../assets/ding2.bin"),
        OFF => include_bytes!("../assets/off.bin"),
        LISTEN => include_bytes!("../assets/listen.bin"),
        OPENING => include_bytes!("../assets/opening.bin"),
        SWITCHING => include_bytes!("../assets/switching.bin"),
        CLOSING => include_bytes!("../assets/closing.bin"),
        PICKUP => include_bytes!("../assets/pickup.bin"),
        ALERT => include_bytes!("../assets/alert.bin"),
        RADIO => include_bytes!("../assets/radio.bin"),
        LEVELUP => include_bytes!("../assets/levelup.bin"),
        _ => include_bytes!("../assets/sorry.bin"),
    }
}

/// A sound the mouse, couch mode or a mod wants played (0 = none; the newest wins), and a line for the tray's balloon.
pub static SOUND: AtomicU8 = AtomicU8::new(0);
pub static NOTE: Mutex<Option<&'static str>> = Mutex::new(None);

/// True once, when a button has been held for `long`.
#[derive(Default)]
pub struct Hold {
    since: Option<Instant>,
    pub fired: bool,
}
impl Hold {
    pub fn long(&mut self, down: bool, now: Instant, long: Duration) -> bool {
        if !down {
            *self = Hold::default();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        !self.fired && now - since >= long && {
            self.fired = true;
            true
        }
    }
}

/// The mic frame in a Bluetooth input report, or None for an ordinary state report.
pub fn mic_frame(r: &[u8]) -> Option<&[u8]> {
    (r.len() >= 78 && r[0] == 0x31 && r[1] & 0x02 != 0).then(|| &r[2..74])
}

/// The mic switch alone, in PadForge's form (layout only): report 0x32 with sequence 0, packet
/// 0x11 [mic, 0, 0, 0, 0, buffer 48, counter] and 64 silent haptic bytes. MEASURED with Steam
/// closed: 101 mic frames a second.
pub fn toggle(open: bool, counter: u8) -> Vec<u8> {
    ds::MIC.store(open, Ordering::Relaxed);
    let mut r = ds::haptic_report(0, counter, &[]);
    r[9] = 48;
    let crc = ds::crc32(0xA2, &r[..138]);
    r[138..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Closes an open mic when dropped (also on a panic).
struct Close<'a>(&'a hidapi::HidDevice);
impl Drop for Close<'_> {
    fn drop(&mut self) {
        for _ in 0..3 {
            if !ds::MIC.load(Ordering::Relaxed) || self.0.write(&toggle(false, 0)).is_ok() {
                break;
            }
            ds::MIC.store(true, Ordering::Relaxed); // not closed yet: try again
        }
        ds::MIC.store(false, Ordering::Relaxed);
    }
}

/// Every running steam.exe, or None when the process list cannot be read.
fn steam_pids() -> Option<Vec<u32>> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = Vec::new();
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let name = String::from_utf16_lossy(&e.szExeFile);
            if name.split('\0').next().unwrap_or("").eq_ignore_ascii_case("steam.exe") {
                found.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        Some(found)
    }
}

/// Steam reads the mic frames as button presses (seen: its button chords fired).
fn steam_running() -> bool {
    steam_pids().is_none_or(|p| !p.is_empty()) // cannot tell: stay safe
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process: windows::Win32::Foundation::HANDLE) -> i32;
    fn NtResumeProcess(process: windows::Win32::Foundation::HANDLE) -> i32;
}

/// After the mic closes, Steam stays paused this long: Windows keeps only the newest reports for
/// a reader that is not reading (32 unless it asked for more; NOT measured for Steam), and about
/// 490 state reports a second arrive, so some 150 push the mic frames out in this time.
const FLUSH: Duration = Duration::from_millis(300);

/// Programs paused while the controller's mic is open: a frozen Steam reads
/// no mic frames, so none become button presses. Dropping it lets them run again, after `FLUSH`.
// shortcut: if WinSense is killed while listening (8 s at most), Steam stays frozen until it is ended in Task Manager; a helper process that resumes it would fix that.
// shortcut: a game or emulator that reads the pad itself (SDL) is not paused and still sees stray presses while Mute is held.
#[derive(Default)]
struct Paused(Vec<(u32, windows::Win32::Foundation::HANDLE)>);
impl Paused {
    /// Pause these programs unless already paused; false if one could not be.
    fn add(&mut self, pids: &[u32]) -> bool {
        pids.iter().filter(|p| !self.0.iter().any(|h| h.0 == **p)).collect::<Vec<_>>().into_iter().all(|&pid| unsafe {
            let Ok(h) = OpenProcess(PROCESS_SUSPEND_RESUME, false, pid) else { return false };
            self.0.push((pid, h)); // kept even if the pause fails, so the drop resumes and closes it
            NtSuspendProcess(h) >= 0
        })
    }
    /// Every steam.exe paused; false if the list cannot be read or one refuses.
    fn steam(&mut self) -> bool {
        steam_pids().is_some_and(|p| self.add(&p))
    }
    fn release(&mut self) {
        if !self.0.is_empty() {
            std::thread::sleep(FLUSH);
        }
        for (_, h) in self.0.drain(..) {
            unsafe {
                NtResumeProcess(h);
                let _ = CloseHandle(h);
            }
        }
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        self.release();
    }
}

fn bluetooth() -> Option<hidapi::HidDevice> {
    let api = hidapi::HidApi::new().ok()?;
    let info = api.device_list().find(|d| d.vendor_id() == ds::VID && ds::PIDS.contains(&d.product_id()) && matches!(d.bus_type(), hidapi::BusType::Bluetooth))?;
    info.open_device(&api).ok()
}

/// Mic frames (FRAME bytes each) to 16 kHz mono 16-bit samples, through Windows' Opus decoder.
fn decode(frames: &[u8]) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let t: IMFTransform = CoCreateInstance(&CLSID_MSOpusDecoder, None, CLSCTX_INPROC_SERVER)?;
        let it = MFCreateMediaType()?;
        it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        it.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Opus)?;
        it.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
        it.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?; // the frames say stereo; mono is what decodes right (PadForge, measured here)
        t.SetInputType(0, &it, 0)?;
        let ot = t.GetOutputAvailableType(0, 0)?; // mono 32-bit float at 48 kHz (seen)
        t.SetOutputType(0, &ot, 0)?;
        let room = t.GetOutputStreamInfo(0)?.cbSize; // the decoder refuses a smaller buffer
        let mut pcm = Vec::new();
        for (i, f) in frames.chunks_exact(FRAME).enumerate() {
            let b = MFCreateMemoryBuffer(71)?;
            let mut p = std::ptr::null_mut();
            b.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(f[1..].as_ptr(), p, 71);
            b.Unlock()?;
            b.SetCurrentLength(71)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&b)?;
            s.SetSampleTime(i as i64 * 100_000)?;
            s.SetSampleDuration(100_000)?;
            t.ProcessInput(0, &s, 0)?;
            loop {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(room)?)?;
                let mut out = [MFT_OUTPUT_DATA_BUFFER { pSample: std::mem::ManuallyDrop::new(Some(s.clone())), ..Default::default() }];
                let mut status = 0;
                let r = t.ProcessOutput(0, &mut out, &mut status);
                std::mem::ManuallyDrop::drop(&mut out[0].pSample);
                std::mem::ManuallyDrop::drop(&mut out[0].pEvents);
                if r.is_err() {
                    break; // needs more input
                }
                let b = s.ConvertToContiguousBuffer()?;
                let (mut p, mut n) = (std::ptr::null_mut(), 0);
                b.Lock(&mut p, None, Some(&mut n))?;
                pcm.extend(std::slice::from_raw_parts(p, n as usize).chunks_exact(4).map(|v| f32::from_le_bytes(v.try_into().unwrap())));
                b.Unlock()?;
            }
        }
        // 48 kHz to the recogniser's 16 kHz: the mean of every three samples.
        Ok(pcm.chunks_exact(3).flat_map(|c| (((c.iter().sum::<f32>() / 3.0).clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect())
    }
}

/// The command said (without the name in front) in 16 kHz mono samples, with the recogniser's confidence;
/// None when nothing in the closed grammar of `phrases` was heard.
fn hear(pcm: &[u8], phrases: &[String]) -> windows::core::Result<Option<(String, f32)>> {
    unsafe {
        let fmt = WAVEFORMATEX { wFormatTag: 1, nChannels: 1, nSamplesPerSec: 16000, nAvgBytesPerSec: 32000, nBlockAlign: 2, wBitsPerSample: 16, cbSize: 0 };
        let mem = SHCreateMemStream(Some(pcm)).ok_or_else(windows::core::Error::empty)?;
        let stream: ISpStream = CoCreateInstance(&SpStream, None, CLSCTX_ALL)?;
        stream.SetBaseStream(&mem, &SPDFID_WAVEFORMATEX, &fmt)?;
        let rec: ISpRecognizer = CoCreateInstance(&SpInprocRecognizer, None, CLSCTX_ALL)?;
        let cat: ISpObjectTokenCategory = CoCreateInstance(&SpObjectTokenCategory, None, CLSCTX_ALL)?;
        cat.SetId(SPCAT_RECOGNIZERS, false)?;
        let id = cat.GetDefaultTokenId()?;
        let token: ISpObjectToken = CoCreateInstance(&SpObjectToken, None, CLSCTX_ALL)?;
        let set = token.SetId(PCWSTR::null(), PCWSTR(id.0), false);
        CoTaskMemFree(Some(id.0 as _));
        set?;
        rec.SetRecognizer(&token)?;
        rec.SetInput(&stream, true)?;
        let ctx = rec.CreateRecoContext()?;
        let g = ctx.CreateGrammar(1)?;
        let (mut top, mut mid) = (SPSTATEHANDLE::default(), SPSTATEHANDLE::default());
        g.GetRule(w!("open"), 1, (SPRAF_TopLevel.0 | SPRAF_Active.0 | SPRAF_Dynamic.0) as u32, true, &mut top)?;
        g.CreateNewState(top, &mut mid)?;
        // The name in front is optional: no word at all, or one of NAMES.
        g.AddWordTransition(top, mid, PCWSTR::null(), w!(" "), SPWT_LEXICAL, 1.0, std::ptr::null())?;
        for n in NAMES {
            let _ = g.AddWordTransition(top, mid, &HSTRING::from(n), w!(" "), SPWT_LEXICAL, 1.0, std::ptr::null());
        }
        for n in phrases {
            // A phrase the engine cannot pronounce is left out.
            let _ = g.AddWordTransition(mid, SPSTATEHANDLE::default(), &HSTRING::from(n.as_str()), w!(" "), SPWT_LEXICAL, 1.0, std::ptr::null());
        }
        g.Commit(0)?;
        g.SetRuleState(PCWSTR::null(), std::ptr::null_mut(), SPRS_ACTIVE)?;
        let bit = |e: SPEVENTENUM| (1u64 << e.0) | (1 << 30) | (1 << 33); // SAPI wants its two reserved bits set
        let want = bit(SPEI_RECOGNITION) | bit(SPEI_FALSE_RECOGNITION) | bit(SPEI_END_SR_STREAM);
        ctx.SetInterest(want, want)?;
        ctx.SetNotifyWin32Event()?;
        let mut heard = None;
        let mut any = true;
        while any && ctx.WaitForNotifyEvent(5000).is_ok() {
            any = false; // a timeout is also Ok: a wait that brought no event ends the loop
            let (mut ev, mut got) = (SPEVENT::default(), 0);
            while ctx.GetEvents(1, &mut ev, &mut got).is_ok() && got == 1 {
                any = true;
                let id = ev._bitfield & 0xFFFF;
                if id == SPEI_END_SR_STREAM.0 {
                    return Ok(heard);
                }
                if id != SPEI_RECOGNITION.0 && id != SPEI_FALSE_RECOGNITION.0 {
                    continue;
                }
                let result = ISpRecoResult::from_raw(ev.lParam.0 as _); // takes the event's reference
                if id == SPEI_RECOGNITION.0 {
                    let mut text = PWSTR::null();
                    result.GetText(u32::MAX, u32::MAX, true, &mut text, None)?;
                    let phrase = result.GetPhrase()?;
                    let said = text.to_string().unwrap_or_default();
                    let rest = NAMES.iter().find_map(|n| said.strip_prefix(n)?.strip_prefix(' ')).unwrap_or(&said);
                    heard = Some((rest.to_string(), (*phrase).Base.Rule.SREngineConfidence));
                    CoTaskMemFree(Some(phrase as _));
                    CoTaskMemFree(Some(text.0 as _));
                }
            }
        }
        Ok(heard)
    }
}

/// How an app's name is said, or None for what voice must never open: prompts, shells, system
/// tools, uninstallers, installers, web links and documents. `parsing` is the Start menu item's
/// id in the Applications folder (a path, a store app id, or steam://rungameid/...).
fn speakable(name: &str, parsing: &str) -> Option<String> {
    let (n, p) = (name.to_lowercase(), parsing.to_lowercase());
    let file = p.rsplit(['\\', '/']).next().unwrap_or("");
    let stem = file.strip_suffix(".exe").unwrap_or(file);
    // Known-folder ids of System32, SysWOW64 and Windows: system tools (this also drops Task Manager and the like).
    let denied = ["http://", "https://", "microsoft.autogenerated.", "{1ac14e77-", "{d65231b0-", "{f38bf404-"].iter().any(|s| p.starts_with(s))
        // A path must be a program: no folders, documents, scripts or shortcuts.
        || ((p.contains('\\') || p.contains(':')) && !p.starts_with("steam://rungameid/") && !p.ends_with(".exe"))
        || ["rundialog", "windowsterminal", "cmdexe", "powershell"].iter().any(|s| p.contains(s))
        || ["microsoft.wsl", "microsoft.windows.controlpanel", "microsoft.windows.administrativetools"].contains(&p.as_str())
        || ["cmd", "pwsh", "wt", "conhost", "regedit", "mmc", "msiexec", "python", "pythonw", "node", "wsl", "bash", "kernelmoduleunloader"].contains(&stem)
        || ["unins", "setup", "install"].iter().any(|s| stem.starts_with(s))
        || ["install", "prompt", "powershell", "registry", "reset", "repair", "readme", "website"].iter().any(|s| n.contains(s));
    let said = words(&n);
    (!denied && !said.is_empty()).then_some(said)
}

/// Lower-case words only, the form names are said and compared in.
fn words(text: &str) -> String {
    text.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What voice may open: (name as said, id in the Applications folder), from the same list the
/// Start menu shows. A name two apps share is left out.
fn apps() -> windows::core::Result<Vec<(String, String)>> {
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        let mut list: Vec<(String, String)> = Vec::new();
        loop {
            let (mut item, mut got) = ([None], 0);
            if items.Next(&mut item, Some(&mut got)).is_err() || got == 0 {
                break;
            }
            let Some(item) = item[0].take() else { break };
            let text = |kind| {
                item.GetDisplayName(kind).ok().map(|p: PWSTR| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as _));
                    s
                })
            };
            if let (Some(name), Some(parsing)) = (text(SIGDN_NORMALDISPLAY), text(SIGDN_PARENTRELATIVEPARSING))
                && let Some(said) = speakable(&name, &parsing)
            {
                list.push((said, parsing));
            }
        }
        let twice: Vec<String> = list.iter().filter(|a| list.iter().filter(|b| b.0 == a.0).count() > 1).map(|a| a.0.clone()).collect();
        list.retain(|a| !twice.contains(&a.0));
        Ok(list)
    }
}

/// Open one item of `apps()`: the "open" verb on that item, nothing else (no path, arguments or
/// verb ever comes from speech).
fn launch(parsing: &str) -> bool {
    unsafe {
        let mut pidl = std::ptr::null_mut();
        if SHParseDisplayName(&HSTRING::from(format!("shell:AppsFolder\\{parsing}")), None, &mut pidl, 0, None).is_err() {
            return false;
        }
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC, // NOASYNC: this thread has no message loop
            lpVerb: w!("open"),
            lpIDList: pidl as _,
            nShow: 1, // SW_SHOWNORMAL
            ..Default::default()
        };
        let ok = ShellExecuteExW(&mut info).is_ok();
        ILFree(Some(pidl));
        ok
    }
}

/// Whether a window (its title, its program's path) belongs to the app said: the name stands in
/// the title as whole words ("Untitled - Notepad"), or the program is the app's own exe.
fn window_of(title: &str, exe: &str, said: &str, parsing: &str) -> bool {
    let file = |p: &str| p.to_lowercase().rsplit(['\\', '/']).next().unwrap_or("").to_string();
    format!(" {} ", words(title)).contains(&format!(" {said} ")) || (parsing.to_lowercase().ends_with(".exe") && file(exe) == file(parsing))
}

/// The topmost visible window of that app.
fn window(said: &str, parsing: &str) -> Option<HWND> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::core::BOOL {
        unsafe {
            let list = &mut *(l.0 as *mut Vec<(HWND, String, String)>);
            let mut title = [0u16; 256];
            let n = GetWindowTextW(h, &mut title) as usize;
            if n > 0 && IsWindowVisible(h).as_bool() && GetWindow(h, GW_OWNER).is_err() {
                let (mut pid, mut path, mut len) = (0, [0u16; 520], 520);
                GetWindowThreadProcessId(h, Some(&mut pid));
                if let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    if QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, windows::core::PWSTR(path.as_mut_ptr()), &mut len).is_err() {
                        len = 0;
                    }
                    let _ = CloseHandle(p);
                } else {
                    len = 0;
                }
                list.push((h, String::from_utf16_lossy(&title[..n]), String::from_utf16_lossy(&path[..len as usize])));
            }
            true.into()
        }
    }
    let mut list: Vec<(HWND, String, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize)); // top of the z-order first
    }
    list.into_iter().find(|(_, title, exe)| window_of(title, exe, said, parsing)).map(|w| w.0)
}

/// Bring a window to the front (restored if minimised). A background program may not take the
/// foreground by itself, so this thread joins the foreground window's input for the moment.
fn focus(h: HWND) -> bool {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let front = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let _ = AttachThreadInput(me, front, true);
        let ok = SetForegroundWindow(h).as_bool();
        let _ = BringWindowToTop(h);
        let _ = AttachThreadInput(me, front, false);
        ok
    }
}

/// The folders "go to" knows: Windows' own, opened in Explorer.
const FOLDERS: [(&str, GUID); 6] = [("downloads", FOLDERID_Downloads), ("documents", FOLDERID_Documents), ("desktop", FOLDERID_Desktop), ("pictures", FOLDERID_Pictures), ("music", FOLDERID_Music), ("videos", FOLDERID_Videos)];

fn open_folder(id: &GUID) -> bool {
    unsafe {
        let Ok(path) = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) else { return false };
        let r = ShellExecuteW(None, w!("open"), PCWSTR(path.0), None, None, SW_SHOWNORMAL);
        CoTaskMemFree(Some(path.0 as _));
        r.0 as usize > 32
    }
}

/// Retro Shelf (a game launcher) takes `--pages` (prints id<TAB>title per page) and
/// `--show <id>` (its running window comes to the front on that page, or it starts there).
const SHELF: &str = "retro shelf";

/// `--pages` output into (title as said, id). An id is letters, digits, - or _ only.
fn areas(listing: &str) -> Vec<(String, String)> {
    let ok = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    listing.lines().filter_map(|l| l.trim_end().split_once('\t')).filter(|(id, title)| ok(id) && !words(title).is_empty()).map(|(id, title)| (words(title), id.to_string())).collect()
}

/// Run Retro Shelf's exe with fixed arguments, hidden. `wait`: its output, given 2 s (an older
/// Retro Shelf does not know `--pages` and would stay open as the app: then nothing is read).
fn shelf(exe: &str, args: &[&str], wait: bool) -> Option<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    if !std::path::Path::new(exe).is_absolute() {
        return None;
    }
    let mut child = Command::new(exe).args(args).stdin(Stdio::null()).stdout(if wait { Stdio::piped() } else { Stdio::null() }).creation_flags(0x0800_0000).spawn().ok()?; // CREATE_NO_WINDOW
    for _ in 0..if wait { 40 } else { 0 } {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    wait.then(String::new)
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Said<'a> {
    Area(&'a str),
    Open(&'a str),
    Focus(&'a str),
    Close(&'a str),
    Folder(&'a str),
    /// Windows' volume a step up (true) or down.
    Volume(bool),
    Mute(bool),
    /// A media key (virtual-key code).
    Key(u16),
    Battery,
    /// Turn the controller off.
    Power,
    /// The touchpad mouse / the gyro mouse on or off.
    Touch(bool),
    Gyro(bool),
    /// Couch mode on or off (the tray's own menu item).
    Couch(bool),
}

const PLAY_PAUSE: u16 = 0xB3; // VK_MEDIA_PLAY_PAUSE
const NEXT: u16 = 0xB0; // VK_MEDIA_NEXT_TRACK
const PREVIOUS: u16 = 0xB1; // VK_MEDIA_PREV_TRACK

/// Commands that are the same on every PC.
// shortcut: "pause" and "play" both press the play/pause key, so "pause" while paused starts it; asking the player for its state (Windows' media session API) would fix that.
// "turn on mouse" and "touchpad on" are there because "mouse on" scored only 0.72 in one of the two test voices (they scored 0.98).
const FIXED: [(&str, Said<'static>); 30] = [
    ("volume up", Said::Volume(true)),
    ("louder", Said::Volume(true)),
    ("volume down", Said::Volume(false)),
    ("quieter", Said::Volume(false)),
    ("mute", Said::Mute(true)),
    ("unmute", Said::Mute(false)),
    ("pause", Said::Key(PLAY_PAUSE)),
    ("play", Said::Key(PLAY_PAUSE)),
    ("next", Said::Key(NEXT)),
    ("skip", Said::Key(NEXT)),
    ("previous", Said::Key(PREVIOUS)),
    ("battery", Said::Battery),
    ("what's my battery", Said::Battery),
    ("how much battery", Said::Battery),
    ("turn off controller", Said::Power),
    ("controller off", Said::Power),
    ("mouse on", Said::Touch(true)),
    ("turn on mouse", Said::Touch(true)),
    ("touchpad on", Said::Touch(true)),
    ("mouse off", Said::Touch(false)),
    ("turn off mouse", Said::Touch(false)),
    ("touchpad off", Said::Touch(false)),
    ("gyro on", Said::Gyro(true)),
    ("turn on gyro", Said::Gyro(true)),
    ("gyro off", Said::Gyro(false)),
    ("turn off gyro", Said::Gyro(false)),
    ("couch mode on", Said::Couch(true)),
    ("turn on couch mode", Said::Couch(true)),
    ("couch mode off", Said::Couch(false)),
    ("turn off couch mode", Said::Couch(false)),
];

/// The volume of the default playback device, which is what the volume keys change.
/// DECISION: this interface, not the volume keys: "mute" and "unmute" must be exact (the key only
/// flips), a step can be a tenth (a key press is 2 %), and injected keys do not reach Windows
/// while an elevated program is in front. Cost: Windows' volume flyout does not show.
fn endpoint() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe { default_device(eRender)?.Activate(CLSCTX_ALL, None) }
}

/// The default device (role console): playback (above) or recording (src/desk.rs).
pub(crate) fn default_device(flow: windows::Win32::Media::Audio::EDataFlow) -> windows::core::Result<windows::Win32::Media::Audio::IMMDevice> {
    unsafe {
        let all: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        all.GetDefaultAudioEndpoint(flow, eConsole)
    }
}

/// One step of "volume up" / "volume down": a tenth of the scale.
fn step(now: f32, up: bool) -> f32 {
    (now + if up { 0.1 } else { -0.1 }).clamp(0.0, 1.0)
}

/// A step up also ends a mute, like the volume key.
fn volume(up: bool) -> windows::core::Result<()> {
    unsafe {
        let v = endpoint()?;
        v.SetMasterVolumeLevelScalar(step(v.GetMasterVolumeLevelScalar()?, up), std::ptr::null())?;
        if up { v.SetMute(false, std::ptr::null()) } else { Ok(()) }
    }
}

/// Press and let go of one key (the media keys; they go to whichever player Windows gives them).
fn key(vk: u16) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let k = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
    let keys = [k(0), k(KEYEVENTF_KEYUP)];
    unsafe { SendInput(2, keys.as_ptr(), size_of::<INPUT>() as i32) == 2 }
}

/// The battery as clips: one of four bands, then "Charging." on a charger. `b` = (level 0-10, state), as `power::battery`.
fn charge(b: Option<(u8, u8)>) -> Vec<u8> {
    let Some((level, state)) = b else { return vec![SORRY] };
    let band = match level {
        _ if state == 2 => BAT_FULL,
        10.. => BAT_FULL,
        7.. => BAT_HIGH, // 75 % and up
        4.. => BAT_HALF, // 45 to 65 %
        _ => BAT_LOW,
    };
    if state == 1 { vec![band, CHARGING] } else { vec![band] }
}

/// Every phrase the grammar takes after "computer", for these app names and Retro Shelf pages.
fn phrases(names: &[String], areas: &[(String, String)]) -> Vec<String> {
    let apps = names.iter().flat_map(|n| ["open", "focus", "switch to", "close"].map(|v| format!("{v} {n}")));
    let folders = FOLDERS.iter().flat_map(|f| [format!("go to {}", f.0), format!("go to my {}", f.0), format!("go to {} folder", f.0), format!("go to my {} folder", f.0)]);
    apps.chain(folders).chain(areas.iter().map(|a| format!("go to {} in {SHELF}", a.0))).chain(FIXED.iter().map(|f| f.0.to_string())).collect()
}

/// A phrase of `phrases` back into what to do.
fn said(phrase: &str) -> Option<Said<'_>> {
    if let Some(f) = FIXED.iter().find(|f| f.0 == phrase) {
        return Some(f.1);
    }
    if let Some(rest) = phrase.strip_prefix("go to ") {
        if let Some(area) = rest.strip_suffix(&format!(" in {SHELF}")) {
            return Some(Said::Area(area));
        }
        return Some(Said::Folder(rest.strip_prefix("my ").unwrap_or(rest).trim_end_matches(" folder")));
    }
    let verb = |v: &str| phrase.strip_prefix(v);
    verb("open ").map(Said::Open).or(verb("focus ").map(Said::Focus)).or(verb("switch to ").map(Said::Focus)).or(verb("close ").map(Said::Close))
}

/// Do what the mic frames ask, if it was clearly heard; the clips to answer with (and `POWER`).
fn command(frames: &[u8]) -> Vec<u8> {
    let run = || -> Option<Vec<u8>> {
        let list = apps().map_err(|e| println!("voice: app list failed: {e}")).ok()?;
        let names: Vec<String> = list.iter().map(|a| a.0.clone()).collect();
        let pcm = decode(frames).map_err(|e| println!("voice: decode failed: {e}")).ok()?;
        // Retro Shelf's pages, when the Start menu lists it as a program.
        let exe = list.iter().find(|a| a.0 == SHELF && a.1.to_lowercase().ends_with(".exe")).map(|a| a.1.as_str());
        let pages = exe.and_then(|e| shelf(e, &["--pages"], true)).map(|l| areas(&l)).unwrap_or_default();
        let (phrase, confidence) = hear(&pcm, &phrases(&names, &pages)).map_err(|e| println!("voice: recogniser failed: {e}")).ok()??;
        println!("voice: heard \"{phrase}\" ({confidence:.2}) in {} frames, {} apps", frames.len() / FRAME, list.len());
        if confidence < CONFIDENCE {
            return None;
        }
        let app = |name: &str| list.iter().find(|a| a.0 == name);
        let switch = |flag: &AtomicBool, on: bool, ding: u8| {
            flag.store(on, Ordering::Relaxed); // the mouse thread reads it; voice only runs in the full report, where the mouse is already awake
            if on { ding } else { OFF }
        };
        let clip = match said(&phrase)? {
            Said::Battery => return Some(charge(crate::power::battery())),
            Said::Power => return Some(vec![OKAY, POWER]),
            Said::Volume(up) => volume(up).is_ok().then_some(OKAY),
            Said::Mute(on) => endpoint().and_then(|v| unsafe { v.SetMute(on, std::ptr::null()) }).is_ok().then_some(OKAY),
            Said::Key(vk) => key(vk).then_some(OKAY),
            // The same dings as the buttons give.
            Said::Touch(on) => Some(switch(&crate::mouse::TOUCH_ON, on, DING)),
            Said::Gyro(on) => Some(switch(&crate::mouse::GYRO_ON, on, DING2)),
            // Through the tray's menu item, which also remembers it and ticks the menu; it flips, so only when it differs.
            Said::Couch(on) => {
                if crate::couch::ON.load(Ordering::Relaxed) != on {
                    let h = unsafe { FindWindowW(w!("WinSenseTray"), PCWSTR::null()) }.ok()?;
                    unsafe { PostMessageW(Some(h), WM_COMMAND, WPARAM(crate::tray::ID_COUCH), LPARAM(0)) }.ok()?;
                }
                Some(if on { DING } else { OFF })
            }
            Said::Open(n) => launch(&app(n)?.1).then_some(OPENING),
            Said::Focus(n) => focus(window(n, &app(n)?.1)?).then_some(SWITCHING),
            // Asks the window to close, as its X button does: the app may still ask about unsaved work.
            Said::Close(n) => unsafe { PostMessageW(Some(window(n, &app(n)?.1)?), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok().then_some(CLOSING) },
            // Only an id Retro Shelf listed itself goes on its command line.
            Said::Area(a) => {
                let exe = exe?;
                shelf(exe, &["--show", &pages.iter().find(|p| p.0 == a)?.1], false);
                // Retro Shelf raises its own window; a background program may be refused, so ask from here too once the window is there.
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(300));
                    if let Some(h) = window(SHELF, exe) {
                        focus(h);
                        break;
                    }
                }
                Some(OPENING)
            }
            Said::Folder(f) => open_folder(&FOLDERS.iter().find(|k| k.0 == f)?.1).then_some(OPENING),
        };
        Some(vec![clip?])
    };
    run().unwrap_or(vec![SORRY])
}

/// The tray's voice thread: plays the sounds asked for through `SOUND`, and runs the hold-Mute
/// command on the first Bluetooth DualSense. Only acts in the full report (Mute is not in the
/// short one), so it never switches the controller itself. A dry tray has no voice.
// shortcut: clips use their own report counter; a mod's haptic tone (0x32) at the same moment may stall the pad's audio for a second (PadForge). Share one counter if that is ever felt.
pub fn run(stop: &AtomicBool, dry: bool) {
    if dry {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
    }
    while !stop.load(Ordering::Relaxed) {
        let Some(dev) = bluetooth() else {
            SOUND.store(0, Ordering::Relaxed); // nobody to play it: not kept for the next connection
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let mut paused = Paused::default(); // before `_close`: on any way out the mic closes first, then Steam runs again
        let _close = Close(&dev);
        // The speaker is silent until it is switched on and turned up (heard): once per connection, before the first sound.
        let mut ready = false;
        let mut play = |id: u8| {
            if !ready {
                // shortcut: this report carries no rumble bits, so a rumble running right now stops until the worker's next write; send it through the worker if that is ever felt.
                ready = dev.write(&ds::speaker_setup(0)).is_ok();
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = crate::audio::play_speaker(std::slice::from_ref(&dev), clip(id), &mut || true);
        };
        let (mut buf, mut hold, mut stray, mut checked) = ([0u8; 128], Hold::default(), Instant::now(), Instant::now());
        let mut listening: Option<(Instant, Vec<u8>)> = None;
        while !stop.load(Ordering::Relaxed) {
            match SOUND.swap(0, Ordering::Relaxed) {
                0 => {}
                _ if listening.is_some() => {} // never into the open mic: dropped, not played later
                id => play(id),
            }
            let Ok(n) = dev.read_timeout(&mut buf, 20) else { break };
            if let Some(f) = mic_frame(&buf[..n]) {
                match &mut listening {
                    Some((_, frames)) => frames.extend_from_slice(f),
                    // A mic nobody here opened (left open by a crash): close it, once a second at most.
                    None if stray.elapsed() > Duration::from_secs(1) => {
                        stray = Instant::now();
                        let _ = dev.write(&toggle(false, 0));
                    }
                    None => {}
                }
                continue;
            }
            if n < 78 || buf[0] != 0x31 {
                continue;
            }
            let mute = buf[11] & MUTE != 0;
            // Steam started while listening (looked at twice a second): pause that one too, or stop at once.
            let steam = listening.is_some() && checked.elapsed() > Duration::from_millis(500) && {
                checked = Instant::now();
                !paused.steam()
            };
            match listening.take() {
                Some(_) if steam => {
                    let _ = dev.write(&toggle(false, 2));
                    paused.release();
                    *NOTE.lock().unwrap() = Some(STEAM);
                    play(OFF);
                }
                None if hold.long(mute, Instant::now(), HOLD) => {
                    play(LISTEN); // before the mic opens, so it does not hear the ding
                    // Steam is frozen from here until the mic is closed again, so it reads no mic frame.
                    if paused.steam() {
                        // shortcut: one open, no resend; a lost one ends in "Didn't quite catch that".
                        let _ = dev.write(&toggle(true, 1));
                        listening = Some((Instant::now(), Vec::new()));
                    } else {
                        paused.release();
                        *NOTE.lock().unwrap() = Some(STEAM);
                        play(OFF);
                    }
                }
                None => {}
                Some((since, frames)) if !mute || since.elapsed() > MAX_LISTEN => {
                    let _ = dev.write(&toggle(false, 2));
                    paused.release(); // before the command: "open Steam" must find a running Steam
                    if !mute {
                        hold = Hold::default(); // a new hold during the answer counts
                    }
                    for id in command(&frames) {
                        if id != POWER {
                            play(id);
                        } else if let Ok(h) = unsafe { FindWindowW(w!("WinSenseTray"), PCWSTR::null()) } {
                            // The tray's own "Turn controller off": its balloon says how it went.
                            let _ = unsafe { PostMessageW(Some(h), WM_COMMAND, WPARAM(crate::tray::ID_OFF), LPARAM(0)) };
                        }
                    }
                }
                keep => listening = keep,
            }
        }
    }
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
}

/// `winsense mic <secs> <file>` (test command): wait (up to 2 minutes) for Mute on the first
/// Bluetooth DualSense, keep the mic open while Mute is held (`secs` at most), close it, store
/// every mic frame in `file` (FRAME bytes each) and print what arrived.
pub fn capture(secs: u64, file: &str) -> Result<(), String> {
    if steam_running() {
        return Err("Steam is running: it reads the mic frames as button presses. Exit Steam first.".into());
    }
    let dev = bluetooth().ok_or("No DualSense is connected by Bluetooth.")?;
    let _ = dev.write(&toggle(false, 0)); // a silent audio report: switches Bluetooth to the full report, where Mute is seen
    let mut buf = [0u8; 100];
    let state = |buf: &[u8], n: usize| (n >= 78 && buf[0] == 0x31 && mic_frame(&buf[..n]).is_none()).then(|| buf[55]);
    println!("hold Mute and speak; let go to stop (waiting up to 2 minutes)");
    let (wait, status);
    wait = Instant::now();
    let mut waiting = 0u32;
    loop {
        if wait.elapsed() > Duration::from_secs(120) {
            return Err("Mute was not held within 2 minutes; nothing opened.".into());
        }
        let Ok(n) = dev.read_timeout(&mut buf, 100) else { continue };
        waiting += state(&buf, n).is_some() as u32;
        if let Some(st) = state(&buf, n)
            && buf[11] & MUTE != 0
        {
            status = st;
            break;
        }
    }
    // The mute flag the controller reported (data 53 bit 2, PadForge: frames are silent while set).
    println!("{:.0} state reports a second while waiting", waiting as f64 / wait.elapsed().as_secs_f64());
    println!("Mute held. Status byte {status:#04x} (bit 0 headphones, bit 1 mic plugged, bit 2 mic muted)");
    if steam_running() {
        return Err("Steam is running: it reads the mic frames as button presses. Exit Steam first.".into());
    }
    // shortcut: Ctrl-C here leaves the mic open (no handler in this test command); the tray's voice thread closes a stray mic, or turn the controller off and on.
    let close = Close(&dev);
    dev.write(&toggle(true, 1)).map_err(|e| format!("open failed: {e}"))?;
    let mut frames = Vec::new();
    let (mut states, mut first, start) = (0u32, None, Instant::now());
    while start.elapsed() < Duration::from_secs(secs) {
        let Ok(n) = dev.read_timeout(&mut buf, 50) else { continue };
        match mic_frame(&buf[..n]) {
            Some(f) => {
                first.get_or_insert(start.elapsed());
                frames.extend_from_slice(f);
            }
            None if state(&buf, n).is_some() => {
                states += 1;
                if buf[11] & MUTE == 0 {
                    break; // let go
                }
            }
            None => {}
        }
    }
    let held = start.elapsed().as_secs_f64();
    drop(close);
    println!("held {held:.1} s: {} mic frames (first at {first:?}), {states} state reports ({:.0} a second)", frames.len() / FRAME, states as f64 / held);
    std::fs::write(file, &frames).map_err(|e| e.to_string())?;
    println!("wrote {file} ({} bytes)", frames.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_frames_are_told_apart() {
        let mut r = [0u8; 78];
        r[0] = 0x31;
        r[1] = 0x11; // an ordinary state report (a real controller)
        assert_eq!(mic_frame(&r), None);
        r[1] = 0x12;
        r[2] = 7;
        r[3] = 0xD4;
        assert_eq!(mic_frame(&r).map(|f| (f.len(), f[0], f[1])), Some((FRAME, 7, 0xD4)));
    }

    /// A paused program does not run, and runs again when let go (a hidden `ping`, about 1 s of work).
    #[test]
    fn a_paused_program_waits() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("ping").args(["-n", "2", "127.0.0.1"]).stdout(std::process::Stdio::null()).creation_flags(0x0800_0000).spawn().unwrap();
        let mut p = Paused::default();
        assert!(p.add(&[child.id()]) && p.add(&[child.id()]), "pausing twice is one pause");
        assert_eq!(p.0.len(), 1);
        std::thread::sleep(Duration::from_millis(2500));
        assert!(child.try_wait().unwrap().is_none(), "still paused after its work would have ended");
        drop(p);
        assert!(child.wait().unwrap().success());
        assert!(!Paused::default().add(&[0xFFFF_FFF0]), "a program that is not there: refused");
    }

    #[test]
    fn a_hold_fires_once() {
        let (mut h, t) = (Hold::default(), Instant::now());
        let ms = Duration::from_millis;
        assert!(!h.long(true, t, HOLD));
        assert!(!h.long(true, t + ms(399), HOLD), "a tap is nothing");
        assert!(h.long(true, t + ms(400), HOLD));
        assert!(!h.long(true, t + ms(900), HOLD), "once per hold");
        assert!(!h.long(false, t + ms(901), HOLD));
        assert!(!h.long(true, t + ms(902), HOLD), "a new hold starts over");
        assert!(h.long(true, t + ms(1302), HOLD));
    }

    #[test]
    fn voice_only_opens_apps() {
        let say = |name, parsing| speakable(name, parsing);
        assert_eq!(say("Retro Shelf", r"C:\Games\RetroShelf\RetroShelf.exe"), Some("retro shelf".into()));
        assert_eq!(say("Papa's Pizzeria Deluxe", "steam://rungameid/3259470"), Some("papa s pizzeria deluxe".into()));
        assert_eq!(say("Calculator", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"), Some("calculator".into()));
        assert_eq!(say("Google Chrome", "Chrome"), Some("google chrome".into()));
        // Real Start menu entries that must stay out.
        for (name, parsing) in [
            ("Command Prompt", r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\cmd.exe"),
            ("Anaconda Prompt", "anaconda.CmdExe.miniconda3.base"),
            ("Anaconda PowerShell Prompt", "anaconda.PowerShell.miniconda3.base"),
            ("Developer PowerShell for VS", "Microsoft.AutoGenerated.{DC5B255D-3AB2-E11C-F3A1-5D6643F869B9}"),
            ("Git CMD", "Microsoft.AutoGenerated.{29591C0F-7C09-B655-C0EB-8544FCA7D1DE}"),
            ("Uninstall Remote Mouse", r"D:\Remote Mouse\unins000.exe"),
            ("Désinstaller", r"D:\Games\Archipelago Setup Tool\Uninstall.exe"),
            ("Steam Support Center", "http://support.steampowered.com/"),
            ("Registry Editor", r"C:\Windows\regedit.exe"),
            ("Terminal", "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"),
            ("Run", "Microsoft.Windows.Shell.RunDialog"),
            ("Windows PowerShell", r"{D65231B0-B2F1-4857-A4CE-A8E7C6EA7D27}\WindowsPowerShell\v1.0\powershell.exe"),
            ("Some Manual", r"C:\Program Files\X\manual.pdf"),
            ("™", "x.exe"),
            ("WSL", "Microsoft.WSL"),
            ("Node.js", r"D:\NodeJS\node.exe"),
            ("Python 3.14", r"C:\Users\x\AppData\Local\Programs\Python\Python314\python.exe"),
            ("System Configuration", r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\msconfig.exe"),
            ("Control Panel", "Microsoft.Windows.ControlPanel"),
            ("Windows Tools", "Microsoft.Windows.AdministrativeTools"),
            ("Voice access", r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\voiceaccess.exe"),
            ("Visual Studio Installer", "Microsoft.VisualStudio.Installer"),
            ("Unload kernel module", r"C:\Program Files\Cheat Engine\Kernelmoduleunloader.exe"),
            ("My Folder", r"D:\Projects\My Folder"),
            ("Some Script", r"D:\x\run.py"),
        ] {
            assert_eq!(say(name, parsing), None, "{name}");
        }
    }

    #[test]
    fn phrases_become_actions() {
        let pages = areas("library\tLibrary\r\nrecomps\tFind recomps\nbad id\tX\n../x\tY\nempty\t!!\nno tab\n");
        assert_eq!(pages, [("library".to_string(), "library".to_string()), ("find recomps".to_string(), "recomps".to_string())], "only clean ids with a sayable title");
        let all = phrases(&["retro shelf".to_string()], &pages);
        assert_eq!(all.len(), 4 + 24 + 2 + FIXED.len());
        assert_eq!(said("go to find recomps in retro shelf"), Some(Said::Area("find recomps")));
        assert_eq!(said("go to downloads in retro shelf"), Some(Said::Area("downloads")), "a page nobody listed: found in no list, so nothing runs");
        assert_eq!(shelf("RetroShelf.exe", &["--pages"], true), None, "never a name the PATH could resolve");
        assert!(all.iter().all(|p| said(p).is_some()));
        assert_eq!(said("open retro shelf"), Some(Said::Open("retro shelf")));
        assert_eq!(said("focus retro shelf"), Some(Said::Focus("retro shelf")));
        assert_eq!(said("switch to retro shelf"), Some(Said::Focus("retro shelf")));
        assert_eq!(said("close retro shelf"), Some(Said::Close("retro shelf")));
        for p in ["go to downloads", "go to my downloads", "go to downloads folder", "go to my downloads folder"] {
            assert_eq!(said(p), Some(Said::Folder("downloads")), "{p}");
        }
        assert_eq!(said("delete retro shelf"), None);
        // Which window is the app's.
        assert!(window_of("Untitled - Notepad", r"C:\Windows\notepad.exe", "notepad", "Microsoft.WindowsNotepad_8wekyb3d8bbwe!App"));
        assert!(window_of("Library", r"C:\Games\RetroShelf\RetroShelf.exe", "retro shelf", r"C:\Games\RetroShelf\RetroShelf.exe"));
        assert!(!window_of("Notepads and pens - Google Chrome", r"C:\x\chrome.exe", "notepad", "Microsoft.WindowsNotepad_8wekyb3d8bbwe!App"), "whole words only");
        assert!(!window_of("Downloads", r"C:\Windows\explorer.exe", "steam", "steam://rungameid/1"));
    }

    /// The commands of 0.13.0 as data: nothing here presses a key, changes the volume or touches a controller.
    #[test]
    fn fixed_commands_become_actions() {
        for (phrase, want) in [
            ("volume up", Said::Volume(true)),
            ("quieter", Said::Volume(false)),
            ("mute", Said::Mute(true)),
            ("unmute", Said::Mute(false)),
            ("pause", Said::Key(0xB3)),
            ("play", Said::Key(0xB3)),
            ("skip", Said::Key(0xB0)),
            ("previous", Said::Key(0xB1)),
            ("how much battery", Said::Battery),
            ("turn off controller", Said::Power),
            ("touchpad off", Said::Touch(false)),
            ("turn on gyro", Said::Gyro(true)),
            ("couch mode on", Said::Couch(true)),
            ("turn off couch mode", Said::Couch(false)),
        ] {
            assert_eq!(said(phrase), Some(want), "{phrase}");
        }
        assert_eq!(said("volume"), None);
        assert_eq!(said("open play"), Some(Said::Open("play")), "an app's name is still an app");
        assert!(FIXED.iter().enumerate().all(|(i, f)| FIXED[..i].iter().all(|g| g.0 != f.0)), "no phrase twice");
        assert_eq!((step(0.5, true), step(0.95, true), step(0.05, false)), (0.6, 1.0, 0.0));
        // Battery in words: (level 0-10, state) as the tray shows it.
        assert_eq!(charge(None), [SORRY], "not known yet");
        assert_eq!(charge(Some((10, 0))), [BAT_FULL]);
        assert_eq!(charge(Some((8, 2))), [BAT_FULL], "the charger says full");
        assert_eq!(charge(Some((7, 0))), [BAT_HIGH]);
        assert_eq!(charge(Some((6, 0))), [BAT_HALF]);
        assert_eq!(charge(Some((4, 1))), [BAT_HALF, CHARGING]);
        assert_eq!(charge(Some((3, 0))), [BAT_LOW]);
        assert_eq!(charge(Some((0, 1))), [BAT_LOW, CHARGING]);
        assert!([OKAY, BAT_FULL, BAT_HIGH, BAT_HALF, BAT_LOW, CHARGING].iter().all(|&c| clip(c) != clip(SORRY) && clip(c).len() % 200 == 0));
    }

    /// Windows-voice recordings of the new phrases, shaped like the controller's mic, against this
    /// PC's real grammar: each comes out as its command, and ordinary speech holding the same
    /// words is refused. (Single near-rhymes such as "plate" for "play" ARE taken.)
    #[test]
    fn new_phrases_are_heard_and_speech_is_refused() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).unwrap();
        }
        let names: Vec<String> = apps().unwrap().into_iter().map(|a| a.0).collect();
        let all = phrases(&names, &[]);
        let taken = |frames: &[u8]| hear(&decode(frames).unwrap(), &all).unwrap().filter(|h| h.1 >= CONFIDENCE).map(|h| h.0);
        let heard: [(&[u8], Said); 9] = [
            (include_bytes!("../assets/test_volume_up.frames"), Said::Volume(true)), // "Computer, volume up."
            (include_bytes!("../assets/test_mute.frames"), Said::Mute(true)),
            (include_bytes!("../assets/test_next.frames"), Said::Key(NEXT)),
            (include_bytes!("../assets/test_play.frames"), Said::Key(PLAY_PAUSE)),
            (include_bytes!("../assets/test_whats_battery.frames"), Said::Battery),
            (include_bytes!("../assets/test_controller_off.frames"), Said::Power),
            (include_bytes!("../assets/test_gyro_on.frames"), Said::Gyro(true)),
            (include_bytes!("../assets/test_turn_on_mouse.frames"), Said::Touch(true)),
            (include_bytes!("../assets/test_couch_on.frames"), Said::Couch(true)), // "Computer, couch mode on."
        ];
        for (frames, want) in heard {
            assert_eq!(taken(frames).as_deref().and_then(said), Some(want));
        }
        let refused: [(&[u8], &str); 6] = [
            (include_bytes!("../assets/test_store.frames"), "I think we should go to the store later."),
            (include_bytes!("../assets/test_pausegame.frames"), "Can you pause the game for a second?"),
            (include_bytes!("../assets/test_greatplay.frames"), "That was a great play by the team."),
            (include_bytes!("../assets/test_playagain.frames"), "Play it again next week."),
            (include_bytes!("../assets/test_cute.frames"), "Cute."),
            (include_bytes!("../assets/test_comfy.frames"), "The couch is so comfy."),
        ];
        for (frames, text) in refused {
            assert_eq!(taken(frames), None, "{text}");
        }
        // The volume interface is there (read only: nothing is changed).
        let level = endpoint().and_then(|v| unsafe { v.GetMasterVolumeLevelScalar() }).unwrap();
        assert!((0.0..=1.0).contains(&level));
    }

    /// Prints what the recogniser makes of every *.frames file in the folder `WINSENSE_FRAMES`,
    /// against the real grammar of this PC (how the confidence numbers were measured):
    /// `cargo test measure -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).unwrap();
        }
        let names: Vec<String> = apps().unwrap().into_iter().map(|a| a.0).collect();
        let all = phrases(&names, &[]);
        let mut files: Vec<_> = std::fs::read_dir(std::env::var("WINSENSE_FRAMES").unwrap()).unwrap().map(|f| f.unwrap().path()).collect();
        files.sort();
        for f in files {
            let heard = hear(&decode(&std::fs::read(&f).unwrap()).unwrap(), &all).unwrap();
            println!("{:<24} {}", f.file_stem().unwrap().to_string_lossy(), heard.map_or("nothing".into(), |(p, c)| format!("{c:.2} {} \"{p}\"", if c >= CONFIDENCE { "TAKEN  " } else { "refused" })));
        }
    }

    /// The whole listening path on this PC's own engines: controller-shaped mic frames (a Windows
    /// voice saying "Computer, open notepad.", encoded like the controller's mic: 71-byte Opus,
    /// 10 ms) through the decoder and the recogniser.
    #[test]
    fn frames_become_an_app_name() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).unwrap();
        }
        // Real voice recordings are not shipped: put test_notepad/plain/jarvis.frames (`winsense mic`) in assets/ to run this.
        let read = |n: &str| std::fs::read(format!("{}/assets/test_{n}.frames", env!("CARGO_MANIFEST_DIR")));
        let (Ok(notepad), Ok(plain), Ok(jarvis)) = (read("notepad"), read("plain"), read("jarvis")) else {
            eprintln!("skipped: no voice recordings in assets/");
            return;
        };
        let pcm = decode(&notepad).unwrap();
        assert!((80_000..100_000).contains(&pcm.len()), "2.8 s at 16 kHz: {}", pcm.len());
        let names = ["steam", "notepad", "retro shelf"].map(String::from);
        // The same command with no name in front, and with another name.
        for other in [&plain, &jarvis] {
            let (phrase, confidence) = hear(&decode(other).unwrap(), &phrases(&names, &[])).unwrap().expect("heard");
            assert_eq!((said(&phrase), confidence >= CONFIDENCE), (Some(Said::Open("notepad")), true), "{phrase} {confidence}");
        }
        let (phrase, confidence) = hear(&pcm, &phrases(&names, &[])).unwrap().expect("heard");
        assert_eq!(said(&phrase), Some(Said::Open("notepad")));
        assert!(confidence >= CONFIDENCE, "{confidence}");
        assert_eq!(hear(&pcm, &phrases(&["steam".to_string(), "calculator".to_string()], &[])).unwrap().filter(|h| h.1 >= CONFIDENCE), None, "not in the list: refused");
        assert!([DING, DING2, OFF, LISTEN, OPENING, SORRY, SWITCHING, CLOSING].iter().all(|&c| !clip(c).is_empty() && clip(c).len() % 200 == 0));
        let list = apps().unwrap();
        assert!(list.len() > 20 && list.iter().all(|a| !a.1.to_lowercase().contains("cmd.exe")), "{} apps", list.len());
    }
}
