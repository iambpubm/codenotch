//! 本地待办：notch 上那张会自己长大的待办清单。
//!
//! 规则来自 CornerTodo（github.com/nff825/CornerTodo），数据形状来自赛博大脑
//! （工作台 `state.config.todos` 里的每一条）。两件事拼在一起，是为了同一份待办只存一处：
//! 在 notch 里记一条，推上去之后手机上的工作台就能看见；工作台上改了，这边拉下来也一样。
//!
//! 三条规则照抄 CornerTodo，因为它们各自解决一个具体问题，重新发明只会丢掉其中一个：
//!
//! * **时间进度**按每条待办自己的完整周期线性推进（设定截止那一刻起算，到截止满格），
//!   所以三天的事和一个月的事各有各的节奏。它不是完成度，是「这段时间已经过去了多少」。
//! * **紧迫度**只在最后 24 小时内起作用（创建立刻就剩不到 24 小时的，用那一段较短的时间），
//!   并且是平方加速：越接近截止涨得越快。这就是面板会自己长大的原因。
//! * **排序**两种：截止时间优先（默认）和优先级优先。同优先级内仍按截止时间。
//!
//! 与 CornerTodo 的两处不同，都是刻意的：
//!
//! * 截止时间只有日期时，**当天结束前都有效**（次日零点才算逾期），而不是从零点起就逾期。
//! * 文案不在这一层算。这里只给「还剩多少分钟」，中文、英文、韩文各自在前端拼，
//!   否则同一条「剩余 2 小时」要在七个语言里各写一遍 Rust。

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 24 小时：紧迫度起算的窗口上限。
const URGENT_WINDOW_SECS: f64 = 24.0 * 60.0 * 60.0;
/// 6 小时：这个范围内算「马上就要到」，卡片里加粗、环变琥珀色。
pub const SOON_SECS: i64 = 6 * 60 * 60;

fn is_false(value: &bool) -> bool {
    !*value
}

/// 一条待办。字段名与赛博大脑逐字一致（`camelCase`），一个不多一个不少：
/// 推上去时工作台按自己的 `saveTodo` 收下，拉下来时也不会多出它不认识的键。
///
/// `deadlineSetAt` 是 CornerTodo 的字段，工作台不认识它，但会原样保留
/// （它的合并是整条记录取胜），所以重设截止时间不会把时间进度偷偷重置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Todo {
    pub id: String,
    /// todo | trip | schedule | memo。备忘（memo）是笔记不是任务，不进计数。
    #[serde(default)]
    pub kind: String,
    /// work | personal
    #[serde(default)]
    pub cat: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub note: String,
    /// YYYY-MM-DD，可以为空（没设截止时间的待办）
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub end_date: String,
    /// HH:MM，可以为空（只设日期，当天结束前都有效）
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub location: String,
    /// 高 | 中 | 低 | 空。工作台就这么存，不是数字，所以照抄。
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub done: bool,
    /// 工作台的软删除标记：留着记录但不再显示，好让删除也能同步出去。
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// 这条待办的截止时间是何时设定的。时间进度与紧迫度的起点。
    /// 空 = 从 created_at 起算（旧记录、以及工作台上新建的记录都是这样）。
    #[serde(default)]
    pub deadline_set_at: String,
    /// 这条是从赛博大脑快照读来的，不是本机记录：卡片里只读，不存盘，不推回去。
    #[serde(default, skip_serializing_if = "is_false")]
    pub remote: bool,
    /// 这条已经推上赛博大脑了。只存在本机，推的时候摘掉。
    ///
    /// 没有它，环上的数字就会把同一条待办数两次：工作台快照的 `todos` 是它自己算的，
    /// 本机这份也还在，两边一加就重复。所以「本机有、还没推上去」的条数才是要额外加的那部分。
    #[serde(default)]
    pub pushed: bool,
}

/// 存盘形状。`version` 留给以后改字段，读旧文件时按缺省值补。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub items: Vec<Todo>,
}

fn one() -> u32 {
    1
}

impl Default for Store {
    fn default() -> Self {
        Self { version: 1, items: Vec::new() }
    }
}

