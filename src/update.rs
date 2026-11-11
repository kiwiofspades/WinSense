//! Updates from GitHub releases. The tray asks the releases API at start and then at most once a
//! day; a newer release shows a balloon, and only a "Yes" in the dialog that follows downloads it.
//! The download must match the SHA-256 published next to it in the same release, then the running
//! exe is renamed to `winsense.exe.old` (Windows allows renaming a running exe), the new one takes
//! its name and starts with `--after <pid>`, and this tray quits. Nothing is installed silently and
//! nothing runs but that checked exe. https only, to GitHub's own hosts (`HOSTS`), redirects
//! checked one by one.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
use windows_sys::w;

pub const REPO: &str = "kiwiofspades/WinSense";
/// The exe and its checksum file, as attached to every release.
const EXE: &str = "winsense.exe";
const SUM: &str = "winsense.exe.sha256";
/// The only hosts a request or a redirect may go to.
const HOSTS: [&str; 4] = ["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
/// The largest download taken (the exe is about 1.5 MB).
const LIMIT: usize = 32 << 20;

/// A release newer than this build.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    exe: String,
    sum: String,
}

/// "v0.17.0" or "0.17.0" as numbers; None for anything else (a pre-release tag such as 0.18.0-beta).
fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut p = v.strip_prefix('v').unwrap_or(v).split('.').map(|n| n.parse().ok());
    let r = (p.next()??, p.next()??, p.next()??);
    p.next().is_none().then_some(r)
}

/// True when `latest` is a higher version than `current`.
pub fn newer(current: &str, latest: &str) -> bool {
    matches!((parse(current), parse(latest)), (Some(c), Some(l)) if l > c)
}

/// The string value of every `"key": "value"` in a JSON text, in order. Enough for the fields
/// read here (tag and download URLs never hold escaped quotes).
fn values<'a>(json: &'a str, key: &str) -> Vec<&'a str> {
    let pat = format!("\"{key}\"");
    json.match_indices(&pat)
        .filter_map(|(i, _)| {
            let rest = json[i + pat.len()..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            Some(&rest[..rest.find('"')?])
        })
        .collect()
}

/// The release in a `releases/latest` answer, if it is newer than `current` and carries both files.
fn release(json: &str, current: &str) -> Option<Release> {
    let tag = *values(json, "tag_name").first()?;
    if !newer(current, tag) {
        return None;
    }
    let urls = values(json, "browser_download_url");
    let file = |name: &str| urls.iter().find(|u| u.rsplit('/').next() == Some(name)).map(|u| u.to_string());
    Some(Release { version: tag.trim_start_matches('v').into(), exe: file(EXE)?, sum: file(SUM)? })
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, null(), 0, data.as_ptr(), data.len() as u32, out.as_mut_ptr(), 32) };
    assert_eq!(status, 0, "BCryptHash");
    out
}

/// The hash a `.sha256` file names (`<64 hex digits>  winsense.exe`, or the digits alone).
fn published(sum: &str) -> Option<[u8; 32]> {
    let hex = sum.split_whitespace().next()?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Host and path of an https URL to one of `HOSTS`; None for anything else.
fn split(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    HOSTS.contains(&host.to_ascii_lowercase().as_str()).then_some((host, path))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Closes a WinHTTP handle when dropped.
struct H(*mut core::ffi::c_void);
impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what} failed (error {})", unsafe { windows_sys::Win32::Foundation::GetLastError() })
}

/// GET an https URL, following up to 5 redirects that stay on `HOSTS`.
fn get(url: &str) -> Result<Vec<u8>, String> {
    let agent = wide(&format!("WinSense/{}", env!("CARGO_PKG_VERSION")));
    let session = H(unsafe { WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0) });
    if session.0.is_null() {
        return Err(last_error("WinHttpOpen"));
    }
    unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000) };
    let mut url = url.to_string();
    for _ in 0..6 {
        let (host, path) = split(&url).ok_or_else(|| format!("refused to go to {url}"))?;
        let (host, path) = (wide(host), wide(path));
        let conn = H(unsafe { WinHttpConnect(session.0, host.as_ptr(), 443, 0) });
        if conn.0.is_null() {
            return Err(last_error("connect"));
        }
        let req = H(unsafe { WinHttpOpenRequest(conn.0, w!("GET"), path.as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE) });
        if req.0.is_null() {
            return Err(last_error("request"));
        }
        let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        let headers = wide("Accept: application/vnd.github+json, application/octet-stream\r\n");
        let (mut status, mut size) = (0u32, 4u32);
        let ok = unsafe {
            WinHttpSetOption(req.0, WINHTTP_OPTION_REDIRECT_POLICY, &never as *const u32 as _, 4) != 0
                && WinHttpSendRequest(req.0, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) != 0
                && WinHttpReceiveResponse(req.0, null_mut()) != 0
                && WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, null(), &mut status as *mut u32 as _, &mut size, null_mut()) != 0
        };
        if !ok {
            return Err(last_error("download"));
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let mut buf = vec![0u16; 4096];
            let mut len = (buf.len() * 2) as u32;
            if unsafe { WinHttpQueryHeaders(req.0, WINHTTP_QUERY_LOCATION, null(), buf.as_mut_ptr() as _, &mut len, null_mut()) } == 0 {
                return Err(last_error("redirect"));
            }
            url = String::from_utf16_lossy(&buf[..len as usize / 2]);
            continue;
        }
        if status != 200 {
            return Err(format!("GitHub answered {status}"));
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let mut n = 0u32;
            if unsafe { WinHttpReadData(req.0, chunk.as_mut_ptr() as _, chunk.len() as u32, &mut n) } == 0 {
                return Err(last_error("read"));
            }
            if n == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&chunk[..n as usize]);
            if body.len() > LIMIT {
                return Err("download too large".into());
            }
        }
    }
    Err("too many redirects".into())
}

