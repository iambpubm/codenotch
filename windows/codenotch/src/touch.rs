//! The touch screen, switched off at the device level.
//!
//! Windows has no per-app way to ignore a touch screen: the glass reports taps to the session
//! whatever window is under them. The one honest switch is the one Device Manager offers — disable
//! the HID collections behind the digitizer — and it is also the one that lasts, so a palm resting
//! on the screen cannot drag the pointer while the machine sits on a stand with a keyboard attached.
//!
//! Four things decide how this is written.
//!
//! * **Which devices.** By HID usage page (`UP:000D_U:0004`, Digitizer / Touch Screen) in the
//!   hardware ids, never by device name, so the answer does not follow the system language. A laptop
//!   that forwards touch through a virtual HID channel (Huawei's `VhidUm`, and the `ROOT\HIDCLASS`
//!   one) reports the same usage more than once, and disabling only the physical controller leaves
//!   touch working — measured: the system still reported ten touch points afterwards. So every
//!   collection with that usage page is switched, and `found` says how many there were.
//! * **CfgMgr32, not PowerShell.** The previous version wrote `.ps1` files into `%TEMP%`, ran them
//!   with `-ExecutionPolicy Bypass`, and asked for rights with `Start-Process -Verb RunAs`. That is
//!   three of the loudest heuristics a security suite looks for, and it showed: the first version was
//!   flagged as a virus, and since the app has no console of its own, every `powershell.exe` it
//!   started flashed a black window over whatever was on screen. `CM_Disable_DevNode` is the call
//!   Device Manager makes and needs nothing but the API, so this module now runs entirely in-process.
//! * **Who may write.** Disabling a devnode needs administrator rights, which Codenotch does not ask
//!   for at launch and should not. Reading the state needs none. So the write is handed to one
//!   process that has those rights, started on demand by a task registered once — no consent dialog
//!   after the first, no long-lived elevated process, and nothing for a signature scanner to read.
//! * **The device tree is the answer.** Every switch ends by re-reading the devices, so what the
//!   notch and Settings show is what Windows reports, never what was asked for.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The task that carries the administrator's rights, created the first time it is needed.
pub const TASK_NAME: &str = "CodenotchTouch";

/// What Device Manager shows as "This device is disabled" (`CM_PROB_DISABLED`). Everything else —
/// including 0 — counts as a screen that still takes taps.
const PROBLEM_DISABLED: u32 = 22;

/// HID usage page 0x0D, usage 0x04 — Digitizer / Touch Screen. Matched against the hardware ids,
/// which is the part of the device that is the same in every language.
const TOUCH_USAGE: &str = "UP:000D_U:0004";

/// How the state reaches the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchState {
    /// HID collections reporting the touch usage page. 0 means there is no touch screen here.
    pub found: usize,
    /// True only when every one of them is disabled: a half-disabled screen still takes taps.
    pub disabled: bool,
}

// CONFIGRET and the flag values, spelled out rather than taken from a binding: these are the numbers
// every call answers with, and one of them being off by one looks exactly like a device error. The
// tests below pin them for that reason.
const CR_SUCCESS: u32 = 0;
/// The answer to "how big is that property" — not a failure, the size arrives with it.
const CR_BUFFER_SMALL: u32 = 26;
/// `CM_DRP_HARDWAREID`. **2**, not 1: 1 is `CM_DRP_DEVICEDESC`, and asking for it returns the
/// friendly description — a string that contains no hardware ids at all and silently matches nothing.
const CM_DRP_HARDWAREID: u32 = 2;
const CM_GETIDLIST_FILTER_ENUMERATOR: u32 = 0x0000_0001;
const CM_GETIDLIST_FILTER_PRESENT: u32 = 0x0000_0100;
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
/// Disable without the "this may stop the device working" prompt: the point is to do it quietly.
const CM_DISABLE_UI_NOT_OK: u32 = 0x0000_0004;

/// Waits for the elevated half: 40 polls of the device tree, 50 ms apart.
const SETTLE_TRIES: usize = 40;
const SETTLE_STEP: Duration = Duration::from_millis(50);

// ------------------------------------------------------------------ pure pieces, all unit-tested

