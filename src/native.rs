//! Authenticated, short-lived native view transport.
use crate::{R, cdp::Cdp, chrome, page, state::{self, Config}};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs, io::{self, BufRead, BufReader, Read, Write}, net::{TcpListener, TcpStream}, path::PathBuf, process::{Command, Stdio}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

const MAX_LINE: u64 = 1024 * 1024;

fn random() -> R<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(64);
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(token)
}
struct Frames {
    dir: PathBuf,
    relative: String,
    _lock: fs::File,
}

impl Frames {
    fn new(home: &std::path::Path) -> R<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let root = home.join("data/herdr-browser/frames");
        fs::create_dir_all(&root)?;
        let guard = fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join(".lock"))?;
        guard.lock()?;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) || !entry.file_type()?.is_dir() { continue; }
            if let Ok(lock) = fs::OpenOptions::new().write(true).open(entry.path().join(".lock"))
                && lock.try_lock().is_ok()
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        let name = random()?;
        let relative = format!("data/herdr-browser/frames/{name}");
        let dir = root.join(name);
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let lock = fs::OpenOptions::new().create_new(true).write(true).open(dir.join(".lock"))?;
        lock.lock()?;
        drop(guard);
        Ok(Self { dir, relative, _lock: lock })
    }
}
impl Drop for Frames { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.dir); } }


pub fn run(home: PathBuf, owner: Option<String>, url: Option<String>, sign_in: bool, child: bool) -> R<()> {
    if !child {
        if sign_in { chrome::login(&Config::load()?, Some(url.as_deref().unwrap_or(page::HOME_URL)))?; }
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(std::env::current_exe()?);
        if let Some(id) = &owner { cmd.args(["--owner", id]); }
        if let Some(profile) = state::selected_profile() { cmd.args(["--profile", profile]); }
        cmd.arg("native-view").arg("--app-home").arg(home).arg("--child");
        if owner.is_none() { cmd.arg("--standalone"); }
        if let Some(url) = url { cmd.arg("--url").arg(url); }
        let mut child = cmd.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let mut line = String::new();
        let n = BufReader::new(child.stdout.take().ok_or("missing child stdout")?).take(MAX_LINE).read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            return Err("native helper failed to start".into());
        }
        print!("{line}");
        return Ok(());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let token = random()?;
    println!("{}", json!({"port": listener.local_addr()?.port(), "token": token}));
    io::stdout().flush()?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.into()),
        }
    };
    drop(listener);
    // On macOS the accepted socket can inherit the listener's nonblocking mode.
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(socket.try_clone()?);
    let mut line = Vec::new();
    let hello = read_line(&mut reader, &mut line)?.ok_or("missing hello")?;
    if hello["t"] != "hello" || hello["token"].as_str() != Some(token.as_str()) { return Err("authentication failed".into()); }
    reader.get_ref().set_read_timeout(None)?;
    let writer = Arc::new(Mutex::new(socket));
    let (tx, rx) = mpsc::sync_channel(64);
    thread::spawn(move || loop {
        match read_line(&mut reader, &mut line) {
            Ok(Some(v)) => if tx.send(Ok(v)).is_err() { break; },
            Ok(None) => break,
            Err(e) => { let _ = tx.send(Err(e.to_string())); break; }
        }
    });
    let result = (|| -> R<()> {
        let frames = Frames::new(&home)?;
        let cfg = Config::load()?;
        let standalone = owner.is_none();
        let mut page = match owner { Some(id) => page::resolve(&cfg, &id, Some("about:blank"))?, None => page::standalone(&cfg, url.as_deref().unwrap_or(page::HOME_URL))? };
        let result = session(&mut page, &writer, rx, frames.dir.clone(), frames.relative.clone());
        if standalone { let root = page.root().to_owned(); let _ = page::close_target(&mut page, &root); }
        result
    })();
    if let Err(e) = &result { let _ = emit(&writer, &json!({"t":"error","message":e.to_string()})); }
    result
}

fn read_line(reader: &mut BufReader<TcpStream>, line: &mut Vec<u8>) -> R<Option<Value>> {
    line.clear();
    let n = (&mut *reader).take(MAX_LINE + 1).read_until(b'\n', line)?;
    if n == 0 { return Ok(None); }
    if n as u64 > MAX_LINE || line.last() != Some(&b'\n') { return Err("invalid or oversized protocol line".into()); }
    Ok(Some(serde_json::from_slice(line)?))
}
fn emit(out: &Arc<Mutex<TcpStream>>, v: &Value) -> R<()> {
    let mut out = out.lock().map_err(|_| "socket lock poisoned")?;
    let mut bytes = serde_json::to_vec(v)?;
    if bytes.len() >= MAX_LINE as usize { return Err("oversized protocol output".into()); }
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    Ok(())
}

