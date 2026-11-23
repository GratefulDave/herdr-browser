//! Lifecycle of the dedicated headless Chrome profile.
use crate::cdp::Cdp;
use crate::state::{self, Config};
use crate::R;
use serde_json::json;
use std::fs;
use std::net::TcpStream;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub fn find_chrome(cfg: &Config) -> R<PathBuf> {
    if let Some(path) = &cfg.chrome_path {
        return if path.is_file() { Ok(path.clone()) } else { Err(format!("chrome_path {} does not exist", path.display()).into()) };
    }
    crate::browser_install::installed().ok_or_else(|| "Chrome for Testing is not installed; run `herdr-browser install-browser`".into())
}

pub fn parse_port_file(s: &str) -> Option<(u16, String)> {
    let mut lines = s.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/").then(|| (port, path.to_string()))
}

pub fn endpoint() -> Option<String> {
    let s = fs::read_to_string(state::profile_dir().join("DevToolsActivePort")).ok()?;
    let (port, path) = parse_port_file(&s)?;
    let addr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    let url = format!("ws://127.0.0.1:{port}{path}");
    Cdp::connect(&url).ok()?;
    Some(url)
}

pub fn ensure(cfg: &Config) -> R<String> {
    let _install = state::lock_install()?;
    let _login = state::login_guard()?;
    let _lock = state::lock_named("chrome")?;
    if let Some(url) = endpoint() {
        return Ok(url);
    }
    if fs::symlink_metadata(state::profile_dir().join("SingletonLock")).is_ok() {
        return Err("profile is already running; stop headless Chrome or close the login window first".into());
    }
    let chrome = find_chrome(cfg)?;
    let profile = state::profile_dir();
    fs::create_dir_all(&profile)?;
    let _ = fs::remove_file(profile.join("DevToolsActivePort"));
    let log = fs::File::options().create(true).append(true).open(state::state_dir().join("chrome.log"))?;
    let mut command = Command::new(&chrome);
    // Keep credentials in the dedicated profile, not in the account's keychain.
    if cfg!(target_os = "macos") {
        command.arg("--use-mock-keychain");
    } else {
        command.arg("--password-store=basic");
    }
    let mut child = command
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--remote-debugging-port=0", "--remote-debugging-address=127.0.0.1", "--enable-unsafe-extension-debugging",
            "--no-first-run", "--no-default-browser-check", "--hide-crash-restore-bubble",
            "--disable-backgrounding-occluded-windows", "--headless=new", "about:blank"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(log).process_group(0)
        .spawn().map_err(|e| format!("launch {}: {e}", chrome.display()))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let url = loop {
        if let Some(url) = endpoint() { break url; }
        if let Some(status) = child.try_wait()? {
            return Err(format!("Chrome exited ({status}); see {}/chrome.log", state::state_dir().display()).into());
        }
        if Instant::now() > deadline {
            return Err(format!("Chrome did not start; see {}/chrome.log", state::state_dir().display()).into());
        }
        sleep(Duration::from_millis(100));
    };
    let ext = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?)
        .join(".local/share/herdr-browser/extension");
    let loaded = Cdp::connect(&url).and_then(|mut c| c.call("Extensions.loadUnpacked", json!({ "path": ext }), None));
    if let Err(e) = loaded {
        eprintln!("herdr-browser: diagnostics recorder unavailable ({e}); console and network are not recording");
    }
    Ok(url)
}

pub fn stop() -> R<bool> {
    let _lock = state::lock_named("chrome")?;
    let _login = state::login_guard()?;
    let Some(url) = endpoint() else { return Ok(false) };
    let mut c = Cdp::connect(&url)?;
    let _ = c.call("Browser.close", json!({}), None);
    let deadline = Instant::now() + Duration::from_secs(15);
    while fs::symlink_metadata(state::profile_dir().join("SingletonLock")).is_ok() || endpoint().is_some() {
        if Instant::now() > deadline { return Err("Chrome did not exit within 15s".into()); }
        sleep(Duration::from_millis(100));
    }
    Ok(true)
}

/// Run a manually controlled window without enabling CDP or the recorder.
pub fn login(cfg: &Config, url: Option<&str>) -> R<()> {
    let url = crate::page::normalize_url(url.unwrap_or("about:blank"));
    if url != "about:blank" {
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))
            .ok_or("login URL must be HTTP(S) or about:blank")?;
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
            || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("invalid login URL".into());
        }
    }
    let install = state::lock_install()?;
    let _startup = state::lock_named("chrome")?;
    let _login = state::login_guard()?;
    if endpoint().is_some() || fs::symlink_metadata(state::profile_dir().join("SingletonLock")).is_ok() {
        return Err("profile is already running; stop headless Chrome or close the login window first".into());
    }
    let chrome = find_chrome(cfg)?;
    let profile = state::profile_dir();
    fs::create_dir_all(&profile)?;
    let log_path = state::state_dir().join("login-chrome.log");
    let log = fs::OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(&log_path)
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    log.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut command = Command::new(&chrome);
    if cfg!(target_os = "macos") {
        command.arg("--use-mock-keychain");
    } else {
        command.arg("--password-store=basic");
    }
    let mut child = command.arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--hide-crash-restore-bubble", "--disable-extensions"])
        .arg(&url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log)
        .spawn().map_err(|e| format!("launch {}: {e}; see {}", chrome.display(), log_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs::symlink_metadata(profile.join("SingletonLock")).is_err() {
        if let Some(status) = child.try_wait()? {
            return Err(format!("login browser exited before opening ({status}); see {}", log_path.display()).into());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("login browser did not start within 20s; see {}", log_path.display()).into());
        }
        sleep(Duration::from_millis(100));
    }
    drop(_startup);
    drop(install);
    eprintln!("Manual login window opened; close it before using this profile headlessly.");
    let status = child.wait()?;
    if !status.success() { return Err(format!("login browser exited ({status}); see {}", log_path.display()).into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_devtools_active_port() {
        assert_eq!(parse_port_file("57047\n/devtools/browser/abc-123\n"), Some((57047, "/devtools/browser/abc-123".into())));
    }
    #[test]
    fn rejects_truncated_port_file() {
        assert_eq!(parse_port_file("57047\n"), None);
        assert_eq!(parse_port_file("nope\n/devtools/browser/x"), None);
    }
}
