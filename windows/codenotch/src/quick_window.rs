//! The quick-add window: one field, opened by the plus on the to-do card, dismissed as soon as it
//! has written or the pointer leaves it empty.
//!
//! It exists as a window rather than a field in the card because the notch is `focusable:false`:
//! no key event ever reaches that WebView, so a text box there would swallow every keystroke in
//! silence. The card can be clicked; only another window can be typed into.
//!
//! Unlike the settings window this one is kept after it is built, hidden instead of destroyed. It
//! is opened on a click from a hover, and paying for a cold WebView2 start each time would make the
//! plus feel broken — the one thing it has to do is be ready to take a line of text.

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "quick";

/// Wide enough for one date row and one priority row in every language the app speaks; Russian's
/// "Приоритет · Высокий · Средний · Низкий" is the worst case and still clears it.
const W: f64 = 320.0;
/// A field, two chip rows and a hint line: 34 + 25 + 25 + 15 of content, three 8 px gaps and 12 px
/// padding either side, with a few px left over. Fixed rather than measured, because a window that
/// resizes itself after appearing reads as a twitch.
const H: f64 = 152.0;
/// The pill is 70 CSS px across and the page is laid out to the design width, so this is the slice
/// of the window it takes whatever the monitor scale or the UI size.
const BAR_CSS: f64 = 70.0;
/// Thickness of the pill when it lies flat along the top or bottom edge: a 44 px ring plus 12 px
/// of padding either side. Its width there is the content's, so only this is needed.
const BAR_FLAT_CSS: f64 = 68.0;
/// Air between the pill and the window, in the same CSS px.
const GAP_CSS: f64 = 10.0;

/// Built on a later turn of the event loop, for the reason the settings window records: a window
/// built inside a synchronous command deadlocks WebView2 and comes up blank, and `run_on_main_thread`
/// called from the main thread itself — where those commands run — builds it on the spot.
pub fn open(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || open_now(&app));
    });
}

/// Hidden, not closed: the WebView is kept so the next plus is instant.
pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.hide();
    }
}

fn open_now(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        // This window outlives a theme change, and a page cannot notice one by itself: WebView2's
        // own colour scheme does not follow a live change, which is why every page here is told.
        let theme = crate::resolved_theme(app);
        let _ = w.eval(&format!("document.documentElement.dataset.theme={theme:?}"));
        // What was typed last time is not what is being typed now
        let _ = w.emit("quick_reset", ());
        place(app, &w);
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let theme = crate::theme_choice(app);
    let mut builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("quick.html".into()))
        .title("Codenotch")
        .inner_size(W, H)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .shadow(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .theme(theme)
        .initialization_script(crate::theme_script(crate::resolved_theme(app)));
    // Best guess before it exists; corrected from the measured window below
    let scale = app
        .get_webview_window("notch")
        .and_then(|n| n.scale_factor().ok())
        .unwrap_or(1.0);
    if let Some((x, y)) = origin(app, (W * scale).round() as i32, (H * scale).round() as i32) {
        builder = builder.position(x as f64 / scale, y as f64 / scale);
    }
    match builder.build() {
        Ok(w) => {
            // Placement asks the window how big it really is: at 150 % the logical size above and
            // the physical window disagree by a pixel or two, which is enough to sit off the pill.
            place(app, &w);
            let _ = w.set_focus();
        }
        Err(e) => crate::applog(&format!("quick window: {e}")),
    }
}

