use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Timelike, Duration};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_yaml;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use luo9_sdk::Bot;
use luo9_sdk::bus::Bus;
use reqwest::blocking::Client;

// ── 常量 ──────────────────────────────────────────────────

/// 最短有效直播时长（秒），低于此值视为异常不计入统计
const MIN_SESSION_SECS: u64 = 180;

/// 最大连续 API 失败次数，超过才判定状态变化
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// ── 全局状态 ──────────────────────────────────────────────

static CONFIG: OnceCell<LiveMonitorConfig> = OnceCell::new();
static DATA_DIR: OnceCell<PathBuf> = OnceCell::new();

// ── 配置结构体 ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveMonitorConfig {
    pub admin: u64,
    pub rooms: Vec<LiveRoom>,
    pub push_groups: Vec<u64>,
    /// 是否推送开播通知（默认 true）
    #[serde(default = "default_true")]
    pub push_on_start: bool,
    /// 是否推送下播通知（默认 false）
    #[serde(default = "default_false")]
    pub push_on_end: bool,
    /// 报告 API 地址，用于推送直播数据生成周报/月报等（可选，不填则不推送）
    #[serde(default)]
    pub report_api_url: Option<String>,
    /// 是否启用报告推送（默认 false）
    #[serde(default = "default_false")]
    pub report_push_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveRoom {
    pub room_id: u64,
    pub name: String,
}

impl LiveMonitorConfig {
    pub fn get() -> &'static LiveMonitorConfig {
        CONFIG.get().expect("LiveMonitorConfig not initialized, call init() first")
    }

    pub fn admin() -> u64 {
        Self::get().admin
    }

    pub fn push_groups() -> &'static [u64] {
        &Self::get().push_groups
    }

    pub fn rooms() -> &'static [LiveRoom] {
        &Self::get().rooms
    }
}

impl Default for LiveMonitorConfig {
    fn default() -> Self {
        Self {
            admin: 123456,
            rooms: vec![
                LiveRoom {
                    room_id: 123456,
                    name: "土豆".into(),
                }
            ],
            push_groups: vec![123456789],
            push_on_start: true,
            push_on_end: false,
            report_api_url: None,
            report_push_enabled: false,
        }
    }
}

const DEFAULT_CONFIG_YAML: &str = r#"# B站直播监控配置
admin: 123456
rooms:
  - room_id: 123456          # 直播间ID
    name: "土豆"              # 主播名称
push_groups:
  - 123456789                # 推送的群号

# 推送开关（默认 true，设为 false 可关闭对应通知）
push_on_start: true          # 开播推送
push_on_end: false           # 下播推送（含详细统计）

# 报告 API 配置（用于生成周报/月报/年度总结，可选）
# report_api_url: "http://your-api.example.com/api/report"
# report_push_enabled: false
"#;

// ── 数据模型 ──────────────────────────────────────────────

/// 一次完整的直播记录（session.json 的每一行）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveSession {
    pub room_id: u64,
    pub start: String,
    pub end: String,
    pub duration_secs: u64,
    pub weekday: u32,
    pub start_hour: u32,
    pub end_hour: u32,
}

/// 运行时状态（防止重启丢失当前直播信息）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RuntimeState {
    pub room_id: u64,
    pub is_live: bool,
    pub current_start: Option<String>,
    pub consecutive_failures: u32,
}

impl RuntimeState {
    fn new(room_id: u64) -> Self {
        Self {
            room_id,
            is_live: false,
            current_start: None,
            consecutive_failures: 0,
        }
    }
}

/// 预聚合缓存（启动时可重算，不怕损坏）
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Aggregate {
    pub total_sessions: u32,
    pub total_seconds: u64,
    pub longest_session_secs: u64,
    pub longest_session_date: Option<String>,
    pub average_session_secs: f64,
    pub average_start_minutes: Option<u32>,
}

// ── 公开初始化函数 ───────────────────────────────────────

