//! Keeps *other* applications' windows on top, the way PowerToys' "Always on Top" does.
//!
//! `topmost.rs` and this module look alike and are not: that one fights to keep our own notch in
//! the topmost band, this one deliberately changes the z-order of windows we do not own. Two
//! clicks and it is done — one on the tray item to arm the picker, one on the window that should
//! stay up — and a second pick on the same window lets it go again.
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

#[cfg(windows)]
pub use win::{begin_pick, shutdown, start};

#[cfg(not(windows))]
pub fn begin_pick() {}
#[cfg(not(windows))]
pub fn shutdown() {}
#[cfg(not(windows))]
pub fn start() {}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    use windows::core::w;
    use windows::Win32::Foundation::{
        COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, SelectObject,
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetAncestor, GetClassNameW,
        GetCursorPos,
        GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
        IsWindow, IsWindowVisible, IsZoomed, KillTimer, PostThreadMessageW, SetTimer, SetWindowPos,
        SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, UpdateLayeredWindow, WindowFromPoint,
        GA_ROOT, GWL_EXSTYLE, HHOOK, HMENU, HWND_NOTOPMOST, HWND_TOPMOST, MSG, MSLLHOOKSTRUCT,
        SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA,
        WH_MOUSE_LL, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_TIMER,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        WS_POPUP,
    };

    /// Where the frame window sits relative to the window it frames, in physical pixels. Wide
    /// enough to hold the halo: the stroke starts at `OFFSET` outside the window and the glow
    /// reaches `OFFSET + STROKE + GLOW` beyond it.
    const GROW: i32 = 8;
    /// How far outside the window's own edge the centre of the stroke sits. Positive, so the line
    /// never covers a pixel the window draws: a frame that ate the first column of a text editor
    /// would be worse than no frame at all.
    ///
    /// A maximised window is the exception and is handled per frame: its rectangle is the whole
    /// screen, so a stroke outside it would be drawn off the edge and never seen. `Frame::inner`
    /// flips the offset for exactly that case.
    const OFFSET: f32 = 1.5;
    /// Outer corner radius. Windows 11 rounds a window at 8 px; a pixel and a half further out, 11
    /// sits on the same arc instead of cutting across it.
    const RADIUS: f32 = 11.0;
    /// Half the thickness of the solid line — 3 px across, counting both antialiased edges.
    const STROKE: f32 = 1.0;
    /// How far the halo reaches outside the line.
    const GLOW: f32 = 4.5;
    /// The frame follows the window on this tick. Fast enough to stay glued while a window is
    /// dragged, slow enough to cost nothing: one `GetWindowRect` per pinned window per tick.
    const TICK_MS: u64 = 33;
    /// A picker that is armed and forgotten must let go of the mouse by itself; a hook that stays
    /// installed with nothing on screen to explain it would leave the user unable to click.
    const PICK_TIMEOUT: Duration = Duration::from_secs(60);
    const PICK_TIMER: usize = 0x7A;

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
    /// True while a picker is armed, so a second tray click cancels instead of stacking a second
    /// hook. Both the tray menu and the frame thread read it.
    static PICKING: AtomicBool = AtomicBool::new(false);
    /// The window under the pointer, updated by the hook and read by the frame thread. Zero when
    /// the pointer is over one of our own windows or over nothing.
    static CANDIDATE: AtomicIsize = AtomicIsize::new(0);
    /// Where the accepted click landed. Zero means the picker was cancelled.
    static RESULT: AtomicIsize = AtomicIsize::new(0);
    /// The picker thread's id, so the hook callback — which gets no user data — can post it the
    /// `WM_QUIT` that ends the message loop.
    static PICK_TID: AtomicU32 = AtomicU32::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);

    /// Arms the picker. Called from the tray menu; also the cancel path, since the menu item is
    /// reachable again while a picker is armed.
    pub fn begin_pick() {
        if PICKING.swap(true, SeqCst) {
            post_quit();
            crate::applog("pin: picker cancelled from the tray");
            return;
        }
        CANDIDATE.store(0, SeqCst);
        RESULT.store(0, SeqCst);
        std::thread::spawn(|| {
            let picked = run_pick();
            CANDIDATE.store(0, SeqCst);
            PICKING.store(false, SeqCst);
            match picked {
                Some(target) => toggle(target),
                None => crate::applog("pin: nothing picked"),
            }
        });
    }

    /// Lets go of every window this session pinned, stops an armed picker, and stops the frame
    /// thread. Called on the way out: a window left topmost with no frame around it would look
    /// like a bug and could only be undone by restarting the app that owns it.
    pub fn shutdown() {
        STOP.store(true, SeqCst);
        post_quit();
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
        }
        if let Ok(mut g) = PINNED.lock() {
            g.clear();
        }
        if !list.is_empty() {
            crate::applog(&format!("pin: released {} window(s) on quit", list.len()));
        }
    }

    /// Starts the frame thread. Frames are created, painted and destroyed there and nowhere else:
    /// a window belongs to the thread that made it, and this thread is the one that keeps looking
    /// at their targets.
    pub fn start() {
        std::thread::spawn(|| {
            // One screen DC for the life of the thread. `GetDC(NULL)` per repaint would leak a
            // handle per resized frame — 30 a second while a window is dragged — and the 10 000
            // handle limit is not far off.
            let screen = unsafe { GetDC(HWND::default()) };
            let mut frames: Vec<Frame> = Vec::new();
            loop {
                if STOP.load(SeqCst) {
                    for f in frames.iter_mut() {
                        f.destroy();
                    }
                    return;
                }
                step(&mut frames, screen);
                std::thread::sleep(Duration::from_millis(TICK_MS));
            }
        });
    }

    fn hwnd_of(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    /// The picker's own loop. Returns the window that was clicked, or `None` for any way out that
    /// is not a left click — right button, Escape, timeout, cancel.
    fn run_pick() -> Option<isize> {
        unsafe {
            let Ok(mouse) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE::default(), 0)
            else {
                crate::applog("pin: could not install the mouse hook");
                return None;
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
            let started = Instant::now();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
                if msg.message == WM_TIMER {
                    if GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 || started.elapsed() > PICK_TIMEOUT {
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
            let _ = UnhookWindowsHookEx(mouse);
            PICK_TID.store(0, SeqCst);
        }
        let result = RESULT.swap(0, SeqCst);
        (result != 0).then_some(result)
    }

    /// Ends the message loop from anywhere, including from inside the hook callback.
    fn post_quit() {
        let tid = PICK_TID.load(SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
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
                RESULT.store(target_at(info.pt).0 as isize, SeqCst);
                post_quit();
                // Swallow it: the window is being chosen, not clicked.
                LRESULT(1)
            }
            WM_RBUTTONDOWN => {
                RESULT.store(0, SeqCst);
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
    /// surfaces. The shell matters most — the tray icon that armed the picker lives in the
    /// taskbar, so a second click on it would otherwise pin the taskbar itself instead of
    /// cancelling, and the user would have no way out but Escape.
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

    /// Pins a window, or releases it if it was already pinned. `SetWindowPos` without
    /// `SWP_NOACTIVATE` on the way in, because the user just chose this window and expects it in
    /// front as well as on top.
    fn toggle(target: isize) {
        let Ok(mut list) = PINNED.lock() else { return };
        if let Some(pos) = list.iter().position(|t| *t == target) {
            list.remove(pos);
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
            crate::applog("pin: released a window");
        } else {
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
            crate::applog("pin: kept a window on top");
        }
    }

    /// Drops a window from the list without touching its z-order — used when it closed, or when
    /// something else un-pinned it, and the state is already what we would have set.
    fn forget(target: isize) {
        if let Ok(mut g) = PINNED.lock() {
            g.retain(|t| *t != target);
        }
    }

    /// Whether a frame's target still deserves one. A window that closed, or that some other
    /// program un-pinned, must lose its frame as well: a border around a window that is no longer
    /// topmost is a lie the user cannot clear.
    fn still_wanted(target: isize, preview: bool) -> bool {
        let hwnd = hwnd_of(target);
        unsafe {
            if !IsWindow(hwnd).as_bool() {
                forget(target);
                return false;
            }
            if !preview {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOPMOST.0 == 0 {
                    forget(target);
                    return false;
                }
            }
        }
        true
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
        /// instead of outside it, or it would be drawn past the edge of the display.
        inner: bool,
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
                let mut rc = RECT::default();
                if GetWindowRect(target, &mut rc).is_err() {
                    self.set_shown(false);
                    return;
                }
                let w = rc.right - rc.left + GROW * 2;
                let h = rc.bottom - rc.top + GROW * 2;
                if w < 8 || h < 8 {
                    self.set_shown(false);
                    return;
                }
                let x = rc.left - GROW;
                let y = rc.top - GROW;
                let inner = IsZoomed(target).as_bool();
                if w != self.w || h != self.h || self.dirty || inner != self.inner {
                    self.inner = inner;
                    self.paint(x, y, w, h, screen);
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

        fn paint(&mut self, x: i32, y: i32, w: i32, h: i32, screen: HDC) {
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
                render(bits as *mut u8, w, h, rgb, peak, offset);

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
        let mut want: Vec<(isize, bool)> = pinned.iter().map(|t| (*t, false)).collect();
        if PICKING.load(SeqCst) && candidate != 0 && !want.iter().any(|(t, _)| *t == candidate) {
            want.push((candidate, true));
        }

        frames.retain_mut(|f| {
            let keep = want.iter().any(|(t, _)| *t == f.target) && still_wanted(f.target, f.preview);
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
    /// sits just outside the window's own edge and its halo falls outside that. `offset` is the
    /// stroke's centre relative to that edge, and only a maximised window passes a negative one.
    fn render(bits: *mut u8, w: i32, h: i32, rgb: [f32; 3], peak: f32, offset: f32) {
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
            let full_row = py.abs() > half_h - RADIUS - band;
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
                let d = sd_round_rect(px, py, half_w, half_h, RADIUS);
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
        use super::{ring_alpha, sd_round_rect, GLOW, RADIUS, STROKE};

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
        /// gone half a pixel further, and nothing on the window side at all.
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
        #[test]
        fn the_halo_reaches_past_the_line_and_stops() {
            assert!(ring_alpha(STROKE + 1.5) > 0.2, "just outside the line");
            assert!(ring_alpha(STROKE + GLOW * 0.5) > 0.0);
            assert_eq!(ring_alpha(STROKE + GLOW), 0.0, "the halo ends where it says");
            assert_eq!(ring_alpha(STROKE + GLOW + 1.0), 0.0);
        }
    }
}
