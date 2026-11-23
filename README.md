# Rdio Sherlock

A small, portable Windows desktop player for [rdio-scanner](https://github.com/chuot/rdio-scanner) servers — built for listening in the background, all day, without a browser window getting in the way.

Point it at **any** rdio-scanner server, enter the access code if that server uses one, and it just plays. Your server is remembered on that machine, so after the first setup it connects on its own every launch.

And yes, before anyone complains - this app was coded primarily by AI, but has been thoroughly tested by me over a monthly period. That will always be the case before any new updates go live, to make sure each function works as it should without any abnormal effects to the system. Linux and Mac builds go untested however. So use them at your own risk. 

![Platform: Windows](https://img.shields.io/badge/platform-Windows-blue) ![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-orange)


<h2 align="center">Screenshots</h2>

<table align="center">
  <tr>
    <td align="center" width="50%">
      <img src="https://github.com/user-attachments/assets/704a3190-fefa-433a-b23b-1b80d810a270" alt="Rdio Sherlock main window" width="100%">
      <br><sub><b>Main window</b><br>The scanner, in its own app window</sub>
    </td>
    <td align="center" width="50%">
      <img src="https://github.com/user-attachments/assets/e921151f-3e56-4321-bd25-b2f2670d2790" alt="Rdio Sherlock window" width="100%">
      <br><sub><b>Login Screen</b>
    </td>
  </tr>
</table>

<br>

<table align="center">
    <td align="center" valign="bottom" width="65%">
      <img src="https://github.com/user-attachments/assets/0b5988a5-8293-4de0-8b07-d954cef845be" alt="Mini player" width="383">
      <br><sub><b>Mini player</b><br>A compact card that stays on top</sub>
    </td>
  </tr>
</table>

## Features

- **Bring your own server** — a setup screen takes any rdio-scanner server address plus an optional access code, and remembers it per machine
- **Lives in the tray** — closing the window tucks the app into the system tray and the audio keeps playing; single-instance, so launching it again just brings the window back
- **Mini-player** — a compact notification-style box showing the current call. Change the opacity and leave it sit, drag it wherever you want, or enable pop-up mode and it will only appear during active calls.
- **Last call in the tray tooltip** — hover the tray icon while idle to see the last talkgroup heard and when
- **Start on login** — an optional tray-menu checkbox registers the app with Windows startup
- **Tray controls** — Mute, Snooze, Quiet hours, Mini player, Audio output, Startup options, Change server.

## Using it

1. Download or build `Rdio Sherlock` (a single portable `.exe`, nothing to install)
2. Run it and enter your rdio-scanner server address, e.g. `http://192.168.1.10:3000` or `https://scanner.example.com`
3. If the server asks for an access code, enter it — it's handed to the scanner page the same way the web client stores it, so you won't be prompted again
4. Click **Connect**. Done — close the window and it keeps playing from the system tray. 

To point it at a different server later, use **Change server…** from the tray menu or by right-clicking anywhere in the app. You can also "start at login/boot" and "mute" by right-clicking the icon in the system tray. Right-clicking the main window will allow you to also change your server, or set the window to always be on-top.

Drag/resize to your liking. Double-clicking the main display in rdio-scanner will fullscreen the application. Double-clicking the top near the -/X buttons will maximize the window.

This application uses the same layout as a typical rdio-scanner server would if you typed it into the web browser. Be sure to check out **my Rdio-Scanner fork** for some themes, statistics page, and various QoL Features. - [here](https://github.com/leloyon/rdio-scanner).

### A note on sleep

The app keeps playing when the **screen** turns off or the window is hidden — that's what it's for. Nothing can play while the PC itself is **asleep**, though: if you listen for long stretches, set Windows Sleep to *Never* (the display can still turn off freely).

## Building from source

Prerequisites:

- [Rust](https://www.rust-lang.org/tools/install) (stable)
- Visual Studio Build Tools with the MSVC C++ workload
- The WebView2 runtime (preinstalled on Windows 11; available from Microsoft for Windows 10)

Then:

```powershell
cd src-tauri
cargo build --release
```

The executable lands in `src-tauri/target/release/` (`rdio-scanner-anywhere.exe` — rename it to whatever you like; the app doesn't care).

## How it works

Rdio Sherlock is a thin [Tauri 2](https://tauri.app) shell around the rdio-scanner web client:

- WYSIWYG. If you run my [custom rdio-scanner fork](https://github.com/leloyon/rdio-scanner), expect to also get the volume slider and themes.
- The bundled page in `dist/` is the setup screen and the config store (kept in the app's local storage)
- On connect, the app navigates to your server and seeds the client's stored access code (`localStorage` key `rdio-scanner-pin`, base64 — the same storage the rdio-scanner client itself uses in v6.6.x and v8)
- The window chrome — a slim drag strip with minimize/close, the whole-window drag, the right-click menu, and the live title — is injected into the page by the Rust side
- The live title is read from the scanner display in the page, so it always matches what the display shows. It targets the rdio-scanner **v6** display layout; on other versions the title simply stays `Rdio Sherlock` and everything else still works
- Tauri permissions are deliberately narrow: remote pages (your server) may only use a handful of window controls — title, minimize, close, hide, drag, always-on-top — plus setting the tray tooltip. Nothing else

## Not affiliated

This is an unofficial companion app. rdio-scanner itself is a separate project: [chuot/rdio-scanner](https://github.com/chuot/rdio-scanner).