/// Splits a `REG_MULTI_SZ` buffer. The list ends at the first empty string, and whatever follows is
/// padding — counting it would invent devices that do not exist.
fn multi_sz(buf: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, unit) in buf.iter().enumerate() {
        if *unit == 0 {
            if i == start {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[start..i]));
            start = i + 1;
        }
    }
    // A buffer that ends without its terminator still holds one more entry.
    if start < buf.len() {
        let tail: String = String::from_utf16_lossy(&buf[start..]).trim_end_matches('\u{0}').to_string();
        if !tail.is_empty() {
            out.push(tail);
        }
    }
    out
}

/// Is this collection the touch screen? True for the usage page, false for the pen (`U:0002`) and
/// the touchpad (`U:0005`), which share the enumerator and must keep working.
fn is_touch_collection(hardware_ids: &[String]) -> bool {
    hardware_ids.iter().any(|id| id.contains(TOUCH_USAGE))
}

impl TouchState {
    fn from_problems(problems: &[u32]) -> Self {
        TouchState {
            found: problems.len(),
            disabled: !problems.is_empty() && problems.iter().all(|c| *c == PROBLEM_DISABLED),
        }
    }
}

/// `off` means "disable the touch screen". Anything else that is not `on` is no request at all.
fn request_from(text: &str) -> Option<bool> {
    match text.trim() {
        "off" => Some(true),
        "on" => Some(false),
        _ => None,
    }
}

/// The exe path goes inside an XML element, so `&` and `<` have to survive it. A path with an
/// ampersand is rare and would otherwise make the whole registration fail with no useful message.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The task that runs the elevated half on demand.
///
/// **No `<Triggers>` element at all** — that is what makes this a button rather than a schedule: the
/// task can only be started by `schtasks /run`, never at sign-in and never on a timer. `RunLevel`
/// is `HighestAvailable`, which is where the rights come from; `InteractiveToken` because it runs as
/// the signed-in user, which is also why starting it raises no consent dialog.
fn task_xml(exe: &str, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Switches the touch screen on and off for Codenotch, with the rights that needs.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT2M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--touch-apply</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        exe = xml_escape(exe),
    )
}

/// Where the page leaves what it wants, for the process that has the rights to read. A file rather
/// than an argument: the task's command line is fixed when it is registered, and two tasks for two
/// directions would be two things to keep in step.
fn request_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("touch.request")
}

fn write_request(disable: bool) -> bool {
    let path = request_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, if disable { "off" } else { "on" }).is_ok()
}

fn read_request() -> Option<bool> {
    request_from(&std::fs::read_to_string(request_path()).ok()?)
}

