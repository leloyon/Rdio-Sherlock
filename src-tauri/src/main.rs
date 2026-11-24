//! Rdio Sherlock (shareable "Anywhere" build): a generic rdio-scanner
//! wrapper. Starts on a bundled setup page where the user enters any
//! rdio-scanner server address and an optional access code; the choice is
//! remembered on that machine. Frameless main window with a drag strip +
//! minimize/close buttons injected into the top of the page itself (close
//! tucks into the tray). Single instance: launching again just shows the
//! existing window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tauri::{
    Listener,
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, Window, WindowEvent,
};

static MUTED: AtomicBool = AtomicBool::new(false);

/// Mute every audio/video element now, and keep muting new ones (rdio-scanner
/// creates elements per call) until unmuted.
const MUTE_JS: &str = r##"(function(){window.__rdioMuted=true;if(window.__rdioSetMuted)window.__rdioSetMuted(true);function m(n){if(n.tagName==='AUDIO'||n.tagName==='VIDEO')n.muted=true;if(n.querySelectorAll)n.querySelectorAll('audio,video').forEach(function(e){e.muted=true;});}m(document);if(!window.__rdioMuteObs){window.__rdioMuteObs=new MutationObserver(function(muts){if(!window.__rdioMuted)return;muts.forEach(function(mu){mu.addedNodes.forEach(function(n){if(n.nodeType===1)m(n);});});});window.__rdioMuteObs.observe(document.documentElement,{childList:true,subtree:true});}})();"##;
const UNMUTE_JS: &str = r##"(function(){window.__rdioMuted=false;if(window.__rdioSetMuted)window.__rdioSetMuted(false);document.querySelectorAll('audio,video').forEach(function(e){e.muted=false;});})();"##;

/// Runs before the page's own scripts on every load. The setup page hands
/// the access code over in the URL fragment (#rdioPin=<base64>); the
/// rdio-scanner client (v6.6.x and v8 alike) keeps its unlock code in
/// localStorage under 'rdio-scanner-pin' as base64, so seed (or clear) it
/// here, then scrub the fragment so it never lingers in the address.
const PIN_SEED_SCRIPT: &str = r##"(function () {
  try {
    var hash = window.location.hash || '';
    if (hash.indexOf('rdioPin=') === -1) return;
    var parts = hash.replace(/^#/, '').split('&');
    var value = null;
    for (var i = 0; i < parts.length; i++) {
      var kv = parts[i].split('=');
      if (kv[0] === 'rdioPin') {
        value = kv.slice(1).join('=');
        try { value = decodeURIComponent(value); } catch (e) {}
        break;
      }
    }
    if (value === null) return;
    try {
      if (value) {
        window.localStorage.setItem('rdio-scanner-pin', value);
      } else {
        window.localStorage.removeItem('rdio-scanner-pin');
      }
    } catch (e) {}
    try {
      window.history.replaceState(null, '', window.location.pathname + window.location.search);
    } catch (e) {}
  } catch (e) {}
})();"##;

/// "Keep PC awake": while on, Windows is asked not to go to sleep (the screen
/// is NOT kept on, so the display can still turn off). Remembered in
/// keep_awake.txt; on by default. A small thread re-asserts the request.
static KEEP_AWAKE: AtomicBool = AtomicBool::new(true);

/// "Start minimized to tray": when on, the app starts hidden in the tray.
static START_HIDDEN: AtomicBool = AtomicBool::new(false);

static AUTO_LIVE: AtomicBool = AtomicBool::new(true);
/// Optional: start in the mini player. Off by default (the app always opens
/// in full view); applied once per app run, after the scanner page loads.
static START_MINI: AtomicBool = AtomicBool::new(false);
static MINI_STARTED: AtomicBool = AtomicBool::new(false);
/// Pop-up mode: while the mini player is active, its window only appears
/// during a call and fades away shortly after.
static POPUP: AtomicBool = AtomicBool::new(false);
/// True only while the page's mini player is actually switched on (the page
/// tells us); pop-up mode and mini opacity apply only then, never to a normal
/// window that merely happens to be narrow.
static MINI_ON: AtomicBool = AtomicBool::new(false);
static POPUP_HOLD: Mutex<Option<Instant>> = Mutex::new(None);

fn hold_popup(secs: u64) {
    if let Ok(mut g) = POPUP_HOLD.lock() {
        *g = Some(Instant::now() + std::time::Duration::from_secs(secs));
    }
}
static MINI_OPACITY: AtomicU32 = AtomicU32::new(100);
static MINI_CORNER: AtomicU32 = AtomicU32::new(0);
static EFFECTIVE_MUTE: AtomicBool = AtomicBool::new(false);
static SNOOZE_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Quiet hours as (start, end) minutes after midnight, or None when off.
/// The source of truth is quiet_hours.txt ("22:00-06:00" or "off").
static QUIET: Mutex<Option<(u32, u32)>> = Mutex::new(None);

fn parse_quiet(text: &str) -> Option<(u32, u32)> {
    let t = text.trim();
    let (a, b) = t.split_once('-')?;
    let hm = |x: &str| -> Option<u32> {
        let (h, m) = x.trim().split_once(':')?;
        let h: u32 = h.trim().parse().ok()?;
        let m: u32 = m.trim().parse().ok()?;
        if h < 24 && m < 60 { Some(h * 60 + m) } else { None }
    };
    let (s, e) = (hm(a)?, hm(b)?);
    if s == e { None } else { Some((s, e)) }
}

fn quiet_text<R: Runtime>(app: &AppHandle<R>) -> String {
    flag_path(app, "quiet_hours.txt")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| "off".to_string())
}

fn reload_quiet<R: Runtime>(app: &AppHandle<R>) {
    let q = parse_quiet(&quiet_text(app));
    if let Ok(mut g) = QUIET.lock() {
        *g = q;
    }
}

fn set_quiet_text<R: Runtime>(app: &AppHandle<R>, text: &str) {
    if let Some(path) = flag_path(app, "quiet_hours.txt") {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
    reload_quiet(app);
}

fn quiet_now() -> bool {
    let q = match QUIET.lock() {
        Ok(g) => *g,
        Err(_) => None,
    };
    match q {
        Some((s, e)) => {
            let now = local_minutes();
            if s < e { now >= s && now < e } else { now >= s || now < e }
        }
        None => false,
    }
}

fn snoozed() -> bool {
    if let Ok(mut g) = SNOOZE_UNTIL.lock() {
        if let Some(t) = *g {
            if t > Instant::now() {
                return true;
            }
            *g = None;
        }
    }
    false
}

fn snooze_minutes_left() -> u64 {
    if let Ok(g) = SNOOZE_UNTIL.lock() {
        if let Some(t) = *g {
            let now = Instant::now();
            if t > now {
                return (t - now).as_secs() / 60 + 1;
            }
        }
    }
    0
}

fn local_minutes() -> u32 {
    #[cfg(windows)]
    {
        winpower::local_minutes()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn fmt_clock(m: u32) -> String {
    let h24 = m / 60;
    let min = m % 60;
    let suffix = if h24 < 12 { "AM" } else { "PM" };
    let h = match h24 % 12 { 0 => 12, x => x };
    if min == 0 {
        if h24 == 0 { "Midnight".to_string() } else { format!("{} {}", h, suffix) }
    } else {
        format!("{}:{:02} {}", h, min, suffix)
    }
}

/// Re-evaluates whether audio should be muted (manual mute, snooze or quiet
/// hours) and pushes the change into the page when it differs.
fn sync_mute<R: Runtime>(app: &AppHandle<R>) -> bool {
    let eff = MUTED.load(Ordering::SeqCst) || snoozed() || quiet_now();
    let prev = EFFECTIVE_MUTE.swap(eff, Ordering::SeqCst);
    if prev != eff {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.eval(if eff { MUTE_JS } else { UNMUTE_JS });
        }
    }
    prev != eff
}

/// Sends the settings the page needs (auto Live Feed, mini corner) and the
/// current mute state; also run after every page load.
fn push_state<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(&format!(
            "window.__rdioPrefs={{autoLive:{},miniCorner:{},miniOpacity:{},popup:{}}};",
            AUTO_LIVE.load(Ordering::SeqCst),
            MINI_CORNER.load(Ordering::SeqCst),
            MINI_OPACITY.load(Ordering::SeqCst),
            POPUP.load(Ordering::SeqCst)
        ));
        if EFFECTIVE_MUTE.load(Ordering::SeqCst) {
            let _ = w.eval(MUTE_JS);
        }
    }
}

fn load_u32<R: Runtime>(app: &AppHandle<R>, name: &str, default: u32) -> u32 {
    flag_path(app, name)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(default)
}

fn save_u32<R: Runtime>(app: &AppHandle<R>, name: &str, v: u32) {
    if let Some(path) = flag_path(app, name) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, v.to_string());
    }
}

/// Tray icon with a small status dot: green = a call is playing, red = muted
/// or snoozed, blue = quiet hours.
fn status_icon<R: Runtime>(app: &AppHandle<R>, state: u32) -> Option<tauri::image::Image<'static>> {
    let base = app.default_window_icon()?;
    let (w, h) = (base.width(), base.height());
    let mut px = base.rgba().to_vec();
    let color: [u8; 3] = match state {
        1 => [63, 185, 80],
        2 => [248, 81, 73],
        3 => [88, 166, 255],
        4 => [210, 153, 34],
        _ => return Some(tauri::image::Image::new_owned(px, w, h)),
    };
    let r = ((w.min(h) as f32) * 0.24).max(4.0);
    let (cx, cy) = (w as f32 - r - 1.0, h as f32 - r - 1.0);
    for y in 0..h {
        for x in 0..w {
            let d = (((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2))).sqrt();
            if d <= r {
                let i = ((y * w + x) * 4) as usize;
                let c = if d > r - 1.6 { [20u8, 20, 24] } else { color };
                px[i] = c[0];
                px[i + 1] = c[1];
                px[i + 2] = c[2];
                px[i + 3] = 255;
            }
        }
    }
    Some(tauri::image::Image::new_owned(px, w, h))
}

/// Background loop: applies snooze/quiet-hours, keeps the tray icon status
/// and the mini-player opacity up to date.
fn start_tick_thread<R: Runtime>(app: AppHandle<R>) {
    std::thread::spawn(move || {
        let mut last_slow: Option<Instant> = None;
        let mut last_quiet: Option<Instant> = None;
        let mut last_loop = Instant::now();
        let mut stage: u32 = 0;
        let mut last_icon: u32 = 99;
        let mut last_sig: (bool, bool, bool) = (false, false, false);
        let mut last_alpha: i32 = -1;
        let mut last_active = Instant::now();
        let mut fade: f32 = 1.0;
        let mut nolink = false;
        let mut active = false;
        // The loop runs every 50 ms only while a window fade is in progress
        // (so fades are smooth) and otherwise every 250 ms, which keeps the
        // app near zero CPU when idle. The slower jobs (title, mute, tray
        // icon) run every 250 ms.
        loop {
            let win = app.get_webview_window("main");
            let mut fast = false;

            // after the PC slept the loop gap is large: restart the
            // heartbeat clock so sleeping is not mistaken for a crash
            if last_loop.elapsed() > std::time::Duration::from_secs(20) {
                if let Ok(mut g) = LAST_BEAT.lock() {
                    if g.is_some() {
                        *g = Some(Instant::now());
                    }
                }
                stage = 0;
            }
            last_loop = Instant::now();

            let slow = last_slow.map(|t| t.elapsed() >= std::time::Duration::from_millis(240)).unwrap_or(true);
            if slow {
                last_slow = Some(Instant::now());
                let title = win.as_ref().and_then(|w| w.title().ok()).unwrap_or_default();
                nolink = title.ends_with("(no link)");
                active = !nolink && title != "Rdio Sherlock" && !title.is_empty();

                ACTIVE.store(active, Ordering::SeqCst);
                if last_quiet.map(|t| t.elapsed() >= std::time::Duration::from_secs(5)).unwrap_or(true) {
                    last_quiet = Some(Instant::now());
                    reload_quiet(&app);
                }

                // crash watchdog (only once the pages have sent a heartbeat)
                let age = LAST_BEAT.lock().ok().and_then(|g| g.map(|t| t.elapsed().as_secs()));
                if let Some(age) = age {
                    if age < 40 {
                        stage = 0;
                    } else if age > 150 && stage == 0 {
                        stage = 1;
                        if let Some(w) = win.as_ref() {
                            let _ = w.eval("location.reload()");
                        }
                    } else if age > 240 && stage == 1 {
                        stage = 2;
                        if recovery_restart_allowed(&app) {
                            relaunch_soon(&app);
                        }
                    }
                }
                let changed = sync_mute(&app);
                let sn = snoozed();
                let qn = quiet_now();
                let sig = (sn, qn, MUTED.load(Ordering::SeqCst));
                if changed || sig != last_sig {
                    last_sig = sig;
                    let _ = refresh_tray_menu(&app);
                }
                // tray icon status: muted/snoozed red, quiet hours blue,
                // no link amber, call playing green
                let state = if sig.2 || sn { 2 } else if qn { 3 } else if nolink { 4 } else if active { 1 } else { 0 };
                if state != last_icon {
                    last_icon = state;
                    if let (Some(tray), Some(img)) = (app.tray_by_id("tray"), status_icon(&app, state)) {
                        let _ = tray.set_icon(Some(img));
                    }
                }
            }
            if active {
                last_active = Instant::now();
            }

            #[cfg(windows)]
            if let Some(w) = win.as_ref() {
                if let (Ok(size), Ok(sf)) = (w.inner_size(), w.scale_factor()) {
                    let mini = MINI_ON.load(Ordering::SeqCst) && (size.width as f64 / sf) < 450.0;
                    let pct = MINI_OPACITY.load(Ordering::SeqCst).clamp(30, 100) as f32;
                    let base = if mini { pct * 255.0 / 100.0 } else { 255.0 };
                    let hwnd = w.hwnd().ok().map(|h| h.0 as isize);
                    let mut target: f32 = 1.0;
                    if mini && POPUP.load(Ordering::SeqCst) {
                        // pop-up mode: fade in for a call, fade out afterwards
                        let held = POPUP_HOLD
                            .lock()
                            .map(|g| g.map(|t| t > Instant::now()).unwrap_or(false))
                            .unwrap_or(false);
                        let visible = w.is_visible().unwrap_or(true);
                        if active && !visible {
                            if !winpower::fullscreen_app_active() {
                                fade = 0.0;
                                if let Some(h) = hwnd {
                                    winpower::set_alpha(h, 0);
                                    last_alpha = 0;
                                }
                                let _ = w.show();
                            }
                        } else if visible && !active && !held && last_active.elapsed() > std::time::Duration::from_secs(4) {
                            target = 0.0;
                        }
                    }
                    if target > fade {
                        fade = (fade + 0.1).min(1.0);
                    } else if target < fade {
                        fade = (fade - 0.05).max(0.0);
                        if fade <= 0.0 {
                            let _ = w.hide();
                        }
                    }
                    fast = fade > 0.0 && fade < 1.0;
                    let alpha = (base * fade).round().clamp(0.0, 255.0) as i32;
                    // Re-apply regularly as well as on change: hiding and
                    // showing the window (pop-up mode) can reset its opacity.
                    if alpha != last_alpha || slow || (mini && POPUP.load(Ordering::SeqCst)) {
                        if let Some(h) = hwnd {
                            winpower::set_alpha(h, alpha as u8);
                            last_alpha = alpha;
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(if fast { 50 } else { 250 }));
        }
    });
}

fn flag_path<R: Runtime>(app: &AppHandle<R>, name: &str) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join(name))
}

fn load_flag<R: Runtime>(app: &AppHandle<R>, name: &str, default: bool) -> bool {
    match flag_path(app, name).and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(t) => t.trim() == "1",
        None => default,
    }
}

