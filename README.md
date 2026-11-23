# Herdr Browser

A live browser split for Herdr Desktop (`herdr-app`) and herdr-core (`hc`). Each agent terminal has its own page. It works headlessly without the desktop open and does not use the separate Herdr TUI.

Built from [Matt Coles’s original terminal browser, `herdr-web-browser`](https://github.com/MattJColes/herdr-web-browser). This adaptation uses Herdr Desktop and `hc`.

[Build recipe](RECIPE.md) · [Preview](preview.png) · [Optional key bindings](keys.json)

## Install locally

The installer needs Rust/Cargo, Python 3, `hc`, and `herdr-app`; downloading Chrome for Testing also needs `curl` and `unzip`. The mod runs the installed local helper. The helper can launch dedicated Chrome, access the network, and write profiles, downloads, and screenshots. It does not use your everyday browser profile or keychain. The installer preserves unrelated entries, occupied shortcuts, and locally edited installed files. No layout is installed.

```sh
./scripts/install
~/.local/bin/herdr-browser install-browser
```

Add `~/.local/bin` to `PATH` if needed. The Herdr Desktop mod is `herdr-browser`; the OMP skill is `skill://herdr-browser`. The browser profile lives under `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-browser/profile`. The `console` and `network` recorder runs in the page's main world on HTTP(S) pages and is not a tamper-proof audit log.

The default installer pins Chrome for Testing `155.0.8059.39`, the build verified end to end on macOS. `158.0.8127.36` was not compatible in that test. Use this build for trusted previews.

## Use from an agent terminal

```sh
herdr-browser open localhost:3000 --show
herdr-browser snapshot
herdr-browser type '#note' 'example'
herdr-browser click 3       # use the actual snapshot reference
herdr-browser console
herdr-browser network
```

Omit `--show` for a headless navigation. From the same agent terminal, `herdr-browser show` opens or reuses its view without navigating. From outside the originating terminal, supply `--owner <exact-local-hc-address>`. No command falls back to the focused page or another terminal. If navigation succeeded but display failed, fix desktop placement and retry `show`.

To send back to the originating agent, run these commands from its terminal after selecting page text or preparing the page:

```sh
herdr-browser send selection
herdr-browser send screenshot
```

For a named profile, put `--profile work` before the command on every invocation, for example `herdr-browser --profile work send screenshot`. Refusals are not deliveries. `herdr-browser stop` closes the dedicated Chrome for the selected profile; owned views reconnect after an explicit `open`, while standalone views must be reopened.

## Browser panel

Open the Browser panel from a **local** space with the installed shortcut (Cmd–Shift–B, or Cmd–Option–B if the primary shortcut was occupied and the fallback was free), or run `herdr-app do mod.open --name herdr-browser`. If both shortcuts were occupied, the named command remains available. Remote spaces must switch to a local space; remote localhost is never opened locally.

1. On **Browser**, select **Default** or a listed Herdr profile, or type a new name using 1–64 lowercase letters, digits, hyphens or underscores. The selection is remembered. The editable start URL is prefilled with `https://www.google.com`.
2. From a local terminal pane, choose **Sign in & open pane** to split a standalone browser pane to its right and launch a dedicated manual Chrome for Testing window with the selected profile and URL. Sign in yourself, then close Chrome; the browser pane starts with that same profile and URL. For a profile already signed in, choose **Open in pane** instead.
3. Under **Chrome settings**, install Chrome for Testing if it is missing; installation runs in a separate terminal tab. If the selected profile is already running before manual sign-in, choose **Stop Chrome for this profile** there. The settings page identifies the selected profile; nothing is stopped automatically.

The panel requests a native right-hand pane split without creating or changing layouts. Its standalone pane is independent of any agent and cannot send to one. Use the agent shortcuts below or the commands above for an agent-owned browser and sends. Check the browser pane for startup or sign-in errors. Click **Close pane** or press Ctrl-Q in the browser view to close it; neither stops the agent, and stopping Chrome is not needed to close the pane.

When Browser is opened from a uniquely displayed local agent, its **Browser** page supports these default overlay-scoped shortcuts: **Cmd–Enter** opens the entered URL beside that agent (an empty URL shows the existing view); **Cmd–Shift–Enter** shows or reuses its view without navigation; **Cmd–Option–S** sends the selection; **Cmd–Option–X** sends a screenshot. They use the selected profile and original agent; sends keep the dialog open. The installer leaves occupied `mod:herdr-browser` keys intact and reports each conflict; it does not change global shortcuts for these actions. While the overlay is open on the Browser page, the named commands are also available, for example `herdr-app do herdr-browser.show`. In the owned browser pane, **Ctrl-S** sends the selection and **Ctrl-X** sends a screenshot.

In the live view, click the address bar (or press Ctrl-L), type or paste an address, then press Enter. The toolbar has Back, Forward and Reload (Ctrl-B, Ctrl-F and Ctrl-R). Existing Herdr shortcuts keep their app-level bindings.

| Browser control | What it does |
| --- | --- |
| **− / % / +** (F5 / F7 / F6) | Zooms the page out, resets to 100%, or zooms in without resizing Herdr. |
| **Find** (F2) | Enter searches visible text on the main page. F3 and F4 move to the next and previous match; Esc closes Find. |
| **+ Tab / Next / Close** (F8 / F9 / F10) | Manages browser tabs within this view, not Herdr tabs. Owned CLI commands use the selected tab of that owner. |
| **Bookmark** (F11) | Saves or removes the current HTTP(S) page. Open a site before bookmarking; blank and internal pages are not saved. When editing an address, Up/Down chooses a history or bookmark suggestion and Enter opens it. |
| **Search** | Sets the profile’s HTTPS search URL with one `{query}` placeholder. Text that is not an address goes to this search engine. |
| **Files** (F12) | Shows download progress and file activity. **Download folder** changes the destination for the running profile. Click a page’s file input to select a file with the macOS picker. |

Herdr Desktop can intercept Cmd–Plus, Cmd–Minus, Cmd–0, and Cmd–F before a terminal sees them. Use the toolbar or function keys for page actions; the installer does not change those Herdr bindings.

New browser panes and browser tabs open [Google](https://www.google.com) unless an explicit start URL is provided. Opening an existing owned browser pane leaves its page unchanged. **+ → Browser** opens a standalone view using the default profile and has no agent recipient.

## Sign in and reuse a profile

```sh
herdr-browser --profile work login https://app.example.com
# Finish sign-in yourself, then quit that Chrome for Testing session.
herdr-browser --profile work open https://app.example.com --show
```

Use the same `--profile work` on later commands. `stop` and `status` apply to the selected profile only; stop it before opening its sign-in window. Named profiles retain their own cookies and storage across stops. Without `--profile`, including in **+ → Browser**, the helper uses the default profile. It never imports your everyday browser's cookies or keychain. An agent authorized to use a signed-in profile can see the sessions in it.

Same-tab HTTP redirects work normally. A sign-in popup with a known opener stays in that owner's view; closing the popup returns to its opener. The manual sign-in window has no DevTools listener or diagnostics recorder. It does not override a provider's browser policy.

## Credits

Adapted from [Matt Coles’s herdr-web-browser](https://github.com/MattJColes/herdr-web-browser), [release 1.3.3](https://github.com/MattJColes/herdr-web-browser/releases/tag/v1.3.3), pinned at [3e648606e2d06a03ac3a3388ae468698e53619c5](https://github.com/MattJColes/herdr-web-browser/tree/3e648606e2d06a03ac3a3388ae468698e53619c5). Reused work includes the terminal renderer and controls, Chrome for Testing and CDP integration, page scripts and recorder, selection/screenshot formatting, browser icon, and demo. This is a separate Herdr Desktop/herdr-core project. The upstream MIT notice is in [LICENSE](LICENSE) and included in the installation.