pub fn init() {
    let data_path = crate::path::to_absolute(
        &PathBuf::from("data").join("plugin_potato_live")
    );
    fs::create_dir_all(&data_path).ok();
    let _ = DATA_DIR.set(data_path.clone());

    let config_path = data_path.join("config.yaml");
    if !config_path.exists() {
        fs::write(&config_path, DEFAULT_CONFIG_YAML).ok();
    }

    let config: LiveMonitorConfig = match fs::read_to_string(&config_path) {
        Ok(content) => serde_yaml::from_str(&content)
            .unwrap_or_else(|_| LiveMonitorConfig::default()),
        Err(_) => LiveMonitorConfig::default(),
    };
    let _ = CONFIG.set(config);

    // 启动时恢复运行状态 & 重算聚合
    let config = CONFIG.get().expect("CONFIG not initialized");
    for room in &config.rooms {
        let runtime = load_runtime_state(room.room_id);
        if runtime.is_live {
            tracing::info!(
                "[live_monitor] 检测到未正常结束的直播: room={} (开始于 {})",
                room.room_id,
                runtime.current_start.as_deref().unwrap_or("unknown")
            );
        }
        // 重算聚合缓存
        recompute_aggregate(room.room_id);
    }

    register_schedule_tasks();
}

// ── 定时任务注册 ─────────────────────────────────────────

fn register_schedule_tasks() {
    let config = CONFIG.get().expect("CONFIG not initialized");

    for room in &config.rooms {
        let req = serde_json::json!({
            "action": "schedule",
            "task_name": format!("bilibili_live_{}", room.room_id),
            "cron": "0 */1 * * * *",
            "payload": serde_json::json!({
                "room_id": room.room_id,
                "name": room.name
            }).to_string()
        });
        let _ = Bus::topic("luo9_task_miso").publish(&req.to_string());
        tracing::info!(
            "[live_monitor] 已注册定时任务: {} ({})",
            room.name, room.room_id
        );
    }
}

// ── 任务事件处理 ─────────────────────────────────────────

pub fn handle_task_event(json: &str) {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    let task_name = event["task_name"].as_str().unwrap_or("");

    if task_name.starts_with("bilibili_live_") {
        let payload_str = event["payload"].as_str().unwrap_or("");
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload_str) else {
            return;
        };
        let room_id = payload["room_id"].as_u64().unwrap_or(0);
        let name = payload["name"].as_str().unwrap_or("unknown").to_string();

        check_and_notify(room_id, &name);
    }
}

// ── 文件路径辅助 ─────────────────────────────────────────

fn data_dir() -> &'static PathBuf {
    DATA_DIR.get().expect("DATA_DIR not set")
}

fn session_file_path(room_id: u64) -> PathBuf {
    data_dir().join(format!("sessions_{}.json", room_id))
}

fn runtime_state_path(room_id: u64) -> PathBuf {
    data_dir().join(format!("runtime_state_{}.json", room_id))
}

fn aggregate_path(room_id: u64) -> PathBuf {
    data_dir().join(format!("aggregate_{}.json", room_id))
}

// ── Session 文件 I/O ─────────────────────────────────────

fn load_sessions(room_id: u64) -> Vec<LiveSession> {
    let path = session_file_path(room_id);
    if !path.exists() {
        return Vec::new();
    }

    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = std::io::BufReader::new(file);
    let mut sessions = Vec::new();

    for line in reader.lines() {
        if let Ok(line) = line {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(session) = serde_json::from_str::<LiveSession>(trimmed) {
                sessions.push(session);
            }
        }
    }

    sessions
}

fn append_session(session: &LiveSession) {
    let path = session_file_path(session.room_id);
    let line = serde_json::to_string(session).unwrap_or_default();
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{}", line);
    }
}

// ── Runtime State I/O ────────────────────────────────────

fn load_runtime_state(room_id: u64) -> RuntimeState {
    let path = runtime_state_path(room_id);
    fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<RuntimeState>(&content).ok())
        .unwrap_or_else(|| RuntimeState::new(room_id))
}

fn save_runtime_state(state: &RuntimeState) {
    let path = runtime_state_path(state.room_id);
    if let Ok(content) = serde_json::to_string_pretty(state) {
        let _ = fs::write(&path, content);
    }
}

// ── Aggregate I/O ────────────────────────────────────────

fn load_aggregate(room_id: u64) -> Aggregate {
    let path = aggregate_path(room_id);
    fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<Aggregate>(&content).ok())
        .unwrap_or_default()
}

fn save_aggregate(aggregate: &Aggregate, room_id: u64) {
    let path = aggregate_path(room_id);
    if let Ok(content) = serde_json::to_string_pretty(aggregate) {
        let _ = fs::write(&path, content);
    }
}