fn save_flag<R: Runtime>(app: &AppHandle<R>, name: &str, on: bool) {
    if let Some(path) = flag_path(app, name) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, if on { "1" } else { "0" });
    }
}

fn keep_awake_path<R: Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("keep_awake.txt"))
}

fn load_keep_awake<R: Runtime>(app: &AppHandle<R>) -> bool {
    match keep_awake_path(app).and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(t) => t.trim() != "0",
        None => true,
    }
}

fn save_keep_awake<R: Runtime>(app: &AppHandle<R>, on: bool) {
    if let Some(path) = keep_awake_path(app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, if on { "1" } else { "0" });
    }
}

#[cfg(windows)]
mod winpower {
    #[repr(C)]
    struct ProcessPowerThrottlingState {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
        fn GetCurrentProcess() -> isize;
        fn SetProcessInformation(process: isize, class: i32, info: *mut core::ffi::c_void, size: u32) -> i32;
    }
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(st: *mut SystemTime);
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn SetLayeredWindowAttributes(hwnd: isize, key: u32, alpha: u8, flags: u32) -> i32;
    }

    pub fn local_minutes() -> u32 {
        unsafe {
            let mut st = SystemTime { year: 0, month: 0, dow: 0, day: 0, hour: 0, minute: 0, second: 0, ms: 0 };
            GetLocalTime(&mut st);
            st.hour as u32 * 60 + st.minute as u32
        }
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const core::ffi::c_void, size: u32) -> i32;
    }

    /// Windows 11 draws a thin border that changes when the window loses
    /// focus. Pin it to one dark colour so the window looks the same either
    /// way (ignored on older Windows).
    pub fn pin_border_color(hwnd: isize) {
        unsafe {
            let color: u32 = 0x0032_241E; // COLORREF 0x00BBGGRR
            DwmSetWindowAttribute(hwnd, 34, &color as *const u32 as *const core::ffi::c_void, 4);
        }
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHQueryUserNotificationState(state: *mut i32) -> i32;
    }

    /// True while a fullscreen app (game, video, presentation) has the screen.
    pub fn fullscreen_app_active() -> bool {
        unsafe {
            let mut st: i32 = 0;
            if SHQueryUserNotificationState(&mut st) != 0 {
                return false;
            }
            // 2 = busy (fullscreen), 3 = Direct3D fullscreen, 4 = presentation mode
            st == 2 || st == 3 || st == 4
        }
    }

    /// Whole-window opacity (used for the mini player).
    pub fn set_alpha(hwnd: isize, alpha: u8) {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, -20);
            if ex & 0x0008_0000 == 0 {
                SetWindowLongPtrW(hwnd, -20, ex | 0x0008_0000);
            }
            SetLayeredWindowAttributes(hwnd, 0, alpha, 2);
        }
    }

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    pub fn set_awake(on: bool) {
        unsafe {
            SetThreadExecutionState(if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
        }
    }

    /// Opt this process out of Windows "Efficiency mode" / execution-speed throttling.
    pub fn opt_out_of_throttling() {
        unsafe {
            let mut st = ProcessPowerThrottlingState { version: 1, control_mask: 1, state_mask: 0 };
            SetProcessInformation(
                GetCurrentProcess(),
                4, // ProcessPowerThrottling
                &mut st as *mut _ as *mut core::ffi::c_void,
                core::mem::size_of::<ProcessPowerThrottlingState>() as u32,
            );
        }
    }
}

fn start_power_thread() {
    #[cfg(windows)]
    {
        winpower::opt_out_of_throttling();
        std::thread::spawn(|| loop {
            // The execution state belongs to this thread, so it is set here
            // (and refreshed) rather than from the menu handler.
            winpower::set_awake(KEEP_AWAKE.load(Ordering::SeqCst));
            std::thread::sleep(std::time::Duration::from_secs(20));
        });
    }
}

/// Pauses animations/transitions while the window is hidden in the tray
/// (audio and the live title are untouched), and resumes them on show.
const QUIET_JS: &str = r##"(function(){try{if(document.getElementById('rdio-quiet'))return;var s=document.createElement('style');s.id='rdio-quiet';s.textContent='*,*::before,*::after{animation-play-state:paused!important;transition:none!important;}';(document.head||document.documentElement).appendChild(s);}catch(e){}})();"##;
const UNQUIET_JS: &str = r##"(function(){try{var s=document.getElementById('rdio-quiet');if(s)s.remove();}catch(e){}})();"##;

