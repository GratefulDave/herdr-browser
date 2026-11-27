import { View, div, image } from "gpui-kit";
import { Button, Input, InputState, h_flex, v_flex } from "gpui-base";
import { appHome, checking, exec, home, nextCommand, props, theme } from "herdr";
import net from "net";

const BIN = `${home()}/.local/bin/herdr-browser`;
const KEY_CONTEXT = "HerdrBrowserPage";
const KEYS = {
  backspace: ["Backspace", "Backspace", 8], tab: ["Tab", "Tab", 9], enter: ["Enter", "Enter", 13],
  escape: ["Escape", "Escape", 27], space: [" ", "Space", 32], pageup: ["PageUp", "PageUp", 33],
  pagedown: ["PageDown", "PageDown", 34], end: ["End", "End", 35], home: ["Home", "Home", 36],
  left: ["ArrowLeft", "ArrowLeft", 37], up: ["ArrowUp", "ArrowUp", 38],
  right: ["ArrowRight", "ArrowRight", 39], down: ["ArrowDown", "ArrowDown", 40],
  delete: ["Delete", "Delete", 46],
};
// GPUI consumes these for focus navigation unless the page claims them.
const CLAIMED = ["tab", "shift-tab", "enter", "escape", "up", "down", "left", "right", "pageup", "pagedown", "home", "end",
  "shift-up", "shift-down", "shift-left", "shift-right", "alt-left", "alt-right"];
const BUTTON = { left: 1, right: 2, middle: 4 };
const modifiers = (m) => (m.alt ? 1 : 0) | (m.control ? 2 : 0) | (m.platform ? 4 : 0) | (m.shift ? 8 : 0);
const errorText = (error) => error instanceof Error ? error.message : String(error);

function utf8(bytes) {
  if (typeof TextDecoder !== "undefined") return new TextDecoder().decode(bytes);
  let encoded = "";
  for (const byte of bytes) encoded += `%${byte.toString(16).padStart(2, "0")}`;
  return decodeURIComponent(encoded);
}

export default class BrowserTab extends View {
  init(_props, cx) {
    this.options = props() || {};
    this.address = InputState.new({ placeholder: "Enter a site address" });
    this.address.on("focus", () => { this.editing = true; });
    this.address.on("blur", () => { this.editing = false; this.address.set_value(this.url); });
    this.address.on("submit", () => {
      const url = this.address.value().trim();
      if (url) this.send({ t: "navigate", url });
      this.pageFocus.focus();
    });
    this.prompt = InputState.new({ placeholder: "" });
    this.prompt.on("submit", () => this.answer(true));
    this.pageFocus = cx.focus_handle();
    this.url = this.options.url || "";
    this.address.set_value(this.url);
    this.tabs = [];
    this.size = { width: 0, height: 0 };
    this.frames = [];
    this.buttons = 0;
    this.status = checking() ? "Browser" : "Starting Chromium…";
    this.error = "";
    this.closed = false;
    if (checking()) return;
    cx.bind_keys(CLAIMED.map((keystroke) => ({ keystroke, action: `herdr-browser-page:${keystroke}`, context: KEY_CONTEXT })));
    cx.spawn(async () => {
      for (;;) {
        if (await nextCommand() === "focus") this.pageFocus.focus();
      }
    });
    this.connect(cx);
  }

  connect(cx) {
    if (this.connecting || this.closed) return;
    this.connecting = true;
    this.error = "";
    this.status = "Starting Chromium…";
    this.frames = [];
    cx.notify();
    cx.spawn(async (cx) => {
      let socket;
      try {
        const { profile, owner, url, signIn } = this.options;
        const result = await exec(BIN, [
          ...(profile ? ["--profile", profile] : []), ...(owner ? ["--owner", owner] : []),
          "native-view", "--app-home", appHome(),
          ...(owner ? [] : ["--standalone", ...(url ? ["--url", url] : []), ...(signIn ? ["--sign-in"] : [])]),
        ], { timeout: signIn ? 600000 : 45000 });
        if (result.code !== 0) throw new Error(result.stderr.trim() || result.stdout.trim() || `Browser helper exited (${result.code}).`);
        if (this.closed) return;
        const { port, token } = JSON.parse(result.stdout);
        if (!Number.isInteger(port) || port < 1 || port > 65535 || typeof token !== "string" || !token) throw new Error("Invalid browser helper endpoint.");
        socket = await net.connect("127.0.0.1", port);
        if (this.closed) return;
        await socket.write(`${JSON.stringify({ t: "hello", token })}\n`);
        this.output = { socket, queue: [], writing: false };
        this.sendSize();
        this.status = "Waiting for Chromium frames…";
        cx.notify();
        await this.read(socket, cx);
        if (!this.closed) this.status = "Browser view disconnected.";
      } catch (error) {
        if (!this.closed) { this.status = "Browser view stopped."; this.error = errorText(error); }
      } finally {
        socket?.close();
        this.output = null;
        this.connecting = false;
        this.buttons = 0;
        this.frames = [];
        this.dialog = null;
        if (!this.closed) cx.notify();
      }
    });
  }