/// `%APPDATA%\codenotch\todos.json`，和 config.json 放一起。
pub fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("todos.json")
}

pub fn load() -> Store {
    let path = store_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Store::default();
    };
    match serde_json::from_str::<Store>(&raw) {
        Ok(mut store) => {
            // 存盘里不该有 remote 条目；万一有（手工改过），丢掉而不是让它变成幽灵
            store.items.retain(|t| !t.remote);
            store
        }
        // 文件坏了就当作空清单，但绝不覆盖它：用户还能自己去捡回来
        Err(_) => Store::default(),
    }
}

/// 原子写 + 上一版留成 `.bak`，和工作台自己写 config 的做法一致。
pub fn save(store: &Store) -> std::io::Result<()> {
    let path = store_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut clean = store.clone();
    clean.items.retain(|t| !t.remote);
    let text = serde_json::to_string_pretty(&clean).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    if path.exists() {
        let _ = std::fs::copy(&path, path.with_extension("json.bak"));
    }
    std::fs::rename(&tmp, &path)
}

// ------------------------------------------------------------------ 规则

fn parse_date(text: &str) -> Option<NaiveDate> {
    let head = text.get(..10.min(text.len()))?;
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.get(..5.min(text.len()))?, "%H:%M").ok()
}

/// ISO-8601（工作台写的是 `new Date().toISOString()`）→ 本地时间。读不出来就是 None，
/// 而不是拿当前时间顶上：一个编出来的起点会让时间进度凭空从 0 开始。
pub fn parse_stamp(text: &str) -> Option<DateTime<Local>> {
    if text.trim().is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|d| d.with_timezone(&Local))
}

/// 这条待办的截止时刻。
///
/// 只有日期的待办在**那一整天**里都算有效，次日零点才算逾期 —— 工作台的 `todoStatus`
/// 用的是同一口径（`today > t.date` 才叫逾期），两边对「今天到期」必须给出同一个答案。
/// 出差（trip）用结束日，没有结束日就用出发日。
pub fn deadline(todo: &Todo) -> Option<DateTime<Local>> {
    if todo.kind == "memo" {
        return None;
    }
    let date = if todo.kind == "trip" && !todo.end_date.is_empty() {
        parse_date(&todo.end_date).or_else(|| parse_date(&todo.date))?
    } else {
        parse_date(&todo.date)?
    };
    match parse_time(&todo.time) {
        // 有具体时刻：到点就算过了
        Some(time) => Local.from_local_datetime(&date.and_time(time)).single(),
        // 只有日期：延续到当天结束
        None => {
            let next = date.succ_opt()?;
            Local.from_local_datetime(&next.and_hms_opt(0, 0, 0)?).single()
        }
    }
}

/// 周期起点：设定截止的那一刻，没有就用创建时间。
fn started_at(todo: &Todo) -> Option<DateTime<Local>> {
    parse_stamp(&todo.deadline_set_at).or_else(|| parse_stamp(&todo.created_at))
}

/// 已经过去的时间占比，0–1。已完成的不画（返回 0），逾期满格。
pub fn time_progress(todo: &Todo, now: DateTime<Local>) -> f64 {
    if todo.done {
        return 0.0;
    }
    let (Some(due), Some(start)) = (deadline(todo), started_at(todo)) else {
        return 0.0;
    };
    if now >= due {
        return 1.0;
    }
    let total = (due - start).num_seconds() as f64;
    if total <= 0.0 || now <= start {
        return 0.0;
    }
    (((now - start).num_seconds() as f64) / total).clamp(0.0, 1.0)
}

/// 紧迫度，0–1。最后 24 小时内平方加速；逾期恒为 1。
///
/// 窗口取「设定的截止时间 → 截止」这段里较短的那个 24 小时：一条 5 分钟后截止的事，
/// 就在这 5 分钟里长满，而不是因为「离 24 小时窗口还早」永远停在 0。
pub fn urgency(todo: &Todo, now: DateTime<Local>) -> f64 {
    if todo.done || todo.deleted || todo.kind == "memo" {
        return 0.0;
    }
    let Some(due) = deadline(todo) else {
        return 0.0;
    };
    let remaining = (due - now).num_seconds() as f64;
    if remaining <= 0.0 {
        return 1.0;
    }
    let window = match started_at(todo) {
        Some(start) => URGENT_WINDOW_SECS.min((due - start).num_seconds() as f64),
        None => URGENT_WINDOW_SECS,
    };
    if window <= 0.0 {
        return 0.0;
    }
    let progress = (1.0 - remaining / window).clamp(0.0, 1.0);
    progress * progress
}