/// Compact "mini player": the window shrinks to a small always-on-top card
/// that shows the current talkgroup. Toggled from the right-click menu or the
/// tray; remembered across launches (per server).
const MINI_SCRIPT: &str = r##"(function () {
  if (window.__rdioMini) return;
  var KEY = 'rdio-sherlock-mini';
  var PREV = 'rdio-sherlock-mini-prev';
  var W = 380, H = 108;
  var state = { on: false };
  window.__rdioMini = state;
  function emitMini(on) {
    try { window.__TAURI__.event.emit('rdio-mini-state', on); } catch (e) {}
  }
  emitMini(false);
  var overlay = null, mainEl = null, subEl = null, dot = null, timer = null;
  var lastCall = '';
  var recent = [];
  var curName = '';
  function api() {
    try {
      var t = window.__TAURI__;
      return (t && t.window && t.window.getCurrentWindow) ? t : null;
    } catch (e) { return null; }
  }
  function Logical(w, h) {
    var t = window.__TAURI__;
    var C = (t.dpi && t.dpi.LogicalSize) || t.window.LogicalSize;
    return new C(w, h);
  }
  function read() {
    var disp = document.querySelector('.rdio-display');
    var led = document.querySelector('.rdio-status .led');
    if (led && !led.classList.contains('on')) return null;
    var big = disp ? disp.querySelector('.row.big span') : null;
    var name = big ? (big.textContent || '').trim() : '';
    if (!name || /^rdio scanner/i.test(name)) return null;
    var label = '', tgid = '';
    try {
      var rows = disp.querySelectorAll('.row');
      var cell = rows[3] && rows[3].querySelector('div span');
      if (cell) label = (cell.textContent || '').trim();
      var m = (rows[5] ? rows[5].textContent || '' : '').match(/TGID:\s*(\d+)/);
      if (m) tgid = m[1];
    } catch (e) {}
    return { name: name, sub: [tgid ? 'TGID ' + tgid : '', (label && label !== name) ? label : ''].filter(Boolean).join('  ·  ') };
  }
  function liveBtn() {
    var bs = document.querySelectorAll('button.rdio-button');
    for (var i = 0; i < bs.length; i++) {
      if (/live\s*feed/i.test(bs[i].textContent || '')) return bs[i];
    }
    return null;
  }
  function update() {
    if (!overlay) return;
    overlay.title = recent.length ? 'Recent calls\n' + recent.join('\n') : 'No calls heard yet';
    var c = read();
    var disp0 = document.querySelector('.rdio-display');
    var nolink = !!(disp0 && (disp0.textContent || '').indexOf('NO LINK') !== -1);
    var led = document.querySelector('.rdio-status .led');
    var col = '#3fb950';
    if (led) {
      var map = { blue: '#58a6ff', cyan: '#39d0d8', green: '#3fb950', magenta: '#d070e8', orange: '#f0883e', red: '#f85149', white: '#e6edf3', yellow: '#e3c33c' };
      for (var k in map) { if (led.classList.contains(k)) { col = map[k]; break; } }
    }
    if (nolink) {
      curName = '';
      mainEl.textContent = 'No link';
      subEl.textContent = 'Waiting for the server';
      dot.style.animation = 'rdioPulse 1.2s ease-in-out infinite';
      dot.style.background = '#d29922';
      dot.style.boxShadow = 'none';
      overlay.style.boxShadow = 'inset 4px 0 0 #d29922';
      return;
    }
    var lb = liveBtn();
    if (lb && !lb.classList.contains('on')) {
      curName = '';
      mainEl.textContent = 'Live feed off';
      subEl.textContent = 'Click the dot to turn it on';
      dot.style.animation = 'none';
      dot.style.background = 'transparent';
      dot.style.boxShadow = 'inset 0 0 0 2px #8e9bb0';
      overlay.style.boxShadow = 'inset 4px 0 0 transparent';
      return;
    }
    if (c) {
      overlay.style.boxShadow = 'inset 4px 0 0 ' + col;
      mainEl.textContent = c.name;
      subEl.textContent = c.sub || 'Receiving';
      dot.style.animation = 'none';
      dot.style.background = col;
      dot.style.boxShadow = '0 0 10px ' + col;
      if (c.name !== curName) {
        curName = c.name;
        recent.unshift(new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) + '  ' + c.name);
        if (recent.length > 8) recent.pop();
      }
    } else {
      curName = '';
      mainEl.textContent = 'Scanning\u2026';
      subEl.textContent = 'Waiting for calls';
      dot.style.animation = 'rdioPulse 2.4s ease-in-out infinite';
      dot.style.background = '#566070';
      dot.style.boxShadow = 'none';
      overlay.style.boxShadow = 'inset 4px 0 0 transparent';
    }
  }
  function build() {
    overlay = document.createElement('div');
    overlay.id = 'rdio-mini';
    overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483646;box-sizing:border-box;padding:12px 16px 12px 20px;display:flex;align-items:center;gap:14px;background:linear-gradient(135deg,#141a26 0%,#0d1119 100%);font-family:-apple-system,"Segoe UI",Roboto,sans-serif;color:#f2f5fa;overflow:hidden;user-select:none;-webkit-user-select:none;cursor:default;';
    overlay.setAttribute('data-tauri-drag-region', '');
    var kf = document.createElement('style');
    kf.id = 'rdio-mini-kf';
    kf.textContent = '@keyframes rdioPulse{0%,100%{opacity:.4}50%{opacity:1}}';
    overlay.appendChild(kf);
    overlay.addEventListener('contextmenu', function (e) {
      e.preventDefault(); e.stopPropagation();
      quickMenu();
    });
    dot = document.createElement('div');
    dot.style.cssText = 'flex:none;width:12px;height:12px;padding:6px;margin:-6px;box-sizing:content-box;background-clip:content-box;border-radius:50%;background:#566070;transition:background .3s,box-shadow .3s;cursor:pointer;';
    dot.title = 'Turn live feed on / off';
    dot.addEventListener('click', function (e) {
      e.stopPropagation();
      var b = liveBtn();
      if (b) b.click();
      setTimeout(update, 150);
    });
    var col = document.createElement('div');
    col.style.cssText = 'flex:1;min-width:0;';
    mainEl = document.createElement('div');
    mainEl.style.cssText = 'font-size:20px;font-weight:600;letter-spacing:.01em;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;';
    subEl = document.createElement('div');
    subEl.style.cssText = 'margin-top:3px;font-size:12px;color:#8e9bb0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;';
    [col, mainEl, subEl].forEach(function (el) { el.setAttribute('data-tauri-drag-region', ''); });
    col.appendChild(mainEl); col.appendChild(subEl);
    var exp = document.createElement('button');
    exp.type = 'button';
    exp.textContent = '⤢';
    exp.title = 'Back to full view';
    exp.style.cssText = 'flex:none;width:34px;height:34px;border:1px solid rgba(255,255,255,0.14);border-radius:9px;background:rgba(255,255,255,0.06);color:#dfe6f1;font-size:18px;line-height:1;cursor:pointer;';
    exp.addEventListener('mouseenter', function () { exp.style.background = 'rgba(255,255,255,0.14)'; });
    exp.addEventListener('mouseleave', function () { exp.style.background = 'rgba(255,255,255,0.06)'; });
    exp.addEventListener('click', function () { state.exit(); });
    overlay.appendChild(dot); overlay.appendChild(col); overlay.appendChild(exp);
    (document.body || document.documentElement).appendChild(overlay);
    update();
    timer = setInterval(update, 700);
  }
  function resizeTo(w, h) {
    var t = api();
    if (!t) return;
    try { var p = t.window.getCurrentWindow().setSize(Logical(w, h)); if (p && p.catch) p.catch(function () {}); } catch (e) {}
  }
  function skipTaskbar(on) {
    var t = api();
    if (!t) return;
    try { var p = t.window.getCurrentWindow().setSkipTaskbar(on); if (p && p.catch) p.catch(function () {}); } catch (e) {}
  }
  function onTop(on) {
    var t = api();
    if (!t) return;
    try { var p = t.window.getCurrentWindow().setAlwaysOnTop(on); if (p && p.catch) p.catch(function () {}); } catch (e) {}
  }
  function pill(label, fn) {
    var b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.style.cssText = 'flex:1;padding:9px 6px;border:1px solid rgba(255,255,255,0.16);border-radius:9px;background:rgba(255,255,255,0.07);color:#f2f5fa;font-size:12px;cursor:pointer;white-space:nowrap;';
    b.addEventListener('mouseenter', function () { b.style.background = 'rgba(255,255,255,0.16)'; });
    b.addEventListener('mouseleave', function () { b.style.background = 'rgba(255,255,255,0.07)'; });
    b.addEventListener('click', function (e) { e.stopPropagation(); fn(); });
    return b;
  }
  // Double-clicking the drag strip normally maximizes the window, which
  // makes no sense for the mini player, so swallow the second click there.
  window.addEventListener('mousedown', function (e) {
    if (state.on && e.detail >= 2) {
      e.stopImmediatePropagation();
      e.preventDefault();
    }
  }, true);
  window.addEventListener('dblclick', function (e) {
    if (state.on) { e.stopImmediatePropagation(); e.preventDefault(); }
  }, true);
  var qm = null;
  function closeQuick() { if (qm) { qm.remove(); qm = null; emitHold(false); } }
  // The normal right-click menu is taller than the mini window, so the mini
  // player shows a compact row of the few things that make sense here.
  function quickMenu() {
    closeQuick();
    if (!overlay) return;
    qm = document.createElement('div');
    emitHold(true);
    qm.style.cssText = 'position:absolute;inset:0;box-sizing:border-box;padding:28px 12px 12px;display:flex;flex-direction:column;gap:6px;justify-content:center;background:rgba(13,17,25,0.98);user-select:none;-webkit-user-select:none;';
    var row = document.createElement('div');
    row.style.cssText = 'display:flex;gap:8px;';
    var hint = document.createElement('div');
    hint.textContent = 'Click anywhere (even outside), right-click again or press Esc to close';
    hint.style.cssText = 'font-size:11px;color:#8e9bb0;text-align:center;';
    row.appendChild(pill('Full view', function () { closeQuick(); state.exit(); }));
    var steps = [100, 85, 70, 50];
    var curOp = (window.__rdioPrefs && window.__rdioPrefs.miniOpacity) || 100;
    var opBtn = pill('Opacity ' + curOp + '%', function () {
      var i = steps.indexOf(curOp);
      curOp = steps[(i + 1) % steps.length];
      opBtn.textContent = 'Opacity ' + curOp + '%';
      try {
        if (window.__rdioPrefs) window.__rdioPrefs.miniOpacity = curOp;
        window.__TAURI__.event.emit('rdio-mini-opacity', curOp);
      } catch (e) {}
    });
    row.appendChild(opBtn);
    qm.appendChild(row);
    qm.appendChild(hint);
    qm.addEventListener('click', closeQuick);
    qm.addEventListener('contextmenu', function (e) { e.preventDefault(); e.stopPropagation(); closeQuick(); });
    overlay.appendChild(qm);
  }
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') closeQuick(); });
  // clicking outside the window (another app, the desktop) also closes it
  window.addEventListener('blur', closeQuick);
  function setMiniStyle(on) {
    var old = document.getElementById('rdio-mini-style');
    if (old) old.remove();
    if (!on) return;
    var st = document.createElement('style');
    st.id = 'rdio-mini-style';
    st.textContent = '#rdio-strip-btns,#rdio-mini-btn{display:none!important;}';
    (document.head || document.documentElement).appendChild(st);
  }
  var curH = H;
  function emitHold(on) {
    try { window.__TAURI__.event.emit('rdio-popup-hold', on); } catch (e) {}
  }
  // Panels such as the audio output list need more room than the mini card:
  // the window grows while one is open (and a pop-up window is kept on
  // screen), then shrinks back when it is closed.
  state.expand = function (on) {
    curH = on ? 320 : H;
    resizeTo(W, curH);
    emitHold(on);
    setTimeout(toCorner, 200);
  };
  // Where the mini card goes for the chosen corner (null = leave in place).
  function cornerXY(h) {
    var corner = (window.__rdioPrefs && window.__rdioPrefs.miniCorner) || 0;
    if (!corner) return null;
    var sx = window.screen.availLeft || 0, sy = window.screen.availTop || 0;
    var sw = window.screen.availWidth, sh = window.screen.availHeight, m = 12;
    return {
      x: (corner === 5 || corner === 6) ? sx + Math.round((sw - W) / 2)
        : (corner === 2 || corner === 4) ? sx + sw - W - m : sx + m,
      y: (corner === 3 || corner === 4 || corner === 6) ? sy + sh - h - m : sy + m
    };
  }
  function toCorner() {
    var t = api();
    var xy = cornerXY(curH);
    if (!t || !xy) return;
    try {
      var C = (t.dpi && t.dpi.LogicalPosition) || t.window.LogicalPosition;
      var p = t.window.getCurrentWindow().setPosition(new C(xy.x, xy.y));
      if (p && p.catch) p.catch(function () {});
    } catch (e) {}
  }
  // Size and position are changed together, so the window lands in its
  // corner at once instead of shrinking first and moving afterwards.
  function placeMini() {
    var t = api();
    if (!t) return Promise.resolve();
    try {
      var win = t.window.getCurrentWindow();
      var C = (t.dpi && t.dpi.LogicalPosition) || t.window.LogicalPosition;
      var xy = cornerXY(H);
      var ps = [];
      if (xy) ps.push(win.setPosition(new C(xy.x, xy.y)));
      ps.push(win.setSize(Logical(W, H)));
      return Promise.all(ps.map(function (p) { return (p && p.catch) ? p.catch(function () {}) : p; }));
    } catch (e) { return Promise.resolve(); }
  }
  state.enter = function () {
    if (state.on) return;
    var t = api();
    var go = function () {
      state.on = true;
      emitMini(true);
      // hide the page while the window changes size, then show the card
      // once it is in place, so nothing is seen shrinking
      var root = document.documentElement;
      root.style.visibility = 'hidden';
      var shown = false;
      var reveal = function () {
        if (shown) return;
        shown = true;
        root.style.visibility = '';
      };
      if (!overlay) build();
      setMiniStyle(true);
      onTop(true);
      skipTaskbar(true);
      placeMini().then(reveal, reveal);
      setTimeout(reveal, 600);
    };
    if (!t) { go(); return; }
    // remember the normal size so leaving mini mode can restore it
    try {
      var win = t.window.getCurrentWindow();
      Promise.all([win.innerSize(), win.scaleFactor()]).then(function (r) {
        var lw = Math.round(r[0].width / r[1]), lh = Math.round(r[0].height / r[1]);
        if (lw > W + 40 && lh > H + 40) {
          try { localStorage.setItem(PREV, lw + 'x' + lh); } catch (e) {}
        }
        go();
      }, go);
    } catch (e) { go(); }
  };
  state.exit = function () {
    state.on = false;
    emitMini(false);
    if (timer) { clearInterval(timer); timer = null; }
    if (overlay) { overlay.remove(); overlay = null; }
    closeQuick();
    setMiniStyle(false);
    var w = 1280, h = 800;
    try {
      var m = (localStorage.getItem(PREV) || '').match(/^(\d+)x(\d+)$/);
      if (m) { w = +m[1]; h = +m[2]; }
    } catch (e) {}
    // size and position are set together, so the window lands centred at
    // once instead of resizing first and moving afterwards
    var t = api();
    if (t) {
      try {
        var win = t.window.getCurrentWindow();
        var sx = window.screen.availLeft || 0, sy = window.screen.availTop || 0;
        var pw = Math.min(w, window.screen.availWidth), ph = Math.min(h, window.screen.availHeight);
        var PC = (t.dpi && t.dpi.LogicalPosition) || t.window.LogicalPosition;
        var p1 = win.setPosition(new PC(Math.round(sx + (window.screen.availWidth - pw) / 2), Math.round(sy + (window.screen.availHeight - ph) / 2)));
        if (p1 && p1.catch) p1.catch(function () {});
        var p2 = win.setSize(Logical(pw, ph));
        if (p2 && p2.catch) p2.catch(function () {});
      } catch (e) { resizeTo(w, h); }
    }
    onTop(false);
    skipTaskbar(false);
  };
  state.toggle = function () { if (state.on) state.exit(); else state.enter(); };
})();"##;

/// Presses Live Feed by itself after the scanner page has loaded (when the
/// tray option is on), once per page load. A synthetic mouse event first lets
/// the scanner create its audio context, the same way a real click would.
const AUTO_SCRIPT: &str = r##"(function () {
  if (window.__rdioAuto) return;
  window.__rdioAuto = true;
  var tries = 0, done = false, timer = null;
  function findBtn() {
    var bs = document.querySelectorAll('button.rdio-button');
    for (var i = 0; i < bs.length; i++) {
      if (/live\s*feed/i.test(bs[i].textContent || '')) return bs[i];
    }
    return null;
  }
  function attempt() {
    if (done) return;
    var prefs = window.__rdioPrefs;
    if (!prefs || !prefs.autoLive) return;
    var b = findBtn();
    if (!b) return;
    if (b.classList.contains('on')) { done = true; return; }
    done = true;
    try { document.body.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })); } catch (e) {}
    setTimeout(function () { if (!b.classList.contains('on')) b.click(); }, 600);
  }
  timer = setInterval(function () {
    tries++;
    attempt();
    if (done || tries > 120) clearInterval(timer);
  }, 1000);
})();"##;

