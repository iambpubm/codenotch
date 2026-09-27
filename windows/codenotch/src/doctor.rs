//! `codenotch.exe doctor` — self-diagnosis: look instead of guessing.
//! Checks the config, every provider's credential source, the provider glyphs and the working-state
//! probe, and writes to stdout plus %APPDATA%\codenotch\doctor.log.
//!
//! The Claude Code hook pipeline (and the local HTTP server and transcript watcher that fed it) is
//! gone, so there is no port to check and no watch root to scan any more.

pub fn run() -> String {
    let mut o = String::new();
    o += &format!("== Codenotch doctor v{} ==\n", env!("CARGO_PKG_VERSION"));

    let cfg = crate::config::load();
    o += &format!(
        "config: lang={} ({})\n",
        cfg.lang,
        crate::config::config_path().display()
    );
    o += &format!("  notch: edge={} scale={} monitor={:?}\n", cfg.notch_edge, cfg.scale, cfg.notch_monitor);
    o += &format!(
        "  visible: notch={} on_hover={} tray={}\n",
        cfg.notch_visible, cfg.notch_on_hover, cfg.tray_visible
    );
    let slots: Vec<&str> = cfg.notch_slots.iter().map(|s| s.provider.as_str()).collect();
    o += &format!(
        "  providers: {} (empty = every provider)\n",
        if slots.is_empty() { "(all)".into() } else { slots.join(", ") }
    );

    o += "\nusage sources:\n";
    for line in [
        crate::workbuddy::probe(),
        crate::workbuddy_tokens::probe(),
        crate::codex::probe(),
        crate::cursor::probe(),
        crate::dsh::probe(),
        crate::glm::probe(),
        crate::opencode::probe(),
        crate::antigravity::probe(),
    ] {
        o += &format!("  {line}\n");
    }

    o += &format!("\nprovider glyphs:\n{}\n", crate::glyphs::probe());
    o += &format!("\nworking state:\n  {}\n", crate::activity::probe());

    o += "\nper-provider snapshot files (each provider owns its own):\n";
    if let Some(dir) = dirs::config_dir() {
        let dir = dir.join("codenotch");
        for name in [
            "workbuddy.json",
            "workbuddy-tokens-cache.json",
            "codex.json",
            "cursor.json",
            "dsh.json",
            "dsh-cache.json",
            "glm.json",
            "opencode.json",
            "antigravity.json",
        ] {
            let p = dir.join(name);
            match std::fs::metadata(&p) {
                Ok(m) => o += &format!("  {name}: {} bytes\n", m.len()),
                Err(_) => o += &format!("  {name}: (none yet)\n"),
            }
        }
    } else {
        o += "  (no config directory)\n";
    }

    o += "\nrun.log (the most recent lines, if any):\n";
    if let Some(dir) = dirs::config_dir() {
        let p = dir.join("codenotch").join("run.log");
        match std::fs::read_to_string(&p) {
            Ok(t) if !t.trim().is_empty() => {
                for line in t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                    o += &format!("  {line}\n");
                }
            }
            _ => o += "  (empty — the app has not run yet, which is normal on first use)\n",
        }
    }
    o
}