struct Stream { stop: Arc<AtomicBool>, thread: Option<thread::JoinHandle<()>> }
impl Drop for Stream { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); if let Some(t) = self.thread.take() { let _ = t.join(); } } }
fn start_stream(target: String, size: (u32,u32), generation: u64, out: Arc<Mutex<TcpStream>>, dir: PathBuf, relative: String, files: Arc<Mutex<std::collections::VecDeque<PathBuf>>>) -> R<Stream> {
    let endpoint = chrome::endpoint().ok_or("browser is not running")?;
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let thread = thread::spawn(move || {
        let mut run = || -> R<()> {
            let mut c = Cdp::connect(&endpoint)?;
            let r = c.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None)?;
            let sid = r["sessionId"].as_str().ok_or("missing stream session")?.to_owned();
            c.call("Emulation.setDeviceMetricsOverride", json!({"width":size.0,"height":size.1,"deviceScaleFactor":1,"mobile":false}), Some(&sid))?;
            c.call("Page.enable", json!({}), Some(&sid))?;
            c.call("Page.startScreencast", json!({"format":"jpeg","quality":80,"maxWidth":size.0,"maxHeight":size.1,"everyNthFrame":1}), Some(&sid))?;
            let mut index = 0u64;
            let mut bytes = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let Some(msg) = c.next_message(Duration::from_millis(100))? else { continue };
                match msg["method"].as_str() {
                    Some("Page.screencastFrame") => {
                        c.send("Page.screencastFrameAck", json!({"sessionId":msg["params"]["sessionId"]}), Some(&sid))?;
                        let Some(data) = msg["params"]["data"].as_str() else { continue };
                        if data.len() > 24 * 1024 * 1024 { continue; }
                        bytes.clear();
                        STANDARD.decode_vec(data, &mut bytes)?;
                        // GPUI caches decoded images by path. Never reuse a frame path.
                        let name = format!("{generation}-{index}.jpg");
                        index += 1;
                        let path = dir.join(&name);
                        fs::write(&path, &bytes)?;
                        let evicted = {
                            let mut files = files.lock().map_err(|_| "frame ring lock poisoned")?;
                            let evicted = if files.len() == 24 { files.pop_front() } else { None };
                            files.push_back(path);
                            evicted
                        };
                        if let Some(path) = evicted { let _ = fs::remove_file(path); }
                        emit(&out, &json!({"t":"frame","file":format!("{relative}/{name}"),"w":size.0,"h":size.1}))?;
                    }
                    Some("Page.javascriptDialogOpening") => { let p = &msg["params"]; emit(&out, &json!({"t":"dialog","kind":p["type"],"message":p["message"],"defaultText":p["defaultPrompt"]}))?; }
                    Some("Page.javascriptDialogClosed") => { emit(&out, &json!({"t":"dialog_closed"}))?; }
                    _ => {}
                }
            }
            Ok(())
        };
        let result = run();
        if let Err(e) = result { let _ = emit(&out, &json!({"t":"error","message":e.to_string()})); }
    });
    Ok(Stream { stop, thread: Some(thread) })
}

fn session(page: &mut page::Page, out: &Arc<Mutex<TcpStream>>, rx: mpsc::Receiver<Result<Value, String>>, dir: PathBuf, relative: String) -> R<()> {
    let mut size = (800, 600);
    let mut stream: Option<Stream> = None;
    let files = Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(24)));
    let mut generation = 0;
    let mut target = String::new();
    let mut refresh = Instant::now();
    let mut keepalive = Instant::now() + Duration::from_secs(10);
    let mut last_page = Value::Null;
    loop {
        if keepalive <= Instant::now() {
            out.lock().map_err(|_| "socket lock poisoned")?.write_all(b"{\"t\":\"keepalive\"}\n")?;
            keepalive = Instant::now() + Duration::from_secs(10);
        }
        if refresh <= Instant::now() {
            let tabs = page::tabs(page)?;
            let restart = target != page.tab.id || stream.is_none();
            if restart { drop(stream.take()); target = page.tab.id.clone(); }
            let info = page.cdp.call_timeout("Target.getTargetInfo", json!({"targetId":target}), None, Duration::from_secs(2))?;
            let tabs: Vec<_> = tabs.into_iter().map(|t| json!({"id":t.id,"title":t.title,"active":t.active})).collect();
            let view = json!({"t":"page","url":info["targetInfo"]["url"],"title":info["targetInfo"]["title"],"tabs":tabs});
            if view != last_page {
                emit(out, &view)?;
                last_page = view;
            }
            if restart {
                generation += 1;
                stream = Some(start_stream(target.clone(), size, generation, out.clone(), dir.clone(), relative.clone(), files.clone())?);
            }
            refresh = Instant::now() + Duration::from_millis(700);
        }
        let msg = match rx.recv_timeout(refresh.saturating_duration_since(Instant::now())) {
            Ok(v) => v?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let old_size = size;
        let result = command(page, &msg, out, &mut size);
        if size != old_size { drop(stream.take()); }
        if let Err(e) = result { emit(out, &json!({"t":"error","message":e.to_string()}))?; }
        if size != old_size || matches!(msg["t"].as_str(), Some("new_tab" | "select_tab" | "close_tab")) {
            refresh = Instant::now();
        }
    }
    drop(stream);
    Ok(())
}