/// Audio output selection (Bluetooth speaker etc.). Runs before the page's own
/// scripts: tracks every AudioContext the scanner creates so the chosen output
/// device can be applied with setSinkId, and exposes window.__rdioPickAudio()
/// which the right-click menu opens. The choice is remembered per server.
const AUDIO_SCRIPT: &str = r##"(function () {
  if (window.__rdioAudio) return;
  window.__rdioAudio = true;
  // Never show the browser's "Leave site?" prompt (for example when changing server).
  try {
    var addL = EventTarget.prototype.addEventListener;
    EventTarget.prototype.addEventListener = function (t, l, o) {
      if (t === 'beforeunload') return;
      return addL.call(this, t, l, o);
    };
    Object.defineProperty(window, 'onbeforeunload', { get: function () { return null; }, set: function () {}, configurable: true });
  } catch (e) {}
  var KEY = 'rdio-sherlock-audio-sink';
  var contexts = [];
  var masters = [];
  window.__rdioSetMuted = function (on) {
    masters.forEach(function (m) { try { m.gain.value = on ? 0 : 1; } catch (e) {} });
  };
  function saved() { try { return localStorage.getItem(KEY) || ''; } catch (e) { return ''; } }
  function applyCtx(c) {
    var id = saved();
    try { if (c.setSinkId) { var p = c.setSinkId(id); if (p && p.catch) p.catch(function () {}); } } catch (e) {}
  }
  function applyEl(el) {
    var id = saved();
    try { if (el.setSinkId) { var p = el.setSinkId(id); if (p && p.catch) p.catch(function () {}); } } catch (e) {}
  }
  function wrap(name) {
    var Orig = window[name];
    if (!Orig) return;
    var Wrapped = function (opts) {
      var c = new Orig(opts);
      // Route everything through one master gain so the app can mute the
      // scanner's audio (tray Mute, Snooze, Quiet hours) reliably.
      try {
        var real = c.destination;
        var master = c.createGain();
        master.gain.value = window.__rdioMuted ? 0 : 1;
        master.connect(real);
        Object.defineProperty(c, 'destination', { value: master, configurable: true });
        masters.push(master);
      } catch (e) {}
      contexts.push(c);
      applyCtx(c);
      return c;
    };
    Wrapped.prototype = Orig.prototype;
    window[name] = Wrapped;
  }
  wrap('AudioContext');
  if (window.webkitAudioContext && window.webkitAudioContext !== window.AudioContext) wrap('webkitAudioContext');
  function applyAll() {
    contexts.forEach(applyCtx);
    document.querySelectorAll('audio,video').forEach(applyEl);
  }
  var panel = null;
  var expanded = false;
  function removePanel() { if (panel) { panel.remove(); panel = null; } }
  function closePanel() {
    removePanel();
    if (expanded) {
      expanded = false;
      try { window.__rdioMini.expand(false); } catch (e) {}
    }
  }
  function row(label, active, onClick) {
    var el = document.createElement('div');
    el.textContent = (active ? '✓ ' : '') + label;
    el.style.cssText = 'padding:8px 12px;font-size:13px;color:#f2f2f2;border-radius:5px;cursor:pointer;user-select:none;';
    el.addEventListener('mouseenter', function () { el.style.background = 'rgba(255,255,255,0.10)'; });
    el.addEventListener('mouseleave', function () { el.style.background = 'transparent'; });
    el.addEventListener('click', onClick);
    return el;
  }
  function note(text) {
    var el = document.createElement('div');
    el.textContent = text;
    el.style.cssText = 'padding:8px 12px;font-size:12px;color:#aab;max-width:300px;';
    return el;
  }
  function show(devices, msg) {
    removePanel();
    if (window.__rdioMini && window.__rdioMini.on && !expanded && window.__rdioMini.expand) {
      expanded = true;
      window.__rdioMini.expand(true);
    }
    panel = document.createElement('div');
    panel.style.cssText = 'position:fixed;z-index:2147483647;left:50%;top:60px;transform:translateX(-50%);background:#1e1e24;border:1px solid rgba(255,255,255,0.16);border-radius:8px;padding:6px;min-width:260px;max-height:70vh;overflow:auto;box-shadow:0 10px 28px rgba(0,0,0,0.5);font-family:-apple-system,"Segoe UI",Roboto,sans-serif;';
    if (window.__rdioMini && window.__rdioMini.on) {
      panel.style.left = '6px'; panel.style.right = '6px'; panel.style.top = '30px'; panel.style.bottom = '6px';
      panel.style.transform = 'none'; panel.style.maxHeight = 'none'; panel.style.minWidth = '0';
    }
    var h = document.createElement('div');
    h.textContent = 'Audio output';
    h.style.cssText = 'padding:6px 12px;font-size:12px;letter-spacing:.06em;text-transform:uppercase;color:#aab;';
    panel.appendChild(h);
    var cur = saved();
    panel.appendChild(row('System default', cur === '', function () { choose(''); }));
    var named = false;
    devices.forEach(function (d) {
      if (d.deviceId === 'default' || d.deviceId === 'communications') return;
      if (d.label) named = true;
      panel.appendChild(row(d.label || 'Audio device', cur === d.deviceId, function () { choose(d.deviceId); }));
    });
    if (!named && devices.length) {
      panel.appendChild(row('Show device names…', false, function () {
        navigator.mediaDevices.getUserMedia({ audio: true }).then(function (st) {
          st.getTracks().forEach(function (t) { t.stop(); });
          list();
        }, function () { show(devices, 'Permission was not given, so names are hidden.'); });
      }));
    }
    if (msg) panel.appendChild(note(msg));
    panel.appendChild(row('Close', false, closePanel));
    document.body.appendChild(panel);
  }
  function choose(id) {
    try { if (id) localStorage.setItem(KEY, id); else localStorage.removeItem(KEY); } catch (e) {}
    applyAll();
    closePanel();
  }
  function list() {
    if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) {
      show([], 'Audio output selection is not available on this page yet. If this server address is new, restart the app once.');
      return;
    }
    navigator.mediaDevices.enumerateDevices().then(function (all) {
      show(all.filter(function (d) { return d.kind === 'audiooutput'; }));
    }, function () { show([], 'Could not list audio devices.'); });
  }
  window.__rdioPickAudio = list;
  document.addEventListener('mousedown', function (e) {
    if (panel && !panel.contains(e.target)) closePanel();
  }, true);
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') closePanel(); });
  try {
    new MutationObserver(function (muts) {
      muts.forEach(function (m) { m.addedNodes.forEach(function (n) {
        if (n.nodeType !== 1) return;
        if (n.tagName === 'AUDIO' || n.tagName === 'VIDEO') applyEl(n);
        else if (n.querySelectorAll) n.querySelectorAll('audio,video').forEach(applyEl);
      }); });
    }).observe(document.documentElement, { childList: true, subtree: true });
  } catch (e) {}
})();"##;