/// 从 session 日志重算聚合缓存
fn recompute_aggregate(room_id: u64) {
    let sessions = load_sessions(room_id);
    if sessions.is_empty() {
        save_aggregate(&Aggregate::default(), room_id);
        return;
    }

    let total_sessions = sessions.len() as u32;
    let total_seconds: u64 = sessions.iter().map(|s| s.duration_secs).sum();

    let longest = sessions.iter()
        .max_by_key(|s| s.duration_secs)
        .map(|s| (s.duration_secs, s.start.clone()));

    let avg_secs = if total_sessions > 0 {
        total_seconds as f64 / total_sessions as f64
    } else {
        0.0
    };

    let avg_start = compute_average_start_minutes(&sessions);

    let agg = Aggregate {
        total_sessions,
        total_seconds,
        longest_session_secs: longest.as_ref().map(|(secs, _)| *secs).unwrap_or(0),
        longest_session_date: longest.map(|(_, date)| date),
        average_session_secs: avg_secs,
        average_start_minutes: avg_start,
    };

    save_aggregate(&agg, room_id);
}

// ── 跨日拆分 ─────────────────────────────────────────────

/// 将一次直播按自然日拆分成多段，返回 (日期, 该日时长秒数)
fn split_session_by_day(start: &DateTime<Local>, end: &DateTime<Local>) -> Vec<(NaiveDate, u64)> {
    if start >= end {
        return Vec::new();
    }

    let start_date = start.date_naive();
    let end_date = end.date_naive();

    if start_date == end_date {
        let duration = (*end - *start).num_seconds() as u64;
        return vec![(start_date, duration)];
    }

    let mut result = Vec::new();

    // 第一天：从 start 到当天 23:59:59
    let first_day_end = start_date
        .and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        .and_local_timezone(Local)
        .unwrap();
    let first_duration = (first_day_end - *start).num_seconds() as u64 + 1;
    result.push((start_date, first_duration));

    // 中间完整天
    let mut current = start_date + Duration::days(1);
    while current < end_date {
        result.push((current, 86400));
        current = current + Duration::days(1);
    }

    // 最后一天：从当天 00:00:00 到 end
    let last_day_start = end_date
        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_local_timezone(Local)
        .unwrap();
    let last_duration = (*end - last_day_start).num_seconds() as u64;
    if last_duration > 0 {
        result.push((end_date, last_duration));
    }

    result
}

// ── 统计计算 ─────────────────────────────────────────────

/// 计算本周内有直播行为的不同天数
fn compute_weekly_live_days(sessions: &[LiveSession]) -> u32 {
    let today = Local::now().date_naive();
    let weekday = today.weekday().num_days_from_monday();
    let week_start = today - Duration::days(weekday as i64);

    let days: HashSet<NaiveDate> = sessions.iter()
        .flat_map(|s| get_session_dates(s))
        .filter(|d| *d >= week_start && *d <= today)
        .collect();

    days.len() as u32
}

/// 计算本周累计直播秒数
fn compute_weekly_seconds(sessions: &[LiveSession]) -> u64 {
    let today = Local::now().date_naive();
    let weekday = today.weekday().num_days_from_monday();
    let week_start = today - Duration::days(weekday as i64);

    sessions.iter()
        .filter_map(|s| {
            let start = parse_datetime(&s.start)?;
            let end = parse_datetime(&s.end)?;
            Some((start, end, s.duration_secs))
        })
        .flat_map(|(start, end, _)| split_session_by_day(&start, &end))
        .filter(|(date, _)| *date >= week_start && *date <= today)
        .map(|(_, secs)| secs)
        .sum()
}

/// 计算本月内有直播行为的不同天数
fn compute_monthly_live_days(sessions: &[LiveSession]) -> u32 {
    let today = Local::now().date_naive();
    let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();

    let days: HashSet<NaiveDate> = sessions.iter()
        .flat_map(|s| get_session_dates(s))
        .filter(|d| *d >= month_start && *d <= today)
        .collect();

    days.len() as u32
}

/// 计算本月累计直播秒数
fn compute_monthly_seconds(sessions: &[LiveSession]) -> u64 {
    let today = Local::now().date_naive();
    let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();

    sessions.iter()
        .filter_map(|s| {
            let start = parse_datetime(&s.start)?;
            let end = parse_datetime(&s.end)?;
            Some((start, end, s.duration_secs))
        })
        .flat_map(|(start, end, _)| split_session_by_day(&start, &end))
        .filter(|(date, _)| *date >= month_start && *date <= today)
        .map(|(_, secs)| secs)
        .sum()
}

