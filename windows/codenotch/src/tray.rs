use crate::i18n::tr;
use crate::traymenu;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

/// The pin icon is a tray icon of its own, with its own id, because it is a mode rather than a
/// command: it has to sit there showing whether clicks are being captured.
const PIN_ID: &str = "pin";

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    let mut builder = TrayIconBuilder::with_id("main");
    // Windows does not tint tray icons and the monochrome outline vanishes on a dark taskbar, so
    // the app's own mark is the icon. trayicon::app_mark explains why at length.
    if let Some(icon) = crate::trayicon::app_mark() {
        builder = builder.icon(icon);
    }
    builder
        .tooltip(concat!("Codenotch v", env!("CARGO_PKG_VERSION")))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| handle(app, ev.id().as_ref()))
        .build(app)?;
    setup_pin(app)?;
    Ok(())
}

/// The always-on-top icon: left click arms the picker, right click opens the three things you can
/// do with it.
///
/// It is separate from the app's own icon on purpose. Pinning another application's window is the
/// one thing in Codenotch that reaches outside Codenotch, and it wants a target you can hit
/// without opening a menu and reading it.
fn setup_pin(app: &AppHandle) -> tauri::Result<()> {
    let mut builder = TrayIconBuilder::with_id(PIN_ID);
    if let Some(icon) = crate::trayicon::pin_mark() {
        builder = builder.icon(icon);
    }
    builder
        .tooltip(pin_tooltip(app))
        .menu(&pin_menu(app)?)
        // Left click must arm the picker, not open a menu: arming is the whole point of the icon.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "pin_mode" => crate::pintop::toggle_mode(),
            "pin_clear" => crate::pintop::release_all(),
            "settings" => crate::settings_window::open(app),
            _ => {}
        })
        .on_tray_icon_event(|_tray, event| {
            // Up, not Down: Windows reports both, and toggling on each would arm and disarm in
            // the same press.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::pintop::toggle_mode();
            }
        })
        .build(app)?;
    Ok(())
}

/// The menu behind a right click on the pin icon. Rebuilt whenever the mode or the count changes,
/// so the first line always says what the next click would do.
fn pin_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let lang = language(app);
    let armed = crate::pintop::is_armed();
    let held = crate::pintop::count();
    let mode = MenuItemBuilder::with_id(
        "pin_mode",
        tr(&lang, if armed { "pin_stop" } else { "pin_start" }),
    )
    .build(app)?;
    let clear = MenuItemBuilder::with_id("pin_clear", tr(&lang, "pin_clear"))
        .enabled(held > 0)
        .build(app)?;
    let settings = MenuItemBuilder::with_id("settings", tr(&lang, "settings")).build(app)?;
    MenuBuilder::new(app)
        .item(&mode)
        .item(&clear)
        .separator()
        .item(&settings)
        .build()
}

/// What the pointer finds when it rests on the pin icon: whether it is listening, and how many
/// windows it is holding up. Without this the mode has no visible state anywhere.
fn pin_tooltip(app: &AppHandle) -> String {
    let lang = language(app);
    if crate::pintop::is_armed() {
        return tr(&lang, "pin_tip_armed").to_string();
    }
    match crate::pintop::count() {
        0 => tr(&lang, "pin_tip_idle").to_string(),
        n => format!("{} · {}", tr(&lang, "pin_tray"), n),
    }
}

/// Puts the icon back in step with the mode. Called from the picker's own threads, so the hop to
/// the main thread happens here: a menu is a Windows UI object and must not be built off it.
pub fn refresh_pin_tray(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = handle.tray_by_id(PIN_ID) else { return };
        if let Ok(menu) = pin_menu(&handle) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(&pin_tooltip(&handle)));
    });
}

/// The readings themselves, as the Mac's menu bar shows them: a line per provider with its headline
/// figure, and under it one greyed line per limit window. The macOS menu is rebuilt as it opens;
/// Tauri has no such hook, so `refresh_menu` is called whenever a reading changes and once a minute
/// besides, which keeps "Resets in 12 min" honest.
/// Every line the menu would show, in order, with the id each carries. Kept apart from building the
/// menu so a refresh can tell whether anything visible changed before it swaps the menu out.
fn menu_lines(app: &AppHandle, lang: &str) -> Vec<(String, String, bool)> {
    let now = crate::now_ms();
    let mut lines = Vec::new();
    for id in crate::TRAY_PROVIDER_IDS {
        let snap = crate::snapshot_of(app, id);
        if snap.status == "absent" {
            continue;
        }
        let head = traymenu::header(
            crate::provider_label(id),
            crate::ring_fraction(app, id),
            traymenu::stale_since(&snap, now),
            now,
            lang,
        );
        // Clicking a provider re-reads that one, as on the Mac.
        lines.push((format!("refresh:{id}"), head, true));
        for (n, line) in traymenu::provider_lines(&snap, now, lang).iter().enumerate() {
            // Windows does not indent submenu-less items, so the indent is in the text.
            lines.push((format!("line:{id}:{n}"), format!("    {line}"), false));
        }
    }
    lines
}