/// Injected into the main window's page: a 28px strip across the top (the
/// drag handle, via data-tauri-drag-region; minimize/close buttons at the
/// right).
const INIT_SCRIPT: &str = r##"(function () {
  if (window.__rdioStrip) return;
  window.__rdioStrip = true;
  // Heartbeat for the app's crash watchdog.
  (function beat() {
    var ok = false;
    try { window.__TAURI__.event.emit('rdio-heartbeat', '1'); ok = true; } catch (e) {}
    setTimeout(beat, ok ? 10000 : 1000);
  })();
  var apiTries = 0;
  function api() {
    try {
      var w = window.__TAURI__ && window.__TAURI__.window;
      return (w && w.getCurrentWindow) ? w : null;
    } catch (e) { return null; }
  }
  function build() {
    try {
      if (document.getElementById('rdio-top-strip')) return true;
      var root = document.documentElement || document.body;
      if (!root) return false;
      var strip = document.createElement('div');
      strip.id = 'rdio-top-strip';
      strip.style.cssText = 'position:fixed;top:0;left:0;right:0;height:28px;z-index:2147483647;display:flex;align-items:stretch;';
      var drag = document.createElement('div');
      drag.setAttribute('data-tauri-drag-region', '');
      drag.title = 'Drag to move';
      drag.style.cssText = 'flex:1;';
      // In fullscreen the top strip must not move the window.
      document.addEventListener('fullscreenchange', function () {
        if (document.fullscreenElement) drag.removeAttribute('data-tauri-drag-region');
        else drag.setAttribute('data-tauri-drag-region', '');
      });
      var btns = document.createElement('div');
      btns.id = 'rdio-strip-btns';
      btns.style.cssText = 'display:flex;align-items:center;gap:6px;align-self:flex-start;margin:5px 10px 0 0;border-radius:12px;';
      if (!document.getElementById('rdio-strip-css')) {
        var css = document.createElement('style');
        css.id = 'rdio-strip-css';
        css.textContent =
          '@keyframes rdio-strip-in{from{opacity:0;transform:translateY(-8px)}to{opacity:1;transform:none}}' +
          '@keyframes rdio-sb-pop{0%{box-shadow:0 0 0 0 rgba(255,255,255,.5)}100%{box-shadow:0 0 0 9px rgba(255,255,255,0)}}' +
          '#rdio-strip-btns,#rdio-mini-btn{animation:rdio-strip-in .45s cubic-bezier(.2,.8,.2,1) both;}' +
          '#rdio-mini-btn{animation-delay:.05s}' +
          '.rdio-sb{position:relative;width:40px;height:30px;padding:0;border:none;border-radius:11px;background:rgba(28,33,44,.55);-webkit-backdrop-filter:blur(10px) saturate(140%);backdrop-filter:blur(10px) saturate(140%);color:#eef2f8;cursor:pointer;display:flex;align-items:center;justify-content:center;outline:none;transition:background .18s ease,color .18s ease,transform .2s cubic-bezier(.2,.8,.2,1),box-shadow .18s ease;}' +
          '.rdio-sb svg{width:15px;height:15px;fill:none;stroke:currentColor;stroke-width:1.9;stroke-linecap:round;stroke-linejoin:round;overflow:visible;transition:transform .25s cubic-bezier(.2,.8,.2,1);}' +
          '.rdio-sb:hover{background:rgba(255,255,255,.26);color:#fff;transform:translateY(-1px);}' +
          '.rdio-sb:active{transform:scale(.86);transition-duration:.06s;}' +
          '.rdio-sb:focus-visible{box-shadow:0 0 0 2px rgba(120,180,255,.9);}' +
          '.rdio-sb.pop{animation:rdio-sb-pop .45s ease-out;}' +
          '.rdio-sb-min:hover svg{transform:translateY(3px);}' +
          '.rdio-sb-close:hover{background:#e5484d;color:#fff;}' +
          '.rdio-sb-close:hover svg{transform:rotate(90deg) scale(1.1);}' +
          '.rdio-sb-mini{width:50px;height:34px;border-radius:13px;background:rgba(58,128,236,.62);}' +
          '.rdio-sb-mini svg{width:20px;height:20px;}' +
          '.rdio-sb-mini:hover{background:rgba(80,150,255,.95);color:#fff;}' +
          '.rdio-sb-mini .inner{fill:currentColor;stroke:none;transform-box:fill-box;transform-origin:100% 100%;transition:transform .25s cubic-bezier(.2,.8,.2,1);}' +
          '.rdio-sb-mini:hover .inner{transform:scale(.6) translate(-2px,-2px);}' +
          '.rdio-sb-mini:hover svg{transform:scale(1.12);}' +
          '@media (max-width:1380px){html:not(.rdio-fs) .panel .mat-toolbar{height:104px!important;padding-top:36px!important;box-sizing:border-box!important;}}' +
          '@media (prefers-reduced-motion:reduce){.rdio-sb,.rdio-sb svg,.rdio-sb .inner{transition:none!important}#rdio-strip-btns,#rdio-mini-btn,.rdio-sb.pop{animation:none!important}}';
        (document.head || root).appendChild(css);
      }
      var ICONS = {
        min: '<svg viewBox="0 0 12 12"><path d="M2 9h8"/></svg>',
        close: '<svg viewBox="0 0 12 12"><path d="M2.5 2.5l7 7M9.5 2.5l-7 7"/></svg>',
        mini: '<svg viewBox="0 0 12 12"><rect x="1" y="1.5" width="10" height="9" rx="2"/><rect class="inner" x="6" y="6" width="4" height="3" rx="0.8"/></svg>'
      };
      function mkBtn(kind, title, fn) {
        var b = document.createElement('button');
        b.type = 'button';
        b.className = 'rdio-sb rdio-sb-' + kind;
        b.innerHTML = ICONS[kind];
        b.title = title;
        b.addEventListener('click', function (e) {
          e.preventDefault(); e.stopPropagation();
          b.classList.remove('pop'); void b.offsetWidth; b.classList.add('pop');
          try {
            var r = fn();
            if (r && r.then) {
              r.then(function () {}, function (err) {
                b.textContent = 'ERR';
                try { b.title = String(err); } catch (e2) {}
              });
            }
          } catch (err) {
            b.textContent = 'ERR';
            try { b.title = String(err); } catch (e2) {}
          }
        });
        return b;
      }
      function withWin(fn) {
        var a = api();
        if (!a) return null;
        try { return fn(a.getCurrentWindow()); } catch (e) { return null; }
      }
      btns.appendChild(mkBtn('min', 'Minimize', function () {
        return withWin(function (w) { return w.minimize(); });
      }));
      btns.appendChild(mkBtn('close', 'Close (tucks into tray)', function () {
        return withWin(function (w) { return w.close(); });
      }));
      var left = document.createElement('div');
      left.id = 'rdio-mini-btn';
      left.style.cssText = 'display:flex;align-items:center;align-self:flex-start;margin:5px 0 0 10px;border-radius:12px;';
      left.appendChild(mkBtn('mini', 'Mini player', function () {
        if (window.__rdioMini) window.__rdioMini.toggle();
      }));
      strip.appendChild(left);
      strip.appendChild(drag);
      strip.appendChild(btns);
      root.appendChild(strip);
      (function markApi() {
        if (api()) return;
        if (++apiTries > 100) {
          btns.style.background = 'rgba(160,30,30,0.85)';
          return;
        }
        setTimeout(markApi, 100);
      })();
      return true;
    } catch (e) { return false; }
  }
  (function tick() {
    if (!build()) setTimeout(tick, 200);
  })();
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { build(); });
  }
  window.addEventListener('load', function () { build(); });
  setTimeout(function () { build(); }, 3000);
  // Whole-window drag: press and drag from any non-interactive part of
  // the page to move the window. The drag only engages after the pointer
  // moves a few pixels, so plain clicks (e.g. clicking a call to play it)
  // still land normally; buttons, inputs, sliders, links and media
  // controls are excluded entirely and keep their normal behavior.
  var INTERACTIVE_SEL = 'button, input, select, textarea, a, label, audio, video, [contenteditable], [role="button"], [role="slider"], [role="listbox"], [role="option"]';
  function isInteractiveTarget(t) {
    return !!(t && t.closest && t.closest(INTERACTIVE_SEL));
  }
  function inStrip(t) {
    return !!(t && t.closest && t.closest('#rdio-top-strip'));
  }
  var dragPending = null;
  function isFull() { return !!document.fullscreenElement; }
  document.addEventListener('mousedown', function (e) {
    if (e.button !== 0) return;
    // Fullscreen: the window never moves.
    if (isFull()) return;
    if (isInteractiveTarget(e.target) || inStrip(e.target)) return;
    dragPending = { x: e.clientX, y: e.clientY };
  }, true);
  document.addEventListener('mousemove', function (e) {
    if (!dragPending) return;
    var dx = e.clientX - dragPending.x;
    var dy = e.clientY - dragPending.y;
    if (dx * dx + dy * dy < 16) return;
    dragPending = null;
    var a = api();
    if (!a) return;
    try {
      var p = a.getCurrentWindow().startDragging();
      if (p && p.catch) p.catch(function () {});
    } catch (err) {}
  }, true);
  document.addEventListener('mouseup', function () { dragPending = null; }, true);
  // Fullscreen zoom: like browser zoom, but only while the scanner is
  // fullscreen. It is applied the moment fullscreen starts, at a size that
  // fills the screen, and can be changed with a slider (opened from the
  // magnifier button), Ctrl + / Ctrl - / Ctrl 0 or Ctrl + mouse wheel. The
  // last size you pick is remembered, and it resets on leaving fullscreen.
  var ZKEY = 'rdio-sherlock-fs-zoom';
  var fsZoom = 1;
  var baseW = 1920, baseH = 1080;   // unzoomed screen size, read on entering
  var zPill = null, zLabel = null, zSlider = null, zBtn = null;
  var zTimer = null, zRaf = 0, zPending = 1;
  function webviewObj() {
    try {
      var t = window.__TAURI__;
      return (t && t.webview && t.webview.getCurrentWebview) ? t.webview.getCurrentWebview() : null;
    } catch (e) { return null; }
  }
  function pushZoom(z) {
    zPending = z;
    if (zRaf) return;
    zRaf = requestAnimationFrame(function () {
      zRaf = 0;
      var wv = webviewObj();
      if (!wv) return;
      try { var p = wv.setZoom(zPending); if (p && p.catch) p.catch(function () {}); } catch (e) {}
    });
  }
  // 100% = 1.00; about 145% on a 1920x1080 screen, larger on bigger screens
  function fitZoom() {
    var z = Math.min(baseW / 1324, baseH / 745);
    return Math.max(1, Math.min(2.5, Math.round(z * 20) / 20));
  }
  function savedZoom() {
    try {
      var v = parseFloat(localStorage.getItem(ZKEY));
      return (isFinite(v) && v >= 0.5 && v <= 3) ? v : null;
    } catch (e) { return null; }
  }
  function setZoom(z, remember) {
    z = Math.max(0.5, Math.min(3, Math.round(z * 20) / 20));
    fsZoom = z;
    pushZoom(z);
    if (remember !== undefined) {
      try {
        if (remember) localStorage.setItem(ZKEY, String(z));
        else localStorage.removeItem(ZKEY);
      } catch (e) {}
    }
    showPill();
  }
  function buildPill() {
    if (zPill) return;
    var st = document.createElement('style');
    st.textContent =
      '#rdio-zoom-pill{position:fixed;right:18px;bottom:18px;z-index:2147483647;display:flex;align-items:center;border-radius:14px;background:rgba(28,33,44,.62);-webkit-backdrop-filter:blur(10px) saturate(140%);backdrop-filter:blur(10px) saturate(140%);color:#eef2f8;font:600 13px -apple-system,"Segoe UI",Roboto,sans-serif;opacity:0;transform:translateY(8px);pointer-events:none;transition:opacity .25s ease,transform .25s cubic-bezier(.2,.8,.2,1);user-select:none;-webkit-user-select:none;}' +
      '#rdio-zoom-pill.show{opacity:1;transform:none;pointer-events:auto;}' +
      '#rdio-zoom-pill .zbtn{width:40px;height:36px;border:none;border-radius:14px;background:transparent;color:inherit;cursor:pointer;display:flex;align-items:center;justify-content:center;transition:background .15s ease,transform .1s ease;}' +
      '#rdio-zoom-pill .zbtn:hover{background:rgba(255,255,255,.2);}' +
      '#rdio-zoom-pill .zbtn:active{transform:scale(.9);}' +
      '#rdio-zoom-pill .zbtn svg{width:19px;height:19px;fill:none;stroke:currentColor;stroke-width:1.9;stroke-linecap:round;stroke-linejoin:round;}' +
      '#rdio-zoom-pill .zpanel{display:flex;align-items:center;gap:10px;max-width:0;opacity:0;overflow:hidden;padding:0;white-space:nowrap;transition:max-width .28s cubic-bezier(.2,.8,.2,1),opacity .2s ease,padding .28s ease;}' +
      '#rdio-zoom-pill.open .zpanel{max-width:260px;opacity:1;padding:0 4px 0 14px;}' +
      '#rdio-zoom-pill .zlbl{min-width:44px;text-align:right;cursor:pointer;}' +
      '#rdio-zoom-pill input[type=range]{-webkit-appearance:none;appearance:none;width:150px;height:4px;border-radius:2px;background:rgba(255,255,255,.3);outline:none;margin:0;cursor:pointer;}' +
      '#rdio-zoom-pill input[type=range]::-webkit-slider-thumb{-webkit-appearance:none;appearance:none;width:16px;height:16px;border-radius:50%;background:#fff;box-shadow:0 1px 4px rgba(0,0,0,.4);transition:transform .12s ease;}' +
      '#rdio-zoom-pill input[type=range]::-webkit-slider-thumb:hover{transform:scale(1.2);}';
    zPill = document.createElement('div');
    zPill.id = 'rdio-zoom-pill';
    zPill.appendChild(st);
    var panel = document.createElement('div');
    panel.className = 'zpanel';
    zSlider = document.createElement('input');
    zSlider.type = 'range'; zSlider.min = '50'; zSlider.max = '300'; zSlider.step = '5';
    // Dragging only previews the percentage; the zoom is applied once on
    // release. Re-zooming the page on every step made the control jump
    // under the pointer.
    zSlider.addEventListener('input', function () {
      zLabel.textContent = zSlider.value + '%';
      armPill();
    });
    zSlider.addEventListener('change', function () { setZoom(parseInt(zSlider.value, 10) / 100, true); });
    zLabel = document.createElement('span');
    zLabel.className = 'zlbl'; zLabel.title = 'Click to fit the screen (Ctrl 0)';
    zLabel.addEventListener('click', function (e) { e.stopPropagation(); setZoom(fitZoom(), false); });
    panel.appendChild(zSlider); panel.appendChild(zLabel);
    zBtn = document.createElement('button');
    zBtn.type = 'button'; zBtn.className = 'zbtn'; zBtn.title = 'Zoom';
    zBtn.innerHTML = '<svg viewBox="0 0 24 24"><circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5L21 21M10.5 8v5M8 10.5h5"/></svg>';
    zBtn.addEventListener('click', function (e) {
      e.stopPropagation();
      zPill.classList.toggle('open');
      showPill();
    });
    // clicks on the control must not reach the scanner (a double-click there leaves fullscreen)
    ['dblclick', 'mousedown'].forEach(function (n) {
      zPill.addEventListener(n, function (e) { e.stopPropagation(); }, true);
    });
    zPill.addEventListener('mouseenter', function () { if (zTimer) clearTimeout(zTimer); });
    zPill.addEventListener('mouseleave', showPill);
    zPill.appendChild(panel); zPill.appendChild(zBtn);
  }
  // Keep the control the same on-screen size whatever the zoom is, using
  // the zoom the page really has (screen width / viewport width) rather than
  // the one that was requested, so the two can never disagree for a frame.
  function syncPillScale() {
    if (!zPill) return;
    var r = window.innerWidth / baseW;
    if (!(r > 0.2 && r < 2)) r = 1 / fsZoom;
    zPill.style.zoom = String(r);
  }
  function armPill() {
    if (zTimer) clearTimeout(zTimer);
    zTimer = setTimeout(function () {
      if (!zPill) return;
      zPill.classList.remove('show');
      zPill.classList.remove('open');
    }, zPill && zPill.classList.contains('open') ? 5000 : 2500);
  }
  function showPill() {
    var host = document.fullscreenElement;
    if (!host) return;
    buildPill();
    if (zPill.parentNode !== host) host.appendChild(zPill);
    syncPillScale();
    if (document.activeElement !== zSlider) {
      zLabel.textContent = Math.round(fsZoom * 100) + '%';
      zSlider.value = String(Math.round(fsZoom * 100));
    }
    zPill.classList.add('show');
    armPill();
  }
  window.addEventListener('resize', function () { if (isFull()) syncPillScale(); });
  document.addEventListener('fullscreenchange', function () {
    document.documentElement.classList.toggle('rdio-fs', !!document.fullscreenElement);
    if (document.fullscreenElement) {
      // zoom is still 1 here, so the screen size is the true unzoomed size
      baseW = window.screen.width || baseW;
      baseH = window.screen.height || baseH;
      var s0 = savedZoom();
      setZoom(s0 !== null ? s0 : fitZoom());
    } else {
      fsZoom = 1;
      pushZoom(1);
      if (zTimer) clearTimeout(zTimer);
      if (zPill) { zPill.classList.remove('show'); zPill.classList.remove('open'); if (zPill.parentNode) zPill.parentNode.removeChild(zPill); }
    }
  });
  document.addEventListener('mousemove', function () { if (isFull()) showPill(); }, true);
  document.addEventListener('keydown', function (e) {
    if (!isFull() || !(e.ctrlKey || e.metaKey)) return;
    var k = e.key;
    if (k === '+' || k === '=') { setZoom(fsZoom + 0.1, true); }
    else if (k === '-' || k === '_') { setZoom(fsZoom - 0.1, true); }
    else if (k === '0') { setZoom(fitZoom(), false); }
    else return;
    e.preventDefault(); e.stopPropagation();
  }, true);
  document.addEventListener('wheel', function (e) {
    if (!isFull() || !(e.ctrlKey || e.metaKey)) return;
    e.preventDefault();
    setZoom(fsZoom + (e.deltaY < 0 ? 0.1 : -0.1), true);
  }, { capture: true, passive: false });
  // Live title: mirror the scanner display's current call in the window
  // title and the tray tooltip ("Rdio Sherlock — <talkgroup>"), back to
  // the plain name when idle. Reads the v6 display DOM (.rdio-display);
  // on other page layouts it simply never fires and the title stays put.
  var BASE_TITLE = 'Rdio Sherlock';
  var lastTitle = BASE_TITLE;
  var lastTooltip = BASE_TITLE;
  var lastCallText = '';
  var trayIconPromise = null;
  function setAppTitle(t) {
    var nolink = / \(no link\)$/.test(t);
    if (t !== BASE_TITLE && !nolink) {
      lastCallText = t + ' · ' + new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
    }
    // The window title goes back to the plain name when idle (per the
    // taskbar rule), but the tray tooltip then carries the last call
    // heard, so hovering the icon shows what was missed.
    var tooltip = nolink
      ? BASE_TITLE + ' — no link to the server'
      : (t === BASE_TITLE && lastCallText)
        ? BASE_TITLE + ' — last: ' + lastCallText
        : t;
    if (t === lastTitle && tooltip === lastTooltip) return;
    lastTitle = t;
    lastTooltip = tooltip;
    var a = api();
    if (!a) return;
    try {
      var p = a.getCurrentWindow().setTitle(t);
      if (p && p.catch) p.catch(function () {});
    } catch (err) {}
    try {
      if (window.__TAURI__.tray && window.__TAURI__.tray.TrayIcon) {
        if (!trayIconPromise) {
          trayIconPromise = window.__TAURI__.tray.TrayIcon.getById('tray');
        }
        trayIconPromise.then(function (icon) {
          if (icon) {
            var q = icon.setTooltip(tooltip);
            if (q && q.catch) q.catch(function () {});
          }
        }).catch(function () {});
      }
    } catch (err) {}
  }
  function readTitle() {
    var disp = document.querySelector('.rdio-display');
    if (!disp) return null;
    if ((disp.textContent || '').indexOf('NO LINK') !== -1) return BASE_TITLE + ' (no link)';
    // The display keeps showing the last talkgroup after a call ends; the LED
    // is only lit while a call is actually playing.
    var ledEl = document.querySelector('.rdio-status .led');
    if (ledEl && !ledEl.classList.contains('on')) return BASE_TITLE;
    var big = disp.querySelector('.row.big span');
    var name = big ? (big.textContent || '').trim() : '';
    if (!name || /^rdio scanner/i.test(name)) return BASE_TITLE;
    var label = '';
    var tgid = '';
    var rows = disp.querySelectorAll('.row');
    try {
      var cell = rows[3] && rows[3].querySelector('div span');
      if (cell) label = (cell.textContent || '').trim();
      var rowText = rows[5] ? (rows[5].textContent || '') : '';
      var m = rowText.match(/TGID:\s*(\d+)/);
      if (m) tgid = m[1];
    } catch (e) {}
    // While a call plays, the talkgroup ID takes the app name's place
    // at the front, so the taskbar shows as much of the call as fits.
    var t = (tgid ? tgid + ' — ' : '') + name;
    if (label && label !== name) t += ' (' + label + ')';
    return t;
  }
  function tickTitle() {
    var t = readTitle();
    if (t !== null) setAppTitle(t);
  }
  try {
    var titleObserver = new MutationObserver(function () { tickTitle(); });
    var startTitleObs = function () {
      if (document.body) {
        titleObserver.observe(document.body, {
          childList: true, subtree: true, characterData: true
        });
      }
    };
    if (document.body) startTitleObs();
    else document.addEventListener('DOMContentLoaded', startTitleObs);
  } catch (e) {}
  setInterval(tickTitle, 2000);
  // Custom right-click menu: the browser default menu is suppressed
  // everywhere; instead a small menu offers Change server (straight to
  // the setup form, no auto-connect).
  var ctxMenu = null;
  var alwaysOnTop = false;
  function closeCtxMenu() {
    if (ctxMenu) { ctxMenu.remove(); ctxMenu = null; }
  }
  function mkCtxItem(label, onClick) {
    var el = document.createElement('div');
    el.textContent = label;
    el.style.cssText = 'padding:8px 12px;font-size:13px;color:#f2f2f2;border-radius:5px;cursor:pointer;user-select:none;';
    el.addEventListener('mouseenter', function () { el.style.background = 'rgba(255,255,255,0.10)'; });
    el.addEventListener('mouseleave', function () { el.style.background = 'transparent'; });
    el.addEventListener('click', function () { closeCtxMenu(); onClick(); });
    return el;
  }
  document.addEventListener('contextmenu', function (e) {
    e.preventDefault();
    closeCtxMenu();
    if (window.__rdioMini && window.__rdioMini.on) return;
    ctxMenu = document.createElement('div');
    ctxMenu.style.cssText = 'position:fixed;z-index:2147483647;background:#1e1e24;border:1px solid rgba(255,255,255,0.16);border-radius:8px;padding:4px;min-width:170px;box-shadow:0 10px 28px rgba(0,0,0,0.5);font-family:-apple-system,"Segoe UI",Roboto,sans-serif;';
    ctxMenu.style.left = Math.min(e.clientX, window.innerWidth - 190) + 'px';
    ctxMenu.style.top = Math.min(e.clientY, window.innerHeight - 190) + 'px';
    ctxMenu.appendChild(mkCtxItem('Change server…', function () {
      if (window.__rdioMini && window.__rdioMini.on) window.__rdioMini.exit();
      window.location.href = 'http://tauri.localhost/index.html#edit';
    }));
    ctxMenu.appendChild(mkCtxItem((alwaysOnTop ? '✓ ' : '') + 'Always on top', function () {
      alwaysOnTop = !alwaysOnTop;
      var a = api();
      if (!a) return;
      try {
        var p = a.getCurrentWindow().setAlwaysOnTop(alwaysOnTop);
        if (p && p.catch) p.catch(function () {});
      } catch (err) {}
    }));
    ctxMenu.appendChild(mkCtxItem(window.__rdioMini && window.__rdioMini.on ? 'Full view' : 'Mini player', function () {
      if (window.__rdioMini) window.__rdioMini.toggle();
    }));
    ctxMenu.appendChild(mkCtxItem('Audio output\u2026', function () {
      if (window.__rdioPickAudio) window.__rdioPickAudio();
    }));
    document.body.appendChild(ctxMenu);
  });
  document.addEventListener('mousedown', function (e) {
    if (ctxMenu && !ctxMenu.contains(e.target)) closeCtxMenu();
  }, true);
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') closeCtxMenu();
  });
  window.addEventListener('blur', closeCtxMenu);
})();"##;

