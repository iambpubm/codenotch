//! Keeps *other* applications' windows on top, the way PowerToys' "Always on Top" does.
//!
//! `topmost.rs` and this module look alike and are not: that one fights to keep our own notch in
//! the topmost band, this one deliberately changes the z-order of windows we do not own.
//!
//! It is a mode, not a one-shot command. The pin in the system tray stays armed until the user says
//! otherwise, so pinning three windows is three clicks and no fiddling with the tray between them:
//! a left click on a window pins it, a second left click on the same window lets it go again, and
//! Escape, a right click, a click on the desktop, or a click back onto the tray icon ends the mode.
//! An armed picker that is simply forgotten releases itself after a minute of no picks.
//!
//! The picker is a low-level mouse hook rather than a shortcut because the click has to select a
//! window without also pressing whatever button is under the pointer: a click that both pins a
//! window and fires one of its controls would be a trap. `WH_MOUSE_LL` can swallow the click
//! (`LRESULT(1)`) and still report where it landed. The hook lives on its own thread with its own
//! message loop, because a low-level hook only ticks while its installer pumps messages.
//!
//! The frame is a `STATIC` popup carrying `WS_EX_LAYERED | WS_EX_TRANSPARENT`: it never takes a
//! click, never takes focus, and its pixels come from `UpdateLayeredWindow`, so the rounded stroke
//! is antialiased instead of the jagged steps a colour-keyed window would leave.
//!
//! Where the frame goes is the part that is easy to get wrong. `GetWindowRect` includes the
//! invisible resize border — up to 8 px of nothing on a normal window — so a frame drawn on it
//! stands well clear of the edge and looks like it belongs to the wallpaper rather than to the
//! window. `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` reports the rectangle that is
//! actually on screen, and the stroke is drawn on that line, which is what makes it read as a
//! border instead of a halo.

#[cfg(windows)]
pub use win::{count, is_armed, release_all, shutdown, start, toggle_mode};

