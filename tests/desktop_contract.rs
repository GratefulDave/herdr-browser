//! CLI ownership tests: only hc and herdr-app are replaced, never browser methods.
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output}, sync::atomic::{AtomicUsize, Ordering}};
use serde_json::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture { root: PathBuf }
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("hb-contract-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(root.join("bin")).unwrap();
        for name in ["hc", "herdr-app", "herdr"] {
            let path = root.join("bin").join(name);
            fs::write(&path, STUB).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { root }
    }
    fn run(&self, scenario: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-browser")).args(args)
            .env("HOME", &self.root).env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HERDR_BIN_PATH", self.root.join("bin/herdr"))
            .env("HC_TERMINAL_ID", "t1").env("HERDR_PANE_ID", "w99:p99")
            .env("FIXTURE_ROOT", &self.root).env("SCENARIO", scenario).output().unwrap()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("calls")).unwrap_or_default().lines().map(|s| serde_json::from_str(s).unwrap()).collect()
    }
    fn assert_no_legacy(&self) { assert!(!self.calls().iter().any(|v| v[0] == "herdr"), "legacy herdr invoked: {:?}", self.calls()); }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }
fn error(out: &Output) -> String { format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)) }

#[test]
fn desktop_unavailable_never_falls_back_to_legacy() {
    let f = Fixture::new();
    let out = f.run("down", &["show"]);
    assert!(!out.status.success(), "{}", error(&out));
    assert!(error(&out).contains("display failed for t1"), "{}", error(&out));
    assert!(f.calls().iter().any(|c| c[0] == "hc" && c[1] == "get" && c[2] == "t1"), "{:?}", f.calls());
    assert!(f.calls().iter().any(|c| c[0] == "herdr-app" && c[1] == "ls"), "{:?}", f.calls());
    f.assert_no_legacy();
}

#[test]
fn stale_and_ambiguous_owners_never_split_or_send() {
    for scenario in ["stale", "ambiguous"] {
        let f = Fixture::new();
        let out = f.run(scenario, &["--owner", "t2", "show"]);
        assert!(!out.status.success(), "{scenario}: {}", error(&out));
        assert!(f.calls().iter().any(|c| c[0] == "hc" && c[1] == "get" && c[2] == "t2"), "{:?}", f.calls());
        assert!(!f.calls().iter().any(|c| (c[0] == "herdr-app" && c[1] == "do") || (c[0] == "hc" && c[1] == "send")), "{:?}", f.calls());
        f.assert_no_legacy();
    }
}

#[test]
fn blocked_recipient_is_checked_before_page_and_never_sent() {
    let f = Fixture::new();
    let out = f.run("blocked", &["--owner", "t2", "send", "selection"]);
    assert!(!out.status.success(), "{}", error(&out));
    assert!(error(&out).contains("agent t2 is blocked; delivery refused"), "{}", error(&out));
    assert!(f.calls().iter().any(|c| c[0] == "hc" && c[1] == "get" && c[2] == "t2"), "{:?}", f.calls());
    assert!(!f.calls().iter().any(|c| c[0] == "hc" && c[1] == "send"), "{:?}", f.calls());
    f.assert_no_legacy();
}

#[test]
fn show_reuses_only_owners_split_and_uses_explicit_panes() {
    let f = Fixture::new();
    for args in [&["show"][..], &["show"][..], &["--owner", "t2", "show"][..], &["--owner", "t2", "show"][..]] {
        let out = f.run("normal", args);
        assert!(out.status.success(), "{args:?}: {}", error(&out));
    }
    let calls = f.calls();
    let splits: Vec<_> = calls.iter().filter(|c| c[0] == "herdr-app" && c[1] == "do" && c[2] == "pane.split").collect();
    assert_eq!(splits.len(), 2, "{calls:?}");
    for (call, pane, owner) in [(splits[0], "p1", "t1"), (splits[1], "p2", "t2")] {
        let a = call.as_array().unwrap();
        let pane_i = a.iter().position(|v| v == "--pane").unwrap();
        assert_eq!(a[pane_i + 1], pane);
        let argv_i = a.iter().position(|v| v == "--argv").unwrap();
        let argv: Vec<String> = serde_json::from_str(a[argv_i + 1].as_str().unwrap()).unwrap();
        assert!(std::path::Path::new(&argv[0]).is_absolute());
        assert_eq!(&argv[1..], ["--owner", owner, "view"]);
    }
    f.assert_no_legacy();
}

