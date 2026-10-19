//! The touch screen, switched off at the device level.
//!
//! Windows has no per-app way to ignore a touch screen: the glass reports taps to the session
//! whatever window is under them. The one honest switch is the one Device Manager offers — disable
//! the HID collections behind the digitizer — and it is also the one that lasts, so a palm resting
//! on the screen cannot drag the pointer while the machine sits on a stand with a keyboard attached.
//!
//! Three things decide how this is written.
//!
//! * **Which devices.** By HID usage page (`UP:000D_U:0004`, Digitizer / Touch Screen) in the
//!   hardware ids, never by device name, so the answer does not follow the system language. A laptop
//!   that forwards touch through a virtual HID channel (Huawei's `VhidUm`, and the `ROOT\HIDCLASS`
//!   one) reports the same usage more than once, and disabling only the physical controller leaves
//!   touch working — so every collection with that usage page is switched, and `found` says how many
//!   there were. The read and the write share the same filter expression, so they cannot drift apart
//!   about which devices this feature means.
//! * **Who may write.** Disabling a devnode needs administrator rights, which Codenotch does not ask
//!   for at launch and should not. Reading the state needs none, so the read happens here and the
//!   write is handed to an elevated copy of the same script, which Windows puts behind its own
//!   consent dialog. One dialog per switch, never at startup.
//! * **Why PowerShell rather than cfgmgr32.** The device list is a CIM query the shell already
//!   answers, and `Disable-PnpDevice` / `Enable-PnpDevice` are the same cfgmgr32 calls Device
//!   Manager makes. Doing it this way keeps the whole feature in two readable scripts instead of a
//!   few hundred lines of unsafe tree walking, and it is the route that has been verified on the
//!   machine this was written for.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What Device Manager shows as "This device is disabled" (`CM_PROB_DISABLED`). Everything else —
/// including 0 — counts as a screen that still takes taps.
const PROBLEM_DISABLED: u32 = 22;

/// How the state reaches the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchState {
    /// HID collections reporting the touch usage page. 0 means there is no touch screen here.
    pub found: usize,
    /// True only when every one of them is disabled: a half-disabled screen still takes taps.
    pub disabled: bool,
}

/// HID usage page 0x0D, usage 0x04 — Digitizer / Touch Screen. Matched against the joined hardware
/// ids, which is the part of the device that is the same in every language.
const TOUCH_FILTER: &str = "$_.PNPClass -eq 'HIDClass' -and (($_.HardwareID -join ';') -match 'UP:000D_U:0004')";

/// One `DeviceID|ConfigManagerErrorCode` line per collection. `-ErrorAction SilentlyContinue` because
/// a machine without a touch screen is an answer, not an error.
const PS_READ: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Get-CimInstance Win32_PnPEntity |
  Where-Object { __FILTER__ } |
  ForEach-Object { '{0}|{1}' -f $_.DeviceID, $_.ConfigManagerErrorCode }
"#;

/// The elevated half. It writes its own verdict to a file because a process started with
/// `-Verb RunAs` cannot hand its stdout back to us; the caller then reads the devices themselves and
/// treats that as the answer.
const PS_APPLY: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$result = '__RESULT__'
try {
  $targets = @(Get-CimInstance Win32_PnPEntity -ErrorAction Stop |
    Where-Object { __FILTER__ } |
    ForEach-Object { $_.DeviceID })
  if ($targets.Count -eq 0) {
    Set-Content -LiteralPath $result -Value 'none' -Encoding UTF8
    exit 3
  }
  $failed = @()
  foreach ($id in $targets) {
    try { __VERB__ -InstanceId $id -Confirm:$false -ErrorAction Stop }
    catch { $failed += ($id + ' - ' + $_.Exception.Message) }
  }
  if ($failed.Count -gt 0) {
    Set-Content -LiteralPath $result -Value ('failed: ' + ($failed -join ' / ')) -Encoding UTF8
    exit 4
  }
  Set-Content -LiteralPath $result -Value 'ok' -Encoding UTF8
  exit 0
} catch {
  Set-Content -LiteralPath $result -Value ('failed: ' + $_.Exception.Message) -Encoding UTF8
  exit 5
}
"#;

/// Asks Windows for the consent dialog once, waits for the answer, and passes the elevated script's
/// exit code back. 1223 is `ERROR_CANCELLED`: No was pressed, or the dialog was dismissed — that is
/// a refusal, not a fault, and the caller says so in its own words.
const PS_LAUNCH: &str = r#"
$ErrorActionPreference = 'Stop'
try {
  $p = Start-Process -FilePath '__PS__' -ArgumentList '__ARGS__' -Verb RunAs -Wait -PassThru -ErrorAction Stop
  if ($null -eq $p.ExitCode) { exit 0 }
  exit $p.ExitCode
} catch {
  exit 1223
}
"#;