/// Ask GitHub for the latest release: Some when it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let body = get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?;
    Ok(release(&String::from_utf8_lossy(&body), env!("CARGO_PKG_VERSION")))
}

/// Download the release's exe, check it against its published SHA-256, and put it in place of the
/// running exe (which becomes `winsense.exe.old`). Then start it with `args` + `--after <pid>`;
/// the caller quits. On any failure the running exe is left as it was.
pub fn install(r: &Release, args: &[&str]) -> Result<(), String> {
    let want = published(&String::from_utf8_lossy(&get(&r.sum)?)).ok_or("the release's checksum file is not a SHA-256")?;
    let exe = get(&r.exe)?;
    if sha256(&exe) != want {
        return Err("the download does not match the release's checksum; nothing was changed".into());
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let (new, old) = (me.with_extension("exe.new"), me.with_extension("exe.old"));
    std::fs::write(&new, &exe).map_err(|e| format!("could not save the download: {e}"))?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&me, &old).map_err(|e| format!("could not move the running exe aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &me) {
        let _ = std::fs::rename(&old, &me);
        return Err(format!("could not put the new exe in place: {e}"));
    }
    let pid = std::process::id().to_string();
    if let Err(e) = std::process::Command::new(&me).args(args).args(["--after", &pid]).spawn() {
        let _ = std::fs::rename(&me, &new);
        let _ = std::fs::rename(&old, &me);
        return Err(format!("could not start the new version: {e}"));
    }
    Ok(())
}

/// The exe an earlier update moved aside; it can go once the new version runs.
pub fn tidy() {
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::remove_file(me.with_extension("exe.old"));
    }
}

/// Wait (10 s at most) for the tray that started this one to end and let go of its lock.
pub fn after(pid: &str) {
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let Ok(pid) = pid.parse() else { return };
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 10_000);
            windows_sys::Win32::Foundation::CloseHandle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(newer("0.16.1", "v0.17.0"));
        assert!(newer("0.16.9", "0.17.0"));
        assert!(newer("0.9.0", "0.10.0"), "not as text");
        assert!(newer("0.17.0", "1.0.0"));
        assert!(!newer("0.17.0", "v0.17.0"));
        assert!(!newer("0.17.0", "0.16.10"));
        assert!(!newer("0.17.0", "0.18.0-beta"), "pre-releases are not offered");
        assert!(!newer("0.17.0", "latest"));
        assert!(!newer("0.17.0", "0.18"));
    }

    #[test]
    fn a_release_needs_a_newer_tag_and_both_files() {
        let json = r#"{"url":"x","tag_name": "v0.17.0","name":"WinSense 0.17.0","assets":[
            {"name":"winsense.exe","browser_download_url":"https://github.com/kiwiofspades/WinSense/releases/download/v0.17.0/winsense.exe"},
            {"name":"winsense.exe.sha256","browser_download_url":"https://github.com/kiwiofspades/WinSense/releases/download/v0.17.0/winsense.exe.sha256"}]}"#;
        let r = release(json, "0.16.9").unwrap();
        assert_eq!(r.version, "0.17.0");
        assert!(r.exe.ends_with("/winsense.exe") && r.sum.ends_with("/winsense.exe.sha256"));
        assert_eq!(release(json, "0.17.0"), None, "same version");
        assert_eq!(release(&json.replace("winsense.exe.sha256", "notes.txt"), "0.16.9"), None, "no checksum, no offer");
        assert_eq!(release("{\"message\":\"Not Found\"}", "0.16.9"), None);
    }

    #[test]
    fn sha256_and_its_file() {
        let abc = sha256(b"abc");
        let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(published(want), Some(abc));
        assert_eq!(published(&format!("{}  winsense.exe\n", want.to_uppercase())), Some(abc));
        assert_eq!(published("abc  winsense.exe"), None);
        assert_eq!(published(""), None);
    }

    #[test]
    fn only_https_to_github() {
        assert_eq!(split("https://api.github.com/repos/a/b/releases/latest"), Some(("api.github.com", "/repos/a/b/releases/latest")));
        assert!(split("https://objects.githubusercontent.com/x?y=1").is_some());
        assert!(split("https://release-assets.githubusercontent.com/x").is_some());
        assert_eq!(split("http://github.com/x"), None, "not https");
        assert_eq!(split("https://github.com.evil.example/x"), None);
        assert_eq!(split("https://evil.example/github.com/x"), None);
        assert_eq!(split("https://user@github.com/x"), None);
    }
}