#[test]
fn profiles_are_ownerless_sorted_and_read_only() {
    let f = Fixture::new();
    let out = f.run("normal", &["--json", "profiles"]);
    assert!(out.status.success(), "{}", error(&out));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), serde_json::json!({"profiles": []}));
    assert!(!f.root.join("state").exists());

    let profiles = f.root.join("state/herdr-browser/profiles");
    for name in ["zebra", "alpha", "default", "Invalid"] {
        fs::create_dir_all(profiles.join(name).join("profile")).unwrap();
    }
    fs::create_dir_all(profiles.join("incomplete")).unwrap();
    fs::write(profiles.join("file"), "").unwrap();
    std::os::unix::fs::symlink(profiles.join("alpha"), profiles.join("linked")).unwrap();
    let out = f.run("normal", &["--owner", "t2", "--profile", "zebra", "--json", "profiles"]);
    assert!(out.status.success(), "{}", error(&out));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), serde_json::json!({"profiles": ["alpha", "default", "zebra"]}));
    assert!(f.calls().is_empty(), "{:?}", f.calls());
}

#[test]
fn standalone_url_requires_standalone_and_profile_is_preserved_in_owned_view_argv() {
    let f = Fixture::new();
    let out = f.run("normal", &["--owner", "t1", "view", "--url", "http://localhost:3000"]);
    assert!(!out.status.success());
    assert!(error(&out).contains("--standalone"), "{}", error(&out));
    assert!(f.calls().is_empty());
    let out = f.run("normal", &["--owner", "t1", "--profile", "work", "show"]);
    assert!(out.status.success(), "{}", error(&out));
    let calls = f.calls();
    let split = calls.iter().find(|c| c[0] == "herdr-app" && c[1] == "do" && c[2] == "pane.split").unwrap();
    let args = split.as_array().unwrap();
    let i = args.iter().position(|v| v == "--argv").unwrap();
    let argv: Vec<String> = serde_json::from_str(args[i + 1].as_str().unwrap()).unwrap();
    assert_eq!(&argv[1..], ["--owner", "t1", "--profile", "work", "view"]);
    f.assert_no_legacy();
}