// ------------------------------------------------------------------------------ the Windows half

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    #[link(name = "cfgmgr32")]
    extern "system" {
        pub fn CM_Get_Device_ID_List_SizeW(len: *mut u32, filter: *const u16, flags: u32) -> u32;
        pub fn CM_Get_Device_ID_ListW(filter: *const u16, buffer: *mut u16, len: u32, flags: u32) -> u32;
        pub fn CM_Locate_DevNodeW(node: *mut u32, id: *const u16, flags: u32) -> u32;
        pub fn CM_Get_DevNode_Status(status: *mut u32, problem: *mut u32, node: u32, flags: u32) -> u32;
        pub fn CM_Get_DevNode_Registry_PropertyW(
            node: u32,
            property: u32,
            kind: *mut u32,
            buffer: *mut c_void,
            len: *mut u32,
            flags: u32,
        ) -> u32;
        pub fn CM_Disable_DevNode(node: u32, flags: u32) -> u32;
        pub fn CM_Enable_DevNode(node: u32, flags: u32) -> u32;
    }

    #[link(name = "shell32")]
    extern "system" {
        pub fn ShellExecuteW(
            hwnd: *mut c_void,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Every present HID collection that reports the touch usage page, with its problem code.
///
/// `PRESENT` keeps the ghosts out — a disabled device is still present, which is what lets this
/// answer "disabled" rather than "gone" — and it is also the difference between switching the screen
/// that is here and one that was unplugged months ago.
#[cfg(windows)]
fn enumerate() -> Vec<(String, u32)> {
    let mut found = Vec::new();
    unsafe {
        let filter = wide("HID");
        let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
        let mut len: u32 = 0;
        if sys::CM_Get_Device_ID_List_SizeW(&mut len, filter.as_ptr(), flags) != CR_SUCCESS || len == 0 {
            return found;
        }
        let mut buf = vec![0u16; len as usize];
        if sys::CM_Get_Device_ID_ListW(filter.as_ptr(), buf.as_mut_ptr(), len, flags) != CR_SUCCESS {
            return found;
        }
        for id in multi_sz(&buf) {
            let node = match locate(&id) {
                Some(node) => node,
                None => continue,
            };
            if !is_touch_collection(&hardware_ids(node)) {
                continue;
            }
            found.push((id, problem_of(node)));
        }
    }
    found
}

#[cfg(windows)]
unsafe fn locate(id: &str) -> Option<u32> {
    let wide_id = wide(id);
    let mut node: u32 = 0;
    if sys::CM_Locate_DevNodeW(&mut node, wide_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
        return None;
    }
    Some(node)
}

/// The device's `HardwareID` list. The first call asks how big it is and is answered with
/// `CR_BUFFER_SMALL` — that is the size, not a failure, and treating it as one returns nothing and
/// looks exactly like "this machine has no touch screen".
#[cfg(windows)]
unsafe fn hardware_ids(node: u32) -> Vec<String> {
    let mut len: u32 = 0;
    let mut kind: u32 = 0;
    let cr = sys::CM_Get_DevNode_Registry_PropertyW(
        node,
        CM_DRP_HARDWAREID,
        &mut kind,
        std::ptr::null_mut(),
        &mut len,
        0,
    );
    if (cr != CR_SUCCESS && cr != CR_BUFFER_SMALL) || len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; (len as usize / 2) + 1];
    let cr = sys::CM_Get_DevNode_Registry_PropertyW(
        node,
        CM_DRP_HARDWAREID,
        &mut kind,
        buf.as_mut_ptr() as *mut std::ffi::c_void,
        &mut len,
        0,
    );
    if cr != CR_SUCCESS {
        return Vec::new();
    }
    multi_sz(&buf)
}

#[cfg(windows)]
unsafe fn problem_of(node: u32) -> u32 {
    let mut status: u32 = 0;
    let mut problem: u32 = 0;
    if sys::CM_Get_DevNode_Status(&mut status, &mut problem, node, 0) != CR_SUCCESS {
        // A device whose status cannot be read is not a disabled one: only 22 is, spelled out above.
        return 0;
    }
    problem
}

/// The devices and their state, straight from the device tree. Read-only and needs no rights.
#[cfg(windows)]
pub fn read() -> TouchState {
    let problems: Vec<u32> = enumerate().into_iter().map(|(_, problem)| problem).collect();
    TouchState::from_problems(&problems)
}

/// Disables (or enables) every touch collection. Answers the first CONFIGRET that was not a success,
/// which is 5 — access denied — whenever the caller has no rights.
#[cfg(windows)]
fn apply(disable: bool) -> Result<usize, u32> {
    let devices = enumerate();
    let mut changed = 0;
    for (id, _) in devices {
        unsafe {
            let node = match locate(&id) {
                Some(node) => node,
                None => continue,
            };
            let cr = if disable {
                sys::CM_Disable_DevNode(node, CM_DISABLE_UI_NOT_OK)
            } else {
                sys::CM_Enable_DevNode(node, 0)
            };
            if cr != CR_SUCCESS {
                return Err(cr);
            }
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new("schtasks.exe");
    c.args(args);
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: this runs while the notch is on screen
    }
    let out = c.output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(windows)]
fn task_exists() -> bool {
    schtasks(&["/query", "/tn", TASK_NAME]).is_some()
}

#[cfg(windows)]
fn run_task() -> bool {
    schtasks(&["/run", "/tn", TASK_NAME]).is_some()
}

/// Registers the task. Best effort: a refusal is reported, and the caller still gets its switch —
/// just behind the consent dialog every time.
#[cfg(windows)]
fn register_task() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) => format!("{domain}\\{name}"),
        (_, Ok(name)) => name,
        _ => return false,
    };
    let xml = task_xml(&exe.to_string_lossy(), &user);
    let path = std::env::temp_dir().join("codenotch-touch-task.xml");
    // schtasks reads the file as Unicode; UTF-16LE with a byte-order mark is what it writes itself.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    if std::fs::write(&path, &bytes).is_err() {
        return false;
    }
    // Bound to a String first: a borrowed Cow inside the argument list is a type the array of &str
    // will not take, and the temporary would end with the statement either way.
    let path_text = path.to_string_lossy().to_string();
    let ok = schtasks(&["/create", "/tn", TASK_NAME, "/xml", path_text.as_str(), "/f"]).is_some();
    let _ = std::fs::remove_file(&path);
    ok
}