  async read(socket, cx) {
    let parts = [], length = 0;
    for (;;) {
      const chunk = await socket.read(65536);
      if (!chunk || this.closed) return;
      let start = 0, changed = false;
      for (let i = 0; i < chunk.length; i++) {
        if (chunk[i] !== 10) continue;
        const tail = chunk.subarray(start, i);
        let line = tail;
        if (length) {
          line = new Uint8Array(length + tail.length);
          let pos = 0;
          for (const part of parts) { line.set(part, pos); pos += part.length; }
          line.set(tail, pos);
        }
        const msg = JSON.parse(utf8(line));
        if (msg.t !== "keepalive") {
          if (msg.t === "frame") {
            this.frames = this.frames.filter((frame) => frame.file !== msg.file);
            this.frames.push(msg);
            if (this.frames.length > 4) this.frames.shift();
          } else this.message(msg, cx);
          changed = true;
        }
        parts = []; length = 0; start = i + 1;
      }
      if (start < chunk.length) {
        const rest = chunk.subarray(start);
        parts.push(rest); length += rest.length;
        if (length > 1048576) throw new Error("Browser helper response exceeded 1 MiB.");
      }
      if (changed) cx.notify();
    }
  }

  message(msg, cx) {
    switch (msg.t) {
      case "page": {
        const previous = this.tabs.find((tab) => tab.active)?.id;
        const current = msg.tabs.find((tab) => tab.active)?.id;
        if (previous && previous !== current) { this.frames = []; this.dialog = null; }
        this.url = msg.url;
        this.tabs = msg.tabs;
        if (!this.editing) this.address.set_value(this.url);
        break;
      }
      case "error": this.error = msg.message; break;
      case "copy": if (msg.text) cx.write_to_clipboard(msg.text); break;
      case "dialog":
        this.dialog = msg;
        if (msg.kind === "prompt") { this.prompt.set_value(msg.defaultText || ""); this.prompt.focus(); }
        break;
      case "dialog_closed": this.dialog = null; break;
    }
  }

  send(message) {
    const out = this.output;
    if (!out) return;
    const last = out.queue.at(-1);
    if ((message.t === "resize" && last?.t === "resize") ||
        (message.kind === "mouseMoved" && last?.kind === "mouseMoved")) {
      out.queue[out.queue.length - 1] = message;
    } else {
      out.queue.push(message);
    }
    if (!out.writing) void this.flush(out);
  }

  async flush(out) {
    out.writing = true;
    try {
      while (this.output === out && out.queue.length) {
        await out.socket.write(`${JSON.stringify(out.queue.shift())}\n`);
      }
    } catch {
      out.socket.close();
    } finally {
      out.writing = false;
    }
  }

  sendSize() {
    const w = Math.round(this.size.width), h = Math.round(this.size.height);
    if (w && h) this.send({ t: "resize", w, h });
  }

  position(event) {
    const frame = this.frames[this.frames.length - 1];
    if (!frame) return null;
    const point = event.local_position;
    if (!point || !this.size.width || !this.size.height) return null;
    const limit = (value, max) => Math.max(0, Math.min(value, max - 1));
    return { x: limit(point.x * frame.w / this.size.width, frame.w), y: limit(point.y * frame.h / this.size.height, frame.h) };
  }

  mouse(kind, event, button = "none") {
    if (this.dialog) return;
    const point = this.position(event);
    if (!point) return;
    if (kind === "mousePressed") this.buttons |= BUTTON[button];
    if (kind === "mouseReleased") this.buttons &= ~BUTTON[button];
    this.send({ t: "mouse", kind, ...point, button, buttons: this.buttons, clicks: kind === "mouseMoved" ? 0 : event.click_count || 1, mods: modifiers(event.modifiers) });
  }

  named(name, mods, kind) {
    const [key, code, vk] = KEYS[name];
    const text = kind === "keyDown" && !(mods & 7) ? ({ enter: "\r", space: " " }[name] || "") : "";
    this.send({ t: "key", kind, key, code, vk, mods, text });
  }

  claimed(chord, cx) {
    const parts = chord.split("-"), key = parts.pop();
    if (this.dialog) {
      if (key === "enter" || key === "escape") { this.answer(key === "enter"); cx.notify(); }
      return;
    }
    if (parts.includes("alt")) { this.send({ t: key === "left" ? "back" : "forward" }); return; }
    const mods = parts.includes("shift") ? 8 : 0;
    this.named(key, mods, "keyDown");
    this.named(key, mods, "keyUp");
  }

