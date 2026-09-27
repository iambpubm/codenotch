//! `codenotch.exe doctor deep`: deep diagnostics for finding "is it working?" signals.
//! Prints only structure, times and scalar types/lengths — never a scalar value itself, and never
//! a process command line, so no token or conversation content ever appears (#160).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mtime_ms(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

/// Files modified within the last `within_s` seconds (depth-limited), sorted newest first
fn recent_files(root: &Path, depth: usize, within_s: u64, out: &mut Vec<(u64, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let now = now_ms();
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if p.is_dir() {
            if depth > 0 && !name.starts_with("node_modules") && name != "Cache" && name != "Code Cache" && name != "GPUCache" {
                recent_files(&p, depth - 1, within_s, out);
            }
            continue;
        }
        if let Some(m) = mtime_ms(&p) {
            let age = now.saturating_sub(m) / 1000;
            if age <= within_s {
                out.push((age, p));
            }
        }
    }
}

/// One SQLite value as type and size only, never the value: even a short text can hold a token or
/// id, so no length threshold may reveal one (#160).
fn short(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value::*;
    match v {
        Null => "<null>".into(),
        Integer(_) => "<integer>".into(),
        Real(_) => "<real>".into(),
        Text(t) => format!("<text {} chars>", t.len()),
        Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

/// Structure of one SQLite database plus, per table, the newest row by a time-like column (each
/// value reduced to its type/length; column names are kept)
fn dump_sqlite(path: &Path) -> String {
    use rusqlite::OpenFlags;
    let mut o = format!("--- {} ({}, modified {}s ago)\n", path.display(), if path.is_file() { "present" } else { "missing" }, now_ms().saturating_sub(mtime_ms(path).unwrap_or(0)) / 1000);
    if !path.is_file() {
        return o;
    }
    let conn = match rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
        Ok(c) => c,
        Err(e) => {
            o += &format!("  cannot open: {e}\n");
            return o;
        }
    };
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    for t in tables.iter().take(25) {
        let cols: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info(\"{t}\")"))
            .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0)).unwrap_or(-1);
        o += &format!("  table {t} ({count} rows): {}\n", cols.join(", "));
        // Time-like columns: updated/created/_at/time/recency
        let timeish: Vec<&String> = cols
            .iter()
            .filter(|c| {
                let l = c.to_lowercase();
                l.contains("updated") || l.contains("created") || l.ends_with("_at") || l.contains("time") || l.contains("recency") || l.contains("modified")
            })
            .collect();
        if let Some(tc) = timeish.first() {
            let sql = format!("SELECT * FROM \"{t}\" ORDER BY \"{tc}\" DESC LIMIT 1");
            if let Ok(mut s) = conn.prepare(&sql) {
                let n = s.column_count();
                if let Ok(mut rows) = s.query([]) {
                    if let Ok(Some(row)) = rows.next() {
                        let mut parts = Vec::new();
                        for i in 0..n {
                            let v: rusqlite::types::Value = row.get(i).unwrap_or(rusqlite::types::Value::Null);
                            parts.push(format!("{}={}", cols.get(i).cloned().unwrap_or_default(), short(&v)));
                        }
                        o += &format!("    newest row (by {tc}): {}\n", parts.join(" | "));
                    }
                }
            }
        }
    }
    o
}

/// JSON file: prints dotted keys with each value reduced to its type (and length for strings/
/// containers) — never a scalar value itself, so a short token cannot slip through (#160)
fn dump_json_scalars(path: &Path) -> String {
    let mut o = format!("--- {} (modified {}s ago)\n", path.display(), now_ms().saturating_sub(mtime_ms(path).unwrap_or(0)) / 1000);
    let Ok(t) = std::fs::read_to_string(path) else {
        o += "  unreadable\n";
        return o;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
        o += "  not JSON\n";
        return o;
    };
    fn walk(v: &serde_json::Value, prefix: &str, depth: usize, o: &mut String) {
        if let Some(obj) = v.as_object() {
            for (k, x) in obj.iter().take(60) {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match x {
                    serde_json::Value::Object(_) if depth < 2 => walk(x, &key, depth + 1, o),
                    serde_json::Value::Object(m) => o.push_str(&format!("  {key}: <object {} keys>\n", m.len())),
                    serde_json::Value::Array(a) => o.push_str(&format!("  {key}: <array {}>\n", a.len())),
                    serde_json::Value::String(s) => o.push_str(&format!("  {key}: <string {} chars>\n", s.len())),
                    serde_json::Value::Number(_) => o.push_str(&format!("  {key}: <number>\n")),
                    serde_json::Value::Bool(_) => o.push_str(&format!("  {key}: <bool>\n")),
                    serde_json::Value::Null => o.push_str(&format!("  {key}: <null>\n")),
                }
            }
        }
    }
    walk(&v, "", 0, &mut o);
    o
}