/// 计算今日累计直播秒数
fn compute_today_seconds(sessions: &[LiveSession]) -> u64 {
    let today = Local::now().date_naive();

    sessions.iter()
        .filter_map(|s| {
            let start = parse_datetime(&s.start)?;
            let end = parse_datetime(&s.end)?;
            Some((start, end, s.duration_secs))
        })
        .flat_map(|(start, end, _)| split_session_by_day(&start, &end))
        .filter(|(date, _)| *date == today)
        .map(|(_, secs)| secs)
        .sum()
}

/// 计算连续开播天数（从今天往前推）
fn compute_streak(sessions: &[LiveSession]) -> u32 {
    let mut live_dates: Vec<NaiveDate> = sessions.iter()
        .flat_map(|s| get_session_dates(s))
        .collect();
    live_dates.sort();
    live_dates.dedup();

    if live_dates.is_empty() {
        return 0;
    }

    let today = Local::now().date_naive();
    let mut streak = 0;
    let mut current = today;

    // 从今天开始往前数
    for date in live_dates.iter().rev() {
        if *date == current || *date == current - Duration::days(1) {
            if *date == current - Duration::days(1) {
                current = *date;
                streak += 1;
            } else if *date == current {
                streak += 1;
                current = *date - Duration::days(1);
            }
        } else if *date < current - Duration::days(1) {
            break;
        }
    }

    streak
}

/// 计算平均开播时间（分钟从午夜开始算）
fn compute_average_start_minutes(sessions: &[LiveSession]) -> Option<u32> {
    if sessions.is_empty() {
        return None;
    }

    let total_minutes: u64 = sessions.iter()
        .filter_map(|s| {
            let dt = parse_datetime(&s.start)?;
            Some(dt.hour() as u64 * 60 + dt.minute() as u64)
        })
        .sum();

    let count = sessions.len() as u64;
    if count == 0 {
        return None;
    }

    Some((total_minutes / count) as u32)
}

/// 格式化秒数为 "X.Xh" 格式
fn fmt_hours(secs: u64) -> String {
    let hours = secs as f64 / 3600.0;
    format!("{:.1}h", hours)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.date_naive())
}

