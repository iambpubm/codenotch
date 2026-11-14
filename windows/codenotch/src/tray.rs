use crate::i18n::tr;
use crate::traymenu;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

/// The pin icon is a tray icon of its own, with its own id, because it is a mode rather than a
/// command: it has to sit there showing whether clicks are being captured.
const PIN_ID: &str = "pin";

/// The touch screen icon. Also its own id: it is the one switch that changes the machine rather
/// than Codenotch, and it has to be reachable without opening a menu.
const TOUCH_ID: &str = "touch";

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    // The three icons are about to get identities of their own (see `own_identity`), so the
    // windows that exist before any of them is built are the baseline the difference is taken
    // against.
    let mut claimed = existing_ids();
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
    own_identity(app, &mut claimed, "");
    setup_pin(app, &mut claimed)?;
    setup_touch(app, &mut claimed)?;
    Ok(())
}

/// The AppUserModelID a tray window carries, given the process's own id and the icon's suffix.
///
/// An empty suffix means "the application itself", which is what the main icon keeps: a tray host
/// that already remembers `com.immidi.codenotch` must go on meaning the same icon.
fn identity_of(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        base.to_string()
    } else {
        format!("{base}.{suffix}")
    }
}

/// Gives the tray window that was just created an AppUserModelID of its own.
///
/// Windows decides which application a tray icon belongs to from the AppUserModelID of the hidden
/// window behind it, and a process that never sets one leaves all three of its icons sharing the
/// application's single id. A tray host that groups by application — MyDockFinder does, and reads
/// the id with `SHGetPropertyStoreForWindow` — then cannot be asked to show two of the three:
/// switching the application off switches all of them off, and the icons it was told to keep
/// disappear again the next time it reads its settings. One id per window turns the three into
/// three separate entries, so the pin and the touch switch can be kept while the app's own icon
/// is dropped.
///
/// `claimed` is the set of tray windows seen so far: whatever is in the process after this call
/// but not before it is the window this icon just created. Taking the difference rather than an
/// index keeps it correct no matter what order the builder registers its windows in.
#[cfg(windows)]
fn own_identity(app: &AppHandle, claimed: &mut Vec<isize>, suffix: &str) {
    let base = app.config().identifier.clone();
    let aumid = identity_of(&base, suffix);
    for hwnd in tray_windows().into_iter().filter(|h| !claimed.contains(h)) {
        claimed.push(hwnd);
        if let Err(why) = set_aumid(hwnd, &aumid) {
            crate::applog(&format!("tray: could not name the {aumid} icon ({why})"));
        }
    }
}

#[cfg(not(windows))]
fn own_identity(_app: &AppHandle, _claimed: &mut Vec<isize>, _suffix: &str) {}

/// The tray windows that already exist, so the ones built afterwards can be told apart.
#[cfg(windows)]
fn existing_ids() -> Vec<isize> {
    tray_windows()
}

#[cfg(not(windows))]
fn existing_ids() -> Vec<isize> {
    Vec::new()
}

/// Every `tray_icon_app` window this process owns — one per tray icon, created by tray-icon.
#[cfg(windows)]
fn tray_windows() -> Vec<isize> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowThreadProcessId};

    // Body is an `unsafe fn`, so the calls below are already in an unsafe context: wrapping them
    // in `unsafe {}` again would only draw an `unused_unsafe` warning.
    unsafe extern "system" fn visit(hwnd: HWND, out: LPARAM) -> BOOL {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
        if pid != windows::Win32::System::Threading::GetCurrentProcessId() {
            return BOOL(1);
        }
        let mut name = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut name);
        if len > 0 && String::from_utf16_lossy(&name[..len as usize]) == "tray_icon_app" {
            let list = &mut *(out.0 as *mut Vec<isize>);
            list.push(hwnd.0 as isize);
        }
        BOOL(1)
    }

    let mut found: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

/// The property key `PKEY_AppUserModel_ID`. Spelled out rather than taken from the `windows`
/// crate's `EnhancedStorage` module, which would pull in a whole feature for one constant.
#[cfg(windows)]
const PKEY_APP_USER_MODEL_ID: windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY =
    windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };

/// Writes one AppUserModelID onto one window.
///
/// The value goes in as a `BSTR`; the shell converts it to the `VT_LPWSTR` the key is defined as,
/// which is what a read finds afterwards (measured, not assumed).
#[cfg(windows)]
fn set_aumid(hwnd: isize, aumid: &str) -> windows::core::Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};

    unsafe {
        let store: IPropertyStore =
            SHGetPropertyStoreForWindow(HWND(hwnd as *mut core::ffi::c_void))?;
        let value = windows::core::PROPVARIANT::from(aumid);
        store.SetValue(&PKEY_APP_USER_MODEL_ID, &value)?;
        store.Commit()?;
    }
    Ok(())
}

/// The always-on-top icon: left click arms the picker, right click opens the three things you can
/// do with it.
///
/// It is separate from the app's own icon on purpose. Pinning another application's window is the
/// one thing in Codenotch that reaches outside Codenotch, and it wants a target you can hit
/// without opening a menu and reading it.
fn setup_pin(app: &AppHandle, claimed: &mut Vec<isize>) -> tauri::Result<()> {
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
    own_identity(app, claimed, "pin");
    Ok(())
}

/// The handle the touch icon's click borrows. Tauri hands a tray event the tray, not the app, and
/// the switch needs the app to reach the config and the notch; one handle is enough for the whole
/// process, so it is kept here rather than threaded through a closure.
static TOUCH_APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