  key(event, cx, down) {
    if (this.dialog) return;
    const key = event.key, mods = modifiers(event.modifiers), shortcut = mods & 6;
    if (shortcut && ["l", "r", "c", "v"].includes(key)) {
      if (!down) return;
      if (key === "l") this.address.focus();
      else if (key === "r") this.send({ t: "reload" });
      else if (key === "c") this.send({ t: "copy" });
      else {
        const text = cx.read_from_clipboard();
        if (text) this.send({ t: "text", text });
      }
      return;
    }
    if (key === "f5") { if (down) this.send({ t: "reload" }); return; }
    const kind = down ? "keyDown" : "keyUp";
    if (KEYS[key]) { this.named(key, mods, kind); return; }
    if (/^[a-z0-9]$/i.test(key)) {
      const name = key.toUpperCase();
      this.send({ t: "key", kind, key: event.key_char || key, code: `${/\d/.test(name) ? "Digit" : "Key"}${name}`,
        vk: name.charCodeAt(0), mods, text: down && !shortcut ? event.key_char || "" : "" });
    } else if (down && !shortcut && event.key_char) {
      this.send({ t: "text", text: event.key_char });
    }
  }

  answer(accept) {
    if (!this.dialog) return;
    this.send({ t: "dialog", accept, ...(accept && this.dialog.kind === "prompt" ? { text: this.prompt.value() } : {}) });
    this.dialog = null;
    this.pageFocus.focus();
  }

  render() {
    const t = theme();
    const button = (id, text, action) => Button.new(id).h(27).px(8).rounded(4)
      .hover((s) => s.bg(t.hover)).on_click(action).child(text);
    const command = (id, label) => button(id, label, () => this.send({ t: id }));
    const layer = ({ file }) => image(file).absolute().top(0).left(0).w_full().h_full();
    const area = div().id("browser-page").relative().flex_1().w_full().min_h(0).overflow_hidden().bg(t.background)
      .track_focus(this.pageFocus).key_context(KEY_CONTEXT)
      .on_size_change((size) => { this.size = size; this.sendSize(); })
      .on_key_down((e, cx) => { this.key(e, cx, true); cx.stop_propagation(); })
      .on_key_up((e, cx) => { this.key(e, cx, false); cx.stop_propagation(); })
      .on_mouse_move((e) => { if (!this.buttons) this.mouse("mouseMoved", e); })
      .on_window_mouse_move((e) => { if (this.buttons) this.mouse("mouseMoved", e, e.pressed_button); })
      .on_scroll_wheel((e) => {
        const point = this.position(e);
        if (point && !this.dialog) this.send({ t: "wheel", ...point, dx: -e.delta.x, dy: -e.delta.y, mods: modifiers(e.modifiers) });
      });
    for (const chord of CLAIMED) area.on_action(`herdr-browser-page:${chord}`, (_e, cx) => this.claimed(chord, cx));
    for (const name of ["left", "right", "middle"]) {
      area.on_mouse_down(name, (e) => { if (!this.dialog) this.pageFocus.focus(); this.mouse("mousePressed", e, name); });
      area.on_window_mouse_up(name, (e) => { if (this.buttons & BUTTON[name]) this.mouse("mouseReleased", e, name); });
    }
    for (const frame of this.frames) area.child(layer(frame));
    if (!this.frames.length) area.child(v_flex().size_full().items_center().justify_center().gap(8)
      .child(this.status).when(!this.connecting && !checking(), (el) => el.child(button("reconnect", "Reconnect", (_e, cx) => this.connect(cx)))));
    if (this.dialog) area.child(div().absolute().top(0).left(0).size_full().flex().items_center().justify_center().bg("#00000066")
      .child(v_flex().w(400).max_w_full().p(16).gap(10).rounded(8).bg(t.popover).child(this.dialog.message || "Leave this page?")
        .when(this.dialog.kind === "prompt", (el) => el.child(Input.new(this.prompt)))
        .child(h_flex().gap(8).justify_end()
          .when(this.dialog.kind !== "alert", (el) => el.child(button("cancel", "Cancel", (_e, cx) => { this.answer(false); cx.notify(); })))
          .child(button("accept", "OK", (_e, cx) => { this.answer(true); cx.notify(); })))));
    return v_flex().size_full().min_h(0).bg(t.chrome).text_color(t.foreground).text_size(13)
      .child(h_flex().id("browser-tabs").flex_none().min_w(0).overflow_x_scroll().gap(4).px(6).py(3)
        .children(this.tabs.map(({ id, title, active }) => h_flex().gap(2)
          .child(button(`tab-${id}`, (title || "New tab").slice(0, 36), () => this.send({ t: "select_tab", id }))
            .when(active, (el) => el.bg(t.raised)))
          .when(active, (el) => el.child(command("close_tab", "×")))))
        .child(command("new_tab", "+")))
      .child(h_flex().flex_none().items_center().gap(4).px(6).py(3)
        .child(command("back", "←")).child(command("forward", "→")).child(command("reload", "↻"))
        .child(Input.new(this.address).flex_1().min_w(0).h(28).bg(t.input).rounded(4)))
      .when(Boolean(this.error), (el) => el.child(div().px(8).py(4).text_color(t.blocked).child(this.error)))
      .child(area);
  }

  close() {
    this.closed = true;
    this.output?.socket.close();
  }
}