fn parse_datetime(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

/// 格式化平均开播时间
fn fmt_average_start_time(minutes: u32) -> String {
    let hour = minutes / 60;
    let minute = minutes % 60;
    format!("{:02}:{:02}", hour, minute)
}

// ── 报告数据构建 & API 请求 ─────────────────────────────

/// 获取一场直播在指定日期中的秒数（跨日时取当天的占比）
fn get_session_day_seconds(session: &LiveSession, date: NaiveDate) -> u64 {
    let start = match parse_datetime(&session.start) {
        Some(dt) => dt,
        None => return 0,
    };
    let end = match parse_datetime(&session.end) {
        Some(dt) => dt,
        None => return 0,
    };
    split_session_by_day(&start, &end)
        .into_iter()
        .find(|(d, _)| *d == date)
        .map(|(_, secs)| secs)
        .unwrap_or(0)
}

/// 获取一场直播覆盖的所有自然日（处理跨日情况）
fn get_session_dates(session: &LiveSession) -> Vec<NaiveDate> {
    let start = match parse_datetime(&session.start) {
        Some(dt) => dt,
        None => return Vec::new(),
    };
    let end = match parse_datetime(&session.end) {
        Some(dt) => dt,
        None => return Vec::new(),
    };
    split_session_by_day(&start, &end)
        .into_iter()
        .map(|(date, _)| date)
        .collect()
}

/// 获取今日涉及的所有直播（按自然日匹配，跨日直播也会被查到）
fn get_today_sessions(sessions: &[LiveSession]) -> Vec<&LiveSession> {
    let today = Local::now().date_naive();
    sessions.iter()
        .filter(|s| get_session_dates(s).iter().any(|d| *d == today))
        .collect()
}

/// 构建周报/月报/年报中的按日分布数据（每天多少小时）
#[allow(dead_code)]
fn build_daily_breakdown(sessions: &[LiveSession], start_date: NaiveDate, end_date: NaiveDate) -> Vec<serde_json::Value> {
    let mut breakdown = Vec::new();
    let mut current = start_date;
    while current <= end_date {
        let day_secs: u64 = sessions.iter()
            .filter_map(|s| {
                let s_start = parse_datetime(&s.start)?;
                let s_end = parse_datetime(&s.end)?;
                Some((s_start, s_end, s.duration_secs))
            })
            .flat_map(|(start, end, _)| split_session_by_day(&start, &end))
            .filter(|(date, _)| *date == current)
            .map(|(_, secs)| secs)
            .sum();

        if day_secs > 0 {
            breakdown.push(serde_json::json!({
                "date": current.format("%Y-%m-%d").to_string(),
                "hours": (day_secs as f64 / 3600.0 * 100.0).round() / 100.0
            }));
        }
        current += Duration::days(1);
    }
    breakdown
}

/// 构建时段分布（凌晨/上午/下午/晚上各多少小时）
#[allow(dead_code)]
fn build_time_period_distribution(sessions: &[LiveSession]) -> serde_json::Value {
    let mut periods = vec![0.0f64; 4]; // 凌晨, 上午, 下午, 晚上

    for s in sessions {
        if let Some(start) = parse_datetime(&s.start) {
            if let Some(end) = parse_datetime(&s.end) {
                let splits = split_session_by_day(&start, &end);
                for (_date, secs) in splits {
                    let hours = secs as f64 / 3600.0;
                    // 粗略按该日 00:00 分配，跨日部分在次日统计
                    periods[3] += hours; // 默认归入晚上
                }
            }
        }
    }

    serde_json::json!({
        "dawn": (periods[0] * 100.0).round() / 100.0,
        "morning": (periods[1] * 100.0).round() / 100.0,
        "afternoon": (periods[2] * 100.0).round() / 100.0,
        "evening": (periods[3] * 100.0).round() / 100.0,
    })
}

/// 构建日报完整数据（用于发送到 API 生成图片）
fn build_daily_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let runtime = load_runtime_state(room_id);
    let agg = load_aggregate(room_id);

    let today_sessions = get_today_sessions(&all_sessions);

    let now = Local::now();
    let today_str = now.format("%Y-%m-%d").to_string();
    let weekday = now.weekday().num_days_from_monday();

    let week_days = compute_weekly_live_days(&all_sessions);
    let week_secs = compute_weekly_seconds(&all_sessions);
    let month_days = compute_monthly_live_days(&all_sessions);
    let month_secs = compute_monthly_seconds(&all_sessions);
    let streak = compute_streak(&all_sessions);
    let today_secs = compute_today_seconds(&all_sessions);

    let avg_time = agg.average_start_minutes
        .map(fmt_average_start_time)
        .unwrap_or_else(|| "暂无".to_string());

    // 今日各场次（跨日场次只展示今天的秒数）
    let session_list: Vec<serde_json::Value> = today_sessions.iter().map(|s| {
        let start = parse_datetime(&s.start)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| "??:??".to_string());
        let end = parse_datetime(&s.end)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| "??:??".to_string());
        let day_secs = get_session_day_seconds(s, now.date_naive());
        serde_json::json!({
            "start": start,
            "end": end,
            "duration_hours": (day_secs as f64 / 3600.0 * 100.0).round() / 100.0
        })
    }).collect();

    // 是否正在直播
    let (is_live, live_elapsed_hours) = if runtime.is_live {
        let start = runtime.current_start
            .as_deref()
            .and_then(parse_datetime)
            .unwrap_or(Local::now());
        let elapsed = (Local::now() - start).num_seconds() as f64 / 3600.0;
        (true, (elapsed * 100.0).round() / 100.0)
    } else {
        (false, 0.0)
    };

    serde_json::json!({
        "report_type": "daily",
        "room_name": name,
        "date": today_str,
        "weekday": weekday,
        "is_live": is_live,
        "live_elapsed_hours": live_elapsed_hours,
        "sessions": session_list,
        "daily_total_hours": (today_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "daily_session_count": today_sessions.len(),
        "weekly_live_days": week_days,
        "weekly_total_hours": (week_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "monthly_live_days": month_days,
        "monthly_total_hours": (month_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "streak_days": streak,
        "longest_session_hours": (agg.longest_session_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "longest_session_date": agg.longest_session_date
            .as_deref()
            .and_then(|s| parse_date(s))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        "average_start_time": avg_time,
        "total_sessions": agg.total_sessions,
        "total_hours": (agg.total_seconds as f64 / 3600.0 * 100.0).round() / 100.0,
        "average_session_hours": (agg.average_session_secs / 3600.0 * 100.0).round() / 100.0,
    })
}

/// 构建周报完整数据
#[allow(dead_code)]
fn build_weekly_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let now = Local::now();

    let weekday = now.weekday().num_days_from_monday();
    let week_start = now.date_naive() - Duration::days(weekday as i64);
    let week_end = now.date_naive();

    let week_secs = compute_weekly_seconds(&all_sessions);
    let week_days = compute_weekly_live_days(&all_sessions);
    let breakdown = build_daily_breakdown(&all_sessions, week_start, week_end);

    let avg_start = compute_average_start_minutes(&all_sessions)
        .map(fmt_average_start_time)
        .unwrap_or_else(|| "暂无".to_string());

    serde_json::json!({
        "report_type": "weekly",
        "room_name": name,
        "date_range": {
            "start": week_start.format("%Y-%m-%d").to_string(),
            "end": week_end.format("%Y-%m-%d").to_string()
        },
        "total_hours": (week_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "live_days": week_days,
        "daily_breakdown": breakdown,
        "average_start_time": avg_start,
        "streak_days": compute_streak(&all_sessions),
    })
}

/// 构建月报完整数据
#[allow(dead_code)]
fn build_monthly_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let agg = load_aggregate(room_id);
    let now = Local::now();

    let month_start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
    let month_end = now.date_naive();

    let month_secs = compute_monthly_seconds(&all_sessions);
    let month_days = compute_monthly_live_days(&all_sessions);
    let breakdown = build_daily_breakdown(&all_sessions, month_start, month_end);

    let avg_start = compute_average_start_minutes(&all_sessions)
        .map(fmt_average_start_time)
        .unwrap_or_else(|| "暂无".to_string());

    serde_json::json!({
        "report_type": "monthly",
        "room_name": name,
        "year": now.year(),
        "month": now.month(),
        "total_hours": (month_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "live_days": month_days,
        "session_count": all_sessions.len(),
        "daily_breakdown": breakdown,
        "average_daily_hours": if month_days > 0 {
            ((month_secs as f64 / month_days as f64 / 3600.0) * 100.0).round() / 100.0
        } else { 0.0 },
        "average_start_time": avg_start,
        "longest_session_hours": (agg.longest_session_secs as f64 / 3600.0 * 100.0).round() / 100.0,
        "longest_session_date": agg.longest_session_date
            .as_deref()
            .and_then(|s| parse_date(s))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
    })
}

/// 发送报告数据到 API 并获取图片 URL
/// 返回 None 表示失败（API 未配置、请求失败等）
fn request_report_image(data: &serde_json::Value) -> Option<String> {
    let config = CONFIG.get().expect("CONFIG not initialized");
    let api_url = match &config.report_api_url {
        Some(url) => url,
        None => return None,
    };

    if !config.report_push_enabled {
        return None;
    }

    let client = Client::new();
    match client
        .post(api_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "PotatoLiveBot/1.0")
        .json(data)
        .send()
    {
        Ok(resp) => {
            if resp.status().is_success() {
                // 尝试从响应中提取图片 URL
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    let url = json["image_url"].as_str()
                        .or_else(|| json["url"].as_str())
                        .or_else(|| json["data"]["url"].as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        return Some(url.to_string());
                    }
                }
                // 如果没有 json 响应体，尝试直接作为 URL
                tracing::info!("[live_monitor] 报告 API 请求成功但未返回图片 URL");
                None
            } else {
                tracing::warn!("[live_monitor] 报告 API 返回非成功: {}", resp.status());
                None
            }
        }
        Err(e) => {
            tracing::warn!("[live_monitor] 报告 API 请求失败: {}", e);
            None
        }
    }
}

/// 处理今日查询：优先尝试 API 获取图片，失败则回退到文本
/// 返回 (image_url 或 text, 是否是图片)
pub fn handle_daily_query(room_id: u64, name: &str) -> (String, bool) {
    // 先尝试 API 生成图片
    let data = build_daily_report_data(room_id, name);
    if let Some(image_url) = request_report_image(&data) {
        return (image_url, true);
    }
    // API 失败或无配置，回退到文本
    let text = format_today_report(room_id, name);
    (text, false)
}

/// 格式化今日直播报告文本（供指令回退使用）
fn format_today_report(room_id: u64, name: &str) -> String {
    let sessions = load_sessions(room_id);
    let runtime = load_runtime_state(room_id);
    let agg = load_aggregate(room_id);

    let today = Local::now().date_naive();
    let today_sessions = get_today_sessions(&sessions);
    let today_total_secs = compute_today_seconds(&sessions);
    let today_total_hours = today_total_secs as f64 / 3600.0;

    // 是否正在直播
    let live_status = if runtime.is_live {
        let start = runtime.current_start
            .as_deref()
            .and_then(parse_datetime)
            .unwrap_or(Local::now());
        let elapsed = (Local::now() - start).num_seconds() as f64 / 3600.0;
        format!("🟢 正在直播中（已播 {:.1}h）\n", elapsed)
    } else {
        String::new()
    };

    // 今日各场次详情（跨日场次只展示今天的占比）
    let mut details = String::new();
    for s in today_sessions.iter() {
        let start = parse_datetime(&s.start)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| "??:??".to_string());
        let end = parse_datetime(&s.end)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| "??:??".to_string());
        let day_secs = get_session_day_seconds(s, today);
        let hours = day_secs as f64 / 3600.0;
        if day_secs > 0 {
            details.push_str(&format!("  · {} → {}（{:.1}h）\n", start, end, hours));
        }
    }

    if today_sessions.is_empty() && !runtime.is_live {
        return format!("{}今天还没有开播哦~", name);
    }

    let today_line = if today_total_secs > 0 {
        format!("🎯 今日累计：{:.1}h\n", today_total_hours)
    } else {
        String::new()
    };

    let streak = compute_streak(&sessions);
    let week_days = compute_weekly_live_days(&sessions);
    let week_secs = compute_weekly_seconds(&sessions);
    let month_days = compute_monthly_live_days(&sessions);
    let month_secs = compute_monthly_seconds(&sessions);

    let longest_h = agg.longest_session_secs as f64 / 3600.0;
    let longest_date = agg.longest_session_date
        .as_deref()
        .and_then(|s| parse_date(s))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "未知".to_string());

    let avg_time = agg.average_start_minutes
        .map(fmt_average_start_time)
        .unwrap_or_else(|| "暂无".to_string());

    format!(
        "📺 {}今日直播情况\n\
        ━━━━━━━━━━━━━━━━━━\n\
        {}{}\
        {}━━━━━━━━━━━━━━━━━━\n\
        🔥 连续开播：{}天\n\
        📈 本周：{}天 / {:.1}h\n\
        🗓 本月：{}天 / {:.1}h\n\
        🏆 最长纪录：{:.1}h（{}）\n\
        ⏰ 平均开播：{}",
        name,
        live_status,
        today_line,
        details,
        streak,
        week_days, week_secs as f64 / 3600.0,
        month_days, month_secs as f64 / 3600.0,
        longest_h, longest_date,
        avg_time,
    )
}