fn refresh_tray_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let muted = MUTED.load(Ordering::SeqCst);
    let sep = |a: &AppHandle<R>| PredefinedMenuItem::separator(a);

    let mute = MenuItem::with_id(app, "mute", if muted { "Unmute" } else { "Mute" }, true, None::<&str>)?;

    // Snooze
    let left = snooze_minutes_left();
    let snooze_title = if left > 0 { format!("Snooze (on, {} min left)", left) } else { "Snooze".to_string() };
    let sn15 = MenuItem::with_id(app, "snooze_15", "Mute for 15 minutes", true, None::<&str>)?;
    let sn30 = MenuItem::with_id(app, "snooze_30", "Mute for 30 minutes", true, None::<&str>)?;
    let sn60 = MenuItem::with_id(app, "snooze_60", "Mute for 1 hour", true, None::<&str>)?;
    let sn120 = MenuItem::with_id(app, "snooze_120", "Mute for 2 hours", true, None::<&str>)?;
    let snoff = MenuItem::with_id(app, "snooze_off", "Cancel snooze", left > 0, None::<&str>)?;
    let sn_sep = sep(app)?;
    let snooze_menu = Submenu::with_items(
        app,
        snooze_title,
        true,
        &[&sn15 as &dyn IsMenuItem<R>, &sn30, &sn60, &sn120, &sn_sep, &snoff],
    )?;

    // Quiet hours
    let cur = quiet_text(app);
    let curq = parse_quiet(&cur);
    let presets: [(&str, &str); 5] = [
        ("21:00-06:00", "9 PM – 6 AM"),
        ("22:00-06:00", "10 PM – 6 AM"),
        ("23:00-07:00", "11 PM – 7 AM"),
        ("00:00-06:00", "Midnight – 6 AM"),
        ("00:00-08:00", "Midnight – 8 AM"),
    ];
    let q_off = CheckMenuItem::with_id(app, "quiet_off", "Off", true, curq.is_none(), None::<&str>)?;
    let mut q_items: Vec<CheckMenuItem<R>> = Vec::new();
    let mut matched = curq.is_none();
    for (val, label) in presets.iter() {
        let on = parse_quiet(val) == curq && curq.is_some();
        if on { matched = true; }
        q_items.push(CheckMenuItem::with_id(app, format!("quiet:{}", val), *label, true, on, None::<&str>)?);
    }
    let custom_label = match (curq, matched) {
        (Some((s, e)), false) => format!("Custom: {} – {} (edit file…)", fmt_clock(s), fmt_clock(e)),
        _ => "Custom (edit file…)".to_string(),
    };
    let q_custom = CheckMenuItem::with_id(app, "quiet_custom", custom_label, true, curq.is_some() && !matched, None::<&str>)?;
    let q_sep = sep(app)?;
    let mut qrefs: Vec<&dyn IsMenuItem<R>> = vec![&q_off];
    for it in q_items.iter() {
        qrefs.push(it);
    }
    qrefs.push(&q_sep);
    qrefs.push(&q_custom);
    let quiet_title = match curq {
        Some((s, e)) => format!("Quiet hours ({} – {})", fmt_clock(s), fmt_clock(e)),
        None => "Quiet hours".to_string(),
    };
    let quiet_menu = Submenu::with_items(app, quiet_title, true, &qrefs)?;

    // Mini player
    let mini = MenuItem::with_id(app, "mini", "Show / hide mini player", true, None::<&str>)?;
    let opa = MINI_OPACITY.load(Ordering::SeqCst);
    let mut opa_items: Vec<CheckMenuItem<R>> = Vec::new();
    for v in [100u32, 85, 70, 50] {
        opa_items.push(CheckMenuItem::with_id(app, format!("opa:{}", v), format!("{}%", v), true, opa == v, None::<&str>)?);
    }
    let orefs: Vec<&dyn IsMenuItem<R>> = opa_items.iter().map(|i| i as &dyn IsMenuItem<R>).collect();
    let opa_menu = Submenu::with_items(app, "Opacity", true, &orefs)?;
    let corner = MINI_CORNER.load(Ordering::SeqCst);
    let mut corner_items: Vec<CheckMenuItem<R>> = Vec::new();
    for (v, label) in [(0u32, "Don't move it"), (1, "Top left"), (5, "Top center"), (2, "Top right"), (3, "Bottom left"), (6, "Bottom center"), (4, "Bottom right")] {
        corner_items.push(CheckMenuItem::with_id(app, format!("corner:{}", v), label, true, corner == v, None::<&str>)?);
    }
    let crefs: Vec<&dyn IsMenuItem<R>> = corner_items.iter().map(|i| i as &dyn IsMenuItem<R>).collect();
    let corner_menu = Submenu::with_items(app, "Opens in", true, &crefs)?;
    let popup = CheckMenuItem::with_id(app, "popup", "Pop-up mode (only show during calls)", true, POPUP.load(Ordering::SeqCst), None::<&str>)?;
    let start_mini = CheckMenuItem::with_id(app, "start_mini", "Mini Player At Launch", true, START_MINI.load(Ordering::SeqCst), None::<&str>)?;
    let mini_menu = Submenu::with_items(app, "Mini player", true, &[&mini as &dyn IsMenuItem<R>, &popup, &start_mini, &opa_menu, &corner_menu])?;

    let audio = MenuItem::with_id(app, "audio", "Audio output…", true, None::<&str>)?;

    // Startup & power
    let autostart = CheckMenuItem::with_id(app, "autostart", "Start on login", true, autostart_enabled(), None::<&str>)?;
    let start_hidden = CheckMenuItem::with_id(app, "start_hidden", "Start minimized to tray", true, START_HIDDEN.load(Ordering::SeqCst), None::<&str>)?;
    let auto_live = CheckMenuItem::with_id(app, "auto_live", "Start Live Feed automatically", true, AUTO_LIVE.load(Ordering::SeqCst), None::<&str>)?;
    let keep_awake = CheckMenuItem::with_id(app, "keep_awake", "Keep PC awake (screen can still turn off)", true, KEEP_AWAKE.load(Ordering::SeqCst), None::<&str>)?;
    let startup_menu = Submenu::with_items(
        app,
        "Startup & power",
        true,
        &[&autostart as &dyn IsMenuItem<R>, &start_hidden, &auto_live, &keep_awake],
    )?;

    let server = MenuItem::with_id(app, "server", "Servers…", true, None::<&str>)?;

    // Updates (Windows self-update)
    #[cfg(windows)]
    let updates_menu = {
        let st = UPDATE_STATE.load(Ordering::SeqCst);
        let latest = UPDATE_LATEST.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
        let (label, enabled) = match st {
            1 => ("Checking…".to_string(), false),
            2 => ("You're on the current build".to_string(), true),
            3 => (format!("Install v{} now", latest.trim_start_matches('v')), true),
            4 => ("Installing…".to_string(), false),
            5 => ("Couldn't reach GitHub — check again".to_string(), true),
            6 => (format!("Update failed — try v{} again", latest.trim_start_matches('v')), true),
            _ => ("Check for updates".to_string(), true),
        };
        let ver = MenuItem::with_id(app, "update_ver", format!("Version {}", env!("CARGO_PKG_VERSION")), false, None::<&str>)?;
        let act = MenuItem::with_id(app, "update_action", label, enabled, None::<&str>)?;
        let auto = CheckMenuItem::with_id(app, "auto_update", "Install updates automatically", true, AUTO_UPDATE.load(Ordering::SeqCst), None::<&str>)?;
        Submenu::with_items(
            app,
            if st == 3 { "Updates (new version!)" } else { "Updates" },
            true,
            &[&ver as &dyn IsMenuItem<R>, &act, &auto],
        )?
    };
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let s1 = sep(app)?;
    let s2 = sep(app)?;
    let s3 = sep(app)?;

    let mut items: Vec<&dyn IsMenuItem<R>> = vec![
        &mute, &snooze_menu, &quiet_menu, &s1, &mini_menu, &audio, &s2, &startup_menu,
    ];
    #[cfg(windows)]
    items.push(&updates_menu);
    items.push(&server);
    items.push(&s3);
    items.push(&quit);
    let menu = Menu::with_items(app, &items)?;
    if let Some(tray) = app.tray_by_id("tray") {
        let _ = tray.set_menu(Some(menu));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Self-update (Windows): checks the GitHub releases for a newer version,
// downloads the new exe, verifies its SHA-256 against the checksum file
// published with the release, swaps it in (a running exe can be renamed but
// not overwritten) and relaunches.
// ---------------------------------------------------------------------------
const RELEASE_REPO: &str = "leloyon/Rdio-Sherlock";
/// 0 unknown, 1 checking, 2 up to date, 3 update available, 4 installing,
/// 5 could not check, 6 install failed
static UPDATE_STATE: AtomicU32 = AtomicU32::new(0);
static UPDATE_LATEST: Mutex<Option<String>> = Mutex::new(None);
static AUTO_UPDATE: AtomicBool = AtomicBool::new(false);
/// True while a call is playing (set by the tick thread).
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn parse_ver(s: &str) -> Vec<u32> {
    s.trim().trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let mut a = parse_ver(candidate);
    let mut b = parse_ver(current);
    while a.len() < b.len() { a.push(0); }
    while b.len() < a.len() { b.push(0); }
    a > b
}

fn valid_tag(t: &str) -> bool {
    let n = t.trim_start_matches('v');
    !n.is_empty() && n.len() <= 20 && n.chars().all(|c| c.is_ascii_digit() || c == '.')
}

#[cfg(windows)]
fn run_ps(script: &str, envs: &[(&str, &str)]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(0x0800_0000);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ok(Some(tag)) = latest release, Ok(None) = no releases published yet,
/// Err = could not reach GitHub.
#[cfg(windows)]
fn fetch_latest_tag() -> Result<Option<String>, ()> {
    let script = format!(
        "$ProgressPreference='SilentlyContinue';[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12;try{{$r=Invoke-RestMethod -Uri 'https://api.github.com/repos/{}/releases/latest' -Headers @{{'User-Agent'='RdioSherlock'}};Write-Output $r.tag_name}}catch{{if($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404){{Write-Output 'NONE'}}else{{Write-Output 'ERR'}}}}",
        RELEASE_REPO
    );
    let out = run_ps(&script, &[]).ok_or(())?;
    let line = out.lines().last().ok_or(())?.trim().to_string();
    if line == "NONE" {
        Ok(None)
    } else if valid_tag(&line) {
        Ok(Some(line))
    } else {
        Err(())
    }
}

#[cfg(not(windows))]
fn fetch_latest_tag() -> Result<Option<String>, ()> {
    Ok(None)
}

fn check_for_updates<R: Runtime>(app: &AppHandle<R>) {
    UPDATE_STATE.store(1, Ordering::SeqCst);
    let _ = refresh_tray_menu(app);
    match fetch_latest_tag() {
        Ok(Some(tag)) => {
            if is_newer(&tag, env!("CARGO_PKG_VERSION")) {
                if let Ok(mut g) = UPDATE_LATEST.lock() {
                    *g = Some(tag);
                }
                UPDATE_STATE.store(3, Ordering::SeqCst);
            } else {
                if let Ok(mut g) = UPDATE_LATEST.lock() {
                    *g = None;
                }
                UPDATE_STATE.store(2, Ordering::SeqCst);
            }
        }
        // no release published yet: nothing newer than this build
        Ok(None) => {
            if let Ok(mut g) = UPDATE_LATEST.lock() {
                *g = None;
            }
            UPDATE_STATE.store(2, Ordering::SeqCst);
        }
        Err(()) => UPDATE_STATE.store(5, Ordering::SeqCst),
    }
    let _ = refresh_tray_menu(app);
}

const UPDATE_NOTICE_JS: &str = r##"(function(){try{if(document.getElementById('rdio-update-notice'))return;var d=document.createElement('div');d.id='rdio-update-notice';d.style.cssText='position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;background:rgba(10,12,18,0.88);font-family:-apple-system,"Segoe UI",Roboto,sans-serif;';var b=document.createElement('div');b.style.cssText='max-width:380px;padding:22px 26px;border-radius:12px;background:#1e1e24;border:1px solid rgba(255,255,255,0.16);color:#f2f2f2;text-align:center;line-height:1.45;';var h=document.createElement('div');h.textContent='Updating Rdio Sherlock';h.style.cssText='font-size:17px;font-weight:600;margin-bottom:8px;';var t=document.createElement('div');t.textContent='Downloading and checking the new version. The app will restart by itself in a moment.';t.style.cssText='font-size:13px;color:#c8c8d0;';b.appendChild(h);b.appendChild(t);d.appendChild(b);(document.body||document.documentElement).appendChild(d);}catch(e){}})();"##;
const UPDATE_NOTICE_OFF_JS: &str = r##"(function(){try{var d=document.getElementById('rdio-update-notice');if(d)d.remove();}catch(e){}})();"##;

#[cfg(windows)]
const DOWNLOAD_PS: &str = "$ProgressPreference='SilentlyContinue';[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12;try{$w=New-Object Net.WebClient;$w.Headers.Add('User-Agent','RdioSherlock');$w.DownloadFile($env:RS_URL,$env:RS_OUT);$want=($w.DownloadString($env:RS_SHA_URL).Trim().Split()[0]).ToLower();$got=(Get-FileHash -Algorithm SHA256 -Path $env:RS_OUT).Hash.ToLower();if($got -ne $want){Remove-Item $env:RS_OUT -Force;Write-Output 'FAIL:hash'}else{Write-Output 'OK'}}catch{Write-Output ('FAIL:'+$_.Exception.Message)}";

fn install_update<R: Runtime>(app: &AppHandle<R>) {
    let tag = match UPDATE_LATEST.lock().ok().and_then(|g| g.clone()) {
        Some(t) if valid_tag(&t) => t,
        _ => return,
    };
    if UPDATE_STATE.load(Ordering::SeqCst) == 4 {
        return;
    }
    UPDATE_STATE.store(4, Ordering::SeqCst);
    let _ = refresh_tray_menu(app);
    #[cfg(windows)]
    {
        let fail = |app: &AppHandle<R>| {
            UPDATE_STATE.store(6, Ordering::SeqCst);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.eval(UPDATE_NOTICE_OFF_JS);
            }
            let _ = refresh_tray_menu(app);
        };
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return fail(app),
        };
        let new = exe.with_extension("exe.new");
        let old = exe.with_extension("exe.old");
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
            let _ = w.eval(UPDATE_NOTICE_JS);
        }
        let base = format!("https://github.com/{}/releases/download/{}/", RELEASE_REPO, tag);
        let url = format!("{}Rdio-Sherlock.exe", base);
        let sha = format!("{}Rdio-Sherlock.exe.sha256", base);
        let out_path = new.to_string_lossy().to_string();
        let res = run_ps(DOWNLOAD_PS, &[("RS_URL", url.as_str()), ("RS_SHA_URL", sha.as_str()), ("RS_OUT", out_path.as_str())]);
        let ok = res.as_deref().map(|r| r.lines().last().unwrap_or("") == "OK").unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&new);
            return fail(app);
        }
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(&exe, &old).is_err() {
            let _ = std::fs::remove_file(&new);
            return fail(app);
        }
        if std::fs::rename(&new, &exe).is_err() {
            let _ = std::fs::rename(&old, &exe);
            return fail(app);
        }
        relaunch_path(app, &exe);
    }
    #[cfg(not(windows))]
    {
        let _ = tag;
        UPDATE_STATE.store(6, Ordering::SeqCst);
        let _ = refresh_tray_menu(app);
    }
}