/// Seconds to wait for the consent dialog and the elevated script. Generous on purpose: this is the
/// human answering a Windows dialog, not a timeout on our own work.
const ELEVATED_WAIT: Duration = Duration::from_secs(180);

fn script_dir() -> PathBuf {
    std::env::temp_dir().join("codenotch-touch")
}

/// Every script is written with a BOM. PowerShell 5.1 reads a BOM-less file as ANSI, and the paths
/// embedded here come from the user's profile, which can be anything — a name outside ASCII turns a
/// path into mojibake and the script then fails looking for a file that does not exist.
fn write_script(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all("\u{feff}".as_bytes())?;
    f.write_all(body.as_bytes())
}

fn powershell() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

fn read_script() -> String {
    PS_READ.replace("__FILTER__", TOUCH_FILTER)
}

/// The devices and their state, straight from the device tree. Read-only and needs no rights.
pub fn read() -> TouchState {
    let path = script_dir().join("read.ps1");
    if write_script(&path, &read_script()).is_err() {
        return TouchState::default();
    }
    let mut cmd = Command::new(powershell());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path);
    let Ok(out) = cmd.output() else { return TouchState::default() };
    parse_read(&String::from_utf8_lossy(&out.stdout))
}

/// `DeviceID|code` per line. A line that is not a HID instance id is dropped rather than counted:
/// PowerShell banners and warnings share this stream, and counting one would make a desktop report a
/// touch screen it does not have.
fn parse_read(out: &str) -> TouchState {
    let codes: Vec<u32> = out
        .lines()
        .map(str::trim)
        .filter(|line| line.to_ascii_uppercase().starts_with("HID\\"))
        .map(|line| {
            line.rsplit_once('|')
                .and_then(|(_, code)| code.trim().parse().ok())
                // A missing code is not "disabled": only 22 is, spelled out above.
                .unwrap_or(0)
        })
        .collect();
    TouchState {
        found: codes.len(),
        disabled: !codes.is_empty() && codes.iter().all(|code| *code == PROBLEM_DISABLED),
    }
}

/// The elevated half of the switch: `disable` true turns touch off. The caller re-reads the devices
/// afterwards; this only answers whether the script ran.
fn apply_elevated(disable: bool) -> Result<(), &'static str> {
    let dir = script_dir();
    let result = dir.join("result.txt");
    let apply = dir.join("apply.ps1");
    let launch = dir.join("launch.ps1");
    let _ = std::fs::remove_file(&result);

    let verb = if disable { "Disable-PnpDevice" } else { "Enable-PnpDevice" };
    let body = PS_APPLY
        .replace("__FILTER__", TOUCH_FILTER)
        .replace("__VERB__", verb)
        .replace("__RESULT__", &escape_single(&result));
    if write_script(&apply, &body).is_err() {
        return Err("failed");
    }

    // The inner command line is one argument to Start-Process, so the script path is quoted inside
    // it — a profile with a space in it is otherwise read as two arguments.
    let args = format!(
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
        apply.display()
    );
    let launcher = PS_LAUNCH
        .replace("__PS__", &escape_single(&powershell()))
        .replace("__ARGS__", &escape_single_str(&args));
    if write_script(&launch, &launcher).is_err() {
        return Err("failed");
    }

    let mut cmd = Command::new(powershell());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&launch);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = cmd.spawn() else { return Err("failed") };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < ELEVATED_WAIT => {
                std::thread::sleep(Duration::from_millis(150));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    match status {
        // 1223 — the consent dialog was answered with No, or dismissed.
        Some(s) if s.code() == Some(1223) => Err("declined"),
        Some(_) => match std::fs::read_to_string(&result) {
            // The script wrote its verdict; the device tree is re-read by the caller either way.
            Ok(text) if text.trim().starts_with("failed") => {
                crate::applog(&format!("touch: {verb} said {}", text.trim()));
                Err("failed")
            }
            Ok(_) => Ok(()),
            // No file at all means the elevated half never got to run.
            Err(_) => Err("failed"),
        },
        None => Err("failed"),
    }
}

/// A single-quoted PowerShell literal: the only character it cannot hold is `'` itself, which is
/// doubled. Paths and argument lists only — nothing here is ever evaluated.
fn escape_single(value: &Path) -> String {
    escape_single_str(&value.display().to_string())
}

fn escape_single_str(value: &str) -> String {
    value.replace('\'', "''")
}

