# Herdr Browser

Open a Browser dialog in Herdr Desktop to choose a site and a dedicated Chrome profile, then open a native browser tab. Sign in yourself when you need a session, or open the browser directly. From a local agent pane, the dialog also shows shortcuts and terminal commands for opening that agent’s browser and sending it a selection or screenshot.

![Herdr Browser](preview.png)

## What you see

- **Header:** a 680 × 840 dialog with 18 px padding, a theme `popover` background, and 13 px `foreground` text. An 18 px semibold **Browser** title sits opposite **Close**.
- **Left rail:** a 148 px column with a `border` divider and two destinations: **Browser** and **Chrome settings**. The selected row uses `raised` and semibold text; rows use `hover` on pointer-over.
- **Browser page:** a vertically scrollable column with site and profile inputs, profile chips, **Sign in & open tab**, **Open in tab**, and agent command rows with configured shortcut labels and CLI forms.
- **Chrome settings:** the same header and rail, with the selected profile name, **Install Chrome for Testing**, and **Stop Chrome for this profile**. The body shows muted progress or results and uses the theme’s `blocked` color for warnings and errors. These are Browser states, not agent statuses: there are no working, blocked, done, or idle agent badges.

## How it behaves

1. Open **Browser** from a local space. **Close** dismisses the dialog; the left rail switches pages in place. The site starts at Google. Choose **Default**, click a saved profile, or type a new name. The dialog remembers the selected profile.
   Use Up/Down to move through menu controls, Left to return to the modal sidebar row for the current section, and Enter to select. Text input keys retain their native behavior.
2. **Sign in & open tab** starts a manual Chrome for Testing window for the chosen profile and site. Sign in, then quit Chrome for Testing so the native browser tab can connect. **Open in tab** skips manual sign-in. A standalone tab has no agent recipient.
3. From a uniquely displayed local agent, the Browser page’s default shortcuts are **Cmd–Enter** to open the entered URL beside that agent (show without navigating if empty), **Cmd–Shift–Enter** to show or reuse its view, **Cmd–Option–S** to send selected text, and **Cmd–Option–X** to send a screenshot. Open and show close the dialog after success; sends keep it open and show the helper’s result. Changing focus cannot retarget the action.
4. In the native view, use the address bar or **Ctrl/Cmd–L** and the toolbar's back, forward and reload controls. The browser's own tab strip is independent of Herdr's. Closing the tab does not stop Chrome. Use **Chrome settings** to stop only the selected profile or open a separate **Install Chrome** tab.

## Built from

- The overlay mod lives in `mods/herdr-browser`; `mods/herdr-browser-tab` provides the native tab and renders the helper's browser frames. The bundled Rust helper manages Chrome, CDP, browser sessions, and agent ownership.
- Open the overlay with `herdr-app do mod.open --name herdr-browser` or the Browser palette entry. The optional `keys.json` proposes global Control–Command–B (`cmd-ctrl-b`), as does the installer. Existing custom Browser bindings and occupied keys are preserved; exact global legacy Browser defaults are removed only when a replacement or custom Browser binding exists.
- The overlay-scoped commands `herdr-browser.open`, `herdr-browser.show`, `herdr-browser.selection`, and `herdr-browser.screenshot` target the original validated agent. The helper accepts `--owner ADDRESS` and optional `--profile NAME`, followed by `open URL --show`, `show`, `send selection`, or `send screenshot`; sends go only to the validated originating agent.
- Adapted from [Matt Coles’s herdr-web-browser](https://github.com/MattJColes/herdr-web-browser), [release 1.3.3](https://github.com/MattJColes/herdr-web-browser/releases/tag/v1.3.3), pinned at [3e648606e2d06a03ac3a3388ae468698e53619c5](https://github.com/MattJColes/herdr-web-browser/tree/3e648606e2d06a03ac3a3388ae468698e53619c5). Keep the upstream MIT notice, `Copyright (c) 2026 Matt Coles`, in [LICENSE](LICENSE) and with the installed files. This version uses Herdr Desktop and herdr-core, not the Herdr TUI.

## Watch out

- Read the package before installing. It runs local programs through `exec`, `herdr-app`, and `hc`; downloads Chrome for Testing; accesses sites over the network; and reads and writes dedicated state and files outside the mod folder. Use trusted previews. The HTTP(S) console/network recorder is not a tamper-proof log.
- Installation requires Rust/Cargo, Python 3, `hc`, and `herdr-app`. Use `scripts/install` to install the helper, recorder, license, both desktop mods, and OMP skill; provision Chrome separately with `~/.local/bin/herdr-browser install-browser` or the settings button. Preserve occupied shortcuts and unrelated settings. If the default launcher shortcut is occupied, use the Browser palette entry or named command.
- Profile names are 1–64 lowercase letters, digits, hyphens, or underscores; blank means default. Under `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-browser`, Chrome uses `profile` for default and `profiles/<name>/profile` for named sessions. Sign-in never imports ordinary Chrome cookies or the keychain. Stop a running profile explicitly before manual sign-in. Agents authorized to use a signed-in profile can access its session; separate page ownership does not separate cookies within a shared profile.
- Refuse remote, stale, or ambiguous placement rather than borrowing focus or opening remote localhost on this Mac. A standalone tab has no agent recipient. A successful navigation, a requested tab, and a successful send are separate outcomes; don’t report a page rendered merely because the tab was requested. Surface helper errors and send refusals.

## Done when

1. The Browser dialog matches the preview: title and Close, two-page rail, selected profile chip, both standalone launch buttons, and agent command rows with the shortcuts actually bound. Chrome settings shows the selected profile and its install and stop actions.
2. Direct launch opens a native browser tab at the entered site; manual sign-in connects after the dedicated Chrome window quits. A standalone tab has no agent recipient.
3. Opening and showing from the same local agent and profile reuse its owned view; show alone leaves the page unchanged. A selection and a readable screenshot reach only that agent. Remote or stale placement, an empty selection, or a refused send does not report success.
