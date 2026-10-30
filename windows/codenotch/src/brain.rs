//! 赛博大脑的同步通道：口令存在哪、怎么加解密、什么时候推上去。
//!
//! 上一版把加解密放在设置页里用 WebCrypto 做，口令只在那个窗口里活一次。那是对 CI 的让步，
//! 代价是「自动同步」没有执行者 —— 设置页关掉之后，没有任何东西能加密。所以这一版把同一套
//! 算法搬进 Rust：PBKDF2(口令, `aiwb-enc-v1:` + uid, 12 万次, SHA-256) → AES-256-GCM 256，
//! 输出格式（tag 附在密文末尾）与 WebCrypto 逐字节一致。这是硬要求：两份密文必须互相解得开，
//! 因为手机上的工作台也在读同一份文件。`tests` 里对着 WebCrypto 生成的固定向量钉住这件事。
//!
//! `ring` 本来就在构建里（rustls 带进来的），所以这次没有新增下载，`Cargo.lock` 只多两行名字。
//!
//! 口令存在 Windows 凭据管理器里，背后是 DPAPI，只有当前 Windows 账户解得开。它在
//! 「控制面板 → 凭据管理器 → Windows 凭据」里能被看见、也能被删掉 —— 这是刻意的：
//! 一把能打开全部经营记录的钥匙放在哪儿，你该有办法亲眼确认，而不是只能信这段注释。

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ring::aead;
use ring::pbkdf2;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

use crate::todos::Todo;

pub const GATEWAY: &str =
    "https://sonnet-ai-d9glodhldc474eb0c-1318171231.ap-shanghai.app.tcloudbase.com/api";
const UA: &str = "Codenotch/1.18 (Windows)";
/// 工作台页面的盐就是这个前缀加账号 id，一个字符都不能差。
const SALT_PREFIX: &str = "aiwb-enc-v1:";
const ROUNDS: u32 = 120_000;
const IV_LEN: usize = 12;
/// 凭据管理器里的目标名。和 Antigravity 的 `gemini:antigravity` 一样，
/// 写成「谁在用它 / 它拿的是谁的钥匙」。
const CRED_TARGET: &str = "Codenotch/CyberBrain";
/// 每天最多留几份覆盖前的密文。自动同步会把这里挤满，所以按天限量而不是留最近 20 份：
/// 一天之内推十次，真正有意义的版本通常是最早那份（他改坏之前的样子）。
const BACKUPS_PER_DAY: usize = 3;

/// 云端那份 blob：`{__enc:1, iv, ct}`，iv 与 ct 都是 base64。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sealed {
    pub iv: String,
    pub ct: String,
}

/// 一次失败分三类，因为三种的下一步完全不同：口令错了重试一万次也没用，
/// 网络错了等一会儿就好，数据形状不对则绝不能覆盖云端。
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// 解不开：口令不是这个工作台的口令，或者在别处改过。
    BadPass,
    /// 网关、网络、DNS。退避之后可以再试。
    Net(String),
    /// 云端那份不是预期的形状。停下来，不要拿本机的数据去覆盖看不懂的东西。
    Shape(String),
    /// 本机的问题：随机数、base64、序列化。重试无用。
    Local(String),
    /// 还没填账号 id。这不是故障，是还没配置 —— 所以要能和「本机出错了」分开说。
    NoUid,
}

impl Fail {
    pub fn text(&self) -> String {
        match self {
            Fail::BadPass => "口令打不开工作台".into(),
            Fail::Net(e) => e.clone(),
            Fail::Shape(e) => e.clone(),
            Fail::Local(e) => e.clone(),
            Fail::NoUid => "还没有填赛博大脑账号 ID".into(),
        }
    }