/// The previous version left three `.ps1` files in `%TEMP%\codenotch-touch` after every read and
/// every switch. Nothing writes there any more; this takes the old ones away so the suite that
/// objected to them has nothing left to look at.
#[cfg(windows)]
pub fn retire_old_scripts() {
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("codenotch-touch"));
}

/// Starts our own exe with the rights this process does not have. The only consent dialog left in
/// the feature is this one, and it is only reached when no task has been registered yet.
#[cfg(windows)]
fn launch_elevated(param: &str) -> Result<(), &'static str> {
    let exe = std::env::current_exe().map_err(|_| "failed")?;
    let (op, file, params) = (wide("runas"), wide(&exe.to_string_lossy()), wide(param));
    let answer = unsafe {
        sys::ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            0, // SW_HIDE: the helper draws nothing
        )
    };
    // 5 is SE_ERR_ACCESSDENIED — the consent dialog answered No. Above 32 means it started.
    if answer <= 32 {
        Err("declined")
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn settle(want_disabled: bool, before: TouchState) -> Result<TouchState, &'static str> {
    let mut state = before;
    for _ in 0..SETTLE_TRIES {
        std::thread::sleep(SETTLE_STEP);
        state = read();
        if state.disabled == want_disabled {
            return Ok(state);
        }
    }
    Err("unchanged")
}

/// Turns the touch screen on or off and answers with the state read back from the devices.
#[cfg(windows)]
pub fn set_enabled(on: bool) -> Result<TouchState, &'static str> {
    let before = read();
    if before.found == 0 {
        return Err("not-found");
    }
    let want_disabled = !on;
    if before.disabled == want_disabled {
        return Ok(before);
    }

    // Already elevated — the helper's own path, and the case of an app started as administrator.
    if apply(want_disabled).is_ok() {
        return settle(want_disabled, before);
    }

    if !write_request(want_disabled) {
        return Err("failed");
    }
    if task_exists() && run_task() {
        return settle(want_disabled, before);
    }
    // First time, or the task is gone: register it and switch in the same elevated run.
    launch_elevated("--touch-setup")?;
    settle(want_disabled, before)
}

/// `--touch-setup` / `--touch-apply`. Both run elevated; the difference is only whether the task is
/// registered first. Called from `main` before anything is built.
#[cfg(windows)]
pub fn cli(args: &[String]) -> Option<i32> {
    let mode = args.first().map(String::as_str)?;
    let setup = match mode {
        "--touch-setup" => true,
        "--touch-apply" => false,
        _ => return None,
    };
    // No request on disk means nobody asked for anything: say so rather than guessing a direction.
    let Some(disable) = read_request() else {
        crate::applog("touch: helper started with no request");
        return Some(3);
    };
    if setup && !register_task() {
        // Not fatal: the switch still has to happen, it just keeps asking for consent afterwards.
        crate::applog("touch: the task could not be registered");
    }
    match apply(disable) {
        Ok(n) => {
            crate::applog(&format!("touch: helper set disabled={disable} on {n} device(s)"));
            Some(0)
        }
        Err(cr) => {
            crate::applog(&format!("touch: helper failed, CONFIGRET {cr}"));
            Some(2)
        }
    }
}

#[cfg(not(windows))]
pub fn read() -> TouchState {
    TouchState::default()
}

#[cfg(not(windows))]
pub fn set_enabled(_on: bool) -> Result<TouchState, &'static str> {
    Err("failed")
}

#[cfg(not(windows))]
pub fn cli(_args: &[String]) -> Option<i32> {
    None
}

