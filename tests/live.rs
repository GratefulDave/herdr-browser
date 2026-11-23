//! Real CLI/Chrome proof. Set HERDR_BROWSER_TEST_CHROME to an installed Chrome for
//! Testing executable, then run `cargo test --locked --test live -- --ignored`.
use std::{fs, io::{BufRead, BufReader, Write}, path::{Path, PathBuf}, process::{Child, Command, Output, Stdio}, sync::atomic::{AtomicUsize, Ordering}, time::{Duration, Instant}};
use serde_json::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture { root: PathBuf, server: Option<Child>, session: String, socket: PathBuf }
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("hb-live-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(root.join("config/herdr-browser")).unwrap();
        let chrome = PathBuf::from(std::env::var_os("HERDR_BROWSER_TEST_CHROME").expect("set HERDR_BROWSER_TEST_CHROME to the dedicated Chrome for Testing executable"));
        assert!(chrome.is_absolute() && chrome.is_file(), "invalid Chrome for Testing path: {}", chrome.display());
        let quoted = toml::Value::String(chrome.to_string_lossy().into_owned()).to_string();
        fs::write(root.join("config/herdr-browser/config.toml"), format!("chrome_path = {quoted}\n")).unwrap();
        let resource = root.join(".local/share/herdr-browser/extension");
        fs::create_dir_all(&resource).unwrap();
        for name in ["manifest.json", "recorder.js"] {
            fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("extension").join(name), resource.join(name)).unwrap();
        }
        let session = format!("hbtest{}{}", std::process::id(), NEXT.load(Ordering::Relaxed));
        let socket = root.join("hc.sock");
        Self { root, server: None, session, socket }
    }
    fn env(&self, c: &mut Command) {
        c.env("HOME", &self.root).env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state")).env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", &self.root).env("HC_SESSION", &self.session).env("HC_SOCKET", &self.socket)
            .env("HERDR_APP_HOME", self.root.join("no-desktop"))
            .env_remove("HC_TERMINAL_ID").env_remove("HERDR_PANE_ID");
    }
    fn hc(&self, args: &[&str]) -> Output {
        let mut c = Command::new("hc"); self.env(&mut c); c.args(args).output().unwrap()
    }
    fn owner(&self) -> String {
        let out = self.hc(&["run", "--bg", "--json", "--no-parent", "--no-notify", "sleep", "300"]);
        assert!(out.status.success(), "hc run: {}", output(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("hc run must return JSON");
        let owner = value["address"].as_str().expect("hc run returned no address").to_owned();
        assert!(owner.starts_with('t') && owner.len() > 1 && owner[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'z').contains(&b)), "unexpected hc address: {owner}");
        let get = self.hc(&["get", &owner, "--json"]);
        assert!(get.status.success(), "hc get {owner}: {}", output(&get));
        owner
    }
    fn start_server(&mut self) -> String {
        let demo = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/demo");
        let code = "import functools,http.server,sys\nh=functools.partial(http.server.SimpleHTTPRequestHandler,directory=sys.argv[1])\ns=http.server.ThreadingHTTPServer(('127.0.0.1',0),h)\nprint(s.server_port,flush=True)\ns.serve_forever()";
        let mut child = Command::new("python3").args(["-u", "-c", code]).arg(demo).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let port: u16 = line.trim().parse().expect("fixture server did not report a port");
        self.server = Some(child);
        format!("http://127.0.0.1:{port}/")
    }
    fn start_navigation_server(&mut self) -> String {
        let code = r#"import http.server, json, threading
counts = {}
lock = threading.Lock()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/favicon.ico':
            self.send_response(204)
            self.end_headers()
            return
        with lock:
            counts[self.path] = counts.get(self.path, 0) + 1
            count = counts[self.path]
        script = "document.body.innerText = 'URL=' + location.href + '\\nREQUEST=' + " + json.dumps(self.path) + " + '\\nLOAD=' + " + str(count)
        data = ('<!doctype html><html><body><script>' + script + '</script></body></html>').encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *args): pass
s = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(s.server_port, flush=True)
s.serve_forever()
"#;
        let mut child = Command::new("python3").args(["-u", "-c", code]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let port: u16 = line.trim().parse().expect("navigation fixture server did not report a port");
        self.server = Some(child);
        format!("http://127.0.0.1:{port}")
    }
    fn start_oauth_server(&mut self) -> String {
        let code = r#"import http.server
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        redirects = {'/chain/start': '/chain/next', '/chain/next': '/callback'}
        if self.path in redirects:
            self.send_response(302)
            self.send_header('Location', redirects[self.path])
            self.end_headers()
            return
        pages = {
            '/callback': '<h1>Same-tab callback reached</h1><a id="sign-in" href="/provider" target="_blank">Sign in</a>',
            '/provider': '<h1>Local simulated provider</h1><button id="complete" onclick="location.replace(\'/complete\')">Complete sign in</button>',
            '/complete': '<h1>Local sign-in callback complete</h1><button id="close" onclick="window.close()">Close</button>',
            '/other': '<h1>Other owner opener</h1>',
        }
        body = pages.get(self.path)
        if body is None:
            self.send_error(404)
            return
        data = body.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *args): pass
s = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(s.server_port, flush=True)
s.serve_forever()
"#;
        let mut child = Command::new("python3").args(["-u", "-c", code]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let port: u16 = line.trim().parse().expect("OAuth fixture server did not report a port");
        self.server = Some(child);
        format!("http://127.0.0.1:{port}")
    }
    fn browser(&self, owner: Option<&str>, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-browser")); self.env(&mut c);
        if let Some(owner) = owner { c.args(["--owner", owner]); }
        c.args(args).output().unwrap()
    }
    fn ok(&self, owner: &str, args: &[&str]) -> String {
        let out = self.browser(Some(owner), args);
        assert!(out.status.success(), "{owner} {args:?}: {}", output(&out));
        String::from_utf8(out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.browser(None, &["stop"]);
        let _ = self.hc(&["server", "stop"]);
        if let Some(mut child) = self.server.take() { let _ = child.kill(); let _ = child.wait(); }
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn output(out: &Output) -> String { format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)) }

#[test]
#[ignore = "requires hc, python3, and a dedicated installed Chrome for Testing"]
fn isolated_owners_navigate_edit_inspect_and_capture() {
    let mut f = Fixture::new();
    let url = f.start_server();
    let a = f.owner(); let b = f.owner();
    assert_ne!(a, b);
    let headless_only = f.browser(Some(&a), &["open", &url, "--show"]);
    let result = output(&headless_only);
    assert!(!headless_only.status.success() && result.contains("navigation succeeded; display failed"), "{result}");
    let without_target = f.browser(Some(&b), &["snapshot"]);
    assert!(!without_target.status.success(), "second owner borrowed first page: {}", output(&without_target));
    assert!(output(&without_target).contains("run `herdr-browser open <url>` first"), "{}", output(&without_target));
    let snapshot = f.ok(&a, &["snapshot"]);
    assert!(snapshot.contains("Save note") && snapshot.contains("Write a note"), "{snapshot}");
    f.ok(&a, &["type", "#note", "browser-proof"]);
    f.ok(&a, &["click", "button"]);
    let text = f.ok(&a, &["text"]);
    assert!(text.contains("browser-proof"), "{text}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let console = f.ok(&a, &["console"]);
        let network = f.ok(&a, &["network"]);
        if console.contains("saved note: browser-proof") && network.contains("/api/notes") && network.contains("501") { break; }
        assert!(Instant::now() < deadline, "missing recorded console/network: {console}\n{network}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let png = f.root.join("proof.png");
    f.ok(&a, &["screenshot", png.to_str().unwrap()]);
    let mut reader = png::Decoder::new(std::io::BufReader::new(fs::File::open(&png).unwrap())).read_info().expect("invalid PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG dimensions too large")];
    let frame = reader.next_frame(&mut pixels).expect("invalid PNG frame");
    assert!(frame.width > 0 && frame.height > 0);
    f.ok(&b, &["open", &url]);
    let b_text = f.ok(&b, &["text"]);
    assert!(!b_text.contains("browser-proof"), "second owner saw first owner's note: {b_text}");
}

#[test]
#[ignore = "requires hc, python3, and a dedicated installed Chrome for Testing"]
fn oauth_same_tab_redirect_and_popup_provider() {
    let mut f = Fixture::new();
    let base = f.start_oauth_server();
    let a = f.owner();
    let b = f.owner();
    f.ok(&b, &["open", &format!("{base}/other")]);
    f.ok(&a, &["open", &format!("{base}/chain/start")]);
    let callback = f.ok(&a, &["text"]);
    assert!(callback.contains("Same-tab callback reached"), "same-tab 302 chain failed: {callback}");
    eprintln!("same-tab 302 chain reached callback: {callback}");
    let mut run = Command::new("hc");
    f.env(&mut run);
    let result = run.args(["run", "--bg", "--json", "--no-parent", "--no-notify",
        env!("CARGO_BIN_EXE_herdr-browser"), "--owner", &a, "view"]).output().unwrap();
    assert!(result.status.success(), "start view: {}", output(&result));
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    let view = value["address"].as_str().expect("view has no address");
    let screen = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let read = f.hc(&["read", view]);
            let text = output(&read);
            assert!(read.status.success(), "read view: {text}");
            if text.contains(needle) { break; }
            assert!(Instant::now() < deadline, "view never displayed {needle:?}: {text}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    screen("/callback");

    f.ok(&a, &["click", "#sign-in"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let provider = loop {
        let text = f.ok(&a, &["text"]);
        if text.contains("Local simulated provider") || Instant::now() >= deadline { break text; }
        std::thread::sleep(Duration::from_millis(200));
    };
    let snapshot = f.ok(&a, &["snapshot"]);
    assert!(provider.contains("Local simulated provider") && snapshot.contains("Complete sign in"),
        "popup provider not reachable via owner CLI; text: {provider}\nsnapshot: {snapshot}");
    screen("/provider");
    f.ok(&a, &["click", "#complete"]);
    let complete = f.ok(&a, &["text"]);
    assert!(complete.contains("Local sign-in callback complete"), "popup callback failed: {complete}");
    let closed = f.ok(&a, &["click", "#close"]);
    assert!(closed.contains("/callback"), "close did not return the opener's info: {closed}");
    let reopened = f.ok(&a, &["text"]);
    assert!(reopened.contains("Same-tab callback reached"), "popup close did not return to opener: {reopened}");
    screen("/callback");
    let other = f.ok(&b, &["text"]);
    assert!(other.contains("Other owner opener"), "other owner was redirected: {other}");
    let killed = f.hc(&["kill", view]);
    assert!(killed.status.success(), "kill fixture view: {}", output(&killed));
}

#[test]
#[ignore = "requires hc, python3, and a dedicated installed Chrome for Testing"]
fn stopped_browser_is_not_restarted_by_live_view_or_snapshot() {
    let mut f = Fixture::new();
    let url = f.start_server();
    let owner = f.owner();
    f.ok(&owner, &["open", &url]);

    let mut run = Command::new("hc");
    f.env(&mut run);
    let result = run.args(["run", "--bg", "--json", "--no-parent", "--no-notify",
        env!("CARGO_BIN_EXE_herdr-browser"), "--owner", &owner, "view"]).output().unwrap();
    assert!(result.status.success(), "start view: {}", output(&result));
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    let view = value["address"].as_str().expect("view has no address");
    let screen = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let read = f.hc(&["read", view]);
            let text = output(&read);
            assert!(read.status.success(), "read view: {text}");
            if text.contains(needle) { break; }
            assert!(Instant::now() < deadline, "view never displayed {needle:?}: {text}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    screen("127.0.0.1");

    let stopped = f.browser(None, &["stop"]);
    assert!(stopped.status.success(), "stop: {}", output(&stopped));
    screen("view lost");
    let profile = f.root.join("state/herdr-browser/profile");
    let assert_stopped = || {
        assert!(fs::symlink_metadata(profile.join("SingletonLock")).is_err(), "Chrome restarted after stop");
        if let Ok(port_file) = fs::read_to_string(profile.join("DevToolsActivePort")) {
            if let Some(port) = port_file.lines().next().and_then(|s| s.parse::<u16>().ok()) {
                assert!(std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(100)).is_err(), "Chrome endpoint is live after stop");
            }
        }
    };
    let snapshot = f.browser(Some(&owner), &["snapshot"]);
    assert!(!snapshot.status.success() && output(&snapshot).contains("run `herdr-browser open <url>` first"), "snapshot after stop: {}", output(&snapshot));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert_stopped();
        std::thread::sleep(Duration::from_millis(100));
    }

    f.ok(&owner, &["open", &url]);
    screen("127.0.0.1");
    let killed = f.hc(&["kill", view]);
    assert!(killed.status.success(), "kill fixture view: {}", output(&killed));
}

#[test]
#[ignore = "requires hc, python3, and a dedicated installed Chrome for Testing"]
fn interactive_address_paste_history_and_reload() {
    let mut f = Fixture::new();
    let base = f.start_navigation_server();
    let owner = f.owner();
    let baseline = format!("{base}/start");
    let destination = format!("{base}/encoded%2Fsegment?q=a%2Bb%26c#frag%20ment");
    f.ok(&owner, &["open", &baseline]);


    let wait_page = |url: &str, request: &str| -> u64 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = f.ok(&owner, &["text"]);
            let mut lines = text.lines();
            let actual_url = lines.find_map(|line| line.strip_prefix("URL="));
            let actual_request = text.lines().find_map(|line| line.strip_prefix("REQUEST="));
            let load = text.lines().find_map(|line| line.strip_prefix("LOAD=").and_then(|n| n.parse::<u64>().ok()));
            if actual_url == Some(url) && actual_request == Some(request) && load.is_some() {
                return load.unwrap();
            }
            assert!(Instant::now() < deadline, "expected URL={url}, REQUEST={request}; got: {text}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // Run exactly one view in a private PTY. Its inherited environment points
    // to this fixture's hc core, HOME and Chrome profile.
    let driver = r#"import fcntl, os, pty, select, signal, struct, sys, termios, threading, time
pid, master = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 100, 0, 0))
    os.execv(sys.argv[1], [sys.argv[1], '--owner', sys.argv[2], 'view'])
try:
    seen = b''
    deadline = time.monotonic() + 20
    while b'/start' not in seen:
        if time.monotonic() >= deadline: raise RuntimeError('view did not display baseline')
        if select.select([master], [], [], 0.2)[0]:
            seen += os.read(master, 65536)
    def drain():
        try:
            while os.read(master, 65536): pass
        except OSError:
            pass
    threading.Thread(target=drain, daemon=True).start()
    print('ready', flush=True)
    commands = {
        'focus': b'\x0c',
        'paste': b'\x1b[200~' + sys.argv[3].encode() + b'\x1b[201~',
        'enter': b'\r',
        'back': b'\x02',
        'forward': b'\x06',
        'reload': b'\x12',
    }
    for line in sys.stdin:
        name = line.strip()
        os.write(master, commands[name])
        print(name, flush=True)
finally:
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    os.close(master)
"#;
    let mut command = Command::new("python3");
    f.env(&mut command);
    let mut driver = command.args(["-u", "-c", driver, env!("CARGO_BIN_EXE_herdr-browser"), &owner, &destination])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
    let mut signals = BufReader::new(driver.stdout.take().unwrap());
    let mut signal = String::new();
    signals.read_line(&mut signal).unwrap();
    assert_eq!(signal.trim(), "ready", "fixture view failed to start");
    let mut send = |name: &str| {
        writeln!(driver.stdin.as_mut().unwrap(), "{name}").unwrap();
        signal.clear();
        signals.read_line(&mut signal).unwrap();
        assert_eq!(signal.trim(), name, "fixture view did not accept {name}");
    };
    wait_page(&baseline, "/start");
    send("focus");
    send("paste");
    let quiet_until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < quiet_until {
        let text = f.ok(&owner, &["text"]);
        assert!(text.lines().any(|line| line == format!("URL={baseline}")), "paste navigated before Enter: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    send("enter");
    wait_page(&destination, "/encoded%2Fsegment?q=a%2Bb%26c");
    send("back");
    wait_page(&baseline, "/start");
    send("forward");
    let before_reload = wait_page(&destination, "/encoded%2Fsegment?q=a%2Bb%26c");
    send("reload");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let load = wait_page(&destination, "/encoded%2Fsegment?q=a%2Bb%26c");
        if load > before_reload { break; }
        assert!(Instant::now() < deadline, "reload did not request destination again (load {load})");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(send);
    drop(driver.stdin.take());
    assert!(driver.wait().unwrap().success(), "fixture view driver failed");
}
