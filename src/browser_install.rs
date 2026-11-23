//! `install-browser`: download Chrome for Testing (Google's Chrome build for automation) and use
//! it instead of the user's own Chrome. Same engine, but its own app: on macOS a hidden Google
//! Chrome takes over the Chrome app, so clicking Chrome in the Dock opens nothing.

use crate::R;
use crate::state;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// Chrome for Testing 158.0.8127.36 stalls even raw CDP navigation on the
// supported Mac. Deliberately provision the last version verified end to end.
const VERSION: &str = "155.0.8059.39";

fn platform() -> R<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        (os, arch) => Err(format!("Chrome for Testing has no build for {os}/{arch}; set chrome_path instead").into()),
    }
}

fn dir() -> PathBuf {
    state::base_dir().join("chrome-for-testing")
}

/// The installed Chrome for Testing executable, if there is one.
pub fn installed() -> Option<PathBuf> {
    let p = platform().ok()?;
    let base = dir().join(format!("chrome-{p}"));
    let exe =
        if p.starts_with("mac") { base.join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing") } else { base.join("chrome") };
    exe.is_file().then_some(exe)
}

fn run(cmd: &mut Command) -> R<Vec<u8>> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| format!("{name}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{name} failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(out.stdout)
}

/// Provision the known-compatible build while the dedicated browser is stopped.
pub fn install() -> R<String> {
    let _lock = state::lock_install()?;
    let p = platform()?;
    if installed().is_some() && fs::read_to_string(dir().join("version")).ok().as_deref() == Some(VERSION) {
        return Ok(VERSION.into());
    }
    let active = |dir: &std::path::Path| fs::symlink_metadata(dir.join("profile/SingletonLock")).is_ok();
    if active(&state::base_dir()) {
        return Err("stop all herdr-browser profiles and close login windows before installing Chrome for Testing".into());
    }
    match fs::read_dir(state::base_dir().join("profiles")) {
        Ok(entries) => for entry in entries {
            if active(&entry?.path()) {
                return Err("stop all herdr-browser profiles and close login windows before installing Chrome for Testing".into());
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let url = format!("https://storage.googleapis.com/chrome-for-testing-public/{VERSION}/{p}/chrome-{p}.zip");
    eprintln!("downloading Chrome for Testing {VERSION} (about 200 MB)…");
    let tmp = state::base_dir().join("chrome-for-testing.download");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let zip = tmp.join("chrome.zip");
    run(Command::new("curl").args(["-fsSL", "--max-time", "900", "--retry", "2", "-o"]).arg(&zip).arg(&url))?;
    run(Command::new("unzip").args(["-q", "-o"]).arg(&zip).arg("-d").arg(&tmp))?;
    fs::remove_file(&zip)?;
    fs::write(tmp.join("version"), VERSION)?;
    let _ = fs::remove_dir_all(dir());
    fs::rename(&tmp, dir())?;
    Ok(VERSION.into())
}