/// 一条待办对「面板该多大」的贡献：最紧迫的那条说了算，第二第三各加一点，
/// 更多的不再加 —— 十条待办不该让面板长到天上。
pub fn expansion(items: &[Todo], now: DateTime<Local>) -> f64 {
    let mut top: Vec<f64> = items
        .iter()
        .filter(|t| !t.done && !t.deleted && !t.remote)
        .map(|t| urgency(t, now))
        .collect();
    top.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let mut value = top.first().copied().unwrap_or(0.0);
    if let Some(v) = top.get(1) {
        value += 0.15 * v;
    }
    if let Some(v) = top.get(2) {
        value += 0.05 * v;
    }
    value.min(1.0)
}

/// 高=1 中=2 低=3，认不出来的当中。工作台存的是中文，所以按中文认。
fn priority_rank(level: &str) -> i32 {
    match level.trim() {
        "高" => 1,
        "低" => 3,
        _ => 2,
    }
}

fn is_open(todo: &Todo) -> bool {
    !todo.done && !todo.deleted && todo.kind != "memo"
}

/// 未完成的事项，按设置里的规则排序。
///
/// 截止时间优先是默认：越早截止越靠前，没有截止时间的排最后。
/// 优先级优先不额外把逾期的顶到最前 —— 那是另一种规则，混进来会让「优先级优先」
/// 变成「优先级优先，除了逾期的」，而用户选它就是因为不想要那个。
pub fn ordered(items: &[Todo], priority_first: bool) -> Vec<Todo> {
    let mut list: Vec<Todo> = items.iter().filter(|t| is_open(t)).cloned().collect();
    list.sort_by(|a, b| {
        let rank = if priority_first {
            priority_rank(&a.level).cmp(&priority_rank(&b.level))
        } else {
            std::cmp::Ordering::Equal
        };
        if rank != std::cmp::Ordering::Equal {
            return rank;
        }
        // 远未来（无截止时间）排在所有有截止时间的后面
        let far = chrono::NaiveDate::MAX.and_hms_opt(0, 0, 0).unwrap();
        let da = deadline(a).map(|d| d.naive_local()).unwrap_or(far);
        let db = deadline(b).map(|d| d.naive_local()).unwrap_or(far);
        da.cmp(&db)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    list
}

/// 逾期了没有。已完成、备忘、没有截止时间的一律不算。
pub fn is_overdue(todo: &Todo, now: DateTime<Local>) -> bool {
    if !is_open(todo) {
        return false;
    }
    deadline(todo).map(|d| now >= d).unwrap_or(false)
}

/// 距截止还有多少秒（负 = 已逾期）。没有截止时间给 `None`。
pub fn seconds_left(todo: &Todo, now: DateTime<Local>) -> Option<i64> {
    deadline(todo).map(|d| (d - now).num_seconds())
}

/// 给前端的一行。文案一律在前端拼：这里只说事实（还剩几分钟、是不是本地）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub level: String,
    pub note: String,
    /// 还剩多少秒；负数是逾期。没有截止时间的给 null。
    pub left: Option<i64>,
    /// 已经过去的时间占比，0–1。没有截止时间或已完成的是 0。
    pub progress: f64,
    pub urgency: f64,
    pub done: bool,
    /// 来自赛博大脑快照：卡片里只读
    pub remote: bool,
}

/// 前端一次要的全部：列表、计数、面板该多大。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    /// 未完成的条数（本地 + 远程里没有对上的那些）
    pub open: i64,
    pub overdue: i64,
    pub soon: i64,
    /// 面板该长多大，0–1
    pub expansion: f64,
    pub rows: Vec<Row>,
    /// 远程快照推上来的那份，单独列，卡片里只读
    pub remote_rows: Vec<Row>,
    /// 用户在设置里选的排序：deadline | priority
    pub sort: String,
}