/// Turns the touch screen on or off and answers with the state read back from the devices.
///
/// The read-back is the point: the elevated script's success only means it did not throw, and a
/// vendor service can re-enumerate the hardware a second later. What the settings switch shows is
/// always what the device tree says, never what we asked for.
pub fn set_enabled(on: bool) -> Result<TouchState, &'static str> {
    let before = read();
    if before.found == 0 {
        return Err("not-found");
    }
    let want_disabled = !on;
    if before.disabled == want_disabled {
        return Ok(before);
    }
    apply_elevated(want_disabled)?;

    let mut state = before;
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        state = read();
        if state.disabled == want_disabled {
            return Ok(state);
        }
    }
    Err("unchanged")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine this was written on, as the read script prints it: a Huawei laptop whose touch
    /// arrives three times — the digitizer, and two virtual HID channels that re-publish it.
    const HUAWEI_DISABLED: &str = "\
HID\\HID_DEVICE_SYSTEM_VHF&COL01\\2&1A164EAB&1&0000|22
HID\\HIDCLASS&COL01\\1&2D595CA7&1&0000|22
HID\\FTSC1000&COL01\\4&293C5861&0&0000|22
";

    #[test]
    fn a_machine_reports_all_of_its_touch_collections() {
        assert_eq!(parse_read(HUAWEI_DISABLED), TouchState { found: 3, disabled: true });
        // Nothing found is not "disabled": a desktop must not look like a screen turned off.
        assert_eq!(parse_read(""), TouchState { found: 0, disabled: false });
        assert_eq!(parse_read("\n \n"), TouchState { found: 0, disabled: false });
    }

    /// One collection left on still takes taps, so the switch reads as off only when all of them are.
    #[test]
    fn one_collection_left_on_means_the_touch_screen_is_on() {
        let half = HUAWEI_DISABLED.replace("FTSC1000&COL01\\4&293C5861&0&0000|22", "FTSC1000&COL01\\4&293C5861&0&0000|0");
        assert_eq!(parse_read(&half), TouchState { found: 3, disabled: false });
        // A code that is not 22 is a different fault, not a disabled device.
        let other = HUAWEI_DISABLED.replace("|22", "|10");
        assert_eq!(parse_read(&other), TouchState { found: 3, disabled: false });
    }

    /// Warnings and Windows' own chatter share the stream; none of it may be counted as a device.
    #[test]
    fn only_hid_instance_ids_count() {
        let noisy = format!(
            "Get-CimInstance : Access denied\r\n  + CategoryInfo : NotSpecified\r\n{HUAWEI_DISABLED}"
        );
        assert_eq!(parse_read(&noisy), TouchState { found: 3, disabled: true });
    }

    /// A machine whose error codes come back empty: unknown, so not disabled — the switch must not
    /// claim to have turned something off it never asked about.
    #[test]
    fn a_missing_problem_code_is_not_a_disabled_device() {
        assert_eq!(
            parse_read("HID\\FTSC1000&COL01\\4&293C5861&0&0000|\n"),
            TouchState { found: 1, disabled: false }
        );
    }

    /// The read and the write have to mean the same devices. Both scripts carry the one filter
    /// expression, and that expression has to be the usage page rather than a device name.
    #[test]
    fn the_read_and_the_write_ask_for_the_same_devices() {
        for script in [read_script(), PS_APPLY.to_string()] {
            assert!(script.contains(TOUCH_FILTER), "a script lost the shared filter");
        }
        assert!(TOUCH_FILTER.contains("UP:000D_U:0004"), "the filter is the touch usage page");
        assert!(TOUCH_FILTER.contains("HIDClass"), "and only HID collections");
        assert!(
            !TOUCH_FILTER.to_lowercase().contains("ftsc"),
            "a device name would stop matching on another machine"
        );
        // The verb is substituted, not hard-coded twice: one direction must not carry the other's.
        let disable = PS_APPLY.replace("__FILTER__", TOUCH_FILTER).replace("__VERB__", "Disable-PnpDevice");
        assert!(disable.contains("Disable-PnpDevice"));
        assert!(!disable.contains("__VERB__"));
    }

    /// Both halves of a toggle exist under one name each, and the result file is quoted as a literal
    /// so a profile with a space survives the trip through the launcher.
    #[test]
    fn the_launcher_quotes_the_paths_it_is_given() {
        assert_eq!(escape_single_str(r"C:\Users\a b\t.ps1"), r"C:\Users\a b\t.ps1");
        assert_eq!(escape_single_str("it's"), "it''s");
        let body = PS_APPLY.replace("__RESULT__", &escape_single(Path::new(r"C:\T\it's.txt")));
        assert!(body.contains("$result = 'C:\\T\\it''s.txt'"), "{body}");
        assert!(!body.contains("__RESULT__"));
    }
}