#[test]
fn failed_login_reports_private_log_without_echoing_chrome_stderr() {
    let f = Fixture::new();
    let config = f.root.join("config/herdr-browser");
    fs::create_dir_all(&config).unwrap();
    let chrome = f.root.join("fails-before-window");
    fs::write(&chrome, "#!/bin/sh\necho sensitive-diagnostic >&2\nexit 23\n").unwrap();
    fs::set_permissions(&chrome, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(config.join("config.toml"), format!("chrome_path = {:?}\n", chrome.to_str().unwrap())).unwrap();
    let out = f.run("normal", &["--profile", "work", "login"]);
    let log = f.root.join("state/herdr-browser/profiles/work/login-chrome.log");
    assert!(!out.status.success());
    assert!(error(&out).contains(&format!("see {}", log.display())), "{}", error(&out));
    assert!(!error(&out).contains("sensitive-diagnostic"));
    assert!(fs::read_to_string(&log).unwrap().contains("sensitive-diagnostic"));
    assert_eq!(fs::metadata(log).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn standalone_startup_failure_restores_terminal_and_reports_configured_chrome() {
    // Let the fixture create the controlling terminal without a Rust pre_exec
    // callback. On macOS TIOCSCTTY from that callback can block Command::spawn.
    const PTY: &str = r#"
import errno, os, pty, select, signal, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])
data = bytearray()
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if select.select([fd], [], [], 0.05)[0]:
        try:
            chunk = os.read(fd, 4096)
        except OSError as error:
            if error.errno != errno.EIO: raise
            chunk = b''
        data.extend(chunk)
        if not chunk: break
    done, status = os.waitpid(pid, os.WNOHANG)
    if done:
        sys.stdout.buffer.write(data)
        sys.exit(os.waitstatus_to_exitcode(status))
else:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    sys.exit('view did not exit after initial startup failure')
_, status = os.waitpid(pid, 0)
sys.stdout.buffer.write(data)
sys.exit(os.waitstatus_to_exitcode(status))
"#;
    let f = Fixture::new();
    let config = f.root.join("config/herdr-browser");
    fs::create_dir_all(&config).unwrap();
    let missing = f.root.join("missing-chrome");
    fs::write(config.join("config.toml"), format!("chrome_path = {:?}\nview_renderer = \"blocks\"\n", missing.to_str().unwrap())).unwrap();
    let out = Command::new("/usr/bin/python3")
        .args(["-c", PTY, env!("CARGO_BIN_EXE_herdr-browser"), "--profile", "work", "view",
               "--standalone", "--url", "http://localhost:3000"])
        .env("HOME", &f.root).env("XDG_STATE_HOME", f.root.join("state"))
        .env("XDG_CONFIG_HOME", f.root.join("config")).env("TERM", "xterm")
        .env_remove("HC_TERMINAL_ID").env_remove("HERDR_PANE_ID")
        .output().unwrap();
    assert!(!out.status.success());
    let screen = error(&out);
    assert!(screen.contains(&format!("chrome_path {} does not exist", missing.display())), "{screen}");
    assert!(screen.contains("\x1b[?1049h") && screen.contains("\x1b[?1049l"), "{screen:?}");
}

const STUB: &str = r##"#!/usr/bin/python3
import json, os, sys
from pathlib import Path
root = Path(os.environ['FIXTURE_ROOT'])
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with (root / 'calls').open('a') as f: f.write(json.dumps([name] + args) + '\n')
scenario = os.environ['SCENARIO']
def fail(message):
    print(message, file=sys.stderr); sys.exit(1)
def records():
    p = root / 'views.json'
    return json.loads(p.read_text()) if p.exists() else {}
if name == 'herdr': fail('legacy sentinel invoked')
if name == 'hc':
    if args and args[0] == 'get':
        target = args[1]
        if scenario == 'stale': fail('stale owner')
        views = records()
        if target in views:
            print(json.dumps({'address':target,'state':'live','argv':views[target]['argv']})); sys.exit()
        if target not in ('t1','t2'): fail('not found')
        status = {'app':'omp','state':'blocked' if scenario == 'blocked' else 'idle'}
        print(json.dumps({'address':target,'state':'live','status':status,'argv':['agent']})); sys.exit()
    fail('unexpected hc command')
if name == 'herdr-app':
    if args == ['ls']:
        if scenario == 'down': fail('desktop unavailable')
        terminals = [{'terminal':'t1','pane':'p1'},{'terminal':'t2','pane':'p2'}]
        if scenario == 'ambiguous': terminals.append({'terminal':'t2','pane':'duplicate'})
        terminals += [{'terminal':k,'pane':v['pane']} for k,v in records().items()]
        print(json.dumps({'spaces':[{'machine':None,'tabs':[{'id':'tab1','terminals':terminals}]}]})); sys.exit()
    if args[:2] == ['do','pane.split']:
        pane = args[args.index('--pane')+1]
        argv = json.loads(args[args.index('--argv')+1])
        view = 't9' if pane == 'p1' else 'tb'
        viewpane = 'p9' if pane == 'p1' else 'pb'
        views = records(); views[view] = {'pane':viewpane,'argv':argv}
        (root / 'views.json').write_text(json.dumps(views))
        print(json.dumps({'terminal':view,'pane':viewpane})); sys.exit()
    fail('unexpected desktop command')
"##;