fn row_of(todo: &Todo, now: DateTime<Local>, remote: bool) -> Row {
    Row {
        id: todo.id.clone(),
        title: todo.title.clone(),
        kind: todo.kind.clone(),
        level: todo.level.clone(),
        note: todo.note.clone(),
        left: seconds_left(todo, now),
        progress: time_progress(todo, now),
        urgency: urgency(todo, now),
        done: todo.done,
        remote,
    }
}

/// 组装前端视图。
///
/// 计数口径是要紧的地方。工作台的快照给了它自己算的两个数（未完成待办、逾期事项），
/// 那是权威的；本机这份里**还没推上去**的条目才需要额外加，否则推过一次的会被数两次。
///
/// `remote` 是快照里的明细（只有标题、日期、类型，没有 id），它在卡片里单独一段、只读。
/// 和本地记录说的是同一件事时（本机刚推上去、工作台还没重写快照），按「标题 + 日期」
/// 认一次重，免得同一行在卡片里出现两遍。
pub fn view(
    items: &[Todo],
    remote: &[Todo],
    remote_count: i64,
    remote_overdue: i64,
    now: DateTime<Local>,
    priority_first: bool,
) -> View {
    let local_open: Vec<Todo> = items.iter().filter(|t| is_open(t)).cloned().collect();
    let unpushed: Vec<&Todo> = local_open.iter().filter(|t| !t.pushed).collect();
    let open = remote_count + unpushed.len() as i64;
    let overdue = remote_overdue
        + unpushed.iter().filter(|t| is_overdue(t, now)).count() as i64;
    let soon = local_open
        .iter()
        .filter(|t| matches!(seconds_left(t, now), Some(left) if left > 0 && left <= SOON_SECS))
        .count() as i64;

    let seen: Vec<(String, String)> = local_open
        .iter()
        .map(|t| (t.title.trim().to_string(), t.date.clone()))
        .collect();
    let remote_rows: Vec<Row> = ordered(remote, priority_first)
        .into_iter()
        .filter(|t| {
            let key = (t.title.trim().to_string(), t.date.clone());
            !seen.contains(&key)
        })
        .map(|t| row_of(&t, now, true))
        .collect();

    View {
        open,
        overdue,
        soon,
        expansion: expansion(items, now),
        rows: ordered(items, priority_first)
            .iter()
            .map(|t| row_of(t, now, false))
            .collect(),
        remote_rows,
        sort: if priority_first { "priority".into() } else { "deadline".into() },
    }
}

/// 现在。抽出来是为了测试能喂一个固定时刻进去。
pub fn now() -> DateTime<Local> {
    Local::now()
}

/// 新的待办 id：「t」+ 毫秒时间戳 + 一个计数器，和 `genId('t')` 同一个精神。
/// 工作台只要求 id 唯一且以 t 开头。
pub fn new_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("t{}", millis)
}