/// The PowerShell process query: a WQL projection of PID and process name only, so `CommandLine`
/// is never even fetched — it can carry tokens, secrets and file paths, and #160 keeps it out of
/// diagnostics entirely rather than truncating it (`-Filter` alone would still materialize it).
#[cfg_attr(not(windows), allow(dead_code))]
fn process_query() -> &'static str {
    "Get-CimInstance -Query \"SELECT ProcessId, Name FROM Win32_Process WHERE Name LIKE 'codex%' OR Name LIKE 'ChatGPT%'\" | ForEach-Object { \"$($_.ProcessId)`t$($_.Name)\" }"
}

/// One `PID<TAB>Name` line of the query output as `PID  Name`. Only those two fields are kept:
/// anything past the name's tab (a command line, should the query ever widen again) is dropped
/// here too, so arguments cannot reach the report even by accident.
#[cfg_attr(not(windows), allow(dead_code))]
fn format_process_line(line: &str) -> Option<String> {
    let mut fields = line.split('\t');
    let pid = fields.next()?.trim();
    let name = fields.next()?.trim();
    if pid.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{pid}  {name}"))
}

pub fn run() -> String {
    let mut o = String::from("== doctor deep: working-state signal survey ==\n(run it while the Codex desktop app is working)\n\n");
    let home = dirs::home_dir().unwrap_or_default();
    let local = dirs::data_local_dir().unwrap_or_default();

    o += "## Files modified in the last 120 s\n";
    let mut recent = Vec::new();
    recent_files(&home.join(".codex"), 2, 120, &mut recent);
    // WorkBuddy's session file and its own data directory: what the WorkBuddy reading is built from
    for root in [local.clone(), dirs::data_dir().unwrap_or_default()] {
        recent_files(&root.join("CodeBuddyExtension").join("Data"), 3, 120, &mut recent);
    }
    recent_files(&dirs::config_dir().unwrap_or_default().join("Cursor").join("User").join("globalStorage"), 1, 120, &mut recent);
    // DeepSeek Harness keeps its transcripts under its own home
    if let Some(dsh) = crate::dsh::home() {
        recent_files(&dsh, 3, 120, &mut recent);
    }
    recent.sort();
    for (age, p) in recent.iter().take(60) {
        o += &format!("  {age:>4}s ago  {}\n", p.display());
    }
    if recent.is_empty() {
        o += "  (none)\n";
    }

    o += "\n## Codex SQLite databases\n";
    for rel in [
        "state_5.sqlite",
        "thread_history_1.sqlite",
        "sqlite/codex-dev.db",
        "goals_1.sqlite",
        "queue_1.sqlite",
    ] {
        o += &dump_sqlite(&home.join(".codex").join(rel));
    }

    o += "\n## Codex global state JSON (keys with type/length only)\n";
    o += &dump_json_scalars(&home.join(".codex").join(".codex-global-state.json"));

    o += "\n## Last line of Codex session_index.jsonl (key names)\n";
    if let Ok(t) = std::fs::read_to_string(home.join(".codex").join("session_index.jsonl")) {
        if let Some(last) = t.lines().rev().find(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<serde_json::Value>(last) {
                Ok(v) => {
                    let keys: Vec<String> = v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
                    o += &format!("  keys: {}\n", keys.join(", "));
                }
                Err(_) => o += "  not JSON\n",
            }
        }
    }

    o += "\n## Codex processes (pid and process name; command lines are never read)\n";
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", process_query()]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
        if let Ok(out) = cmd.output() {
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(l) = format_process_line(l) {
                    o += &format!("  {l}\n");
                }
            }
        }
    }

    // Structure only: session directory names are derived from working directories and can name a
    // customer or a project, so nothing under DeepSeek Harness's own home is printed verbatim.
    o += "\n## DeepSeek Harness session store (counts and sizes only, never a path)\n";
    match crate::dsh::sessions_root() {
        Some(root) if root.is_dir() => {
            let mut dirs = 0usize;
            let mut logs = 0usize;
            let mut total = 0u64;
            let mut newest = 0u64;
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if !e.path().is_dir() {
                        continue;
                    }
                    dirs += 1;
                    if let Ok(inner) = std::fs::read_dir(e.path()) {
                        for f in inner.flatten() {
                            let name = f.file_name().to_string_lossy().to_lowercase();
                            if !name.starts_with("session") || !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zstd")) {
                                continue;
                            }
                            logs += 1;
                            if let Ok(m) = f.metadata() {
                                total += m.len();
                                if let Some(t) = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
                                    newest = newest.max(t.as_millis() as u64);
                                }
                            }
                        }
                    }
                }
            }
            o += &format!("  {dirs} session directories, {logs} transcripts, {} KiB in total\n", total / 1024);
            if newest > 0 {
                o += &format!("  newest transcript written {}s ago\n", now_ms().saturating_sub(newest) / 1000);
            }
        }
        _ => o += "  (no sessions directory — DeepSeek Harness has not run here)\n",
    }

    // Counts and sizes only, for the same reason: a project directory is an encoded working path,
    // which can name a customer or a project, so nothing under WorkBuddy's own home is printed.
    o += "\n## WorkBuddy session store (counts and sizes only, never a path)\n";
    if !crate::workbuddy_tokens::present() {
        o += "  (no projects directory under ~/.workbuddy or ~/.workbuddy-ai)\n";
    } else {
        let (projects, logs, bytes, newest) = crate::workbuddy_tokens::survey();
        o += &format!("  {projects} project directories, {logs} transcripts, {} KiB in total\n", bytes / 1024);
        if newest > 0 {
            o += &format!("  newest transcript written {}s ago\n", now_ms().saturating_sub(newest) / 1000);
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::{dump_json_scalars, dump_sqlite, format_process_line, process_query, short};

    /// #160's rule: no scalar is safe merely because it is short.
    const SECRET: &str = "sk-ant-api03-topsecret";

    #[test]
    fn sqlite_values_render_as_type_and_length_only() {
        let t = short(&rusqlite::types::Value::Text(SECRET.into()));
        assert!(!t.contains(SECRET), "text value must not appear: {t}");
        assert_eq!(t, format!("<text {} chars>", SECRET.len()));

        let i = short(&rusqlite::types::Value::Integer(987654321));
        assert!(!i.contains("987654321"), "integer value must not appear: {i}");
        assert_eq!(i, "<integer>");

        let r = short(&rusqlite::types::Value::Real(123.456789));
        assert!(!r.contains("123.456789"), "real value must not appear: {r}");
        assert_eq!(r, "<real>");

        assert_eq!(short(&rusqlite::types::Value::Null), "<null>");
        assert_eq!(short(&rusqlite::types::Value::Blob(vec![1, 2, 3])), "<blob 3 bytes>");
    }

    /// End to end through dump_sqlite: column names and the newest row's structure stay, its
    /// values do not.
    #[test]
    fn dump_sqlite_keeps_columns_but_not_values() {
        let path = std::env::temp_dir().join(format!("codenotch-diag-{}-sqlite.db", std::process::id()));
        let _ = std::fs::remove_file(&path); // a recycled pid must not see a stale table
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (secret TEXT, updated_at INTEGER)", []).unwrap();
            conn.execute("INSERT INTO t VALUES (?1, ?2)", [SECRET, "987654321"]).unwrap();
        }
        let out = dump_sqlite(&path);
        let _ = std::fs::remove_file(&path);

        assert!(out.contains("secret") && out.contains("updated_at"), "column names must stay:\n{out}");
        assert!(!out.contains(SECRET), "cell value must not appear:\n{out}");
        assert!(!out.contains("987654321"), "cell value must not appear:\n{out}");
        assert!(out.contains(&format!("<text {} chars>", SECRET.len())), "type/length must stay:\n{out}");
    }

    /// End to end through dump_json_scalars: dotted keys, types, lengths and container sizes stay;
    /// scalar values do not.
    #[test]
    fn json_scalars_render_as_type_and_length_only() {
        let path = std::env::temp_dir().join(format!("codenotch-diag-{}-state.json", std::process::id()));
        std::fs::write(
            &path,
            format!(r#"{{"token":"{SECRET}","port":987654321,"darkMode":true,"absent":null,"windows":[1,2],"nested":{{"password":"hunter2"}}}}"#),
        )
        .unwrap();
        let out = dump_json_scalars(&path);
        let _ = std::fs::remove_file(&path);

        assert!(!out.contains(SECRET), "string value must not appear:\n{out}");
        assert!(!out.contains("987654321"), "number value must not appear:\n{out}");
        assert!(!out.contains("hunter2"), "nested string value must not appear:\n{out}");
        assert!(!out.contains("true"), "boolean value must not appear:\n{out}");
        assert!(out.contains(&format!("token: <string {} chars>", SECRET.len())), "key/type/length must stay:\n{out}");
        assert!(out.contains("port: <number>"), "key/type must stay:\n{out}");
        assert!(out.contains("darkMode: <bool>"), "key/type must stay:\n{out}");
        assert!(out.contains("absent: <null>"), "key/type must stay:\n{out}");
        assert!(out.contains("windows: <array 2>"), "key/size must stay:\n{out}");
        assert!(out.contains("nested.password: <string 7 chars>"), "dotted key/type/length must stay:\n{out}");
    }

    /// Even a line that somehow carried a third field (a command line) keeps only pid and name.
    #[test]
    fn process_lines_keep_pid_and_name_but_drop_arguments() {
        let line = format_process_line(&format!("4242\tcodex.exe\tcodex.exe --token {SECRET}"))
            .expect("a pid<TAB>name line formats");
        assert_eq!(line, "4242  codex.exe");

        assert_eq!(format_process_line(""), None);
        assert_eq!(format_process_line("garbage without tabs"), None);
    }

    #[test]
    fn the_process_query_never_requests_command_lines() {
        let q = process_query();
        // PowerShell property names are case-insensitive, so no casing of "commandline" may appear
        assert!(!q.to_lowercase().contains("commandline"), "the query must not request CommandLine:\n{q}");
        assert!(q.contains("ProcessId") && q.contains("$($_.Name)"), "pid and name must be selected:\n{q}");
    }
}