#[cfg(not(windows))]
pub fn retire_old_scripts() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine this was written on, as `CM_Get_DevNode_Registry_Property` returns it: a Huawei
    /// laptop whose touch arrives three times — the digitizer, and two virtual HID channels that
    /// re-publish it. Copied from a real read, not invented.
    const HUAWEI_HARDWARE_IDS: [&str; 3] = [
        "HID\\VID_2808&UP:000D_U:0004",
        "HID\\VhidUm&Col01",
        "HID\\HID_DEVICE_SYSTEM_VHF&Col01",
    ];

    #[test]
    fn the_constants_are_the_ones_windows_documents() {
        // Pinned because a wrong CM_DRP_* does not fail: it returns another property and the list
        // that comes back silently matches nothing. Measured on the machine this was written for.
        assert_eq!(CM_DRP_HARDWAREID, 2, "1 is DEVICEDESC and returns the friendly name");
        assert_eq!(CR_BUFFER_SMALL, 26, "the size answer for a property read");
        assert_eq!(PROBLEM_DISABLED, 22, "CM_PROB_DISABLED");
        assert_eq!(CM_GETIDLIST_FILTER_PRESENT, 0x100);
        assert_eq!(CM_DISABLE_UI_NOT_OK, 4);
    }

    #[test]
    fn a_machine_reports_all_of_its_touch_collections() {
        assert_eq!(TouchState::from_problems(&[22, 22, 22]), TouchState { found: 3, disabled: true });
        // Nothing found is not "disabled": a desktop must not look like a screen turned off.
        assert_eq!(TouchState::from_problems(&[]), TouchState { found: 0, disabled: false });
    }

    /// One collection left on still takes taps, so the switch reads as off only when all of them are.
    #[test]
    fn one_collection_left_on_means_the_touch_screen_is_on() {
        assert_eq!(TouchState::from_problems(&[22, 0, 22]), TouchState { found: 3, disabled: false });
        // A code that is not 22 is a different fault, not a disabled device.
        assert_eq!(TouchState::from_problems(&[10, 10, 10]), TouchState { found: 3, disabled: false });
    }

    #[test]
    fn the_touch_usage_page_is_what_is_matched() {
        let ids: Vec<String> = HUAWEI_HARDWARE_IDS.iter().map(|s| s.to_string()).collect();
        assert!(is_touch_collection(&ids), "the digitizer reports U:0004");
        // The same enumerator also carries the pen and the touchpad, and neither may be switched.
        assert!(!is_touch_collection(&["HID\\VID_347D&UP:000D_U:0005".into()]), "touchpad");
        assert!(!is_touch_collection(&["HID\\VID_347D&UP:000D_U:0002".into()]), "pen");
        assert!(!is_touch_collection(&[]));
        // A device name would stop matching on another machine, or in another language.
        assert!(!is_touch_collection(&["符合 HID 标准的触摸屏".into()]));
    }

    #[test]
    fn a_multi_sz_stops_at_its_terminator() {
        let buf: Vec<u16> = "HID\\A\0HID\\B\0\0\0\0".encode_utf16().collect();
        assert_eq!(multi_sz(&buf), vec!["HID\\A", "HID\\B"]);
        let empty: Vec<u16> = "\0".encode_utf16().collect();
        assert!(multi_sz(&empty).is_empty());
        // One entry and no double terminator: the list still has to survive it.
        let single: Vec<u16> = "HID\\C\0".encode_utf16().collect();
        assert_eq!(multi_sz(&single), vec!["HID\\C"]);
    }

    #[test]
    fn a_request_is_only_a_request_in_two_spellings() {
        assert_eq!(request_from("off"), Some(true));
        assert_eq!(request_from(" on\n"), Some(false));
        assert_eq!(request_from(""), None);
        assert_eq!(request_from("toggle"), None);
    }

    /// The task has to be a button, not a schedule: no trigger, highest rights, and the argument
    /// that means "apply what the page asked for" rather than "register yourself again".
    #[test]
    fn the_task_can_only_be_started_on_demand() {
        let xml = task_xml(r"C:\Users\a b\codenotch.exe", r"PC\User");
        assert!(!xml.contains("<Triggers"), "a trigger would run it on its own");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"), "no password, no dialog");
        assert!(xml.contains("<AllowStartOnDemand>true</AllowStartOnDemand>"));
        assert!(xml.contains("<Arguments>--touch-apply</Arguments>"));
        assert!(xml.contains(r"C:\Users\a b\codenotch.exe"));
        // An ampersand in a path would otherwise break the XML and the registration with it.
        let odd = task_xml(r"C:\A&B\x.exe", "PC\\User");
        assert!(odd.contains("C:\\A&amp;B\\x.exe"), "{odd}");
    }
}