/// The touch screen icon: left click switches the glass off or back on, right click offers the same
/// switch plus a way into Settings.
///
/// Separate from the app's own icon because it is a switch, not a reading. The moment it is wanted
/// is the moment a palm on the glass is dragging the pointer about, and that is not a moment to open
/// a menu and read it first. The right-click menu names the switch ("Turn the touch screen off"), so
/// the icon does not have to be memorised.
fn setup_touch(app: &AppHandle, claimed: &mut Vec<isize>) -> tauri::Result<()> {
    let _ = TOUCH_APP.set(app.clone());
    let mut builder = TrayIconBuilder::with_id(TOUCH_ID);
    if let Some(icon) = crate::trayicon::touch_mark(crate::touch::read().disabled) {
        builder = builder.icon(icon);
    }
    builder
        .tooltip(touch_tooltip(app))
        .menu(&touch_menu(app)?)
        // A left click switches the glass; it must not open a menu first.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "touch_toggle" => switch_touch(app),
            "settings" => crate::settings_window::open(app),
            _ => {}
        })
        .on_tray_icon_event(|_tray, event| {
            // Up, not Down: Windows reports both, and switching on each would turn the glass off
            // and straight back on in the same press.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(app) = TOUCH_APP.get() {
                    switch_touch(app);
                }
            }
        })
        .build(app)?;
    own_identity(app, claimed, "touch");
    Ok(())
}

/// Flips the glass on a thread of its own.
///
/// `touch::set_enabled` hands the write to an elevated process and then waits up to five seconds for
/// the device tree to agree -- and on the very first switch that wait includes a consent dialog. A
/// tray callback that blocks for that long freezes the taskbar, so the click only starts the thread.
fn switch_touch(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        crate::applog("touch: switching from the tray");
        let before = crate::touch::read();
        if before.found == 0 {
            // Nothing to switch. Refreshing anyway keeps the tooltip from claiming otherwise.
            refresh_touch_tray(&handle);
            return;
        }
        match crate::touch::set_enabled(before.disabled) {
            Ok(state) => {
                crate::applog(&format!(
                    "touch: switched from the tray (found {}, disabled {})",
                    state.found, state.disabled
                ));
                // The notch draws the same switch. It polls every two seconds, but the page should
                // not be left showing the old state right after the click that changed it.
                let _ = handle.emit("touch_state", crate::touch::read());
            }
            Err(why) => crate::applog(&format!("touch: the tray switch failed ({why})")),
        }
        refresh_touch_tray(&handle);
    });
}

/// The menu behind a right click on the touch icon: the switch, by name, and the way into Settings.
fn touch_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let lang = language(app);
    let state = crate::touch::read();
    let label = if state.disabled { "touch_do_on" } else { "touch_do_off" };
    let switch = MenuItemBuilder::with_id("touch_toggle", tr(&lang, label))
        .enabled(state.found > 0)
        .build(app)?;
    let settings = MenuItemBuilder::with_id("settings", tr(&lang, "settings")).build(app)?;
    MenuBuilder::new(app)
        .item(&switch)
        .separator()
        .item(&settings)
        .build()
}

/// What the pointer finds on the touch icon: whether the glass takes taps, and what a click does.
fn touch_tooltip(app: &AppHandle) -> String {
    let lang = language(app);
    let state = crate::touch::read();
    if state.found == 0 {
        return tr(&lang, "touch_tip_absent").to_string();
    }
    tr(&lang, if state.disabled { "touch_tip_off" } else { "touch_tip_on" }).to_string()
}

/// Puts the icon, the tooltip and the menu back in step with the device tree. Called from the
/// switching thread, so the hop to the main thread happens here, where a menu is safe to build.
pub fn refresh_touch_tray(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = handle.tray_by_id(TOUCH_ID) else { return };
        let state = crate::touch::read();
        // The icon is the state: the greyed tile is the whole point of the tray entry, so it swaps
        // with the reading, not just the tooltip.
        if let Some(icon) = crate::trayicon::touch_mark(state.disabled) {
            let _ = tray.set_icon(Some(icon));
        }
        if let Ok(menu) = touch_menu(&handle) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(&touch_tooltip(&handle)));
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The three icons have to end up as three different applications, or a tray host can only
    /// switch them together — which is the bug this exists to fix.
    #[test]
    fn each_icon_gets_its_own_identity() {
        let base = "com.immidi.codenotch";
        assert_eq!(identity_of(base, ""), base, "the main icon keeps the application's own id");
        assert_eq!(identity_of(base, "pin"), "com.immidi.codenotch.pin");
        assert_eq!(identity_of(base, "touch"), "com.immidi.codenotch.touch");
        // Distinct, which is the whole point: a host that groups by id must see three of them.
        let all = [identity_of(base, ""), identity_of(base, "pin"), identity_of(base, "touch")];
        assert_eq!(all.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    }

    /// The key is written out by hand so the `windows` crate's `EnhancedStorage` feature is not
    /// dragged in for one constant. A wrong pid would quietly write to another property, and the
    /// write would still report success — so it is pinned against the crate's own definition.
    #[cfg(windows)]
    #[test]
    fn the_property_key_is_pkey_appusermodel_id() {
        assert_eq!(PKEY_APP_USER_MODEL_ID.pid, 5);
        assert_eq!(
            PKEY_APP_USER_MODEL_ID.fmtid,
            windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3)
        );
    }
}
