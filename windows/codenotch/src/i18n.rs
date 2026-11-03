//! Rust-side (tray menu) strings. The page has its own dictionary; keys are kept identical on both sides.

pub fn resolve_auto() -> &'static str {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Globalization::GetUserDefaultLocaleName;
        let mut buf = [0u16; 85];
        let n = GetUserDefaultLocaleName(&mut buf);
        if n > 0 {
            let name = String::from_utf16_lossy(&buf[..(n as usize - 1)]).to_lowercase();
            if let Some(lang) = language_from_windows_locale(&name) {
                return lang;
            }
        }
    }
    "en"
}

/// Map a Windows locale name onto a language code this crate ships.
///
/// Traditional regions must be matched before the leftover `zh*` fallback —
/// `starts_with("zh")` used to send zh-TW/zh-HK to Simplified.
fn language_from_windows_locale(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name.starts_with("zh-tw")
        || name.starts_with("zh-hant")
        || name.starts_with("zh-hk")
        || name.starts_with("zh-mo")
    {
        return Some("zh-Hant");
    }
    if name.starts_with("zh-cn")
        || name.starts_with("zh-hans")
        || name.starts_with("zh-sg")
        || name.starts_with("zh")
    {
        return Some("zh");
    }
    if name.starts_with("ja") {
        return Some("ja");
    }
    if name.starts_with("ko") {
        return Some("ko");
    }
    if name.starts_with("pt-br") {
        return Some("pt-BR");
    }
    if name.starts_with("ru") {
        return Some("ru");
    }
    if name.starts_with("uk") {
        return Some("uk");
    }
    None
}

/// Whether the region settings write times on a 24-hour clock. The page can't tell: WebView2's
/// locale follows the browser language, not the regional format.
pub fn clock_24h() -> bool {
    time_format().is_some_and(|pattern| is_24h_pattern(&pattern))
}

fn time_format() -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SSHORTTIME, LOCALE_STIMEFORMAT};
        // The taskbar clock shows the short time, or the long time once it shows seconds; the two are set separately
        let kind = if taskbar_shows_seconds() { LOCALE_STIMEFORMAT } else { LOCALE_SSHORTTIME };
        let mut buf = [0u16; 80];
        let n = GetLocaleInfoEx(PCWSTR::null(), kind, Some(&mut buf));
        if n > 0 {
            return Some(String::from_utf16_lossy(&buf[..(n as usize - 1)]));
        }
    }
    None
}

#[cfg(windows)]
fn taskbar_shows_seconds() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced"),
            w!("ShowSecondsInSystemClock"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    status.is_ok() && value != 0
}

/// "HH:mm" against "hh:mm tt"; text between single quotes is literal.
fn is_24h_pattern(pattern: &str) -> bool {
    pattern.split('\'').step_by(2).any(|part| part.contains('H'))
}