fn place(app: &AppHandle, w: &tauri::WebviewWindow) {
    let Some(size) = w.outer_size().ok() else { return };
    let Some((x, y)) = origin(app, size.width as i32, size.height as i32) else { return };
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

/// A rectangle in physical pixels, as the window reports it.
#[derive(Clone, Copy)]
struct Frame {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Where the window goes: beside the pill on an upright edge, under (or over) it on a flat one.
///
/// The notch window is much larger than the pill — 360 × 900 with the pill occupying its right-hand
/// 70 px — so the anchor is recovered from the window's own rectangle and the design width the page
/// is laid out to, not guessed from the scale factor. `w / design` is the physical size of one CSS
/// px in that window, and the pill's share of it is a constant.
///
/// Split out from `origin` so the arithmetic can be checked without a window: every case here is a
/// wrong number the user sees as a window half off the screen, and none of it is reachable from CI
/// otherwise.
fn origin_for(edge: &str, notch: Frame, design: f64, qw: i32, qh: i32) -> (i32, i32) {
    let upright = crate::config::edge_is_vertical(edge);
    let k = notch.w as f64 / design.max(1.0);
    let bar = ((if upright { BAR_CSS } else { BAR_FLAT_CSS }) * k).round() as i32;
    let gap = (GAP_CSS * k).round() as i32;
    // Upright edges are centred along the edge, so the window is centred on the same line
    match edge {
        "left" => (notch.x + bar + gap, notch.y + (notch.h - qh) / 2),
        "top" => (notch.x + (notch.w - qw) / 2, notch.y + bar + gap),
        "bottom" => (notch.x + (notch.w - qw) / 2, notch.y + notch.h - bar - gap - qh),
        _ => (notch.x + notch.w - bar - gap - qw, notch.y + (notch.h - qh) / 2),
    }
}

fn origin(app: &AppHandle, qw: i32, qh: i32) -> Option<(i32, i32)> {
    let notch = app.get_webview_window("notch")?;
    let pos = notch.outer_position().ok()?;
    let size = notch.outer_size().ok()?;
    let edge = {
        let st = app.state::<crate::AppState>();
        let c = st.cfg.lock().unwrap();
        crate::config::edge_or_right(&c.notch_edge)
    };
    let design = if crate::config::edge_is_vertical(&edge) { crate::NOTCH_W } else { crate::NOTCH_LONG };
    let frame = Frame { x: pos.x, y: pos.y, w: size.width as i32, h: size.height as i32 };
    Some(origin_for(&edge, frame, design, qw, qh))
}

#[cfg(test)]
mod tests {
    use super::{origin_for, Frame};

    /// The upright notch: 360 CSS px wide, 900 tall, placed anywhere.
    fn upright() -> Frame {
        Frame { x: 1000, y: 0, w: 360, h: 900 }
    }
    /// The flat one: 900 × 900, the ring row across the top or bottom.
    fn flat() -> Frame {
        Frame { x: 0, y: 0, w: 900, h: 900 }
    }

    #[test]
    fn the_window_sits_beside_the_pill_on_the_right_edge() {
        // 320 wide against a 70 px pill and 10 px of air: it starts 40 px left of the window
        assert_eq!(origin_for("right", upright(), 360.0, 320, 152), (960, 374));
    }

    #[test]
    fn the_same_gap_reads_the_same_at_a_hundred_and_fifty_percent() {
        // The window is 540 physical px wide, so one CSS px is 1.5 and every constant follows;
        // the point of the test is that it stays the same *visual* distance from the pill.
        let frame = Frame { x: 2000, y: 0, w: 540, h: 1350 };
        assert_eq!(origin_for("right", frame, 360.0, 480, 228), (1940, 561));
    }

    #[test]
    fn a_left_edge_pill_puts_the_window_on_its_right() {
        assert_eq!(origin_for("left", Frame { x: 0, y: 0, w: 360, h: 900 }, 360.0, 320, 152), (80, 374));
    }

    #[test]
    fn a_flat_pill_puts_the_window_below_or_above_it() {
        // 68 CSS px of pill lies along the top edge, then the same 10 px of air
        assert_eq!(origin_for("top", flat(), 900.0, 320, 152), (290, 78));
        // And a bottom pill is measured from the bottom of the window upwards
        assert_eq!(origin_for("bottom", flat(), 900.0, 320, 152), (290, 670));
    }
}