#[cfg(not(windows))]
pub fn count() -> usize {
    0
}
#[cfg(not(windows))]
pub fn is_armed() -> bool {
    false
}
#[cfg(not(windows))]
pub fn release_all() {}
#[cfg(not(windows))]
pub fn shutdown() {}
#[cfg(not(windows))]
pub fn start(_app: tauri::AppHandle) {}
#[cfg(not(windows))]
pub fn toggle_mode() {}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        COLORREF, HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
    };
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetDeviceCaps,
        SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, LOGPIXELSX,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, FindWindowExW, GetAncestor,
        GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindow,
        GetWindowLongPtrW, GetWindowRect,
        GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer,
        PostThreadMessageW, SetTimer, SetWindowPos, SetWindowsHookExW, ShowWindow,
        UnhookWindowsHookEx, UpdateLayeredWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE,
        GW_OWNER,
        HHOOK, HMENU, HWND_NOTOPMOST, HWND_TOPMOST, MSG, MSLLHOOKSTRUCT, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA, WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN,
        WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    /// Where the frame window sits relative to the window it frames, in physical pixels. Just wide
    /// enough for the stroke's outer half and its halo: the stroke is centred on the window's own
    /// visible edge, so the ink reaches `STROKE + GLOW` beyond it.
    const GROW: i32 = 6;
    /// How far outside the window's visible edge the centre of the stroke sits. Zero, so the line
    /// sits *on* the edge the way PowerToys' border does — half of it inside, half outside. The
    /// inside half covers the window's own one-pixel frame and its rounded corner, never its
    /// content: `DWMWA_EXTENDED_FRAME_BOUNDS` already excludes the invisible resize border.
    ///
    /// A maximised window is the exception and is handled per frame: its rectangle is the whole
    /// screen, so a stroke centred on it would lose its outer half off the edge of the display.
    /// `Frame::place` flips the offset for exactly that case.
    const OFFSET: f32 = 0.0;
    /// Corner radius at 96 dpi. Windows 11 rounds a window at 8 px; `paint` scales this by the
    /// screen's dpi, because at 150 % the window's own corner is 12 physical pixels and a fixed 8
    /// would cut across it.
    const RADIUS: f32 = 8.0;
    /// Half the thickness of the solid line — 3 px across, counting both antialiased edges.
    const STROKE: f32 = 1.0;
    /// How far the halo reaches outside the line. Short: a wide glow is what makes a border read as
    /// "the window is bigger than it is".
    const GLOW: f32 = 3.0;
    /// The frame follows the window on this tick. Fast enough to stay glued while a window is
    /// dragged, slow enough to cost nothing: one `GetWindowRect` per pinned window per tick.
    const TICK_MS: u64 = 33;
    /// How often the frame thread looks for windows *owned* by a pinned one. A program's own
    /// dropdowns, menus and pickers (WeChat's emoji panel is one) are separate top-level windows
    /// that are not topmost themselves, so pinning the window they belong to covers them up: the
    /// user clicks the emoji button and nothing appears. Lifting them as they show keeps the
    /// program behaving as it did. 60 ms is two frame ticks — short enough that the panel is up
    /// before the user has finished moving the pointer into it.
    const POPUP_SCAN_MS: u64 = 60;
    /// A window this small in either direction is never a dropdown: it is a hidden scratch window
    /// or a caret, and lifting it would only put handles in the topmost band for nothing.
    const MIN_POPUP_SIDE: i32 = 8;
    /// A window this close to the size of the pinned one is another window of the same program,
    /// not something that dropped out of it: Directory Opus opens listers, and Explorer opens
    /// folder windows, at exactly the size of the last one.
    const SAME_SIZE_PCT: i32 = 90;
    /// How much of a popup must be over its window before it counts as belonging to it. A context
    /// menu hangs a little over the edge; it never sits mostly outside.
    const OVERLAP_PCT: i64 = 70;
    /// How far up an owner chain to look. Two levels cover the observed cases — Qt's scratch window
    /// and the Chromium window hanging off it — and a bound is what keeps a malformed chain from
    /// walking forever.
    const MAX_OWNER_DEPTH: usize = 6;
    /// An armed picker that is forgotten must let go of the mouse by itself; a hook that stays
    /// installed with nothing on screen to explain it would leave the user unable to click
    /// anything. Reset on every successful pick, so pinning one window after another never times
    /// out mid-way.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    const PICK_TIMER: usize = 0x7A;
    /// Posted by the hook to the picker thread when a window has been chosen. `WM_APP` is the
    /// first message id Windows leaves to applications.
    const WM_APP_PICK: u32 = WM_APP + 1;

    /// Cyan, in the notch's own family. The preview is the same hue at half strength so the two
    /// states read as one thing at two intensities rather than as two unrelated colours.
    const PINNED_RGB: [f32; 3] = [34.0, 211.0, 238.0];
    const PREVIEW_RGB: [f32; 3] = [34.0, 211.0, 238.0];
    const PINNED_PEAK: f32 = 0.95;
    const PREVIEW_PEAK: f32 = 0.42;

    /// Top-level windows the user pinned, as `HWND.0 as isize`.
    ///
    /// `isize` and not `HWND` because `HWND` wraps a raw pointer, so a `Vec<HWND>` is not `Sync`
    /// and cannot live in a `static`. This list is written by the picker's thread, read by the
    /// frame thread, and cleared on the way out.
    static PINNED: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());
    /// True while the picker is armed. The tray icon, its menu and the frame thread all read it.
    static ARMED: AtomicBool = AtomicBool::new(false);
    /// The window under the pointer, updated by the hook and read by the frame thread. Zero when
    /// the pointer is over one of our own windows or over nothing.
    static CANDIDATE: AtomicIsize = AtomicIsize::new(0);
    /// Hand-off from the hook to the picker thread: the window the last click chose. The thread
    /// swaps it back to zero, so a click that arrives while the previous one is being handled is
    /// not lost.
    static PICKED: AtomicIsize = AtomicIsize::new(0);
    /// The picker thread's id, so the hook callback — which gets no user data — can post it the
    /// message that ends the mode.
    static PICK_TID: AtomicU32 = AtomicU32::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    /// Set when the mode is asked to end. The `WM_QUIT` carries the same request, but a mode asked
    /// to stop in the few microseconds before its thread has published its id would otherwise miss
    /// the message and stay armed with nobody asking for it.
    static QUIT_REQ: AtomicBool = AtomicBool::new(false);
    /// The app handle, so a pick can refresh the tray icon's own tooltip from the picker thread.
    static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
    /// Windows this module raised because they are a pinned window's own popup, as
    /// `(owner, popup)`. Kept so releasing a window can put them back: a dropdown left topmost
    /// after its parent is not would float over everything with nothing to explain it.
    static LIFTED: std::sync::Mutex<Vec<(isize, isize)>> = std::sync::Mutex::new(Vec::new());
    /// How many times a pinned window has had to be put back on top. Counted rather than logged,
    /// because it happens for as long as the other program keeps clearing the bit.
    static DEMOTIONS: AtomicU32 = AtomicU32::new(0);
    /// Frame ticks so far. Only used to run the cheap upkeep every few ticks instead of all of them.
    static TICKS: AtomicU32 = AtomicU32::new(0);

    /// The four answers the gesture gives, as raw WAV bytes in the binary: no files to install, no
    /// crate to pull in, and `PlaySoundW` reads them straight from memory.
    ///
    /// Rising for "on top", falling for "let go", and a single tick each for the mode arming and
    /// disarming — the mode is invisible otherwise, and "did it hear me?" is the whole question the
    /// user has when he clicks the tray icon.
    const PIN_SOUND: &[u8] = include_bytes!("../sounds/pin.wav");
    const UNPIN_SOUND: &[u8] = include_bytes!("../sounds/unpin.wav");
    const ARMED_SOUND: &[u8] = include_bytes!("../sounds/armed.wav");
    const OFF_SOUND: &[u8] = include_bytes!("../sounds/off.wav");

    /// Plays one of them and returns at once.
    ///
    /// `SND_ASYNC` matters as much as `SND_MEMORY`: this is called from the low-level mouse hook and
    /// from the frame loop, and neither may wait for 200 ms of audio. The bytes are a WAV file, and
    /// the API wants a `PCWSTR` — it takes the pointer and ignores the type, which is why the cast
    /// is here rather than a string.
    fn chime(wav: &'static [u8]) {
        unsafe {
            let _ = PlaySoundW(
                PCWSTR(wav.as_ptr() as *const u16),
                HMODULE::default(),
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            );
        }
    }

    /// Starts the frame thread and remembers the handle the tray notifications need.
    pub fn start(app: tauri::AppHandle) {
        let _ = APP.set(app);
        std::thread::spawn(|| {
            // One screen DC for the life of the thread. `GetDC(NULL)` per repaint would leak a
            // handle per resized frame — 30 a second while a window is dragged — and the 10 000
            // handle limit is not far off.
            let screen = unsafe { GetDC(HWND::default()) };
            let mut frames: Vec<Frame> = Vec::new();
            let mut last_popup_scan = Instant::now();
            loop {
                if STOP.load(SeqCst) {
                    for f in frames.iter_mut() {
                        f.destroy();
                    }
                    return;
                }
                step(&mut frames, screen);
                // Off the frame's own path: this walks the top-level window list, which is a few
                // hundred handle lookups, and none of it is needed 30 times a second.
                if last_popup_scan.elapsed() >= Duration::from_millis(POPUP_SCAN_MS) {
                    last_popup_scan = Instant::now();
                    lift_owned_popups();
                }
                std::thread::sleep(Duration::from_millis(TICK_MS));
            }
        });
    }

    /// How many windows this session is holding up. The tray tooltip and menu say so.
    pub fn count() -> usize {
        PINNED.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Whether the picker is catching clicks right now. The tray icon reads it to decide whether
    /// its click starts or ends the mode.
    pub fn is_armed() -> bool {
        ARMED.load(SeqCst)
    }

    /// Arms the picker, or disarms it if it was already armed. Called by the tray icon's own click
    /// and by the tray menu; the two are the same switch.
    ///
    /// A second call while the mode is on does not stack a second hook: it posts the quit that
    /// makes the one running hook tear itself down.
    pub fn toggle_mode() {
        if ARMED.swap(true, SeqCst) {
            post_quit();
            crate::applog("pin: mode off from the tray");
            return;
        }
        CANDIDATE.store(0, SeqCst);
        PICKED.store(0, SeqCst);
        QUIT_REQ.store(false, SeqCst);
        // The mode has no on-screen presence of its own; this tick is how the user learns that the
        // next click will pick a window instead of clicking it.
        chime(ARMED_SOUND);
        std::thread::spawn(run_mode);
    }

    /// Lets go of every window this session pinned, without leaving the mode.
    pub fn release_all() {
        let list: Vec<isize> = match PINNED.lock() {
            Ok(g) => g.clone(),
            Err(_) => Vec::new(),
        };
        for target in &list {
            unsafe {
                let _ = SetWindowPos(
                    hwnd_of(*target),
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            drop_owned_popups(*target);
        }
        if let Ok(mut g) = PINNED.lock() {
            g.clear();
        }
        if !list.is_empty() {
            crate::applog(&format!("pin: released all {} window(s)", list.len()));
        }
        notify();
    }

    /// Lets go of every window this session pinned, stops an armed picker, and stops the frame
    /// thread. Called on the way out: a window left topmost with no frame around it would look
    /// like a bug and could only be undone by restarting the app that owns it.
    pub fn shutdown() {
        STOP.store(true, SeqCst);
        post_quit();
        release_all();
    }

    fn hwnd_of(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    /// Tells the tray icon that the mode or the count changed. Cheap: one tooltip and a three-item
    /// menu, both built on the main thread.
    fn notify() {
        if let Some(app) = APP.get() {
            crate::tray::refresh_pin_tray(app);
        }
    }

    /// The picker's own loop. Stays until it is told to stop; every pick it accepts resets the
    /// idle clock rather than ending the mode, which is what lets the user pin one window after
    /// another without going back to the tray.
    fn run_mode() {
        unsafe {
            let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE::default(), 0)
            else {
                crate::applog("pin: could not install the mouse hook");
                ARMED.store(false, SeqCst);
                notify();
                return;
            };
            PICK_TID.store(GetCurrentThreadId(), SeqCst);
            // Seed the preview before the mouse moves. The pointer is already somewhere, and a
            // picker that shows nothing until the mouse twitches reads as broken.
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_ok() {
                CANDIDATE.store(target_at(pt).0 as isize, SeqCst);
            }
            // A timer, not a keyboard hook: it wakes the loop often enough to notice Escape, and
            // it is the only thing here that could ever hang the mouse if it went wrong.
            let timer = SetTimer(HWND::default(), PICK_TIMER, 120, None);
            let mut last = Instant::now();
            let mut msg = MSG::default();
            // The request is checked before the message is waited for: a stop that arrives before
            // this thread published its id has no message to catch, and `swap` makes sure the
            // next lap does not act on it twice.
            while !QUIT_REQ.swap(false, SeqCst)
                && GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0
            {
                if msg.message == WM_APP_PICK {
                    let target = PICKED.swap(0, SeqCst);
                    if target != 0 {
                        toggle(target);
                        last = Instant::now();
                    }
                    continue;
                }
                if msg.message == WM_TIMER {
                    if GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 || last.elapsed() > IDLE_TIMEOUT {
                        break;
                    }
                    continue;
                }
                // A low-level hook is called from the dispatch of a message the system posts to
                // the installing thread, so this is not optional even though we own no window.
                DispatchMessageW(&msg);
            }
            if timer != 0 {
                let _ = KillTimer(HWND::default(), timer);
            }
            let _ = UnhookWindowsHookEx(hook);
            PICK_TID.store(0, SeqCst);
        }
        ARMED.store(false, SeqCst);
        CANDIDATE.store(0, SeqCst);
        // The one place the mode actually ends, whichever way it ended -- the tray icon again, the
        // right button, Escape or the idle timeout -- so the tick belongs here and nowhere else.
        chime(OFF_SOUND);
        crate::applog("pin: mode off");
        notify();
    }

    /// Ends the message loop from anywhere, including from inside the hook callback.
    fn post_quit() {
        QUIT_REQ.store(true, SeqCst);
        post(WM_QUIT);
    }

    fn post(message: u32) {
        let tid = PICK_TID.load(SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, message, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// The hook callback. Runs on the picker thread, inside `GetMessageW`, and on the critical
    /// path of every mouse event on the machine — so it does the two cheap lookups it needs and
    /// nothing else. Nothing here may panic: an unwind across the FFI boundary takes the process
    /// down with the mouse still captured.
    unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code < 0 {
            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
        }
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        match wparam.0 as u32 {
            WM_MOUSEMOVE => {
                CANDIDATE.store(target_at(info.pt).0 as isize, SeqCst);
                CallNextHookEx(HHOOK::default(), code, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                let target = target_at(info.pt);
                if target.is_invalid() {
                    // Not a pick: the desktop, the taskbar — including the pin icon that armed
                    // this — or one of our own windows. All of them mean "I am done", which is
                    // also the only way to end the mode by clicking the tray icon again.
                    post_quit();
                } else {
                    PICKED.store(target.0 as isize, SeqCst);
                    post(WM_APP_PICK);
                }
                // Swallow it either way: the window is being chosen, not clicked.
                LRESULT(1)
            }
            WM_RBUTTONDOWN => {
                post_quit();
                LRESULT(1)
            }
            _ => CallNextHookEx(HHOOK::default(), code, wparam, lparam),
        }
    }

    /// The top-level window under a screen point, or nothing.
    ///
    /// Nothing means "this click is not a pick": our own windows (the notch and the settings page
    /// are already above everything, and framing them would be noise) and the shell's own
    /// surfaces. The shell matters most — the tray icon that arms the picker lives in the taskbar,
    /// so clicking it again would otherwise pin the taskbar itself.
    fn target_at(pt: POINT) -> HWND {
        unsafe {
            let under = WindowFromPoint(pt);
            if under.is_invalid() {
                return HWND::default();
            }
            let top = GetAncestor(under, GA_ROOT);
            if top.is_invalid() {
                return HWND::default();
            }
            if is_shell(top) {
                return HWND::default();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(top, Some(&mut pid as *mut u32));
            if pid == std::process::id() {
                return HWND::default();
            }
            top
        }
    }

    /// The taskbar, its overflow flyout and the desktop. Compared by class name, without
    /// allocating: this runs inside the mouse hook, on the critical path of every mouse event.
    fn is_shell(hwnd: HWND) -> bool {
        const CLASSES: [&str; 5] = [
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
            "NotifyIconOverflowWindow",
            "Progman",
            "WorkerW",
        ];
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) };
        if n <= 0 {
            return false;
        }
        let name = &buf[..n as usize];
        CLASSES.iter().any(|want| {
            let mut i = 0;
            for unit in want.encode_utf16() {
                if i >= name.len() || name[i] != unit {
                    return false;
                }
                i += 1;
            }
            i == name.len()
        })
    }

    /// The rectangle the window actually occupies on screen.
    ///
    /// `DwmGetWindowAttribute` with `DWMWA_EXTENDED_FRAME_BOUNDS` drops the invisible resize
    /// border that `GetWindowRect` includes — 8 px on a stock window, which is exactly the gap
    /// that made the first version of this frame look larger than the window. The fallback keeps
    /// windows that DWM does not manage (some consoles, some games) from losing their frame
    /// altogether; on those the stroke sits a little further out, which is the lesser evil.
    fn visible_rect(hwnd: HWND) -> Option<RECT> {
        let mut rc = RECT::default();
        unsafe {
            let ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rc as *mut RECT as *mut core::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok();
            if ok && rc.right > rc.left && rc.bottom > rc.top {
                return Some(rc);
            }
            if GetWindowRect(hwnd, &mut rc).is_err() {
                return None;
            }
        }
        Some(rc)
    }

    /// Pins a window, or releases it if it was already pinned. `SetWindowPos` without
    /// `SWP_NOACTIVATE` on the way in, because the user just chose this window and expects it in
    /// front as well as on top.
    ///
    /// Toggling is what makes "pin the next one" and "let this one go" the same gesture, so the
    /// mode can stay armed across several windows instead of closing after the first.
    fn toggle(target: isize) {
        let Ok(mut list) = PINNED.lock() else { return };
        if let Some(pos) = list.iter().position(|t| *t == target) {
            list.remove(pos);
            drop(list);
            unsafe {
                let _ = SetWindowPos(
                    hwnd_of(target),
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            drop_owned_popups(target);
            chime(UNPIN_SOUND);
            crate::applog("pin: released a window");
            notify();
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                hwnd_of(target),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        list.push(target);
        let count = list.len();
        drop(list);
        chime(PIN_SOUND);
        crate::applog(&format!("pin: kept a window on top ({count} now)"));
        notify();
    }

    /// Drops a window from the list without touching its z-order — used when it closed, or when
    /// something else un-pinned it, and the state is already what we would have set.
    fn forget(target: isize) {
        if let Ok(mut g) = PINNED.lock() {
            let before = g.len();
            g.retain(|t| *t != target);
            if g.len() != before {
                drop(g);
                notify();
            }
        }
    }

    /// Whether a frame's target still deserves one. A window that closed, or that some other
    /// program un-pinned, must lose its frame as well: a border around a window that is no longer
    /// topmost is a lie the user cannot clear.
    fn still_wanted(target: isize) -> bool {
        let hwnd = hwnd_of(target);
        unsafe {
            if !IsWindow(hwnd).as_bool() {
                forget(target);
                return false;
            }
        }
        true
    }

    /// Puts the topmost bit back on any pinned window that lost it.
    ///
    /// The bit is not ours to keep: several programs clear it while laying themselves out — WeChat
    /// and QQ both do, once a second or so, and both were caught doing it on this machine — and a
    /// window whose bit was cleared is simply a window that stopped being pinned. Believing the bit
    /// and giving up when it is gone is what made a pin vanish on its own, which reads as "sometimes
    /// it is just not on top". So it is re-asserted instead, for the price of one query per pinned
    /// window per tick.
    fn hold_on_top(pinned: &[isize]) {
        for target in pinned {
            let hwnd = hwnd_of(*target);
            unsafe {
                if !IsWindow(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                    continue;
                }
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOPMOST.0 != 0 {
                    continue;
                }
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                // Worth knowing that it happens, but not worth a line every 33 ms: the whole point
                // is that this is routine. The first one, then roughly every seven seconds.
                let n = DEMOTIONS.fetch_add(1, SeqCst) + 1;
                if n == 1 || n % 200 == 0 {
                    crate::applog(&format!(
                        "pin: put a window back on top {n} time(s) so far, something keeps demoting it"
                    ));
                }
            }
        }
    }

    /// Walks the top-level window list in z-order, front first, handing each window and its owner
    /// to `visit`. The owner is a null handle for the many windows that have none — which includes
    /// the ones this module cares most about, so it is handed over rather than skipped.
    ///
    /// `FindWindowExW` with a null parent and a null class is the cheapest way to do this: one
    /// call per window, no callback, and unlike `EnumWindows` it can start from where it stopped.
    fn walk_top_level(mut visit: impl FnMut(HWND, HWND)) {
        let mut after = HWND::default();
        loop {
            let Ok(next) = (unsafe {
                FindWindowExW(HWND::default(), after, PCWSTR::null(), PCWSTR::null())
            }) else {
                break;
            };
            after = next;
            let owner = unsafe { GetWindow(next, GW_OWNER) }.unwrap_or_default();
            visit(next, owner);
        }
    }

    /// The process a window belongs to, or 0 if the handle is gone.
    fn pid_of(hwnd: HWND) -> u32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
        pid
    }

    /// The processes in a window's owner chain, nearest first, and whether `hosts` is in there.
    ///
    /// A window can be *drawn* by a process that does not own it: WeChat 4 renders its panels in a
    /// Chromium helper (`WeChatAppEx.exe`), and what hangs the panel off the chat window is a
    /// scratch window back in the chat window's own process. Comparing only the popup's pid
    /// therefore rejects the very window this exists for, and following the chain is what puts it
    /// back.
    fn chain_reaches(owner: HWND, hosts: &[(isize, u32)]) -> Option<isize> {
        let mut cur = owner;
        for _ in 0..MAX_OWNER_DEPTH {
            if cur.is_invalid() {
                return None;
            }
            let pid = pid_of(cur);
            if pid != 0 {
                if let Some((host, _)) = hosts.iter().find(|(_, p)| *p == pid) {
                    return Some(*host);
                }
            }
            cur = match unsafe { GetWindow(cur, GW_OWNER) } {
                Ok(next) => next,
                Err(_) => return None,
            };
        }
        None
    }

    /// Raises the windows a pinned window put on screen, and keeps them in the order it showed
    /// them.
    ///
    /// Pinning a window makes *it* topmost; the dropdowns, menus and pickers it puts up are
    /// ordinary windows, so they end up behind it — the user clicks WeChat's emoji button and the
    /// panel appears underneath the chat window, which reads as "the emoji panel is broken". This
    /// is the one cost of using the topmost band that the user cannot see coming, so it is paid
    /// here: each of those windows is lifted the first time it shows, and left alone afterwards.
    ///
    /// Nothing else in this module touches the z-order of a window the user did not pick.
    fn lift_owned_popups() {
        let pinned: Vec<isize> = match PINNED.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        // Before the focus test, and whatever happens next: a raised window that is no longer
        // showing must go back down even while another program is in front, or it floats there.
        drop_stale_lifted(&pinned);
        if pinned.is_empty() {
            return;
        }
        let hosts: Vec<(isize, u32)> = pinned
            .iter()
            .map(|t| (*t, pid_of(hwnd_of(*t))))
            .filter(|(_, pid)| *pid != 0)
            .collect();
        if hosts.is_empty() {
            return;
        }
        // Only while the program being pinned is the one in use. A program can only be showing a
        // dropdown while it, or something it owns, has focus — and everything else on the desktop
        // is skipped before a single further call is made. Following the chain here matters as
        // much as it does below: the emoji panel's own window belongs to a helper process, so a
        // plain pid comparison would call the app unfocused exactly when it is not.
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_invalid() || chain_reaches(fg, &hosts).is_none() {
            return;
        }
        // Collected first, then raised from the bottom of the app's own stack upwards: raising a
        // backdrop after the panel that sits on it would put the backdrop on top of the panel.
        // WeChat has exactly that pair — a 900 x 900 save-bits window under a 711 x 531 panel.
        let mut queue: Vec<(isize, HWND)> = Vec::new();
        walk_top_level(|popup, owner| {
            unsafe {
                // A hidden owned window is a scratch buffer — size 0, never drawn. Lifting it would
                // only add handles to the topmost band for nothing.
                if !IsWindowVisible(popup).as_bool() {
                    return;
                }
                if GetWindowLongPtrW(popup, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 {
                    return;
                }
                if pinned.contains(&(popup.0 as isize)) {
                    // One of the windows the user pinned himself: the frame thread already keeps
                    // its z-order, and lifting it here would fight it.
                    return;
                }
                if let Some(host) = host_of(popup, owner, &hosts) {
                    queue.push((host, popup));
                }
            }
        });
        for (host, popup) in queue.iter().rev() {
            unsafe {
                let _ = SetWindowPos(
                    *popup,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let raw = popup.0 as isize;
            if let Ok(mut lifted) = LIFTED.lock() {
                if !lifted.iter().any(|(_, p)| *p == raw) {
                    lifted.push((*host, raw));
                }
            }
            crate::applog(&format!("pin: lifted {} ({})", class_name(*popup), described(*popup)));
        }
    }

    /// Which pinned window a popup belongs to, if any.
    ///
    /// Three answers, cheapest first, and all three are needed on one machine:
    ///
    /// * The owner *is* the pinned window. That is what ownership means on Windows, and it settles
    ///   the ordinary case with no geometry at all.
    /// * The owner chain reaches the pinned window's *process*. This is the one that matters for
    ///   WeChat 4: it draws its panels in a Chromium helper (`WeChatAppEx.exe`), and hangs them off
    ///   a scratch window — `Qt51514QWindowToolSaveBits`, owner 0 — back in the chat window's own
    ///   process. The panel's pid is therefore the helper's, and its owner is a window the user
    ///   never sees; both of the obvious tests reject it, and the chain does not.
    /// * Same process, no owner at all. Qt's own tool windows are like that.
    ///
    /// Geometry still decides, because ownership alone would let a program's unrelated windows come
    /// along: a dropdown is never larger than the window it drops out of, never the same size as
    /// another window of that program, and it sits mostly over it.
    fn host_of(popup: HWND, owner: HWND, hosts: &[(isize, u32)]) -> Option<isize> {
        if let Some((host, _)) = hosts.iter().find(|(t, _)| *t == owner.0 as isize) {
            return Some(*host);
        }
        let pid = pid_of(popup);
        // The cheap rejection, before any geometry: a window that is neither in a pinned window's
        // process nor hanging off one cannot be its dropdown, and this is nearly every window on
        // the desktop. Only what survives it costs a DWM call.
        let same_process = hosts.iter().any(|(_, p)| *p == pid);
        let chained = if same_process {
            None
        } else {
            chain_reaches(owner, hosts)
        };
        if !same_process && chained.is_none() {
            return None;
        }
        let popup_rect = visible_rect(popup)?;
        let mut best: Option<(i64, isize)> = None;
        for (host, host_pid) in hosts {
            // Either the same program, or the program that this window hangs off.
            if *host_pid != pid && chained != Some(*host) {
                continue;
            }
            let Some(host_rect) = visible_rect(hwnd_of(*host)) else { continue };
            let Some(cover) = could_drop_from(popup_rect, host_rect) else { continue };
            if best.is_none_or(|(prev, _)| cover > prev) {
                best = Some((cover, *host));
            }
        }
        best.map(|(_, host)| host)
    }

    /// A window's class name, for the log: `WeChatAppEx.exe` and `Weixin.exe` windows are told
    /// apart by class and not by title, and the next person to read this needs to know which
    /// window was raised.
    fn class_name(hwnd: HWND) -> String {
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) };
        if n <= 0 {
            return "?".to_string();
        }
        String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())])
    }

    /// Size and process, for the same log line.
    fn described(hwnd: HWND) -> String {
        let rc = visible_rect(hwnd);
        let size = match rc {
            Some(rc) => format!("{}x{}", rc.right - rc.left, rc.bottom - rc.top),
            None => "no size".to_string(),
        };
        format!("{size}, pid {}", pid_of(hwnd))
    }

    /// How much of `popup` sits over `host`, if it could be a dropdown of it at all.
    ///
    /// Pure, so the thresholds can be tested without a desktop: everything here is a rule about
    /// rectangles, and a rule that can only be checked by opening WeChat's emoji panel is a rule
    /// that never gets checked. Returns the overlap in percentage-points × area so the caller can
    /// pick the best host; `None` when it is not a dropdown.
    fn could_drop_from(popup: RECT, host: RECT) -> Option<i64> {
        let (aw, ah) = (popup.right - popup.left, popup.bottom - popup.top);
        let (bw, bh) = (host.right - host.left, host.bottom - host.top);
        if aw < MIN_POPUP_SIDE || ah < MIN_POPUP_SIDE || bw <= 0 || bh <= 0 {
            return None;
        }
        // A dropdown is never larger than the window it drops out of.
        if aw > bw || ah > bh {
            return None;
        }
        // Nor is it the same size: that is another window of the same program.
        if aw * 100 >= bw * SAME_SIZE_PCT && ah * 100 >= bh * SAME_SIZE_PCT {
            return None;
        }
        let w = (popup.right.min(host.right) - popup.left.max(host.left)).max(0) as i64;
        let h = (popup.bottom.min(host.bottom) - popup.top.max(host.top)).max(0) as i64;
        let cover = w * h * 100;
        if cover < aw as i64 * ah as i64 * OVERLAP_PCT {
            return None;
        }
        Some(cover)
    }

    /// Puts back any window this module raised that no longer has a reason to be on top.
    ///
    /// A dropdown is topmost only while it is showing and the window it belongs to is still
    /// pinned. Nothing else clears the bit for us — WeChat hides its 900 x 900 save-bits window the
    /// moment the panel closes, and a hidden topmost window is still *in* the topmost band, ready
    /// to reappear over whatever is on screen next. That is the other half of "my window is not
    /// really in front": something of the pinned app, or of the app that was pinned a minute ago,
    /// floating above the desktop.
    fn drop_stale_lifted(pinned: &[isize]) {
        let Ok(mut lifted) = LIFTED.lock() else { return };
        let mut kept: Vec<(isize, isize)> = Vec::with_capacity(lifted.len());
        for (host, popup) in lifted.drain(..) {
            let hwnd = hwnd_of(popup);
            let alive = unsafe { IsWindow(hwnd).as_bool() };
            let still_wanted =
                alive && pinned.contains(&host) && unsafe { IsWindowVisible(hwnd).as_bool() };
            if still_wanted {
                kept.push((host, popup));
            } else if alive {
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_NOTOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }
        *lifted = kept;
    }

    /// Puts back what `lift_owned_popups` lifted for one window that is being released.
    ///
    /// Only the windows this module raised, so a popup its program pinned for itself is left as
    /// its program wants it.
    fn drop_owned_popups(target: isize) {
        let Ok(mut lifted) = LIFTED.lock() else { return };
        let mut kept = Vec::with_capacity(lifted.len());
        for (owner, popup) in lifted.drain(..) {
            if owner != target {
                kept.push((owner, popup));
                continue;
            }
            let hwnd = hwnd_of(popup);
            unsafe {
                if !IsWindow(hwnd).as_bool() {
                    continue;
                }
                let _ = SetWindowPos(
                    hwnd,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        *lifted = kept;
    }

    struct Frame {
        /// The border window itself.
        hwnd: HWND,
        target: isize,
        preview: bool,
        w: i32,
        h: i32,
        x: i32,
        y: i32,
        shown: bool,
        /// True when the target fills the screen. The stroke then has to go inside the window
        /// instead of straddling its edge, or its outer half would be drawn past the display.
        inner: bool,
        /// The dpi the bitmap was drawn at, so a frame moved to a screen with different scaling
        /// is redrawn rather than stretched.
        dpi: u32,
        /// The memory DC and the 32-bit top-down bitmap `UpdateLayeredWindow` uploads from. Kept
        /// between ticks so a frame that only moves is never repainted.
        hdc: HDC,
        bmp: HBITMAP,
        dirty: bool,
    }

    impl Frame {
        fn new(target: isize, preview: bool) -> Option<Frame> {
            unsafe {
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_NOACTIVATE
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST,
                    w!("STATIC"),
                    w!(""),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    HWND::default(),
                    HMENU::default(),
                    HINSTANCE::default(),
                    None,
                )
                .ok()?;
                Some(Frame {
                    hwnd,
                    target,
                    preview,
                    w: -1,
                    h: -1,
                    x: i32::MIN,
                    y: i32::MIN,
                    shown: false,
                    inner: false,
                    dpi: 0,
                    hdc: HDC::default(),
                    bmp: HBITMAP::default(),
                    dirty: true,
                })
            }
        }

        fn destroy(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
                if !self.bmp.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.bmp.0));
                }
                if !self.hdc.is_invalid() {
                    let _ = DeleteDC(self.hdc);
                }
            }
            self.bmp = HBITMAP::default();
            self.hdc = HDC::default();
        }

        fn set_shown(&mut self, on: bool) {
            if self.shown == on {
                return;
            }
            unsafe {
                let _ = ShowWindow(self.hwnd, if on { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
            self.shown = on;
        }

        /// Follows the target: same place, same size, and the same frame the whole way. A moved
        /// window only costs a `SetWindowPos`; a resized one costs a repaint.
        fn place(&mut self, screen: HDC) {
            let target = hwnd_of(self.target);
            unsafe {
                if !IsWindowVisible(target).as_bool() || IsIconic(target).as_bool() {
                    self.set_shown(false);
                    return;
                }
                let Some(rc) = visible_rect(target) else {
                    self.set_shown(false);
                    return;
                };
                let w = rc.right - rc.left + GROW * 2;
                let h = rc.bottom - rc.top + GROW * 2;
                if w < 8 || h < 8 {
                    self.set_shown(false);
                    return;
                }
                let x = rc.left - GROW;
                let y = rc.top - GROW;
                let inner = IsZoomed(target).as_bool();
                // The target's own dpi, not the screen's: a frame around a window on a 100 %
                // monitor must not take the 200 % monitor's corner radius.
                let mut dpi = GetDpiForWindow(target);
                if dpi == 0 {
                    dpi = if screen.is_invalid() {
                        96
                    } else {
                        GetDeviceCaps(screen, LOGPIXELSX).clamp(96, 480) as u32
                    };
                }
                let dpi = dpi.clamp(96, 480);
                if w != self.w || h != self.h || self.dirty || inner != self.inner || dpi != self.dpi {
                    self.inner = inner;
                    self.dpi = dpi;
                    self.paint(x, y, w, h, screen, dpi);
                    self.w = w;
                    self.h = h;
                    self.x = x;
                    self.y = y;
                    self.dirty = false;
                } else if x != self.x || y != self.y {
                    self.x = x;
                    self.y = y;
                    let _ = SetWindowPos(
                        self.hwnd,
                        HWND_TOPMOST,
                        x,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            self.set_shown(true);
        }

        fn paint(&mut self, x: i32, y: i32, w: i32, h: i32, screen: HDC, dpi: u32) {
            unsafe {
                if screen.is_invalid() {
                    return;
                }
                if !self.bmp.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.bmp.0));
                    self.bmp = HBITMAP::default();
                }
                if !self.hdc.is_invalid() {
                    let _ = DeleteDC(self.hdc);
                    self.hdc = HDC::default();
                }
                let mem = CreateCompatibleDC(screen);
                if mem.is_invalid() {
                    return;
                }
                let mut bmi = BITMAPINFO::default();
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = w;
                // Negative: rows run top to bottom, the way the renderer indexes them.
                bmi.bmiHeader.biHeight = -h;
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB.0;
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let Ok(bmp) = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)
                else {
                    let _ = DeleteDC(mem);
                    return;
                };
                let (rgb, peak) = if self.preview {
                    (PREVIEW_RGB, PREVIEW_PEAK)
                } else {
                    (PINNED_RGB, PINNED_PEAK)
                };
                // A maximised window owns the whole screen, so its frame moves inside the edge.
                let offset = if self.inner { -(STROKE + 1.0) } else { OFFSET };
                let radius = RADIUS * (dpi as f32 / 96.0);
                render(bits as *mut u8, w, h, rgb, peak, offset, radius);

                let old = SelectObject(mem, HGDIOBJ(bmp.0));
                let dst = POINT { x, y };
                let size = SIZE { cx: w, cy: h };
                let src = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let _ = UpdateLayeredWindow(
                    self.hwnd,
                    screen,
                    Some(&dst as *const POINT),
                    Some(&size as *const SIZE),
                    mem,
                    Some(&src as *const POINT),
                    COLORREF(0),
                    Some(&blend as *const BLENDFUNCTION),
                    ULW_ALPHA,
                );
                let _ = SelectObject(mem, old);

                self.hdc = mem;
                self.bmp = bmp;
            }
        }
    }

    /// One tick: work out which frames should exist, then let each one follow its window.
    fn step(frames: &mut Vec<Frame>, screen: HDC) {
        let pinned: Vec<isize> = match PINNED.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let candidate = CANDIDATE.load(SeqCst);
        // Before anything else: a pinned window that some other program demoted is not pinned any
        // more, and the frame around it would be telling a lie. Every fourth tick, not every tick:
        // a program that clears the bit on every resize would otherwise be answered 30 times a
        // second, and 130 ms is far below what anyone can see.
        if TICKS.fetch_add(1, SeqCst) % 4 == 0 {
            hold_on_top(&pinned);
        }
        let mut want: Vec<(isize, bool)> = pinned.iter().map(|t| (*t, false)).collect();
        if ARMED.load(SeqCst) && candidate != 0 && !want.iter().any(|(t, _)| *t == candidate) {
            want.push((candidate, true));
        }

        frames.retain_mut(|f| {
            let keep = want.iter().any(|(t, _)| *t == f.target) && still_wanted(f.target);
            if !keep {
                f.destroy();
            }
            keep
        });

        for (target, preview) in &want {
            // `position` and not the `Option` from `iter_mut().find()`: the borrow that a match on
            // a live iterator keeps would rule out pushing the frame it just decided to add.
            if let Some(i) = frames.iter().position(|f| f.target == *target) {
                let f = &mut frames[i];
                // A preview that just became pinned changes colour, not identity: recolouring
                // beats destroying and rebuilding the window in the same tick.
                if f.preview != *preview {
                    f.preview = *preview;
                    f.dirty = true;
                }
            } else if let Some(f) = Frame::new(*target, *preview) {
                frames.push(f);
            }
        }

        for f in frames.iter_mut() {
            f.place(screen);
        }
    }

    /// Fills a 32-bit premultiplied BGRA bitmap with the frame's ring and nothing else.
    ///
    /// The interior stays at alpha zero, so the window inside the frame is untouched — the stroke
    /// is centred on the window's visible edge and its halo falls outside that. `offset` is the
    /// stroke's centre relative to that edge, and only a maximised window passes a negative one.
    fn render(bits: *mut u8, w: i32, h: i32, rgb: [f32; 3], peak: f32, offset: f32, radius: f32) {
        let len = (w as usize) * (h as usize) * 4;
        unsafe {
            std::ptr::write_bytes(bits, 0, len);
        }
        let cx = w as f32 * 0.5;
        let cy = h as f32 * 0.5;
        let half_w = cx - GROW as f32 + offset;
        let half_h = cy - GROW as f32 + offset;
        // Everything that can carry ink lies within this distance of the ring.
        let band = STROKE + GLOW + 2.0;
        // Rows inside this band are the ones where the top and bottom edges run across the whole
        // width; the rest only carry the left and right edges, a few pixels each.
        let span_hi = (cx - half_w + band).max(0.0) as i32;
        let span_lo = (cx + half_w - band).min(w as f32) as i32;

        for y in 0..h {
            let py = y as f32 + 0.5 - cy;
            let full_row = py.abs() > half_h - radius - band;
            let mut x = 0;
            while x < w {
                // Interior and the empty margin outside the halo: skip to the far edge in one
                // step. Without this a 1000 x 800 frame is 800 000 pixels of arithmetic per
                // repaint, which a window being dragged edge-first would pay 30 times a second.
                if !full_row && x > span_hi && x < span_lo {
                    x = span_lo;
                    continue;
                }
                let px = x as f32 + 0.5 - cx;
                let d = sd_round_rect(px, py, half_w, half_h, radius);
                let a = ring_alpha(d) * peak;
                if a > 0.002 {
                    let i = ((y as usize) * (w as usize) + x as usize) * 4;
                    unsafe {
                        *bits.add(i) = (rgb[2] * a) as u8;
                        *bits.add(i + 1) = (rgb[1] * a) as u8;
                        *bits.add(i + 2) = (rgb[0] * a) as u8;
                        *bits.add(i + 3) = (a * 255.0) as u8;
                    }
                }
                x += 1;
            }
        }
    }

    /// Signed distance to a rounded rectangle centred on the origin: negative inside, zero on the
    /// outline, positive outside. Standard rounded-box SDF.
    fn sd_round_rect(px: f32, py: f32, half_w: f32, half_h: f32, r: f32) -> f32 {
        let r = r.min(half_w).min(half_h);
        let qx = px.abs() - (half_w - r);
        let qy = py.abs() - (half_h - r);
        let ax = qx.max(0.0);
        let ay = qy.max(0.0);
        (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// Coverage at a distance from the outline: a solid core with one antialiased pixel each side,
    /// a short halo outside, and nothing at all inside.
    fn ring_alpha(d: f32) -> f32 {
        let core = (STROKE + 0.5 - d.abs()).clamp(0.0, 1.0);
        if core >= 1.0 {
            return 1.0;
        }
        let outside = d - STROKE;
        let halo = if outside > 0.0 {
            (1.0 - outside / GLOW).clamp(0.0, 1.0).powi(2) * 0.55
        } else {
            0.0
        };
        core.max(halo)
    }

    #[cfg(test)]
    mod tests {
        use super::{ring_alpha, sd_round_rect, GLOW, GROW, RADIUS, STROKE};

        /// The outline is where the renderer expects it: at the rectangle's edge, not a pixel off.
        #[test]
        fn the_distance_is_zero_on_the_outline() {
            // 100 x 60 box, 30 of half-width either side of centre
            assert!(sd_round_rect(30.0, 0.0, 30.0, 20.0, RADIUS).abs() < 0.001, "right edge");
            assert!(sd_round_rect(0.0, 20.0, 30.0, 20.0, RADIUS).abs() < 0.001, "top edge");
            assert!(sd_round_rect(0.0, 0.0, 30.0, 20.0, RADIUS) < -10.0, "middle is inside");
            assert!(sd_round_rect(60.0, 0.0, 30.0, 20.0, RADIUS) > 10.0, "30 past the edge is out");
            // Corners are rounded, so the one sharp corner of the box is outside the outline
            let corner = sd_round_rect(30.0, 20.0, 30.0, 20.0, RADIUS);
            assert!(corner > 1.0, "a square corner must fall outside a rounded one: {corner}");
        }

        /// A 3 px line centred on the outline: solid in the middle, half-covered at each edge,
        /// gone half a pixel further.
        #[test]
        fn the_line_is_three_pixels_wide() {
            assert_eq!(ring_alpha(0.0), 1.0, "the centre of the line");
            assert_eq!(ring_alpha(0.4), 1.0, "still solid half a pixel out");
            let edge = ring_alpha(STROKE);
            assert!(edge > 0.4 && edge < 0.6, "the antialiased edge: {edge}");
            assert_eq!(ring_alpha(-(STROKE + 0.5)), 0.0, "nothing one and a half pixels inside");
            assert_eq!(ring_alpha(-5.0), 0.0, "the window's content is untouched");
        }

        /// The halo has to be there, or the frame reads as a hairline that a bright wallpaper
        /// eats — and it has to stop, or the frame bleeds into the window beside it.
        ///
        /// Held to "strongest right outside the line, weaker further out, gone at the limit"
        /// rather than to a figure at a fixed distance: an earlier version asserted
        /// `ring_alpha(STROKE + 1.5) > 0.2`, which was only true while `GLOW` was 4.5 and broke
        /// the build the moment the halo was shortened to 3.0 — a test that pinned the parameter
        /// it was supposed to be checking the shape of.
        #[test]
        fn the_halo_reaches_past_the_line_and_stops() {
            let near = ring_alpha(STROKE + 0.5);
            let far = ring_alpha(STROKE + GLOW * 0.5);
            assert!(near > 0.3, "the halo is strongest just outside the line: {near}");
            assert!(far > 0.0, "and still there halfway out: {far}");
            assert!(far < near, "and weaker than the near part: {far} < {near}");
            assert_eq!(ring_alpha(STROKE + GLOW), 0.0, "the halo ends where it says");
            assert_eq!(ring_alpha(STROKE + GLOW + 1.0), 0.0);
        }

        /// The bitmap is only `GROW` wider than the window on each side, and the ink that reaches
        /// furthest is the stroke's outer half plus the halo. If that ever exceeded `GROW` the
        /// halo would be clipped into a hard edge exactly where the frame ends.
        #[test]
        fn the_frame_window_is_wide_enough_for_its_ink() {
            let reach = super::OFFSET + STROKE + GLOW;
            assert!(
                reach <= GROW as f32,
                "the ink reaches {reach} px past the edge but the window only has {GROW}"
            );
        }

        /// The stroke is centred on the window's visible edge, which is what makes the frame sit
        /// against the window instead of floating outside it.
        #[test]
        fn the_stroke_sits_on_the_edge_not_outside_it() {
            assert_eq!(super::OFFSET, 0.0);
        }

        use super::{could_drop_from, RECT};

        /// A rectangle as Win32 reports one: left, top, width, height.
        fn rect(left: i32, top: i32, width: i32, height: i32) -> RECT {
            RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            }
        }

        /// The case the rule exists for: WeChat's emoji panel is a window of its own, it opens
        /// inside the chat window, and pinning the chat window used to bury it.
        #[test]
        fn a_panel_inside_its_window_could_drop_from_it() {
            let cover = could_drop_from(rect(300, 240, 320, 400), rect(100, 100, 900, 700));
            assert_eq!(cover, Some(12_800_000), "a panel wholly inside covers all of it");
        }

        /// A context menu hangs a little over the edge of the window it drops from, and must still
        /// count as that window's: 260 of its 300 px are over it.
        #[test]
        fn a_menu_hanging_over_the_edge_could_still_drop_from_it() {
            let cover = could_drop_from(rect(740, 200, 300, 400), rect(100, 100, 900, 700));
            assert_eq!(cover, Some(10_400_000));
        }

        /// The same menu with only a fifth of itself over the window is a window of its own, and
        /// lifting it would put it over everything with nothing to explain why.
        #[test]
        fn a_menu_mostly_outside_is_not_a_dropdown() {
            assert_eq!(
                could_drop_from(rect(940, 200, 300, 400), rect(100, 100, 900, 700)),
                None
            );
        }

        /// Another window of the same program: never larger than the pinned one, and never the
        /// same size — Directory Opus opens listers, Explorer folder windows, at the last size.
        #[test]
        fn another_window_of_the_program_is_not_a_dropdown() {
            let host = rect(100, 100, 900, 700);
            assert_eq!(could_drop_from(rect(100, 100, 1000, 700), host), None, "larger");
            assert_eq!(could_drop_from(rect(100, 100, 900, 700), host), None, "the same size");
            assert_eq!(could_drop_from(rect(100, 100, 810, 630), host), None, "90 % of it");
            assert!(
                could_drop_from(rect(100, 100, 801, 623), host).is_some(),
                "just under the guard it is a dropdown again"
            );
        }

        /// A hidden scratch window is size 0 and a caret is a few pixels across; neither is a
        /// dropdown, and lifting them would only add handles to the topmost band.
        #[test]
        fn a_speck_is_not_a_dropdown() {
            let host = rect(100, 100, 900, 700);
            assert_eq!(could_drop_from(rect(300, 300, 7, 40), host), None, "under the minimum");
            assert_eq!(
                could_drop_from(rect(300, 300, 100, 100), rect(0, 0, 0, 0)),
                None,
                "a host with no area at all"
            );
            assert_eq!(could_drop_from(rect(300, 300, 8, 8), host), Some(6_400), "on it");
        }
    }
}