pub fn tr(lang: &str, key: &str) -> &'static str {
    let l = if lang == "auto" { resolve_auto() } else { lang };
    match (l, key) {
        ("pt-BR", "open_data") => "Abrir pasta de dados (logs / ícones)",
        ("pt-BR", "language") => "Idioma",
        ("pt-BR", "lang_auto") => "Seguir o sistema",
        ("pt-BR", "reset_pos") => "Redefinir posição da barra",
        ("pt-BR", "quit") => "Sair",
        ("pt-BR", "autostart") => "Iniciar com o Windows",
        ("pt-BR", "refresh_all") => "Atualizar tudo",
        ("pt-BR", "waiting") => "Aguardando a primeira leitura…",
        ("pt-BR", "quit_app") => "Encerrar o Codenotch",
        ("pt-BR", "settings") => "Ajustes…",
        ("pt-BR", "refresh_now") => "Atualizar agora",
        ("pt-BR", "open_host") => "Abrir o %@",
        ("pt-BR", "keep_open") => "Manter aberto",
        ("zh", "language") => "语言",
        ("zh-Hant", "language") => "語言",
        ("zh", "lang_auto") => "跟随系统",
        ("zh-Hant", "lang_auto") => "跟隨系統",
        ("zh", "reset_pos") => "重置悬浮条位置",
        ("zh-Hant", "reset_pos") => "重置懸浮列位置",
        ("zh", "quit") => "退出",
        ("zh-Hant", "quit") => "結束",
        ("zh", "autostart") => "开机自启（静默待命）",
        ("zh-Hant", "autostart") => "開機自動啟動（靜默待命）",
        ("ja", "autostart") => "Windows起動時に自動開始",
        ("ko", "autostart") => "Windows 시작 시 자동 실행",
        ("zh", "refresh_all") => "全部刷新",
        ("zh-Hant", "refresh_all") => "全部重新整理",
        ("zh", "open_data") => "打开数据文件夹（日志 / 图标）",
        ("zh-Hant", "open_data") => "開啟資料資料夾（日誌 / 圖示）",
        ("ja", "open_data") => "データフォルダを開く（ログ / アイコン）",
        ("ko", "open_data") => "데이터 폴더 열기 (로그 / 아이콘)",
        ("ru", "open_data") => "Открыть папку данных (журналы / значки)",
        ("uk", "open_data") => "Відкрити теку даних (журнали / значки)",
        (_, "open_data") => "Open data folder (logs / icons)",
        ("ja", "refresh_all") => "すべて更新",
        ("ko", "refresh_all") => "모두 새로 고침",
        ("ja", "language") => "言語",
        ("ja", "lang_auto") => "システムに従う",
        ("ja", "reset_pos") => "バー位置をリセット",
        ("ja", "quit") => "終了",
        ("ko", "language") => "언어",
        ("ko", "lang_auto") => "시스템 따르기",
        ("ko", "reset_pos") => "바 위치 초기화",
        ("ko", "quit") => "종료",
        ("ru", "language") => "Язык",
        ("uk", "language") => "Мова",
        ("ru", "lang_auto") => "Как в системе",
        ("uk", "lang_auto") => "Як у системі",
        ("ru", "reset_pos") => "Сбросить положение панели",
        ("uk", "reset_pos") => "Скинути положення панелі",
        ("ru", "quit") => "Выйти",
        ("uk", "quit") => "Вийти",
        ("ru", "autostart") => "Запускать с Windows (в фоне)",
        ("uk", "autostart") => "Запускати разом із Windows (у фоні)",
        ("ru", "refresh_all") => "Обновить всё",
        ("uk", "refresh_all") => "Оновити все",
        (_, "language") => "Language",
        (_, "lang_auto") => "Follow system",
        (_, "reset_pos") => "Reset bar position",
        (_, "quit") => "Quit",
        (_, "autostart") => "Start with Windows (silent)",
        (_, "refresh_all") => "Refresh all",

        ("zh", "waiting") => "正在等待首次读数…",
        ("zh-Hant", "waiting") => "正在等待首次讀數…",
        ("ja", "waiting") => "最初の読み取りを待っています…",
        ("ko", "waiting") => "첫 측정값을 기다리는 중…",
        ("ru", "waiting") => "Ожидание первых данных…",
        ("uk", "waiting") => "Очікування першого показника…",
        (_, "waiting") => "Waiting for the first reading…",

        ("zh", "quit_app") => "退出 Codenotch",
        ("zh-Hant", "quit_app") => "結束 Codenotch",
        ("ja", "quit_app") => "Codenotch を終了",
        ("ko", "quit_app") => "Codenotch 종료",
        ("ru", "quit_app") => "Выйти из Codenotch",
        ("uk", "quit_app") => "Вийти з Codenotch",
        (_, "quit_app") => "Quit Codenotch",
        ("zh", "settings") => "设置…",
        ("zh-Hant", "settings") => "設定…",
        ("ja", "settings") => "設定…",
        ("ko", "settings") => "설정…",
        ("ru", "settings") => "Настройки…",
        ("uk", "settings") => "Налаштування…",
        (_, "settings") => "Settings…",
        ("zh", "refresh_now") => "立即刷新",
        ("zh-Hant", "refresh_now") => "立即重新整理",
        ("ja", "refresh_now") => "今すぐ更新",
        ("ru", "refresh_now") => "Обновить сейчас",
        ("uk", "refresh_now") => "Оновити зараз",
        ("ko", "refresh_now") => "지금 새로 고침",
        (_, "refresh_now") => "Refresh now",
        ("zh", "open_host") => "打开 %@",
        ("zh-Hant", "open_host") => "開啟 %@",
        ("ja", "open_host") => "%@ を開く",
        ("ru", "open_host") => "Открыть %@",
        ("uk", "open_host") => "Відкрити %@",
        ("ko", "open_host") => "%@ 열기",
        (_, "open_host") => "Open %@",
        ("zh", "keep_open") => "保持展开",
        ("zh-Hant", "keep_open") => "保持展開",
        ("ja", "keep_open") => "開いたままにする",
        ("ko", "keep_open") => "열어 두기",
        ("ru", "keep_open") => "Оставить открытым",
        ("uk", "keep_open") => "Тримати відкритим",
        (_, "keep_open") => "Keep open",
        // The ellipsis is the only hint that this item starts something rather than doing it at
        // once, so it stays in every translation.
        ("zh", "pin_top") => "窗口置顶…（再点一下目标窗口）",
        ("zh-Hant", "pin_top") => "視窗置頂…（再點一下目標視窗）",
        ("ja", "pin_top") => "ウィンドウを最前面に固定…（次に対象をクリック）",
        ("ko", "pin_top") => "창을 항상 위에 고정…(대상 창 클릭)",
        ("ru", "pin_top") => "Поверх всех окон… (затем щёлкните окно)",
        ("uk", "pin_top") => "Поверх усіх вікон… (потім клацніть вікно)",
        ("pt-BR", "pin_top") => "Fixar janela no topo… (clique na janela)",
        (_, "pin_top") => "Keep a window on top… (then click it)",
        // The pin tray icon and the three-line menu behind its right click. The idle and armed
        // tooltips are what tell the user, without opening anything, whether clicks are still
        // being captured — the mode has no other visible home.
        ("zh", "pin_tray") => "窗口置顶",
        ("zh-Hant", "pin_tray") => "視窗置頂",
        ("ja", "pin_tray") => "ウィンドウを最前面に固定",
        ("ko", "pin_tray") => "창 항상 위",
        ("ru", "pin_tray") => "Поверх всех окон",
        ("uk", "pin_tray") => "Поверх усіх вікон",
        ("pt-BR", "pin_tray") => "Fixar no topo",
        (_, "pin_tray") => "Always on top",
        ("zh", "pin_start") => "开始置顶窗口…",
        ("zh-Hant", "pin_start") => "開始置頂視窗…",
        ("ja", "pin_start") => "ウィンドウを固定…",
        ("ko", "pin_start") => "창 고정…",
        ("ru", "pin_start") => "Закрепить окна…",
        ("uk", "pin_start") => "Закріпити вікна…",
        ("pt-BR", "pin_start") => "Fixar janelas…",
        (_, "pin_start") => "Pin windows…",
        ("zh", "pin_stop") => "结束置顶",
        ("zh-Hant", "pin_stop") => "結束置頂",
        ("ja", "pin_stop") => "固定を終了",
        ("ko", "pin_stop") => "고정 종료",
        ("ru", "pin_stop") => "Прекратить закрепление",
        ("uk", "pin_stop") => "Припинити закріплення",
        ("pt-BR", "pin_stop") => "Parar de fixar",
        (_, "pin_stop") => "Stop pinning",
        ("zh", "pin_clear") => "取消全部置顶",
        ("zh-Hant", "pin_clear") => "取消全部置頂",
        ("ja", "pin_clear") => "すべての固定を解除",
        ("ko", "pin_clear") => "모든 고정 해제",
        ("ru", "pin_clear") => "Открепить все окна",
        ("uk", "pin_clear") => "Відкріпити всі вікна",
        ("pt-BR", "pin_clear") => "Soltar todas as janelas",
        (_, "pin_clear") => "Unpin all windows",
        ("zh", "pin_tip_armed") => "点一个窗口即置顶 · 再点已置顶的窗口可取消",
        ("zh-Hant", "pin_tip_armed") => "點一個視窗即置頂 · 再點已置頂的視窗可取消",
        ("ja", "pin_tip_armed") => "クリックで固定 · 固定済みをクリックで解除",
        ("ko", "pin_tip_armed") => "클릭하면 고정 · 고정된 창을 클릭하면 해제",
        ("ru", "pin_tip_armed") => "Щёлкните окно, чтобы закрепить · ещё раз — чтобы открепить",
        ("uk", "pin_tip_armed") => "Клацніть вікно, щоб закріпити · ще раз — щоб відкріпити",
        ("pt-BR", "pin_tip_armed") => "Clique numa janela para fixar · de novo para soltar",
        (_, "pin_tip_armed") => "Click a window to pin it · click a pinned one to unpin",
        ("zh", "pin_tip_idle") => "窗口置顶 · 点一下开始",
        ("zh-Hant", "pin_tip_idle") => "視窗置頂 · 點一下開始",
        ("ja", "pin_tip_idle") => "常に最前面 · クリックで開始",
        ("ko", "pin_tip_idle") => "항상 위 · 클릭하여 시작",
        ("ru", "pin_tip_idle") => "Поверх всех окон · щёлкните, чтобы начать",
        ("uk", "pin_tip_idle") => "Поверх усіх вікон · клацніть, щоб почати",
        ("pt-BR", "pin_tip_idle") => "Fixar no topo · clique para começar",
        (_, "pin_tip_idle") => "Always on top · click to start",
        ("zh", "touch_do_off") => "关掉触摸屏",
        ("zh-Hant", "touch_do_off") => "關掉觸控螢幕",
        ("ja", "touch_do_off") => "タッチスクリーンをオフにする",
        ("ko", "touch_do_off") => "터치스크린 끄기",
        ("ru", "touch_do_off") => "Выключить сенсорный экран",
        ("uk", "touch_do_off") => "Вимкнути сенсорний екран",
        ("pt-BR", "touch_do_off") => "Desligar a tela de toque",
        (_, "touch_do_off") => "Turn the touch screen off",
        ("zh", "touch_do_on") => "打开触摸屏",
        ("zh-Hant", "touch_do_on") => "打開觸控螢幕",
        ("ja", "touch_do_on") => "タッチスクリーンをオンにする",
        ("ko", "touch_do_on") => "터치스크린 켜기",
        ("ru", "touch_do_on") => "Включить сенсорный экран",
        ("uk", "touch_do_on") => "Увімкнути сенсорний екран",
        ("pt-BR", "touch_do_on") => "Ligar a tela de toque",
        (_, "touch_do_on") => "Turn the touch screen on",
        ("zh", "touch_tip_on") => "触摸屏已开启 · 点一下关掉",
        ("zh-Hant", "touch_tip_on") => "觸控螢幕已開啟 · 點一下關掉",
        ("ja", "touch_tip_on") => "タッチスクリーンはオン · クリックでオフ",
        ("ko", "touch_tip_on") => "터치스크린 켜짐 · 클릭하면 끕니다",
        ("ru", "touch_tip_on") => "Сенсорный экран включён · щёлкните, чтобы выключить",
        ("uk", "touch_tip_on") => "Сенсорний екран увімкнено · клацніть, щоб вимкнути",
        ("pt-BR", "touch_tip_on") => "Tela de toque ligada · clique para desligar",
        (_, "touch_tip_on") => "Touch screen is on · click to turn it off",
        ("zh", "touch_tip_off") => "触摸屏已关闭 · 点一下打开",
        ("zh-Hant", "touch_tip_off") => "觸控螢幕已關閉 · 點一下打開",
        ("ja", "touch_tip_off") => "タッチスクリーンはオフ · クリックでオン",
        ("ko", "touch_tip_off") => "터치스크린 꺼짐 · 클릭하면 켭니다",
        ("ru", "touch_tip_off") => "Сенсорный экран выключен · щёлкните, чтобы включить",
        ("uk", "touch_tip_off") => "Сенсорний екран вимкнено · клацніть, щоб увімкнути",
        ("pt-BR", "touch_tip_off") => "Tela de toque desligada · clique para ligar",
        (_, "touch_tip_off") => "Touch screen is off · click to turn it on",
        ("zh", "touch_tip_absent") => "这台电脑上没有触摸屏",
        ("zh-Hant", "touch_tip_absent") => "這台電腦上沒有觸控螢幕",
        ("ja", "touch_tip_absent") => "この PC にタッチスクリーンはありません",
        ("ko", "touch_tip_absent") => "이 PC에는 터치스크린이 없습니다",
        ("ru", "touch_tip_absent") => "На этом компьютере нет сенсорного экрана",
        ("uk", "touch_tip_absent") => "На цьому комп'ютері немає сенсорного екрана",
        ("pt-BR", "touch_tip_absent") => "Nenhuma tela de toque neste PC",
        (_, "touch_tip_absent") => "No touch screen on this PC",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::tr;

    const RUSSIAN_KEYS: &[(&str, &str)] = &[
        ("settings", "Настройки…"),
        ("refresh_all", "Обновить всё"),
        ("waiting", "Ожидание первых данных…"),
        ("quit_app", "Выйти из Codenotch"),
        ("refresh_now", "Обновить сейчас"),
        ("open_host", "Открыть %@"),
        ("keep_open", "Оставить открытым"),
        ("quit", "Выйти"),
        ("language", "Язык"),
        ("lang_auto", "Как в системе"),
        ("reset_pos", "Сбросить положение панели"),
        ("autostart", "Запускать с Windows (в фоне)"),
        ("open_data", "Открыть папку данных (журналы / значки)"),
    ];

    #[test]
    fn russian_translates_every_known_key() {
        for (key, value) in RUSSIAN_KEYS {
            assert_eq!(
                tr("ru", key),
                *value,
                "missing Russian translation for {key}"
            );
            assert_ne!(tr("ru", key), "?", "unknown Russian key {key}");
        }
    }

    const UKRAINIAN_KEYS: &[(&str, &str)] = &[
        ("keep_open", "Тримати відкритим"),
        ("open_data", "Відкрити теку даних (журнали / значки)"),
        ("language", "Мова"),
        ("lang_auto", "Як у системі"),
        ("reset_pos", "Скинути положення панелі"),
        ("quit", "Вийти"),
        ("autostart", "Запускати разом із Windows (у фоні)"),
        ("refresh_all", "Оновити все"),
        ("waiting", "Очікування першого показника…"),
        ("quit_app", "Вийти з Codenotch"),
        ("settings", "Налаштування…"),
        ("refresh_now", "Оновити зараз"),
        ("open_host", "Відкрити %@"),
    ];

    #[test]
    fn ukrainian_translates_every_known_key() {
        for (key, value) in UKRAINIAN_KEYS {
            assert_eq!(
                tr("uk", key),
                *value,
                "missing Ukrainian translation for {key}"
            );
            assert_ne!(tr("uk", key), "?", "unknown Ukrainian key {key}");
        }
    }

    const TRADITIONAL_CHINESE_KEYS: &[(&str, &str)] = &[
        ("settings", "設定…"),
        ("refresh_all", "全部重新整理"),
        ("waiting", "正在等待首次讀數…"),
        ("quit_app", "結束 Codenotch"),
        ("refresh_now", "立即重新整理"),
        ("open_host", "開啟 %@"),
        ("quit", "結束"),
        ("language", "語言"),
        ("lang_auto", "跟隨系統"),
        ("reset_pos", "重置懸浮列位置"),
        ("autostart", "開機自動啟動（靜默待命）"),
        ("open_data", "開啟資料資料夾（日誌 / 圖示）"),
    ];

    #[test]
    fn traditional_chinese_translates_every_known_key() {
        for (key, value) in TRADITIONAL_CHINESE_KEYS {
            assert_eq!(
                tr("zh-Hant", key),
                *value,
                "missing Traditional Chinese translation for {key}"
            );
            assert_ne!(tr("zh-Hant", key), "?", "unknown Traditional Chinese key {key}");
        }
    }

    #[test]
    fn traditional_chinese_locales_resolve_apart_from_simplified() {
        use super::language_from_windows_locale;
        assert_eq!(language_from_windows_locale("zh-TW"), Some("zh-Hant"));
        assert_eq!(language_from_windows_locale("zh-Hant"), Some("zh-Hant"));
        assert_eq!(language_from_windows_locale("zh-Hant-TW"), Some("zh-Hant"));
        assert_eq!(language_from_windows_locale("zh-HK"), Some("zh-Hant"));
        assert_eq!(language_from_windows_locale("zh-MO"), Some("zh-Hant"));
        assert_eq!(language_from_windows_locale("zh-CN"), Some("zh"));
        assert_eq!(language_from_windows_locale("zh-Hans"), Some("zh"));
        assert_eq!(language_from_windows_locale("zh-Hans-CN"), Some("zh"));
        assert_eq!(language_from_windows_locale("zh-SG"), Some("zh"));
        assert_eq!(language_from_windows_locale("zh"), Some("zh"));
    }

    const BRAZILIAN_PORTUGUESE_KEYS: &[(&str, &str)] = &[
        ("settings", "Ajustes…"),
        ("refresh_all", "Atualizar tudo"),
        ("waiting", "Aguardando a primeira leitura…"),
        ("quit_app", "Encerrar o Codenotch"),
        ("refresh_now", "Atualizar agora"),
        ("open_host", "Abrir o %@"),
        ("keep_open", "Manter aberto"),
        ("quit", "Sair"),
        ("language", "Idioma"),
        ("lang_auto", "Seguir o sistema"),
        ("reset_pos", "Redefinir posição da barra"),
        ("autostart", "Iniciar com o Windows"),
        ("open_data", "Abrir pasta de dados (logs / ícones)"),
    ];

    #[test]
    fn brazilian_portuguese_translates_every_known_key() {
        for (key, value) in BRAZILIAN_PORTUGUESE_KEYS {
            assert_eq!(tr("pt-BR", key), *value, "missing Brazilian Portuguese translation for {key}");
            assert_ne!(tr("pt-BR", key), "?", "unknown Brazilian Portuguese key {key}");
        }
    }

    #[test]
    fn brazilian_portuguese_locale_resolves() {
        use super::language_from_windows_locale;
        assert_eq!(language_from_windows_locale("pt-BR"), Some("pt-BR"));
        assert_eq!(language_from_windows_locale("pt-br"), Some("pt-BR"));
        assert_eq!(language_from_windows_locale("pt-PT"), None);
    }

    #[test]
    fn korean_locale_and_tray_copy() {
        assert_eq!(super::language_from_windows_locale("ko-KR"), Some("ko"));
        assert_eq!(super::language_from_windows_locale("ko"), Some("ko"));
        for (key, expected) in [
            ("settings", "설정…"),
            ("refresh_all", "모두 새로 고침"),
            ("refresh_now", "지금 새로 고침"),
            ("open_host", "%@ 열기"),
            ("waiting", "첫 측정값을 기다리는 중…"),
            ("keep_open", "열어 두기"),
        ] {
            assert_eq!(tr("ko", key), expected);
        }
    }

    #[test]
    fn unknown_language_keeps_the_english_fallback() {
        assert_eq!(tr("xx", "settings"), "Settings…");
    }

    #[test]
    fn the_hour_symbol_outside_quotes_decides_the_clock() {
        assert!(super::is_24h_pattern("HH:mm"));
        assert!(super::is_24h_pattern("H:mm"));
        assert!(super::is_24h_pattern("HH' h 'mm"));
        assert!(!super::is_24h_pattern("hh:mm tt"));
        assert!(!super::is_24h_pattern("tt hh:mm"));
        assert!(!super::is_24h_pattern("h:mm 'Hrs'"));
    }
}