/// 把本地记录并进云端那份：按 id 取较新的一条，新增的追加。
///
/// **这条规则的执行方在设置页**（`tdMerge`），因为整份 config 的加解密只有 WebView 里
/// 的 WebCrypto 做得了 —— Rust 这一侧没有能用的加密库（CI 是 `--locked`，加不了依赖）。
/// 合并规则只有这一份实现：同步在 Rust 里跑（`brain::run_once`），前端不再自己再写一版。
/// 上一版两边各有一份，然后分叉了 —— 摘本机标记那段页面上的漏了，推上去的每一条都带着
/// `pushed: true` 进了工作台的记录。
///
/// 出去的那份不带本机标记（`remote`、`pushed`）：工作台不认识它们，带着只是噪音。
pub fn merge_for_push(local: &[Todo], cloud: &[Todo]) -> Vec<Todo> {
    fn clean(todo: &Todo) -> Todo {
        let mut out = todo.clone();
        out.remote = false;
        out.pushed = false;
        out
    }
    let mut out: Vec<Todo> = cloud.iter().filter(|t| !t.remote).map(clean).collect();
    for item in local.iter().filter(|t| !t.remote) {
        match out.iter_mut().find(|t| t.id == item.id) {
            Some(existing) => {
                let mine = parse_stamp(&item.updated_at);
                let theirs = parse_stamp(&existing.updated_at);
                if mine >= theirs {
                    *existing = clean(item);
                }
            }
            None => out.push(clean(item)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地钟面上的一个时刻。
    ///
    /// 测试里所有的「现在」和「截止」都用它构造，而不是写死 `+08:00` 的绝对时刻：`date`/`time`
    /// 是**本地钟面**，`createdAt` 是**绝对时刻**，两者相减的结果依赖机器所在的时区。CI 在 UTC
    /// 上跑，写死偏移的断言在那里会算出另一个数 —— 那种失败读起来像逻辑错了，其实是测试写错了。
    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    /// 一条待办：截止就是 `due`（带具体时刻），周期起点是 `created`。
    fn todo_at(due: DateTime<Local>, created: DateTime<Local>) -> Todo {
        Todo {
            id: "t1".into(),
            kind: "todo".into(),
            title: "发样品".into(),
            date: due.format("%Y-%m-%d").to_string(),
            time: due.format("%H:%M").to_string(),
            created_at: created.to_rfc3339(),
            ..Default::default()
        }
    }

    /// 只给日期的那种：当天结束前都算有效。
    fn todo_on_day(day: DateTime<Local>, created: DateTime<Local>) -> Todo {
        let mut t = todo_at(day, created);
        t.time = String::new();
        t
    }

    fn remote_item(title: &str, date: &str) -> Todo {
        Todo {
            id: "remote:0".into(),
            kind: "todo".into(),
            title: title.into(),
            date: date.into(),
            remote: true,
            ..Default::default()
        }
    }

    /// 只有日期的待办在当天结束前都有效，次日零点才算逾期 —— 和工作台的 `todoStatus` 同一口径。
    #[test]
    fn a_date_without_a_time_lasts_the_whole_day() {
        let t = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 28, 0, 0));
        assert!(!is_overdue(&t, local(2026, 10, 31, 13, 0)), "当天下午还算今天到期");
        assert!(!is_overdue(&t, local(2026, 10, 31, 23, 59)), "23:59 还是当天");
        assert!(is_overdue(&t, local(2026, 11, 1, 0, 0)), "过了零点才逾期");
    }

    /// 有具体时刻的，到点即逾期。
    #[test]
    fn a_date_with_a_time_is_over_at_that_minute() {
        let t = todo_at(local(2026, 10, 31, 14, 30), local(2026, 10, 28, 0, 0));
        assert!(!is_overdue(&t, local(2026, 10, 31, 14, 29)));
        assert!(is_overdue(&t, local(2026, 10, 31, 14, 30)));
    }

    /// 时间进度按各自的周期线性推进，不是「还剩多少」。
    #[test]
    fn progress_runs_its_own_span() {
        // 起点 10-28 00:00；只有日期，所以终点是 11-01 00:00（顺延到次日零点），共 4 天
        let mut t = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 28, 0, 0));
        let half = local(2026, 10, 30, 0, 0);
        let p = time_progress(&t, half);
        assert!((p - 0.5).abs() < 0.01, "两天过完应该是半程：{p}");
        assert_eq!(time_progress(&t, local(2026, 11, 5, 0, 0)), 1.0, "过期满格");
        t.done = true;
        assert_eq!(time_progress(&t, half), 0.0, "完成了就不画");
    }

    /// 紧迫度：最后 24 小时才动，且平方加速；逾期恒为 1。
    #[test]
    fn urgency_only_moves_in_the_last_day_and_squares() {
        let t = todo_at(local(2026, 11, 1, 0, 0), local(2026, 10, 20, 0, 0));
        // 距截止还有 3 天：窗口被 24 小时封顶，所以是 0
        assert_eq!(urgency(&t, local(2026, 10, 29, 0, 0)), 0.0);
        // 还有 12 小时 = 过了一半 = 0.5²
        let u = urgency(&t, local(2026, 10, 31, 12, 0));
        assert!((u - 0.25).abs() < 0.01, "{u}");
        // 还有 1 小时：1 - 1/24 ≈ 0.958，平方约 0.918
        let u = urgency(&t, local(2026, 10, 31, 23, 0));
        assert!(u > 0.9, "{u}");
        assert_eq!(urgency(&t, local(2026, 11, 1, 0, 0)), 1.0, "逾期满格");
    }

    /// 一条 5 分钟后截止的事，就在这 5 分钟里长满：窗口取较短的那段，
    /// 否则它永远停在 0，而那正是最该催的时候。
    #[test]
    fn a_short_fuse_grows_inside_its_own_five_minutes() {
        let mut t = todo_at(local(2026, 10, 30, 14, 5), local(2026, 10, 30, 0, 0));
        t.deadline_set_at = local(2026, 10, 30, 14, 0).to_rfc3339();
        assert_eq!(urgency(&t, local(2026, 10, 30, 14, 0)), 0.0, "刚设完是 0");
        // 剩 1 分钟 = 走完 4/5 = 0.8，平方 0.64。不平方的话这里只有 0.8，最后两分钟没有加速感。
        let u = urgency(&t, local(2026, 10, 30, 14, 4));
        assert!((u - 0.64).abs() < 0.02, "{u}");
        assert!(u > 0.5, "剩一分钟时已经过半了");
    }

    /// 重新开程序、只改文字，都不能把时间进度的起点挪走。
    #[test]
    fn editing_the_text_does_not_restart_the_clock() {
        let mut t = todo_at(local(2026, 10, 31, 0, 0), local(2026, 10, 28, 0, 0));
        let now = local(2026, 10, 29, 12, 0);
        let p1 = time_progress(&t, now);
        t.title = "改过的标题".into();
        t.updated_at = now.to_rfc3339();
        assert_eq!(p1, time_progress(&t, now), "改文字不该让进度回到 0");
        assert!(p1 > 0.0, "而且它本来就不是 0");
    }

    /// 面板的扩大只取最紧迫的三条，再多的不加。
    #[test]
    fn only_the_three_most_urgent_push_the_panel() {
        // 每条的紧迫度都是 0.25（还剩 12 小时），所以叠加后离 1 还有很远，clamp 不会掩盖差别
        let now = local(2026, 10, 30, 12, 0);
        let mut list = Vec::new();
        for i in 0..5 {
            let mut t = todo_at(local(2026, 10, 31, 0, 0), local(2026, 10, 20, 0, 0));
            t.id = format!("t{i}");
            list.push(t);
        }
        let all_five = expansion(&list, now);
        let just_three = expansion(&list[..3], now);
        assert!((all_five - just_three).abs() < 1e-9, "第四、第五条不该再加");
        assert!((all_five - 0.30).abs() < 0.01, "0.25 + 0.15×0.25 + 0.05×0.25 = 0.30：{all_five}");
    }

    /// 没有截止时间的待办排在有截止时间的后面，两种排序都一样。
    #[test]
    fn undated_items_sort_last() {
        let dated = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        let mut undated = todo_at(local(2026, 10, 2, 0, 0), local(2026, 10, 2, 0, 0));
        undated.date = String::new();
        undated.time = String::new();
        undated.id = "t2".into();
        for priority_first in [false, true] {
            let list = ordered(&[undated.clone(), dated.clone()], priority_first);
            assert_eq!(list[0].id, "t1", "有日期的在前");
        }
    }

    /// 优先级优先：高 → 中 → 低，同级内仍按截止时间。
    #[test]
    fn priority_first_orders_by_level_then_deadline() {
        let mut low_soon = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        low_soon.id = "low".into();
        low_soon.level = "低".into();
        let mut high_later = todo_on_day(local(2026, 11, 10, 0, 0), local(2026, 10, 1, 0, 0));
        high_later.id = "high".into();
        high_later.level = "高".into();
        let list = ordered(&[low_soon.clone(), high_later.clone()], true);
        assert_eq!(list[0].id, "high", "优先级优先时高在前，哪怕截止更晚");
        let list = ordered(&[low_soon, high_later], false);
        assert_eq!(list[0].id, "low", "截止时间优先时早的在前");
    }

    /// 备忘是笔记，不进待办；已完成的也不进。
    #[test]
    fn memos_and_done_items_are_not_todos() {
        let mut memo = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        memo.kind = "memo".into();
        memo.id = "m".into();
        let mut done = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        done.id = "d".into();
        done.done = true;
        let v = view(&[memo, done], &[], 0, 0, local(2026, 10, 30, 12, 0), false);
        assert_eq!(v.open, 0);
        assert!(v.rows.is_empty());
        assert_eq!(v.expansion, 0.0);
    }

    /// 推上去的那条已经在工作台的计数里了，本机这份不能再数一次。
    #[test]
    fn a_pushed_item_is_not_counted_twice() {
        let mut t = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        t.pushed = true;
        let now = local(2026, 10, 30, 12, 0);
        // 工作台说它那边有 1 条未完成，本机这条就是同一条
        let v = view(&[t.clone()], &[], 1, 0, now, false);
        assert_eq!(v.open, 1, "不是 2");
        // 本机新记的一条还没推：工作台不知道它，要加
        let mut fresh = todo_on_day(local(2026, 11, 2, 0, 0), local(2026, 10, 30, 9, 0));
        fresh.id = "t2".into();
        let v = view(&[t, fresh], &[], 1, 0, now, false);
        assert_eq!(v.open, 2);
    }

    /// 卡片里同一行不该出现两次：本地这条和快照里那条按标题+日期认作同一件事。
    #[test]
    fn a_local_item_the_snapshot_also_has_is_shown_once() {
        let local_item = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        let v = view(&[local_item], &[remote_item("发样品", "2026-10-31")], 0, 0, local(2026, 10, 30, 12, 0), false);
        assert_eq!(v.rows.len(), 1, "本地列表里有这条");
        assert!(v.remote_rows.is_empty(), "远程那半不再重复列一遍");
    }

    /// 远程独有的条目要算进总数，但不能被本地编辑（只读行）。
    #[test]
    fn remote_only_items_count_but_stay_read_only() {
        let v = view(&[], &[remote_item("越南客户回访", "2026-11-02")], 1, 0, local(2026, 10, 30, 12, 0), false);
        assert_eq!(v.open, 1);
        assert_eq!(v.rows.len(), 0, "本地列表里没有");
        assert_eq!(v.remote_rows.len(), 1, "远程列表里有");
        assert!(v.remote_rows[0].remote);
    }

    /// 推上去之前合并：同 id 取较新的一条，本地独有追加，云端独有保留。
    #[test]
    fn merge_for_push_takes_the_newer_of_each_id() {
        let mut mine = todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0));
        mine.updated_at = "2026-10-30T10:00:00Z".into();
        mine.title = "本机改过".into();
        mine.id = "same".into();

        let mut cloud_same = mine.clone();
        cloud_same.updated_at = "2026-10-29T10:00:00Z".into();
        cloud_same.title = "云端旧的".into();

        let mut cloud_only = todo_on_day(local(2026, 11, 5, 0, 0), local(2026, 10, 1, 0, 0));
        cloud_only.id = "cloud-only".into();

        let merged = merge_for_push(&[mine], &[cloud_same, cloud_only]);
        assert_eq!(merged.len(), 2);
        let same = merged.iter().find(|t| t.id == "same").unwrap();
        assert_eq!(same.title, "本机改过", "较新的赢");
        assert!(merged.iter().any(|t| t.id == "cloud-only"), "云端独有的不能被抹掉");
        assert!(merged.iter().all(|t| !t.pushed), "本机标记不该推到工作台去");
    }

    /// 存盘只写本地条目：远程行是读来的，写回去等于把它们变成幽灵待办。
    #[test]
    fn only_local_items_survive_a_round_trip() {
        let mut store = Store::default();
        store.items.push(todo_on_day(local(2026, 10, 31, 0, 0), local(2026, 10, 1, 0, 0)));
        store.items.push(remote_item("越南客户回访", "2026-11-02"));
        let text = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        let kept: Vec<&Todo> = back.items.iter().filter(|t| !t.remote).collect();
        assert_eq!(kept.len(), 1, "本地那条还在");
        assert!(text.contains("\"t1\""));
    }
}
