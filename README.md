# Herdr Browser

A native browser tab and agent-owned browser view for Herdr Desktop (`herdr-app`) and herdr-core (`hc`). Each agent terminal has its own page. It works headlessly without the desktop open and does not use the separate Herdr TUI.

[Build recipe](RECIPE.md) · [Native browser preview](preview.png) · [Optional key bindings](keys.json)

## Install locally

The installer needs Rust/Cargo, Python 3, `hc`, and `herdr-app`. The desktop mods can run the installed local helper. The helper can launch a dedicated Chrome, access the network, and write its profile, downloaded browser and screenshot files under project-owned paths. It does not use your everyday browser profile or keychain. The installer validates and installs both mods, removes only the exact obsolete stock terminal launcher, and preserves custom program entries, unrelated shortcuts and locally edited installed files.

```sh
./scripts/install
~/.local/bin/herdr-browser install-browser
```

Add `~/.local/bin` to `PATH` if needed. The overlay mod is `herdr-browser`, the native tab mod is `herdr-browser-tab`, and the OMP skill is `skill://herdr-browser`. The browser profile lives under `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-browser/profile`. The `console` and `network` recorder runs in the page's main world on HTTP(S) pages and is not a tamper-proof audit log.

The installer pins Chrome for Testing `155.0.8059.39`. `158.0.8127.36` timed out before issuing requests on the tested Mac; `155.0.8059.39` passed the end-to-end browser test. Use this build for trusted previews and retest a current Chrome build before publishing.

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

For a named profile, put `--profile work` before the command on every invocation, for example `herdr-browser --profile work send screenshot`. Refusals are not deliveries. `herdr-browser stop` closes the dedicated Chrome for the selected profile; an owned view can reconnect after an explicit `open`, while a standalone view's **Reconnect** starts a new target.

## Browser panel

Open the Browser panel from a **local** space using the command palette's **Browser** entry, the installed Control–Command–B shortcut (`cmd-ctrl-b`), or `herdr-app do mod.open --name herdr-browser`. Custom Browser bindings are preserved. The installer removes the exact former global Browser defaults `cmd-shift-y` and `cmd-alt-b` only when it installs the new shortcut or another Browser binding exists. If the new key is occupied and no custom Browser binding exists, the existing configuration is preserved; use the palette or command. Remote spaces must switch to a local space; remote localhost is never opened locally.

1. On **Browser**, select **Default** or a listed Herdr profile, or type a new name using 1–64 lowercase letters, digits, hyphens or underscores. The selection is remembered. The editable start URL is prefilled with `https://www.google.com`.
2. Choose **Sign in & open tab** to launch a dedicated manual Chrome for Testing window with the selected profile and URL. Sign in yourself, then quit Chrome; the native browser tab connects with that profile and URL. For a profile already signed in, choose **Open in tab** instead.
3. Under **Chrome settings**, install Chrome for Testing if it is missing; installation runs in a separate terminal tab. If the selected profile is already running before manual sign-in, choose **Stop Chrome for this profile** there.

The standalone tab needs no terminal pane and has no agent recipient. Use the agent shortcuts below or commands above for an agent-owned browser and sends. Closing a browser tab does not stop Chrome or the agent.

When Browser is opened from a uniquely displayed local agent, its **Browser** page supports these default overlay-scoped shortcuts: **Cmd–Enter** opens the entered URL beside that agent (an empty URL shows the existing view); **Cmd–Shift–Enter** shows or reuses its view without navigation; **Cmd–Option–S** sends the selection; **Cmd–Option–X** sends a screenshot. They use the selected profile and original agent; sends keep the dialog open. The installer leaves occupied `mod:herdr-browser` keys intact and reports each conflict. The named commands are also available while the overlay is open on the Browser page, for example `herdr-app do herdr-browser.show`. In the terminal owned view, **Ctrl-S** sends the selection and **Ctrl-X** sends a screenshot.

The native tab displays the page in its viewport. Use the address bar or Ctrl/Cmd-L, and Ctrl/Cmd-R to reload; the toolbar arrows navigate back and forward. Its native tab strip manages browser tabs inside the view, independently of Herdr's tab strip. Existing Herdr shortcuts retain their app-level bindings.

The terminal view, launched with `herdr-browser view --standalone`, has additional Find, Files, zoom, bookmarks and search controls and function-key shortcuts. These advanced controls are not part of the native tab UI. Its **Files** control shows downloads and opens a native file picker for page uploads.
## Sign in and reuse a profile
New standalone views and browser tabs open [Google](https://www.google.com) unless an explicit start URL is provided. Opening an existing owned browser view leaves its page unchanged.
```sh
herdr-browser --profile work login https://app.example.com
# Finish sign-in yourself, then quit that Chrome for Testing session.
herdr-browser --profile work open https://app.example.com --show
```

Use the same `--profile work` on later commands. `stop` and `status` apply to the selected profile only; stop it before opening its sign-in window. Named profiles retain their own cookies and storage across stops. Without `--profile`, the helper uses the default profile. It never imports your everyday browser's cookies or keychain. An agent authorized to use a signed-in profile can see the sessions in it.
Same-tab HTTP redirects work normally. A sign-in popup with a known opener stays in that owner's view; closing the popup returns to its opener. The manual sign-in window has no DevTools listener or diagnostics recorder. It does not override a provider's browser policy.

## Credits

Adapted from [Matt Coles’s herdr-web-browser](https://github.com/MattJColes/herdr-web-browser), [release 1.3.3](https://github.com/MattJColes/herdr-web-browser/releases/tag/v1.3.3), pinned at [3e648606e2d06a03ac3a3388ae468698e53619c5](https://github.com/MattJColes/herdr-web-browser/tree/3e648606e2d06a03ac3a3388ae468698e53619c5). Reused work includes the terminal renderer and controls, Chrome for Testing and CDP integration, page scripts and recorder, selection/screenshot formatting, browser icon, and demo. This is a separate Herdr Desktop/herdr-core project. The upstream MIT notice is in [LICENSE](LICENSE) and included in the installation.