pub fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let lang = language(app);
    build_menu_from(app, &lang, &menu_lines(app, &lang))
}

fn build_menu_from(app: &AppHandle, lang: &str, lines: &[(String, String, bool)]) -> tauri::Result<Menu<Wry>> {
    let lang = lang.to_string();
    let mut items: Vec<tauri::menu::MenuItem<Wry>> = Vec::new();
    for (id, text, enabled) in lines {
        items.push(MenuItemBuilder::with_id(id.clone(), text.clone()).enabled(*enabled).build(app)?);
    }
    if items.is_empty() {
        items.push(
            MenuItemBuilder::with_id("waiting", tr(&lang, "waiting"))
                .enabled(false)
                .build(app)?,
        );
    }
    let pin = MenuItemBuilder::with_id("pin_top", tr(&lang, "pin_top")).build(app)?;
    let refresh = MenuItemBuilder::with_id("refresh", tr(&lang, "refresh_all")).build(app)?;
    let settings = MenuItemBuilder::with_id("settings", tr(&lang, "settings")).build(app)?;
    let quit = MenuItemBuilder::with_id("quit", tr(&lang, "quit_app")).build(app)?;
    // The pin item sits on its own at the top: it is the only entry that changes something
    // outside Codenotch, and burying it between the readings would make it look like one of them.
    let mut menu = MenuBuilder::new(app).item(&pin).separator();
    for item in &items {
        menu = menu.item(item);
    }
    menu.separator()
        .item(&refresh)
        .item(&settings)
        .separator()
        .item(&quit)
        .build()
}

/// The language the menu speaks, already resolved: `traymenu` picks its wording by code and has no
/// "auto" of its own.
pub(crate) fn language(app: &AppHandle) -> String {
    let st = app.state::<crate::AppState>();
    let raw = st.cfg.lock().unwrap().lang.clone();
    if raw == "auto" {
        crate::i18n::resolve_auto().to_string()
    } else {
        raw
    }
}

/// The hover text: the same figures the menu opens with, for when the menu is not open.
fn tooltip(app: &AppHandle) -> String {
    let mut parts: Vec<String> = Vec::new();
    for id in crate::TRAY_PROVIDER_IDS {
        if crate::snapshot_of(app, id).status == "absent" {
            continue;
        }
        let value = crate::ring_fraction(app, id)
            .map(|f| format!("{}%", traymenu::pct(f)))
            .unwrap_or_else(|| "—".into());
        parts.push(format!("{} {value}", crate::provider_label(id)));
    }
    if parts.is_empty() {
        concat!("Codenotch v", env!("CARGO_PKG_VERSION")).to_string()
    } else {
        format!("Codenotch — {}", parts.join(" · "))
    }
}

/// Rebuilds the tray menu, ALWAYS on the main thread.
///
/// A menu is a Windows UI object. Building one or swapping it in from another thread leaves the
/// tray holding a menu that never opens again — and since changing the language is what triggers a
/// rebuild, the user is then locked out of the only place they could change it back. The tray's own
/// click handlers already run on the main thread, but the readings poller and the settings window
/// do not, so the hop is done here once rather than being remembered at every call site.
/// What the menu last showed, so an unchanged refresh leaves it alone.
static SHOWN: std::sync::Mutex<Option<(String, Vec<(String, String, bool)>)>> = std::sync::Mutex::new(None);

/// Swaps the menu only when a line of it would read differently. `set_menu` replaces the menu the
/// user may have open this moment — the refresh runs on the main thread, which the open popup's
/// message loop still serves — so the minute tick used to close it under the pointer even when
/// "Resets in 12 min" still said 12 min.
pub fn refresh_menu(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id("main") {
            let lang = language(&handle);
            let lines = menu_lines(&handle, &lang);
            let key = (lang.clone(), lines.clone());
            if SHOWN.lock().unwrap().as_ref() == Some(&key) {
                // A tooltip can change without a line changing, and setting it closes nothing.
                let _ = tray.set_tooltip(Some(&tooltip(&handle)));
                return;
            }
            match build_menu_from(&handle, &lang, &lines) {
                Ok(menu) => {
                    let _ = tray.set_menu(Some(menu));
                    let _ = tray.set_tooltip(Some(&tooltip(&handle)));
                    *SHOWN.lock().unwrap() = Some(key);
                }
                Err(e) => crate::applog(&format!("tray menu: {e}")),
            }
        }
    });
}

/// Provider rows carry `refresh:<id>`; everything else the menu offers is one of the four fixed
/// items. Settings, language, hooks and the rest arrive as commands from the settings window.
fn handle(app: &AppHandle, id: &str) {
    if let Some(provider) = id.strip_prefix("refresh:") {
        crate::refresh_provider(app, provider);
        return;
    }
    match id {
        "pin_top" => crate::pintop::toggle_mode(),
        "refresh" => crate::refresh_all(app),
        "settings" => crate::settings_window::open(app),
        "quit" => {
            // Before the process goes: a window left topmost with no frame around it cannot be
            // released from anywhere in this app again.
            crate::pintop::shutdown();
            app.exit(0);
        }
        _ => {}
    }
}