    /// 给页面用的分类。口令错这一句要按语言说，所以不能靠 `text()` 里那句中文 ——
    /// 界面有八种语言，把一句中文塞进另外七种里，等于没翻。
    pub fn code(&self) -> &'static str {
        match self {
            Fail::BadPass => "badpass",
            Fail::Net(_) => "net",
            Fail::Shape(_) => "shape",
            Fail::Local(_) => "local",
            Fail::NoUid => "nouid",
        }
    }

    /// 只有网络错误值得退避重试。口令错会一直错，形状错再试也是同一份。
    pub fn retryable(&self) -> bool {
        matches!(self, Fail::Net(_))
    }
}

fn less_safe(pass: &str, uid: &str) -> Result<aead::LessSafeKey, Fail> {
    let mut key = [0u8; 32];
    let salt = format!("{SALT_PREFIX}{uid}");
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).expect("12 万次不是零"),
        salt.as_bytes(),
        pass.as_bytes(),
        &mut key,
    );
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| Fail::Local("AES-256-GCM 初始化失败".into()))?;
    Ok(aead::LessSafeKey::new(unbound))
}

/// 固定的 iv 版本，留给测试用：真实调用永远走随机 iv，但比对方必须能指定输入。
fn seal_with_iv(pass: &str, uid: &str, iv: [u8; IV_LEN], plain: &[u8]) -> Result<Sealed, Fail> {
    let key = less_safe(pass, uid)?;
    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(iv),
        aead::Aad::empty(),
        &mut buf,
    )
    .map_err(|_| Fail::Local("加密失败".into()))?;
    Ok(Sealed { iv: B64.encode(iv), ct: B64.encode(&buf) })
}

pub fn seal(pass: &str, uid: &str, plain: &[u8]) -> Result<Sealed, Fail> {
    use ring::rand::SecureRandom;
    let mut iv = [0u8; IV_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut iv)
        .map_err(|_| Fail::Local("系统随机数不可用".into()))?;
    seal_with_iv(pass, uid, iv, plain)
}

pub fn open(pass: &str, uid: &str, sealed: &Sealed) -> Result<Vec<u8>, Fail> {
    let key = less_safe(pass, uid)?;
    let iv: [u8; IV_LEN] = B64
        .decode(&sealed.iv)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| Fail::Shape("云端那份的 iv 不是 12 字节".into()))?;
    let mut buf = B64
        .decode(&sealed.ct)
        .map_err(|_| Fail::Shape("云端那份的密文不是 base64".into()))?;
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(iv),
            aead::Aad::empty(),
            &mut buf,
        )
        .map_err(|_| Fail::BadPass)?;
    Ok(plain.to_vec())
}

/* ---- 口令：Windows 凭据管理器 ------------------------------------------ */

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
mod cred {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    pub fn save(target: &str, secret: &str) -> bool {
        let name = super::wide(target);
        let mut blob = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: windows::Win32::Security::Credentials::CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: windows::core::PWSTR(name.as_ptr() as *mut u16),
            Comment: PCWSTR::null(),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: windows::core::PWSTR::null(),
            UserName: windows::core::PWSTR::null(),
        };
        unsafe { CredWriteW(&cred, 0).is_ok() }
    }

    pub fn load(target: &str) -> Option<String> {
        let name = super::wide(target);
        let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe {
            if CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pcred).is_err()
                || pcred.is_null()
            {
                return None;
            }
            let c = &*pcred;
            let bytes = if c.CredentialBlobSize > 0 && !c.CredentialBlob.is_null() {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            } else {
                Vec::new()
            };
            CredFree(pcred as *const core::ffi::c_void);
            String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
        }
    }

    pub fn forget(target: &str) -> bool {
        use windows::Win32::Security::Credentials::CredDeleteW;
        let name = super::wide(target);
        unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, 0).is_ok() }
    }
}

#[cfg(not(windows))]
mod cred {
    pub fn save(_: &str, _: &str) -> bool {
        false
    }
    pub fn load(_: &str) -> Option<String> {
        None
    }
    pub fn forget(_: &str) -> bool {
        false
    }
}