/// Background checker: first check shortly after start, then every 6 hours.
/// With "install automatically" on, an available update is installed once no
/// call has been playing for a short while.
fn start_update_thread<R: Runtime>(app: AppHandle<R>) {
    #[cfg(windows)]
    std::thread::spawn(move || {
        // clean up leftovers from a previous update
        if let Ok(exe) = std::env::current_exe() {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let _ = std::fs::remove_file(exe.with_extension("exe.old"));
            let _ = std::fs::remove_file(exe.with_extension("exe.new"));
        }
        std::thread::sleep(std::time::Duration::from_secs(20));
        loop {
            check_for_updates(&app);
            if AUTO_UPDATE.load(Ordering::SeqCst) && UPDATE_STATE.load(Ordering::SeqCst) == 3 {
                let mut quiet_for = 0;
                while quiet_for < 5 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if ACTIVE.load(Ordering::SeqCst) { quiet_for = 0; } else { quiet_for += 1; }
                }
                install_update(&app);
            }
            std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
        }
    });
    #[cfg(not(windows))]
    let _ = app;
}

// ---------------------------------------------------------------------------
// Crash recovery: every page (setup page and scanner) sends a heartbeat. If
// they stop (renderer crashed or hung), reload the page, and if that does not
// bring the heartbeat back, restart the app. A restart is attempted at most
// once every 30 minutes so a persistent fault cannot cause a restart loop.
// ---------------------------------------------------------------------------
static LAST_BEAT: Mutex<Option<Instant>> = Mutex::new(None);

fn recovery_restart_allowed<R: Runtime>(app: &AppHandle<R>) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let last = load_u32(app, "last_recovery.txt", 0);
    if now.saturating_sub(last) < 1800 {
        return false;
    }
    save_u32(app, "last_recovery.txt", now);
    true
}

/// Start-on-login state: an "Rdio Sherlock" value under the per-user
/// Run key pointing at the current exe. Enabled only when the stored
/// path is this exe, so moving the portable exe reads as off until the
/// user toggles it again.
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_NAME: &str = "Rdio Sherlock";

fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Ok(out) = Command::new("reg")
            .args(["query", RUN_KEY, "/v", AUTOSTART_NAME])
            .creation_flags(0x0800_0000)
            .output()
        {
            if out.status.success() {
                if let Ok(exe) = std::env::current_exe() {
                    return String::from_utf8_lossy(&out.stdout)
                        .contains(&exe.to_string_lossy().into_owned());
                }
            }
        }
    }
    false
}

fn set_autostart(enable: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if enable {
            if let Ok(exe) = std::env::current_exe() {
                let value = format!("\"{}\"", exe.display());
                let _ = Command::new("reg")
                    .args([
                        "add",
                        RUN_KEY,
                        "/v",
                        AUTOSTART_NAME,
                        "/t",
                        "REG_SZ",
                        "/d",
                        &value,
                        "/f",
                    ])
                    .creation_flags(0x0800_0000)
                    .output();
            }
        } else {
            let _ = Command::new("reg")
                .args(["delete", RUN_KEY, "/v", AUTOSTART_NAME, "/f"])
                .creation_flags(0x0800_0000)
                .output();
        }
    }
    #[cfg(not(windows))]
    let _ = enable;
}

/// Browsers only offer audio device lists/selection on "secure" pages, and a
/// plain http:// rdio-scanner server (typical on a home network) is not one.
/// The app therefore remembers the server's origin and, on the next start,
/// tells its web engine to treat exactly that origin as secure. The first
/// time a new http server is connected the app restarts itself once.
fn secure_origin_path<R: Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("secure_origin.txt"))
}

fn load_secure_origin<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let text = std::fs::read_to_string(secure_origin_path(app)?).ok()?;
    let t = text.trim().to_string();
    if t.starts_with("http://") && !t.contains(char::is_whitespace) && !t.contains(',') {
        Some(t)
    } else {
        None
    }
}

fn relaunch_soon<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(exe) = std::env::current_exe() {
        relaunch_path(app, &exe);
    }
}

fn relaunch_path<R: Runtime>(app: &AppHandle<R>, exe: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Wait a moment so this instance has exited (the app is
        // single-instance), then start the app again.
        let line = format!(
            "/C \"ping -n 3 127.0.0.1 >nul & start \"\" \"{}\"\"",
            exe.display()
        );
        let _ = Command::new("cmd")
            .raw_arg(line)
            .creation_flags(0x0800_0000)
            .spawn();
        app.exit(0);
    }
    #[cfg(not(windows))]
    {
        let _ = (app, exe);
    }
}

/// Shown for a few seconds before the one-time restart so it is clear why the
/// app is closing and reopening.
const RESTART_NOTICE_JS: &str = r##"(function(){try{if(document.getElementById('rdio-restart-notice'))return;var d=document.createElement('div');d.id='rdio-restart-notice';d.style.cssText='position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;background:rgba(10,12,18,0.88);font-family:-apple-system,"Segoe UI",Roboto,sans-serif;';var b=document.createElement('div');b.style.cssText='max-width:380px;padding:22px 26px;border-radius:12px;background:#1e1e24;border:1px solid rgba(255,255,255,0.16);color:#f2f2f2;text-align:center;line-height:1.45;';var h=document.createElement('div');h.textContent='Restarting Rdio Sherlock';h.style.cssText='font-size:17px;font-weight:600;margin-bottom:8px;';var t=document.createElement('div');t.textContent='This is a one-time restart for this server. It turns on audio output selection (for example a Bluetooth speaker). The app will reopen by itself in a few seconds.';t.style.cssText='font-size:13px;color:#c8c8d0;';b.appendChild(h);b.appendChild(t);d.appendChild(b);(document.body||document.documentElement).appendChild(d);}catch(e){}})();"##;