// ── 核心检测逻辑 ─────────────────────────────────────────

fn check_and_notify(room_id: u64, name: &str) {
    let status = get_live_status(room_id);
    let mut runtime = load_runtime_state(room_id);
    let config = CONFIG.get().expect("CONFIG not initialized");

    match status {
        Some(1) => {
            // API 返回直播中
            runtime.consecutive_failures = 0;

            if !runtime.is_live {
                let now = Local::now();
                runtime.is_live = true;
                runtime.current_start = Some(now.to_rfc3339());
                save_runtime_state(&runtime);

                if config.push_on_start {
                    let msg = format!("{}开播啦！", name);
                    push_to_all_groups(&msg);
                }
                tracing::info!(
                    "[live_monitor] 开播: room={}, time={}",
                    room_id, now.to_rfc3339()
                );
            }
        }
        Some(0) | Some(2) => {
            // API 返回未直播或轮播
            runtime.consecutive_failures = 0;

            if runtime.is_live {
                let end_time = Local::now();
                let start_str = runtime.current_start
                    .take()
                    .unwrap_or_else(|| end_time.to_rfc3339());
                let start_time = parse_datetime(&start_str)
                    .unwrap_or(end_time);
                let duration_secs = (end_time - start_time).num_seconds() as u64;

                runtime.is_live = false;
                save_runtime_state(&runtime);

                if duration_secs >= MIN_SESSION_SECS {
                    let session = LiveSession {
                        room_id,
                        start: start_str,
                        end: end_time.to_rfc3339(),
                        duration_secs,
                        weekday: end_time.weekday().num_days_from_monday(),
                        start_hour: start_time.hour(),
                        end_hour: end_time.hour(),
                    };

                    append_session(&session);
                    recompute_aggregate(room_id);

                    if config.push_on_end {
                        let msg = format_offline_message(name, room_id);
                        push_to_all_groups(&msg);
                    }
                    tracing::info!(
                        "[live_monitor] 下播: room={}, 时长={}s",
                        room_id, duration_secs
                    );
                } else {
                    tracing::warn!(
                        "[live_monitor] 异常短直播已忽略: room={}, 时长={}s",
                        room_id, duration_secs
                    );
                }
            }
        }
        Some(other) => {
            // 未知状态（如 B站 API 返回了预期外的值）
            runtime.consecutive_failures = 0;
            tracing::warn!(
                "[live_monitor] 未知直播状态 {}: room={}",
                other, room_id
            );
        }
        None => {
            // API 请求失败
            runtime.consecutive_failures += 1;
            save_runtime_state(&runtime);

            if runtime.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                tracing::warn!(
                    "[live_monitor] API 连续 {} 次失败，跳过本次检测: room={}",
                    runtime.consecutive_failures, room_id
                );
            } else {
                tracing::warn!(
                    "[live_monitor] API 请求失败 ({}/{}): room={}",
                    runtime.consecutive_failures, MAX_CONSECUTIVE_FAILURES, room_id
                );
            }
        }
    }
}