/// 记住口令。返回它是否真的落在了凭据管理器里 —— 落不下就是「这次运行期有效」，
/// 上层要照实告诉人，而不是假装记住了。
pub fn remember(pass: &str) -> bool {
    cred::save(CRED_TARGET, pass)
}

pub fn saved_pass() -> Option<String> {
    cred::load(CRED_TARGET)
}

pub fn forget_pass() {
    cred::forget(CRED_TARGET);
}

/* ---- 网关 ------------------------------------------------------------- */

pub fn post(body: &str) -> Result<serde_json::Value, Fail> {
    // 函数把 body 当纯文本读，所以 content type 写 text，和页面里的 fetch 一致
    let response = ureq::post(GATEWAY)
        .timeout(std::time::Duration::from_secs(30))
        .set("Content-Type", "text/plain;charset=UTF-8")
        .set("Accept", "application/json")
        .set("User-Agent", UA)
        .send_string(body)
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => Fail::Net(format!("赛博大脑网关回了 {code}")),
            other => Fail::Net(other.to_string()),
        })?;
    let value: serde_json::Value = response
        .into_json()
        .map_err(|e| Fail::Net(format!("网关的回复读不出来：{e}")))?;
    if value.get("code").and_then(|c| c.as_i64()) == Some(0)
        || value.get("ok").and_then(|o| o.as_bool()) == Some(true)
    {
        Ok(value)
    } else {
        Err(Fail::Net(
            value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("赛博大脑没有接受这次请求")
                .to_string(),
        ))
    }
}

/// 拉回来的一份：解出来的密文包装（可能没有，说明工作台还没同步过），以及原始回复
/// —— 备份要存的是原始回复那一整块，重新拼一个只会丢掉我们不认识的字段。
pub struct Pulled {
    pub sealed: Option<Sealed>,
    pub raw: serde_json::Value,
}

pub fn pull(uid: &str) -> Result<Pulled, Fail> {
    let body = serde_json::json!({ "op": "pull", "tbl": "config", "uid": uid }).to_string();
    let reply = post(&body)?;
    let data = reply.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let first = match data {
        serde_json::Value::Array(rows) => rows.into_iter().next().unwrap_or(serde_json::Value::Null),
        other => other,
    };
    // 单例 blob 存在 `{ cfg: {__enc, iv, ct} }` 里
    let cfg = first.get("cfg").cloned().unwrap_or(serde_json::Value::Null);
    let sealed = if cfg.get("__enc").and_then(|v| v.as_i64()) == Some(1) {
        let iv = cfg.get("iv").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let ct = cfg.get("ct").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if iv.is_empty() || ct.is_empty() {
            None
        } else {
            Some(Sealed { iv, ct })
        }
    } else {
        None
    };
    Ok(Pulled { sealed, raw: reply })
}

pub fn push(uid: &str, sealed: &Sealed) -> Result<(), Fail> {
    let body = serde_json::json!({
        "op": "push",
        "tbl": "config",
        "uid": uid,
        "rows": [{ "cfg": { "__enc": 1, "iv": sealed.iv, "ct": sealed.ct } }]
    })
    .to_string();
    post(&body).map(|_| ())
}

/// 覆盖前的旧版留档。存的是密文，所以留在这里也不比云端更暴露。
///
/// 按天限量：自动同步一天可能推好几次，留最近 20 份会被今天挤满，反而看不到真正
/// 有意义的那份（今天最早、他动手之前的样子）。所以每天至多 `BACKUPS_PER_DAY` 份。
pub fn backup(raw: &serde_json::Value) {
    // 空答复说明云端还没有这份配置，那就没有东西可以被覆盖
    if raw.get("data").map_or(true, |d| d.is_null()) {
        return;
    }
    let dir = crate::config::config_path().with_file_name("brain-backup");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let day = chrono::Local::now().format("%Y%m%d").to_string();
    let prefix = format!("config-{day}-");
    let today = std::fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .count()
        })
        .unwrap_or(0);
    if today >= BACKUPS_PER_DAY {
        return;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let _ = std::fs::write(dir.join(format!("config-{stamp}.json")), raw.to_string());
}