fn note_server_origin<R: Runtime>(app: &AppHandle<R>, url: &tauri::Url) {
    if url.scheme() != "http" {
        return;
    }
    let host = match url.host_str() {
        Some(h) => h.to_ascii_lowercase(),
        None => return,
    };
    if host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host.ends_with(".localhost") {
        return;
    }
    let origin = url.origin().ascii_serialization();
    if !origin.starts_with("http://") || origin.contains(',') {
        return;
    }
    if load_secure_origin(app).as_deref() == Some(origin.as_str()) {
        return;
    }
    if let Some(path) = secure_origin_path(app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(path, &origin).is_ok() {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval(RESTART_NOTICE_JS);
            }
            let handle = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(4));
                relaunch_soon(&handle);
            });
        }
    }
}

/// Window geometry memory: last position/size (logical units), saved to
/// geometry.json in the app data dir and restored on the next launch.
#[derive(Clone, Copy)]
struct Geometry {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn geometry_path<R: Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("geometry.json"))
}

fn load_geometry<R: Runtime>(app: &AppHandle<R>) -> Option<Geometry> {
    let text = std::fs::read_to_string(geometry_path(app)?).ok()?;
    let get = |key: &str| -> Option<f64> {
        let pat = format!("\"{}\":", key);
        let start = text.find(&pat)? + pat.len();
        let rest = &text[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .unwrap_or(rest.len());
        rest[..end].trim().parse::<f64>().ok()
    };
    let g = Geometry {
        x: get("x")?,
        y: get("y")?,
        w: get("w")?,
        h: get("h")?,
    };
    if g.w >= 400.0 && g.h >= 300.0 && g.x.abs() < 10000.0 && g.y.abs() < 10000.0 {
        Some(g)
    } else {
        None
    }
}

fn save_geometry<R: Runtime>(app: &AppHandle<R>, g: Geometry) {
    if let Some(path) = geometry_path(app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            g.x, g.y, g.w, g.h
        );
        let _ = std::fs::write(path, text);
    }
}

static LAST_GEO_SAVE: Mutex<Option<Instant>> = Mutex::new(None);

fn save_window_geometry<R: Runtime>(window: &Window<R>) {
    if window.is_minimized().unwrap_or(false) {
        return;
    }
    {
        let mut last = LAST_GEO_SAVE.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < std::time::Duration::from_millis(250) {
                return;
            }
        }
        *last = Some(Instant::now());
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    if let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) {
        let w = size.width as f64 / scale;
        let h = size.height as f64 / scale;
        if w < 400.0 || h < 300.0 {
            return;
        }
        save_geometry(
            window.app_handle(),
            Geometry {
                x: pos.x as f64 / scale,
                y: pos.y as f64 / scale,
                w,
                h,
            },
        );
    }
}

fn main() {
    tauri::Builder::default()
        .on_page_load(|webview, payload| {
            if let tauri::webview::PageLoadEvent::Finished = payload.event() {
                note_server_origin(webview.app_handle(), payload.url());
                push_state(webview.app_handle());
                let u = payload.url();
                let on_server = u.host_str().map(|h| h != "tauri.localhost").unwrap_or(false);
                if on_server
                    && START_MINI.load(Ordering::SeqCst)
                    && !MINI_STARTED.swap(true, Ordering::SeqCst)
                {
                    if let Some(w) = webview.app_handle().get_webview_window("main") {
                        let _ = w.eval("setTimeout(function(){if(window.__rdioMini&&!window.__rdioMini.on){window.__rdioMini.enter();}},1500);");
                    }
                }
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            KEEP_AWAKE.store(load_keep_awake(app.handle()), Ordering::SeqCst);
            START_HIDDEN.store(load_flag(app.handle(), "start_hidden.txt", false), Ordering::SeqCst);
            AUTO_LIVE.store(load_flag(app.handle(), "auto_live.txt", true), Ordering::SeqCst);
            POPUP.store(load_flag(app.handle(), "popup.txt", false), Ordering::SeqCst);
            AUTO_UPDATE.store(load_flag(app.handle(), "auto_update.txt", false), Ordering::SeqCst);
            START_MINI.store(load_flag(app.handle(), "start_mini.txt", false), Ordering::SeqCst);
            MINI_OPACITY.store(load_u32(app.handle(), "mini_opacity.txt", 100), Ordering::SeqCst);
            MINI_CORNER.store(load_u32(app.handle(), "mini_corner.txt", 0), Ordering::SeqCst);
            reload_quiet(app.handle());
            start_power_thread();
            #[allow(unused_mut)]
            let mut wb = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("Rdio Sherlock")
            .decorations(false)
            .initialization_script(INIT_SCRIPT)
            .initialization_script(AUDIO_SCRIPT)
            .initialization_script(MINI_SCRIPT)
            .initialization_script(AUTO_SCRIPT)
            .visible(!START_HIDDEN.load(Ordering::SeqCst))
            .initialization_script(PIN_SEED_SCRIPT);
            #[cfg(windows)]
            {
                let mut args = String::from("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion,IntensiveWakeUpThrottling --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows --renderer-process-limit=1 --autoplay-policy=no-user-gesture-required");
                if let Some(origin) = load_secure_origin(app.handle()) {
                    args.push_str(&format!(" --unsafely-treat-insecure-origin-as-secure={}", origin));
                }
                wb = wb.additional_browser_args(&args);
            }
            if let Some(g) = load_geometry(app.handle()) {
                wb = wb.inner_size(g.w, g.h).position(g.x, g.y);
            } else {
                wb = wb.inner_size(1280.0, 800.0);
            }
            let _main = wb.build()?;
            #[cfg(windows)]
            if let Ok(h) = _main.hwnd() {
                winpower::pin_border_color(h.0 as isize);
            }

            let mut builder = TrayIconBuilder::with_id("tray")
                .tooltip("Rdio Sherlock")
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "minimize" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.minimize();
                        }
                    }
                    "server" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            // Back to the bundled setup page, straight to
                            // the form (#edit skips the auto-connect) —
                            // Windows serves app pages at
                            // http://tauri.localhost/.
                            let _ = window.eval(
                                "if(window.__rdioMini&&window.__rdioMini.on){window.__rdioMini.exit();}window.location.href='http://tauri.localhost/index.html#edit';",
                            );
                        }
                    }
                    "mute" => {
                        let muted = !MUTED.load(Ordering::SeqCst);
                        MUTED.store(muted, Ordering::SeqCst);
                        sync_mute(app);
                        let _ = refresh_tray_menu(app);
                    }
                    "snooze_off" => {
                        if let Ok(mut g) = SNOOZE_UNTIL.lock() {
                            *g = None;
                        }
                        sync_mute(app);
                        let _ = refresh_tray_menu(app);
                    }
                    id if id.starts_with("snooze_") => {
                        let mins: u64 = id[7..].parse().unwrap_or(15);
                        if let Ok(mut g) = SNOOZE_UNTIL.lock() {
                            *g = Some(Instant::now() + std::time::Duration::from_secs(mins * 60));
                        }
                        sync_mute(app);
                        let _ = refresh_tray_menu(app);
                    }
                    "quiet_off" => {
                        set_quiet_text(app, "off");
                        sync_mute(app);
                        let _ = refresh_tray_menu(app);
                    }
                    "quiet_custom" => {
                        if parse_quiet(&quiet_text(app)).is_none() {
                            set_quiet_text(app, "22:00-06:00");
                        }
                        if let Some(path) = flag_path(app, "quiet_hours.txt") {
                            let _ = Command::new("notepad").arg(path).spawn();
                        }
                        let _ = refresh_tray_menu(app);
                    }
                    id if id.starts_with("quiet:") => {
                        set_quiet_text(app, &id[6..]);
                        sync_mute(app);
                        let _ = refresh_tray_menu(app);
                    }
                    id if id.starts_with("opa:") => {
                        let v: u32 = id[4..].parse().unwrap_or(100);
                        MINI_OPACITY.store(v, Ordering::SeqCst);
                        save_u32(app, "mini_opacity.txt", v);
                        push_state(app);
                        let _ = refresh_tray_menu(app);
                    }
                    id if id.starts_with("corner:") => {
                        let v: u32 = id[7..].parse().unwrap_or(0);
                        MINI_CORNER.store(v, Ordering::SeqCst);
                        save_u32(app, "mini_corner.txt", v);
                        push_state(app);
                        let _ = refresh_tray_menu(app);
                    }
                    "start_mini" => {
                        let on = !START_MINI.load(Ordering::SeqCst);
                        START_MINI.store(on, Ordering::SeqCst);
                        save_flag(app, "start_mini.txt", on);
                        let _ = refresh_tray_menu(app);
                    }
                    "popup" => {
                        let on = !POPUP.load(Ordering::SeqCst);
                        POPUP.store(on, Ordering::SeqCst);
                        save_flag(app, "popup.txt", on);
                        push_state(app);
                        if !on {
                            // turning pop-up mode off brings a hidden mini player back
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                            }
                        }
                        let _ = refresh_tray_menu(app);
                    }
                    "auto_live" => {
                        let on = !AUTO_LIVE.load(Ordering::SeqCst);
                        AUTO_LIVE.store(on, Ordering::SeqCst);
                        save_flag(app, "auto_live.txt", on);
                        push_state(app);
                        let _ = refresh_tray_menu(app);
                    }
                    "audio" => {
                        hold_popup(30);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = window.eval(
                                "if(window.__rdioPickAudio){window.__rdioPickAudio();}",
                            );
                        }
                    }
                    "mini" => {
                        hold_popup(20);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = window.eval("if(window.__rdioMini){window.__rdioMini.toggle();}");
                        }
                    }
                    "start_hidden" => {
                        let on = !START_HIDDEN.load(Ordering::SeqCst);
                        START_HIDDEN.store(on, Ordering::SeqCst);
                        save_flag(app, "start_hidden.txt", on);
                        let _ = refresh_tray_menu(app);
                    }
                    "keep_awake" => {
                        let on = !KEEP_AWAKE.load(Ordering::SeqCst);
                        KEEP_AWAKE.store(on, Ordering::SeqCst);
                        save_keep_awake(app, on);
                        let _ = refresh_tray_menu(app);
                    }
                    "autostart" => {
                        set_autostart(!autostart_enabled());
                        let _ = refresh_tray_menu(app);
                    }
                    "update_action" => {
                        let h = app.clone();
                        std::thread::spawn(move || {
                            let st = UPDATE_STATE.load(Ordering::SeqCst);
                            let have = UPDATE_LATEST.lock().ok().map(|g| g.is_some()).unwrap_or(false);
                            if st == 3 || (st == 6 && have) {
                                install_update(&h);
                            } else {
                                check_for_updates(&h);
                            }
                        });
                    }
                    "auto_update" => {
                        let on = !AUTO_UPDATE.load(Ordering::SeqCst);
                        AUTO_UPDATE.store(on, Ordering::SeqCst);
                        save_flag(app, "auto_update.txt", on);
                        let _ = refresh_tray_menu(app);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        hold_popup(10);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                builder = builder.icon(icon.clone());
            }
            let _tray = builder.build(app)?;
            let _ = refresh_tray_menu(app.handle());
            start_tick_thread(app.handle().clone());
            start_update_thread(app.handle().clone());
            app.listen("rdio-heartbeat", move |_ev| {
                if let Ok(mut g) = LAST_BEAT.lock() {
                    *g = Some(Instant::now());
                }
            });
            // The mini player's own right-click menu changes its opacity by
            // emitting this (only) event.
            {
                let h = app.handle().clone();
                let h2 = app.handle().clone();
                app.listen("rdio-mini-popup", move |ev| {
                    let on = ev.payload().trim().trim_matches('"') == "true";
                    POPUP.store(on, Ordering::SeqCst);
                    save_flag(&h2, "popup.txt", on);
                    if !on {
                        if let Some(w) = h2.get_webview_window("main") {
                            let _ = w.show();
                        }
                    }
                    let _ = refresh_tray_menu(&h2);
                });
                app.listen("rdio-mini-state", move |ev| {
                    MINI_ON.store(ev.payload().trim().trim_matches('"') == "true", Ordering::SeqCst);
                });
                app.listen("rdio-popup-hold", move |ev| {
                    if ev.payload().trim().trim_matches('"') == "true" {
                        hold_popup(600);
                    } else {
                        hold_popup(5);
                    }
                });
                app.listen("rdio-mini-opacity", move |ev| {
                    let v: u32 = ev.payload().trim().trim_matches('"').parse().unwrap_or(100);
                    let v = v.clamp(30, 100);
                    MINI_OPACITY.store(v, Ordering::SeqCst);
                    save_u32(&h, "mini_opacity.txt", v);
                    let _ = refresh_tray_menu(&h);
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    save_window_geometry(window);
                    let _ = window.hide();
                    if let Some(w) = window.app_handle().get_webview_window("main") {
                        let _ = w.eval(QUIET_JS);
                    }
                    api.prevent_close();
                }
                WindowEvent::Focused(true) => {
                    if let Some(w) = window.app_handle().get_webview_window("main") {
                        let _ = w.eval(UNQUIET_JS);
                    }
                }
                WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                    save_window_geometry(window);
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