// ── API 调用 ─────────────────────────────────────────────

fn get_live_status(room_id: u64) -> Option<u32> {
    let url = format!(
        "https://api.live.bilibili.com/room/v1/Room/get_info?room_id={}",
        room_id
    );
    let client = Client::new();
    match client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (compatible; LiveBot/1.0)")
        .send()
    {
        Ok(resp) => match resp.json::<serde_json::Value>() {
            Ok(json) if json["code"] == 0 => {
                json["data"]["live_status"].as_u64().map(|v| v as u32)
            }
            _ => None,
        },
        Err(_) => None,
    }
}

// ── 消息格式化 ───────────────────────────────────────────

fn format_offline_message(name: &str, room_id: u64) -> String {
    let sessions = load_sessions(room_id);
    let agg = load_aggregate(room_id);

    // 获取最近一次直播信息
    let last_session = sessions.last();
    let duration_str = last_session
        .map(|s| fmt_hours(s.duration_secs))
        .unwrap_or_else(|| "0h".to_string());

    let today_secs = compute_today_seconds(&sessions);
    let today_str = fmt_hours(today_secs);

    let streak = compute_streak(&sessions);
    let streak_str = if streak > 0 {
        format!("{}天", streak)
    } else {
        "暂无".to_string()
    };

    let week_days = compute_weekly_live_days(&sessions);
    let week_secs = compute_weekly_seconds(&sessions);
    let week_str = format!("{}天 / {}", week_days, fmt_hours(week_secs));

    let month_days = compute_monthly_live_days(&sessions);
    let month_secs = compute_monthly_seconds(&sessions);
    let month_str = format!("{}天 / {}", month_days, fmt_hours(month_secs));

    let longest_hours = fmt_hours(agg.longest_session_secs);
    let longest_date = agg.longest_session_date
        .as_deref()
        .and_then(|s| parse_date(s))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "未知".to_string());

    let avg_start_str = agg.average_start_minutes
        .map(fmt_average_start_time)
        .unwrap_or_else(|| "暂无".to_string());

    format!(
        "{}下播啦！\n\n\
        ⏱ 本次：{}\n\
        今日累计：{}\n\
        连续开播：{}\n\
        本周：{}\n\
        本月：{}\n\
        最长纪录：{}（{}）\n\
        平均开播：{}",
        name, duration_str, today_str, streak_str,
        week_str, month_str, longest_hours, longest_date, avg_start_str
    )
}

// ── 推送 ─────────────────────────────────────────────────

fn push_to_all_groups(msg: &str) {
    let config = CONFIG.get().expect("CONFIG not initialized");
    for &group_id in &config.push_groups {
        let _ = Bot::send_group_msg(group_id, CString::new(msg).unwrap());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
