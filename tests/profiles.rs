//! Real profile isolation proof. Set HERDR_BROWSER_TEST_CHROME and run with --ignored.
use std::{fs, io::{BufRead, BufReader}, path::PathBuf, process::{Child, Command, Output, Stdio}};
use serde_json::Value;

struct Fixture { root: PathBuf, server: Option<Child>, socket: PathBuf, session: String }
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("hb-profiles-{}", std::process::id()));
        fs::create_dir_all(root.join("config/herdr-browser")).unwrap();
        let chrome = PathBuf::from(std::env::var_os("HERDR_BROWSER_TEST_CHROME").expect("set HERDR_BROWSER_TEST_CHROME"));
        assert!(chrome.is_absolute() && chrome.is_file());
        fs::write(root.join("config/herdr-browser/config.toml"), format!("chrome_path = {}\n", toml::Value::String(chrome.to_string_lossy().into_owned()))).unwrap();
        let socket = root.join("hc.sock");
        Self { root, server: None, socket, session: format!("hbprofiles{}", std::process::id()) }
    }
    fn env(&self, c: &mut Command) {
        c.env("HOME", &self.root).env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state")).env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", &self.root).env("HC_SESSION", &self.session).env("HC_SOCKET", &self.socket)
            .env("HERDR_APP_HOME", self.root.join("no-desktop")).env_remove("HC_TERMINAL_ID").env_remove("HERDR_PANE_ID");
    }
    fn hc(&self, args: &[&str]) -> Output { let mut c = Command::new("hc"); self.env(&mut c); c.args(args).output().unwrap() }
    fn owner(&self) -> String {
        let out = self.hc(&["run", "--bg", "--json", "--no-parent", "--no-notify", "sleep", "300"]);
        assert!(out.status.success(), "{}", output(&out));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["address"].as_str().unwrap().to_owned()
    }
    fn browser(&self, owner: Option<&str>, profile: Option<&str>, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-browser")); self.env(&mut c);
        if let Some(owner) = owner { c.args(["--owner", owner]); }
        if let Some(profile) = profile { c.args(["--profile", profile]); }
        c.args(args).output().unwrap()
    }
    fn ok(&self, owner: Option<&str>, profile: Option<&str>, args: &[&str]) -> String {
        let out = self.browser(owner, profile, args); assert!(out.status.success(), "{args:?}: {}", output(&out)); String::from_utf8(out.stdout).unwrap()
    }
    fn start_server(&mut self) -> String {
        let code = "import http.server\nclass H(http.server.BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200)\n  if self.path == '/set': self.send_header('Set-Cookie','proof=profile; Max-Age=3600; Path=/')\n  self.send_header('Content-Type','text/html')\n  self.end_headers()\n  self.wfile.write(('cookie: '+self.headers.get('Cookie','none')).encode())\n def log_message(self,*args): pass\ns=http.server.ThreadingHTTPServer(('127.0.0.1',0),H)\nprint(s.server_port,flush=True)\ns.serve_forever()";
        let mut child = Command::new("python3").args(["-u", "-c", code]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut line = String::new(); BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let port: u16 = line.trim().parse().unwrap(); self.server = Some(child); format!("http://127.0.0.1:{port}")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for profile in [None, Some("work")] { let _ = self.browser(None, profile, &["stop"]); }
        let _ = self.hc(&["server", "stop"]); if let Some(mut c) = self.server.take() { let _ = c.kill(); let _ = c.wait(); } let _ = fs::remove_dir_all(&self.root);
    }
}
fn output(o: &Output) -> String { format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) }

#[test]
#[ignore = "requires hc, python3, and a dedicated installed Chrome for Testing"]
fn named_profiles_isolate_persist_and_stop_independently() {
    let mut f = Fixture::new(); let base = f.start_server(); let owner = f.owner();
    f.ok(Some(&owner), Some("work"), &["open", &format!("{base}/set")]);
    f.ok(Some(&owner), Some("work"), &["open", &base]);
    assert!(f.ok(Some(&owner), Some("work"), &["text"]).contains("proof=profile"));
    f.ok(Some(&owner), None, &["open", &base]);
    assert!(!f.ok(Some(&owner), None, &["text"]).contains("proof=profile"));
    f.ok(None, Some("work"), &["stop"]);
    assert!(f.ok(Some(&owner), None, &["text"]).contains("cookie:"));
    f.ok(Some(&owner), Some("work"), &["open", &base]);
    assert!(f.ok(Some(&owner), Some("work"), &["text"]).contains("proof=profile"));
    let bad = f.browser(None, Some("../work"), &["status"]); assert!(!bad.status.success());
}

#[test]
fn invalid_profile_names_are_rejected_before_any_action() {
    for name in ["../work", ".", "Work", "a/b", ""] {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr-browser")).args(["--profile", name, "status"]).output().unwrap();
        assert!(!out.status.success(), "accepted {name:?}");
        assert!(output(&out).contains("invalid profile name"), "{}", output(&out));
    }
}