/* ---- 一次同步 --------------------------------------------------------- */

/// 同步的对外状态。前端只读它，不去猜「到底同没同」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// off（没有记住的口令）| idle（在等）| working | ok | error
    pub state: String,
    /// 上次成功同步的 unix 秒。前端按本地时区拼成人话。
    pub last_ok: i64,
    /// 上一次失败的原因；成功一次就清空。
    pub error: String,
    /// 上一次失败的分类（badpass / net / shape / local）。页面靠它挑一句能翻译的话，
    /// 而不是把 `error` 里那段原文直接贴出来。
    pub code: String,
    /// 本机有改动，还没推上去。
    pub pending: bool,
    /// 凭据管理器里有没有那句口令。
    pub saved: bool,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            state: "off".into(),
            last_ok: 0,
            error: String::new(),
            code: String::new(),
            pending: false,
            saved: false,
        }
    }
}

pub struct Tick {
    /// 云端有、本机没有的条目。只用于显示，不写盘 —— 它们已经在云上了，
    /// 存成本地记录会让下一次合并把它们当成「本机新记的」再推一遍。
    pub remote: Vec<Todo>,
    /// 这次是否真的写回了云端。
    pub pushed: bool,
}

/// 拉一次云端、解出来、按 id 合并。`write_back` 为真时把合并结果加密推回去。
///
/// 只拉不写也是有用的：手机上加的条目要出现在条子的清单里，而那条路径不需要改动任何
/// 云端数据 —— 定时拉取走的就是这一支。
pub fn run_once(pass: &str, uid: &str, local: &[Todo], write_back: bool) -> Result<Tick, Fail> {
    let pulled = pull(uid)?;
    let mut config = match &pulled.sealed {
        Some(sealed) => {
            let plain = open(pass, uid, sealed)?;
            serde_json::from_slice::<serde_json::Value>(&plain).map_err(|e| {
                Fail::Shape(format!("云端那份解开来不是 JSON：{e}"))
            })?
        }
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    if !config.is_object() {
        return Err(Fail::Shape("云端那份解开来不是一个配置对象".into()));
    }
    // 类型不对时**必须停下**：把它当空数组继续，就会拿本机的几条待办覆盖掉云端那一整份
    let cloud: Vec<Todo> = match config.get("todos") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Fail::Shape(format!("云端的 todos 不是待办列表：{e}")))?,
    };

    let mine: Vec<Todo> = local.iter().filter(|t| !t.remote).cloned().collect();
    let merged = crate::todos::merge_for_push(&mine, &cloud);

    let ids: std::collections::HashSet<&str> = mine.iter().map(|t| t.id.as_str()).collect();
    let remote: Vec<Todo> = merged
        .iter()
        .filter(|t| !ids.contains(t.id.as_str()))
        .cloned()
        .map(|mut t| {
            t.remote = true;
            t.pushed = true;
            t
        })
        .collect();

    if !write_back {
        return Ok(Tick { remote, pushed: false });
    }

    config["todos"] = serde_json::to_value(&merged)
        .map_err(|e| Fail::Local(format!("待办序列化失败：{e}")))?;
    let plain =
        serde_json::to_vec(&config).map_err(|e| Fail::Local(format!("配置序列化失败：{e}")))?;
    let sealed = seal(pass, uid, &plain)?;
    // 备份用的是刚才那次 pull 的原始回复：再拉一次既多一个来回，也多一个「这中间工作台
    // 又写了一次」的窗口。
    backup(&pulled.raw);
    push(uid, &sealed)?;
    Ok(Tick { remote, pushed: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由 WebCrypto 生成（node -e 里同一段代码，和设置页改动前用的是同一套）：
    /// 口令 `pa55-word`、账号 `u-abc-123`、iv 000102…0b、明文
    /// `{"todos":[{"id":"t1","title":"越南回访"}],"n":42}`。
    ///
    /// 两个方向都钉住：我们的实现要能造出这一份，也要能打开这一份。只测一个方向，
    /// 就等于假设了「tag 在末尾」这类格式约定不必验证 —— 而它正是两边能不能互通的全部。
    const PASS: &str = "pa55-word";
    const UID: &str = "u-abc-123";
    const IV: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    const PLAIN: &str = r#"{"todos":[{"id":"t1","title":"越南回访"}],"n":42}"#;
    const CIPHER: &str = "b349d16461a1a5b67ba43f148f9a746457087cd60eddc27f2bcf0b4033a7f19c\
367042517489d11774a50e0f960c462f94f2d414f43696aa71a64afec8a6f83478584b8566";

    #[test]
    fn the_ciphertext_matches_the_one_webcrypto_made() {
        // 密钥本身没法从 LessSafeKey 里读回来，所以用「造出同一份密文」间接钉住它：
        // 算法、盐、轮数、哈希任何一项不同，这一条都会挂。
        let sealed = seal_with_iv(PASS, UID, IV, PLAIN.as_bytes()).expect("seal");
        assert_eq!(sealed.iv, B64.encode(IV));
        assert_eq!(sealed.ct, CIPHER, "密文必须与 WebCrypto 逐字节一致");
    }

    #[test]
    fn the_ciphertext_webcrypto_made_opens_here() {
        let sealed = Sealed { iv: B64.encode(IV), ct: CIPHER.to_string() };
        let plain = open(PASS, UID, &sealed).expect("open");
        assert_eq!(String::from_utf8(plain).unwrap(), PLAIN);
    }

    #[test]
    fn another_passphrase_does_not_open_it() {
        let sealed = Sealed { iv: B64.encode(IV), ct: CIPHER.to_string() };
        assert_eq!(open("wrong", UID, &sealed).unwrap_err(), Fail::BadPass);
    }

    #[test]
    fn another_account_id_does_not_open_it() {
        // 盐里带着账号 id，所以同一句口令在另一个账号下解不开同一份密文。
        let sealed = Sealed { iv: B64.encode(IV), ct: CIPHER.to_string() };
        assert_eq!(open(PASS, "someone-else", &sealed).unwrap_err(), Fail::BadPass);
    }

    #[test]
    fn a_round_trip_survives_non_ascii_and_a_fresh_iv() {
        let text = "{\"标题\":\"客户回访\",\"note\":\"胡志明市那家\"}";
        let sealed = seal(PASS, UID, text.as_bytes()).expect("seal");
        // 随机 iv：两次加密不该给出同一个 iv（12 字节取自系统随机源）
        let again = seal(PASS, UID, text.as_bytes()).expect("seal");
        assert_ne!(sealed.iv, again.iv, "每次加密都要换 iv");
        assert_eq!(String::from_utf8(open(PASS, UID, &sealed).expect("open")).unwrap(), text);
    }

    #[test]
    fn a_short_iv_is_reported_as_a_broken_blob_not_a_bad_passphrase() {
        let sealed = Sealed { iv: B64.encode([1u8, 2, 3]), ct: CIPHER.to_string() };
        match open(PASS, UID, &sealed) {
            Err(Fail::Shape(_)) => {}
            other => panic!("形状错误要说形状，别怪口令：{other:?}"),
        }
    }

    #[test]
    fn only_network_failures_are_worth_retrying() {
        assert!(Fail::Net("502".into()).retryable());
        assert!(!Fail::BadPass.retryable(), "口令错了，重试一万次也还是错");
        assert!(!Fail::Shape("todos 不是列表".into()).retryable());
        assert!(!Fail::Local("随机数".into()).retryable());
    }
}