fn command(p: &mut page::Page, m: &Value, out: &Arc<Mutex<TcpStream>>, size: &mut (u32,u32)) -> R<()> {
    let s = |key: &str| m[key].as_str().ok_or_else(|| format!("missing {key}"));
    let send = |p: &mut page::Page, method: &str, params: Value| -> R<()> {
        p.cdp.send(method, params, Some(&p.session))?;
        Ok(())
    };
    match s("t")? {
        "resize" => {
            let w = m["w"].as_u64().ok_or("missing width")?;
            let h = m["h"].as_u64().ok_or("missing height")?;
            if !(1..=4096).contains(&w) || !(1..=4096).contains(&h) {
                return Err("invalid viewport".into());
            }
            *size = (w as u32, h as u32);
        }
        "navigate" => send(p, "Page.navigate", json!({"url":page::normalize_url(s("url")?)}))?,
        "back" | "forward" => {
            let history = p.call("Page.getNavigationHistory", json!({}))?;
            let index = history["currentIndex"].as_i64().ok_or("missing history index")?;
            let next = index + if m["t"] == "back" { -1 } else { 1 };
            if let Ok(next) = usize::try_from(next)
                && let Some(id) = history["entries"].get(next).and_then(|entry| entry["id"].as_i64())
            {
                send(p, "Page.navigateToHistoryEntry", json!({"entryId":id}))?;
            }
        }
        "reload" => send(p,"Page.reload",json!({}))?,
        "new_tab" => page::new_tab(p,None)?,
        "select_tab" => page::select_tab(p,s("id")?)?,
        "close_tab" => {
            if page::tabs(p)?.len() == 1 {
                send(p, "Page.navigate", json!({"url":page::HOME_URL}))?;
            } else {
                page::close_tab(p)?;
            }
        }
        "mouse" => {
            let kind = s("kind")?;
            if !matches!(kind, "mousePressed" | "mouseReleased" | "mouseMoved") {
                return Err("invalid mouse kind".into());
            }
            send(p, "Input.dispatchMouseEvent", json!({
                "type": kind, "x": m["x"], "y": m["y"], "button": m["button"],
                "buttons": m["buttons"], "clickCount": m["clicks"], "modifiers": m["mods"],
            }))?;
        }
        "wheel" => send(p, "Input.dispatchMouseEvent", json!({
            "type": "mouseWheel", "x": m["x"], "y": m["y"], "deltaX": m["dx"],
            "deltaY": m["dy"], "modifiers": m["mods"],
        }))?,
        "key" => {
            let kind = s("kind")?;
            if !matches!(kind, "keyDown" | "keyUp" | "rawKeyDown" | "char") {
                return Err("invalid key kind".into());
            }
            send(p, "Input.dispatchKeyEvent", json!({
                "type": kind, "key": m["key"], "code": m["code"], "windowsVirtualKeyCode": m["vk"],
                "nativeVirtualKeyCode": m["vk"], "modifiers": m["mods"], "text": m["text"],
            }))?;
        }
        "text" => send(p,"Input.insertText",json!({"text":s("text")?}))?,
        "copy" => {
            let text = p.selection()?;
            emit(out, &json!({"t":"copy","text":text}))?;
        }
        "dialog" => {
            let mut params = json!({"accept":m["accept"]});
            if let Some(text) = m["text"].as_str() {
                params["promptText"] = json!(text);
            }
            p.call("Page.handleJavaScriptDialog", params)?;
        }
        _ => return Err("unknown native view command".into()),
    }
    Ok(())
}
