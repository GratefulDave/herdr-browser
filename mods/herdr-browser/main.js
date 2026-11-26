// @ts-check
// Adapted from Matt Coles' herdr-web-browser. See README.md for the pinned source and license.
import { View, div } from "gpui-kit";
import { v_flex, h_flex, InputState, Input, Button, ScrollHandle } from "gpui-base";
import { exec, focused, panes, spaces, agents, props, closeOverlay, home, appHome, theme, checking, shared, setShared, run, nextCommand, keys } from "herdr";

const BIN = `${home()}/.local/bin/herdr-browser`;
const PROFILE_KEY = "herdr-browser.profile";
const validProfile = (name) => name === "" || /^[a-z0-9_-]{1,64}$/.test(name);

const AGENT_COMMANDS = [
  ["open", "Open the entered URL", "open https://example.com --show"],
  ["show", "Show without navigating", "show"],
  ["selection", "Send selected page text", "send selection"],
  ["screenshot", "Send a screenshot", "send screenshot"],
];

function keyLabel(key) {
  return key.replace(/cmd-/g, "⌘").replace(/shift-/g, "⇧").replace(/alt-/g, "⌥")
    .replace(/ctrl-/g, "⌃").replace(/enter\b/g, "↵").replace(/([⌘⇧⌥⌃])([a-z])$/, (_, modifier, letter) => modifier + letter.toUpperCase());
}

function localPanes() {
  const localTabs = new Set(spaces().filter((s) => s.machine === null).flatMap((s) => s.tabs.map((t) => t.id)));
  return panes().filter((p) => p.terminal && localTabs.has(p.tab));
}



export default class Browser extends View {
  init(_props, cx) {
    const f = focused();
    const matches = localPanes().filter((p) => p.pane === f?.pane && p.terminal === f?.terminal);
    this.launchTerminal = matches.length === 1 ? matches[0].terminal : null;
    this.launchPane = matches.length === 1 ? matches[0].pane : null;
    this.launchTab = matches.length === 1 ? matches[0].tab : null;
    this.launchAgent = this.launchTerminal && agents().filter((a) => a.address === this.launchTerminal && a.pane === this.launchPane && a.tab === matches[0].tab).length === 1 ? this.launchTerminal : null;
    this.handle = cx.focus_handle();
    this.handle.focus();
    const saved = shared(PROFILE_KEY);
    this.profile = typeof saved === "string" && validProfile(saved) ? saved : "";
    this.profileInput = InputState.new({ value: this.profile, placeholder: "Default (or type a new profile name)" });
    this.urlInput = InputState.new({ value: "https://www.google.com", placeholder: "Start URL" });
    this.selection = "hb-nav-browser";
    this.bodyScroll = ScrollHandle.new();
    this.profileScroll = ScrollHandle.new();
    this.urlInput.on("submit", () => this.handle.focus());
    this.profileInput.on("submit", () => this.handle.focus());
    this.urlInput.on("focus", (_e, cx) => { this.selection = "hb-url"; cx.notify(); });
    this.profileInput.on("focus", (_e, cx) => { this.selection = "hb-profile-input"; cx.notify(); });
    this.profileInput.on("change", (_e, cx) => {
      this.profile = this.profileInput.value();
      this.error = "";
      cx.notify();
    });
    this.urlInput.on("change", (_e, cx) => { this.error = ""; cx.notify(); });
    this.profiles = [];
    this.loading = !checking();
    this.busy = false;
    this.error = "";
    this.section = "browser";
    if (!checking()) cx.spawn(async (cx) => {
      for (;;) {
        const command = await nextCommand();
        if (command === "focus") this.handle.focus();
        else if (this.section === "browser" &&
          (command === "open" || command === "show" || command === "selection" || command === "screenshot")) {
          this.agentAction(command, cx);
        }
      }
    });
    if (!checking()) cx.spawn(async (cx) => {
      try {
        const result = await exec(BIN, ["--json", "profiles"]);
        if (result.code !== 0) throw new Error(result.stderr.trim() || `Profile discovery failed (${result.code})`);
        const data = JSON.parse(result.stdout);
        if (!Array.isArray(data.profiles) || data.profiles.some((p) => typeof p !== "string" || !p || !validProfile(p))) throw new Error("Invalid profile list from browser helper.");
        this.profiles = data.profiles;
      } catch (error) { this.error = String(error); }
      this.loading = false;
      cx.notify();
    });
  }

  profileArgs() {
    return this.profile ? ["--profile", this.profile] : [];
  }

  ready() {
    if (!validProfile(this.profile)) {
      this.error = "Profile: use 1–64 lowercase letters, digits, - or _.";
      return false;
    }
    const space = spaces().find((s) => s.active);
    if (!space || space.machine !== null) {
      this.error = "Switch to a local space before opening or controlling the browser.";
      return false;
    }
    return true;
  }

  chooseProfile(name, cx) {
    this.profile = name;
    this.profileInput.set_value(name);
    setShared(PROFILE_KEY, name || null);
    this.error = "";
    cx.notify();
  }

  agentAction(action, cx) {
    if (this.busy || !this.ready()) { cx.notify(); return; }
    const matches = localPanes().filter((p) => p.pane === this.launchPane && p.terminal === this.launchAgent && p.tab === this.launchTab);
    if (!this.launchAgent || matches.length !== 1 || agents().filter((a) => a.address === this.launchAgent && a.pane === this.launchPane && a.tab === this.launchTab).length !== 1) {
      this.error = "The original local agent pane is no longer uniquely available.";
      cx.notify();
      return;
    }
    const url = this.urlInput.value().trim();
    const command = action === "open" && url ? ["open", url, "--show"]
      : action === "open" || action === "show" ? ["show"] : ["send", action];
    const profile = this.profile;
    this.busy = true;
    this.error = "";
    this.message = "";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const result = await exec("/usr/bin/env", [`HERDR_APP_HOME=${appHome()}`, BIN, "--owner", this.launchAgent, ...(profile ? ["--profile", profile] : []), ...command], { timeout: 120000 });
        if (result.code !== 0) throw new Error(result.stderr.trim() || result.stdout.trim() || `Command failed (${result.code}).`);
        setShared(PROFILE_KEY, profile || null);
        if (action === "open" || action === "show") {
          this.busy = false;
          closeOverlay(props().overlay);
          return;
        }
        this.message = result.stdout.trim() || `Send ${action} accepted.`;
      } catch (error) { this.error = String(error); }
      this.busy = false;
      cx.notify();
    });
  }


  openTab(signIn, cx) {
    if (this.busy || !this.ready()) { cx.notify(); return; }
    const space = spaces().find((s) => s.active).id;
    const profile = this.profile;
    const url = this.urlInput.value().trim();
    try {
      run("tab.new", {
        name: "Browser",
        space,
        mod: "herdr-browser-tab",
        props: { ...(profile ? { profile } : {}), ...(url ? { url } : {}), ...(signIn ? { signIn: true } : {}) },
      });
      setShared(PROFILE_KEY, profile || null);
      closeOverlay(props().overlay);
    } catch (error) { this.error = String(error); cx.notify(); }
  }
  tab(cx) {
    if (this.busy || !this.ready()) { cx.notify(); return; }
    const space = spaces().find((s) => s.active).id;
    try {
      run("tab.new", { name: "Install Chrome", space, argv: [BIN, ...this.profileArgs(), "install-browser"] });
      closeOverlay(props().overlay);
    } catch (error) { this.error = String(error); cx.notify(); }
  }

  stopChrome(cx) {
    if (this.busy || !this.ready()) { cx.notify(); return; }
    const profileArgs = this.profileArgs();
    this.busy = true;
    this.error = "";
    this.message = "";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const result = await exec(BIN, [...profileArgs, "stop"], { timeout: 120000 });
        if (result.code !== 0) throw new Error(result.stderr.trim() || `Command failed (${result.code})`);
        this.message = result.stdout.trim() || "Selected profile stopped.";
      } catch (error) { this.error = String(error); }
      this.busy = false;
      cx.notify();
    });
  }

  row(t, id, label, fn) {
    return div().id(id).px(8).py(4).rounded(6).cursor_pointer()
      .when(this.selection === id, (el) => el.bg(t.accent).text_color(t.accent_foreground))
      .hover((s) => s.bg(t.hover)).on_click(fn).child(label);
  }

  button(t, id, label, primary, fn) {
    return Button.new(id).self_start().items_center().px(12).py(7).rounded(6).border(1)
      .border_color(primary ? t.solid : t.foreground).bg(primary ? t.solid : t.background)
      .text_color(primary ? t.solid_foreground : t.foreground).font_semibold().cursor_pointer()
      .when(this.selection === id, (el) => el.border_color(t.accent).bg(t.accent).text_color(t.accent_foreground))
      .hover((s) => primary ? s.bg(t.accent).border_color(t.accent).text_color(t.accent_foreground) : s.bg(t.hover))
      .on_click(fn).child(label);
  }

  profileChip(t, id, label, selected, fn) {
    return div().id(id).px(8).py(4).rounded(6).border(1)
      .border_color(selected ? t.accent : t.input).bg(selected ? t.raised : t.surface)
      .when(selected, (el) => el.font_semibold())
      .when(this.selection === id, (el) => el.border_color(t.accent).bg(t.accent).text_color(t.accent_foreground))
      .cursor_pointer().hover((s) => s.bg(t.hover)).on_click(fn).child(label);
  }

  commandHelp(t) {
    const bindings = keys();
    return v_flex().gap(3).pt(6).border_t(1).border_color(t.border)
      .child(div().font_semibold().child("Agent commands"))
      .child(div().text_color(t.muted).child("Focus a local agent pane for shortcuts; run commands in its terminal."))
      .children(AGENT_COMMANDS.map(([action, label, args]) => {
        const shortcuts = bindings.filter((b) => b.action === `herdr-browser.${action}`).map((b) => keyLabel(b.keys));
        return v_flex()
          .child(div().font_semibold().child(shortcuts.length ? `${label} · ${shortcuts.join(", ")}` : label))
          .child(div().text_color(t.muted).child(`herdr-browser${this.profile && validProfile(this.profile) ? ` --profile ${this.profile}` : ""} ${args}`));
      }));
  }

  render() {
    const t = theme();
    const controls = [];
    const bodyChildren = [];
    const add = (id, action, element, profileIndex = null) => {
      controls.push({ id, action, bodyIndex: bodyChildren.length, profileIndex });
      bodyChildren.push(element);
    };
    const navItems = [["browser", "Browser"], ["settings", "Chrome settings"]];
    const nav = v_flex().w(148).flex_none().gap(4).pr(12).border_r(1).border_color(t.border)
      .children(navItems.map(([section, label]) => {
        const id = `hb-nav-${section}`;
        const action = (_e, cx) => { this.selection = id; this.section = section; cx.notify(); };
        controls.push({ id, action });
        return this.row(t, id, label, action).when(this.section === section, (el) => el.font_semibold().when(this.selection !== id, (row) => row.bg(t.raised)));
      }));
    if (spaces().find((s) => s.active)?.machine !== null) bodyChildren.push(div().text_color(t.blocked).child("Switch to a local space to use Browser."));
    if (this.section === "browser") {
      bodyChildren.push(div().font_semibold().child("1. Choose a site and profile"));
      bodyChildren.push(div().child("Site address"));
      const field = (id, state) => Input.new(state).h(30).px(8).items_center().rounded(6).border(1)
        .border_color(this.selection === id ? t.accent : t.input).bg(this.selection === id ? t.raised : t.background).text_color(t.foreground);
      add("hb-url", () => this.urlInput.focus(), field("hb-url", this.urlInput));
      bodyChildren.push(div().child("Profile"));
      add("hb-profile-input", () => this.profileInput.focus(), field("hb-profile-input", this.profileInput));
      bodyChildren.push(div().text_color(t.muted).child("Named profiles keep separate sessions; everyday Chrome logins are not imported."));
      bodyChildren.push(div().text_color(t.muted).child(this.loading ? "Loading profiles…" : "Saved profiles:"));
      const profileBodyIndex = bodyChildren.length;
      const defaultAction = (_e, cx) => this.chooseProfile("", cx);
      controls.push({ id: "hb-default", action: defaultAction, bodyIndex: profileBodyIndex });
      const chips = this.profiles.map((p, index) => {
        const id = `hb-profile-${p}`;
        const action = (_e, cx) => this.chooseProfile(p, cx);
        controls.push({ id, action, bodyIndex: profileBodyIndex, profileIndex: index });
        return this.profileChip(t, id, this.profile === p ? `✓ ${p}` : p, this.profile === p, action);
      });
      bodyChildren.push(h_flex().gap(4)
        .child(this.profileChip(t, "hb-default", this.profile === "" ? "✓ Default" : "Default", this.profile === "", defaultAction))
        .child(h_flex().id("hb-profiles").flex_1().overflow_x_scroll().track_scroll(this.profileScroll).gap(4).children(chips)));
      bodyChildren.push(div().font_semibold().child("2. Start the browser"));
      const login = (_e, cx) => this.openTab(true, cx);
      add("hb-login", login, this.button(t, "hb-login", "Sign in & open tab", true, login));
      bodyChildren.push(div().text_color(t.muted).child("3. Finish signing in and quit Chrome for Testing; the tab then connects."));
      bodyChildren.push(div().text_color(t.muted).child("Already signed in?"));
      const open = (_e, cx) => this.openTab(false, cx);
      add("hb-open-tab", open, this.button(t, "hb-open-tab", "Open in tab", false, open));
      bodyChildren.push(this.commandHelp(t));
    } else {
      bodyChildren.push(div().font_semibold().child("Chrome settings"));
      bodyChildren.push(div().text_color(t.muted).child(`Selected profile: ${this.profile || "Default"}. Change it on the Browser page.`));
      const install = (_e, cx) => this.tab(cx);
      add("hb-install", install, this.button(t, "hb-install", "Install Chrome for Testing", false, install));
      bodyChildren.push(div().text_color(t.muted).child("Closing a browser tab exits the view, not Chrome. Stop shuts down Chrome for the selected profile."));
      const stop = (_e, cx) => this.stopChrome(cx);
      add("hb-stop", stop, this.button(t, "hb-stop", "Stop Chrome for this profile", false, stop));
    }
    if (this.busy) bodyChildren.push(div().text_color(t.muted).child("Working…"));
    if (this.error) bodyChildren.push(div().text_color(t.blocked).child(this.error));
    if (this.message && !this.error) bodyChildren.push(div().text_color(t.muted).child(this.message));
    const close = () => closeOverlay(props().overlay);
    controls.push({ id: "hb-dismiss", action: close });
    const body = v_flex().id(`hb-body-${this.section}`).flex_1().min_w(0).h_full().overflow_y_scrollbar().track_scroll(this.bodyScroll).pr(8).gap(8).children(bodyChildren);
    return v_flex().id("root").track_focus(this.handle).size_full().min_h(0).bg(t.popover).p(18).gap(14).text_size(13).text_color(t.foreground)
      .on_key_down((e, cx) => {
        if (!this.handle.is_focused() || e.modifiers.shift || e.modifiers.alt || e.modifiers.control || e.modifiers.platform) return;
        const index = controls.findIndex((item) => item.id === this.selection);
        if (e.key === "down" || e.key === "up") {
          const step = e.key === "down" ? 1 : -1;
          const next = controls[index < 0 ? (step === 1 ? 0 : controls.length - 1) : (index + step + controls.length) % controls.length];
          this.selection = next.id;
          if (next.bodyIndex !== undefined) this.bodyScroll.scroll_to_item(next.bodyIndex);
          if (next.profileIndex != null) this.profileScroll.scroll_to_item(next.profileIndex);
        } else if (e.key === "left") this.selection = `hb-nav-${this.section}`;
        else if (e.key === "enter" && index >= 0) controls[index].action(e, cx);
        else return;
        cx.stop_propagation();
        cx.notify();
      })
      .child(h_flex().items_center().justify_between()
        .child(div().text_size(18).font_semibold().child("Browser"))
        .child(this.button(t, "hb-dismiss", "Close", false, close)))
      .child(div().text_size(11).text_color(t.muted).child("↑/↓ Navigate · ← Sidebar · Enter select"))
      .child(h_flex().flex_1().min_h(0).gap(16).items_start().child(nav).child(body));
  }
}
