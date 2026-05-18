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

/// 对一场直播按天拆分，返回每天的具体时段信息（用于周报/月报的 daily_stats）
/// 对于跨日场次（如 19:00→02:00），会正确拆分为：
///   Day1: start_time="19:00", end_time="24:00", duration_minutes=300, crosses_midnight=true
///   Day2: start_time="00:00", end_time="02:00", duration_minutes=120, crosses_midnight=true
fn split_session_into_segments(session: &LiveSession) -> Vec<serde_json::Value> {
    let start_dt = match parse_datetime(&session.start) {
        Some(dt) => dt,
        None => return Vec::new(),
    };
    let end_dt = match parse_datetime(&session.end) {
        Some(dt) => dt,
        None => return Vec::new(),
    };

    let start_date = start_dt.date_naive();
    let end_date = end_dt.date_naive();
    let original_crosses = start_date != end_date;

    if start_date == end_date {
        // 同一天，无需拆分
        let start_min = start_dt.hour() as u64 * 60 + start_dt.minute() as u64;
        let end_min = end_dt.hour() as u64 * 60 + end_dt.minute() as u64;
        return vec![serde_json::json!({
            "start_time": start_dt.format("%H:%M").to_string(),
            "end_time": end_dt.format("%H:%M").to_string(),
            "duration_minutes": end_min - start_min,
            "crosses_midnight": false,
        })];
    }

    let mut segments = Vec::new();

    // 第一天：start → 24:00
    {
        let start_min = start_dt.hour() as u64 * 60 + start_dt.minute() as u64;
        let day_end_min = 24 * 60;
        segments.push(serde_json::json!({
            "start_time": start_dt.format("%H:%M").to_string(),
            "end_time": "24:00".to_string(),
            "duration_minutes": day_end_min - start_min,
            "crosses_midnight": original_crosses,
        }));
    }

    // 中间完整天
    let mut current = start_date + Duration::days(1);
    while current < end_date {
        segments.push(serde_json::json!({
            "start_time": "00:00".to_string(),
            "end_time": "24:00".to_string(),
            "duration_minutes": 1440,
            "crosses_midnight": original_crosses,
        }));
        current += Duration::days(1);
    }

    // 最后一天：00:00 → end
    {
        let end_min = end_dt.hour() as u64 * 60 + end_dt.minute() as u64;
        segments.push(serde_json::json!({
            "start_time": "00:00".to_string(),
            "end_time": end_dt.format("%H:%M").to_string(),
            "duration_minutes": end_min,
            "crosses_midnight": original_crosses,
        }));
    }

    segments
}

/// 计算指定时间范围内各小时的直播总分钟数（0-23）
fn compute_hour_distribution(sessions: &[&LiveSession], start: NaiveDate, end: NaiveDate) -> [u64; 24] {
    let mut hours = [0u64; 24];

    for session in sessions {
        let start_dt = match parse_datetime(&session.start) {
            Some(dt) => dt,
            None => continue,
        };
        let end_dt = match parse_datetime(&session.end) {
            Some(dt) => dt,
            None => continue,
        };

        if end_dt.date_naive() < start || start_dt.date_naive() > end {
            continue;
        }

        let segments = split_session_into_segments(session);
        for seg in segments {
            let start_str = seg["start_time"].as_str().unwrap_or("00:00");
            let end_str = seg["end_time"].as_str().unwrap_or("00:00");

            let parts: Vec<&str> = start_str.split(':').collect();
            let start_hour: usize = parts[0].parse().unwrap_or(0);
            let start_min: u64 = parts[1].parse().unwrap_or(0);

            if end_str == "24:00" {
                if start_min > 0 {
                    hours[start_hour] += 60 - start_min;
                    for h in (start_hour + 1)..24 {
                        hours[h] += 60;
                    }
                } else {
                    for h in start_hour..24 {
                        hours[h] += 60;
                    }
                }
            } else {
                let end_parts: Vec<&str> = end_str.split(':').collect();
                let end_hour: usize = end_parts[0].parse().unwrap_or(0);
                let end_min: u64 = end_parts[1].parse().unwrap_or(0);

                if start_hour == end_hour {
                    hours[start_hour] += end_min - start_min;
                } else {
                    hours[start_hour] += 60 - start_min;
                    for h in (start_hour + 1)..end_hour {
                        hours[h] += 60;
                    }
                    hours[end_hour] += end_min;
                }
            }
        }
    }

    hours
}

/// 计算峰值时段（直播分钟数最多的小时），返回 (hour, minutes)
fn compute_peak_hour(hour_dist: &[u64; 24]) -> (u32, u64) {
    let mut peak_hour = 0u32;
    let mut peak_min = 0u64;
    for (h, &m) in hour_dist.iter().enumerate() {
        if m > peak_min {
            peak_min = m;
            peak_hour = h as u32;
        }
    }
    (peak_hour, peak_min)
}

/// 计算星期分布（7天，从周一开始，每个元素是该天的总分钟数）
fn compute_weekday_distribution(sessions: &[&LiveSession]) -> [u64; 7] {
    let mut dist = [0u64; 7];
    for session in sessions {
        let segments = split_session_into_segments(session);
        // 拿到 session 的日期列表以匹配 weekday
        let dates = get_session_dates(session);
        for (i, seg) in segments.iter().enumerate() {
            if let Some(date) = dates.get(i) {
                let wday = date.weekday().num_days_from_monday() as usize;
                if let Some(min) = seg["duration_minutes"].as_u64() {
                    dist[wday] += min;
                }
            }
        }
    }
    dist
}

/// 计算历史最长连续开播天数（从所有 session 中统计）
fn compute_longest_streak(sessions: &[LiveSession]) -> u32 {
    let mut live_dates: Vec<NaiveDate> = sessions.iter()
        .flat_map(|s| get_session_dates(s))
        .collect();
    live_dates.sort();
    live_dates.dedup();

    if live_dates.is_empty() {
        return 0;
    }

    let mut longest = 1u32;
    let mut current = 1u32;
    for i in 1..live_dates.len() {
        let diff = (live_dates[i] - live_dates[i - 1]).num_days();
        if diff == 1 {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 1;
        }
    }
    longest
}

/// 获取指定日期范围内的所有 session（含跨日覆盖）
fn filter_sessions_in_range(sessions: &[LiveSession], range_start: NaiveDate, range_end: NaiveDate) -> Vec<&LiveSession> {
    sessions.iter()
        .filter(|s| {
            let dates = get_session_dates(s);
            dates.iter().any(|d| *d >= range_start && *d <= range_end)
        })
        .collect()
}

/// 构建周报 API 数据
/// POST /api/report/weekly
pub fn build_weekly_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let now = Local::now();

    let weekday = now.weekday().num_days_from_monday();
    let week_start = now.date_naive() - Duration::days(weekday as i64);
    let week_end = now.date_naive();

    let week_sessions = filter_sessions_in_range(&all_sessions, week_start, week_end);
    let hour_dist = compute_hour_distribution(&week_sessions, week_start, week_end);
    let (peak_hour, peak_hour_min) = compute_peak_hour(&hour_dist);

    let mut total_stream_minutes = 0u64;
    let mut stream_days = 0u32;
    let mut session_count = 0u32;
    let mut longest_session_minutes = 0u64;
    let mut daily_stats = Vec::new();

    // 遍历周一到周日（共7天）
    let mut current = week_start;
    while current <= week_end {
        let day_sessions: Vec<&LiveSession> = week_sessions.iter()
            .filter(|s| get_session_dates(s).iter().any(|d| *d == current))
            .copied()
            .collect();

        let mut day_total_minutes = 0u64;
        let mut day_sessions_json = Vec::new();

        for &s in &day_sessions {
            // 获取该 session 在当天的拆分片段
            let all_segments = split_session_into_segments(s);
            let day_seg_opt = {
                let dates = get_session_dates(s);
                let mut found = None;
                for (i, date) in dates.iter().enumerate() {
                    if *date == current {
                        found = all_segments.get(i);
                        break;
                    }
                }
                found
            };

            if let Some(seg) = day_seg_opt {
                let dur_min = seg["duration_minutes"].as_u64().unwrap_or(0);
                day_total_minutes += dur_min;
                day_sessions_json.push(seg.clone());
                if dur_min > longest_session_minutes {
                    longest_session_minutes = dur_min;
                }
            }
        }

        if day_total_minutes > 0 {
            stream_days += 1;
        }
        total_stream_minutes += day_total_minutes;
        session_count += day_sessions.len() as u32;

        daily_stats.push(serde_json::json!({
            "date": current.format("%Y-%m-%d").to_string(),
            "total_minutes": day_total_minutes,
            "session_count": day_sessions.len(),
            "sessions": day_sessions_json,
        }));

        current += Duration::days(1);
    }

    serde_json::json!({
        "streamer_name": name,
        "week_start": week_start.format("%Y-%m-%d").to_string(),
        "week_end": week_end.format("%Y-%m-%d").to_string(),
        "total_stream_minutes": total_stream_minutes,
        "stream_days": stream_days,
        "session_count": session_count,
        "peak_hour": peak_hour,
        "peak_hour_minutes": peak_hour_min,
        "longest_session_minutes": longest_session_minutes,
        "streak_days": compute_streak(&all_sessions),
        "daily_stats": daily_stats,
    })
}

/// 构建月报 API 数据
/// POST /api/report/monthly
pub fn build_monthly_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let now = Local::now();

    let month_start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
    let month_end = now.date_naive();

    let month_sessions = filter_sessions_in_range(&all_sessions, month_start, month_end);
    let hour_dist = compute_hour_distribution(&month_sessions, month_start, month_end);
    let (peak_hour, peak_hour_min) = compute_peak_hour(&hour_dist);
    let weekday_dist = compute_weekday_distribution(&month_sessions);

    let mut total_stream_minutes = 0u64;
    let mut stream_days_set: HashSet<NaiveDate> = HashSet::new();
    let mut session_count = 0u32;
    let mut longest_session_minutes = 0u64;
    let mut weekly_stats = Vec::new();

    for &s in &month_sessions {
        let dates = get_session_dates(s);
        for d in &dates {
            if *d >= month_start && *d <= month_end {
                stream_days_set.insert(*d);
            }
        }
        session_count += 1;
        if s.duration_secs / 60 > longest_session_minutes {
            longest_session_minutes = s.duration_secs as u64 / 60;
        }
        // 累计本月总分钟（按整个 session 算，不是只算本月部分）
        // 但更精确应该按 split 算。用 get_session_day_seconds 按天累加
    }

    // 精确按月范围算 total
    let mut current = month_start;
    while current <= month_end {
        total_stream_minutes += month_sessions.iter()
            .map(|s| get_session_day_seconds(s, current))
            .sum::<u64>() / 60;
        current += Duration::days(1);
    }

    // 周统计：将本月按周拆分（可能跨月边界，但只算本月内的天）
    current = month_start;
    let mut week_num = 1u32;
    while current <= month_end {
        let week_day = current.weekday().num_days_from_monday();
        let week_start_day = current - Duration::days(week_day as i64);
        let week_end_day = week_start_day + Duration::days(6);
        let week_start_clamped = if week_start_day < month_start { month_start } else { week_start_day };
        let week_end_clamped = if week_end_day > month_end { month_end } else { week_end_day };

        let week_sessions = filter_sessions_in_range(&all_sessions, week_start_clamped, week_end_clamped);

        let mut week_minutes = 0u64;
        let mut wd = week_start_clamped;
        while wd <= week_end_clamped {
            week_minutes += month_sessions.iter()
                .map(|s| get_session_day_seconds(s, wd))
                .sum::<u64>() / 60;
            wd += Duration::days(1);
        }

        weekly_stats.push(serde_json::json!({
            "week_number": week_num,
            "total_minutes": week_minutes,
            "session_count": week_sessions.len() as u32,
        }));

        current = week_end_day + Duration::days(1);
        week_num += 1;
    }

    serde_json::json!({
        "streamer_name": name,
        "month": format!("{:04}-{:02}", now.year(), now.month()),
        "total_stream_minutes": total_stream_minutes,
        "stream_days": stream_days_set.len() as u32,
        "session_count": session_count,
        "peak_hour": peak_hour,
        "peak_hour_minutes": peak_hour_min,
        "longest_session_minutes": longest_session_minutes,
        "streak_days": compute_streak(&all_sessions),
        "weekly_stats": weekly_stats,
        "weekday_distribution": weekday_dist.map(|m| serde_json::Value::from(m)).to_vec(),
    })
}

/// 构建年度总结 API 数据
/// POST /api/report/yearly
pub fn build_yearly_report_data(room_id: u64, name: &str) -> serde_json::Value {
    let all_sessions = load_sessions(room_id);
    let now = Local::now();
    let year = now.year();
    let year_start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    let year_end = NaiveDate::from_ymd_opt(year, 12, 31).unwrap();

    let year_sessions = filter_sessions_in_range(&all_sessions, year_start, year_end);
    let hour_dist = compute_hour_distribution(&year_sessions, year_start, year_end);
    let (peak_hour, peak_hour_min) = compute_peak_hour(&hour_dist);
    let weekday_dist = compute_weekday_distribution(&year_sessions);

    let mut total_stream_minutes = 0u64;
    let mut stream_days_set: HashSet<NaiveDate> = HashSet::new();
    let mut session_count = 0u32;
    let mut longest_session_minutes = 0u64;

    // 按年范围精确计算
    let mut current = year_start;
    while current <= year_end {
        let day_min: u64 = year_sessions.iter()
            .map(|s| get_session_day_seconds(s, current))
            .sum::<u64>() / 60;
        total_stream_minutes += day_min;

        let day_has_session = year_sessions.iter().any(|s| {
            get_session_dates(s).iter().any(|d| *d == current)
        });
        if day_has_session {
            stream_days_set.insert(current);
        }

        current += Duration::days(1);
    }

    for &s in &year_sessions {
        session_count += 1;
        if s.duration_secs as u64 / 60 > longest_session_minutes {
            longest_session_minutes = s.duration_secs as u64 / 60;
        }
    }

    let longest_streak = compute_longest_streak(&all_sessions);

    // 逐月统计
    let mut monthly_stats = Vec::new();
    for m in 1..=12 {
        let month_start = NaiveDate::from_ymd_opt(year, m, 1).unwrap();
        let month_end = if m == 12 {
            year_end
        } else {
            NaiveDate::from_ymd_opt(year, m + 1, 1).unwrap() - Duration::days(1)
        };

        let mut month_minutes = 0u64;
        let mut month_stream_days_set: HashSet<NaiveDate> = HashSet::new();

        let mut d = month_start;
        while d <= month_end {
            let day_min: u64 = year_sessions.iter()
                .map(|s| get_session_day_seconds(s, d))
                .sum::<u64>() / 60;
            month_minutes += day_min;

            let day_has = year_sessions.iter().any(|s| {
                get_session_dates(s).iter().any(|dt| *dt == d)
            });
            if day_has {
                month_stream_days_set.insert(d);
            }
            d += Duration::days(1);
        }

        monthly_stats.push(serde_json::json!({
            "month": m,
            "total_minutes": month_minutes,
            "stream_days": month_stream_days_set.len() as u32,
        }));
    }

    // 最活跃月份（前3）
    let mut top_months: Vec<(u32, u64)> = monthly_stats.iter()
        .map(|m| (m["month"].as_u64().unwrap() as u32, m["total_minutes"].as_u64().unwrap_or(0)))
        .collect();
    top_months.sort_by(|a, b| b.1.cmp(&a.1));
    let top_streaming_months: Vec<serde_json::Value> = top_months.iter()
        .take(3)
        .map(|(month, minutes)| serde_json::json!({
            "month": month,
            "total_minutes": minutes,
        }))
        .collect();

    // weekday_distribution 完整格式
    let weekday_dist_full: Vec<serde_json::Value> = weekday_dist.iter().enumerate()
        .map(|(wday, &minutes)| serde_json::json!({
            "weekday": wday,
            "total_minutes": minutes,
            "session_count": session_count,
        }))
        .collect();

    serde_json::json!({
        "streamer_name": name,
        "year": year,
        "total_stream_minutes": total_stream_minutes,
        "stream_days": stream_days_set.len() as u32,
        "session_count": session_count,
        "peak_hour": peak_hour,
        "peak_hour_minutes": peak_hour_min,
        "longest_session_minutes": longest_session_minutes,
        "longest_streak_days": longest_streak,
        "monthly_stats": monthly_stats,
        "top_streaming_months": top_streaming_months,
        "weekday_distribution": weekday_dist_full,
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

// ── 单元测试 ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── 辅助函数 ──────────────────────────────────────────

    fn make_session(start: &str, end: &str, duration_secs: u64) -> LiveSession {
        LiveSession {
            room_id: 999,
            start: start.to_string(),
            end: end.to_string(),
            duration_secs,
            weekday: 0,
            start_hour: 0,
            end_hour: 0,
        }
    }

    fn dt(hour: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 5, 15, hour, min, 0)
            .single()
            .expect("无效时间")
    }

    // ── split_session_by_day ──────────────────────────────

    /// 同一天，无需拆分
    #[test]
    fn test_split_same_day() {
        let start = dt(10, 0);
        let end = dt(14, 30);
        let result = split_session_by_day(&start, &end);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, NaiveDate::from_ymd_opt(2026, 5, 15).unwrap());
        assert_eq!(result[0].1, 4 * 3600 + 30 * 60); // 4.5h
    }

    /// 跨日凌晨（23:00 → 02:00），应拆成两天
    #[test]
    fn test_split_cross_midnight() {
        let start = Local.with_ymd_and_hms(2026, 5, 23, 23, 0, 0).single().unwrap();
        let end = Local.with_ymd_and_hms(2026, 5, 24, 2, 0, 0).single().unwrap();
        let result = split_session_by_day(&start, &end);

        assert_eq!(result.len(), 2);
        // Day1: 23:00 → 23:59:59 = 1h
        assert_eq!(result[0].0, NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        assert_eq!(result[0].1, 3600);
        // Day2: 00:00 → 02:00 = 2h
        assert_eq!(result[1].0, NaiveDate::from_ymd_opt(2026, 5, 24).unwrap());
        assert_eq!(result[1].1, 2 * 3600);
    }

    /// 跨两天半（第一天 12:00 → 第三天 06:00）
    #[test]
    fn test_split_multi_day() {
        let start = Local.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap();
        let end = Local.with_ymd_and_hms(2026, 5, 3, 6, 0, 0).single().unwrap();
        let result = split_session_by_day(&start, &end);

        assert_eq!(result.len(), 3);
        // Day1: 12:00 → 23:59:59 = 12h
        assert_eq!(result[0].0, NaiveDate::from_ymd_opt(2026, 5, 1).unwrap());
        assert_eq!(result[0].1, 12 * 3600);
        // Day2: full day = 24h
        assert_eq!(result[1].0, NaiveDate::from_ymd_opt(2026, 5, 2).unwrap());
        assert_eq!(result[1].1, 24 * 3600);
        // Day3: 00:00 → 06:00 = 6h
        assert_eq!(result[2].0, NaiveDate::from_ymd_opt(2026, 5, 3).unwrap());
        assert_eq!(result[2].1, 6 * 3600);
    }

    /// start >= end 返回空
    #[test]
    fn test_split_invalid_range() {
        let start = dt(14, 0);
        let end = dt(10, 0);
        let result = split_session_by_day(&start, &end);
        assert!(result.is_empty());
    }

    // ── split_session_into_segments ───────────────────────

    /// 同天，crosses_midnight=false
    #[test]
    fn test_segments_same_day() {
        let s = make_session("2026-05-15T10:00:00+08:00", "2026-05-15T14:30:00+08:00", 4 * 3600 + 30 * 60);
        let segs = split_session_into_segments(&s);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0]["start_time"], "10:00");
        assert_eq!(segs[0]["end_time"], "14:30");
        assert_eq!(segs[0]["duration_minutes"], 270);
        assert_eq!(segs[0]["crosses_midnight"], false);
    }

    /// 跨日 19:00→02:00
    #[test]
    fn test_segments_cross_midnight() {
        let s = make_session("2026-01-16T19:00:00+08:00", "2026-01-17T02:00:00+08:00", 7 * 3600);
        let segs = split_session_into_segments(&s);
        assert_eq!(segs.len(), 2);

        // Day1: 19:00 → 24:00 = 5h
        assert_eq!(segs[0]["start_time"], "19:00");
        assert_eq!(segs[0]["end_time"], "24:00");
        assert_eq!(segs[0]["duration_minutes"], 300);
        assert_eq!(segs[0]["crosses_midnight"], true);

        // Day2: 00:00 → 02:00 = 2h
        assert_eq!(segs[1]["start_time"], "00:00");
        assert_eq!(segs[1]["end_time"], "02:00");
        assert_eq!(segs[1]["duration_minutes"], 120);
        assert_eq!(segs[1]["crosses_midnight"], true);
    }

    /// 跨两天 18:00→03:00（你给的例子）
    #[test]
    fn test_segments_cross_midnight_18_to_3() {
        let s = make_session("2026-01-19T18:00:00+08:00", "2026-01-20T03:00:00+08:00", 9 * 3600);
        let segs = split_session_into_segments(&s);
        assert_eq!(segs.len(), 2);

        assert_eq!(segs[0]["start_time"], "18:00");
        assert_eq!(segs[0]["end_time"], "24:00");
        assert_eq!(segs[0]["duration_minutes"], 360);
        assert_eq!(segs[0]["crosses_midnight"], true);

        assert_eq!(segs[1]["start_time"], "00:00");
        assert_eq!(segs[1]["end_time"], "03:00");
        assert_eq!(segs[1]["duration_minutes"], 180);
        assert_eq!(segs[1]["crosses_midnight"], true);
    }

    // ── get_session_dates ─────────────────────────────────

    #[test]
    fn test_session_dates_same_day() {
        let s = make_session("2026-05-10T08:00:00+08:00", "2026-05-10T12:00:00+08:00", 4 * 3600);
        let dates = get_session_dates(&s);
        assert_eq!(dates.len(), 1);
        assert_eq!(dates[0], NaiveDate::from_ymd_opt(2026, 5, 10).unwrap());
    }

    #[test]
    fn test_session_dates_cross_midnight() {
        let s = make_session("2026-05-23T23:00:00+08:00", "2026-05-24T02:00:00+08:00", 3 * 3600);
        let dates = get_session_dates(&s);
        assert_eq!(dates.len(), 2);
        assert_eq!(dates[0], NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        assert_eq!(dates[1], NaiveDate::from_ymd_opt(2026, 5, 24).unwrap());
    }

    // ── get_session_day_seconds ───────────────────────────

    #[test]
    fn test_session_day_seconds_cross() {
        let s = make_session("2026-05-23T23:00:00+08:00", "2026-05-24T02:00:00+08:00", 3 * 3600);
        let day23 = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
        let day24 = NaiveDate::from_ymd_opt(2026, 5, 24).unwrap();
        assert_eq!(get_session_day_seconds(&s, day23), 3600);
        assert_eq!(get_session_day_seconds(&s, day24), 7200);
    }

    // ── compute_peak_hour ─────────────────────────────────

    #[test]
    fn test_peak_hour() {
        let mut dist = [0u64; 24];
        dist[20] = 480;  // 晚8点最多
        dist[21] = 300;
        dist[14] = 360;
        let (hour, mins) = compute_peak_hour(&dist);
        assert_eq!(hour, 20);
        assert_eq!(mins, 480);
    }

    #[test]
    fn test_peak_hour_all_zero() {
        let dist = [0u64; 24];
        let (hour, mins) = compute_peak_hour(&dist);
        assert_eq!(hour, 0);
        assert_eq!(mins, 0);
    }

    // ── compute_streak ────────────────────────────────────

    #[test]
    fn test_streak_consecutive() {
        let sessions = vec![
            make_session("2026-05-11T10:00:00+08:00", "2026-05-11T14:00:00+08:00", 4 * 3600),
            make_session("2026-05-12T10:00:00+08:00", "2026-05-12T14:00:00+08:00", 4 * 3600),
            make_session("2026-05-13T10:00:00+08:00", "2026-05-13T14:00:00+08:00", 4 * 3600),
        ];
        // compute_streak 基于 Local::now()，验证函数能正常执行即可
        let _streak = compute_streak(&sessions);
    }

    #[test]
    fn test_streak_empty() {
        let sessions: Vec<LiveSession> = vec![];
        assert_eq!(compute_streak(&sessions), 0);
    }

    // ── compute_longest_streak ────────────────────────────

    #[test]
    fn test_longest_streak_basic() {
        let sessions = vec![
            make_session("2026-05-10T10:00:00+08:00", "2026-05-10T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-11T10:00:00+08:00", "2026-05-11T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-12T10:00:00+08:00", "2026-05-12T12:00:00+08:00", 2 * 3600),
            // 断一天
            make_session("2026-05-14T10:00:00+08:00", "2026-05-14T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-15T10:00:00+08:00", "2026-05-15T12:00:00+08:00", 2 * 3600),
        ];
        assert_eq!(compute_longest_streak(&sessions), 3);
    }

    #[test]
    fn test_longest_streak_cross_midnight() {
        // 跨日场次应同时计入前后两天
        let sessions = vec![
            make_session("2026-05-10T22:00:00+08:00", "2026-05-11T02:00:00+08:00", 4 * 3600),
            make_session("2026-05-11T22:00:00+08:00", "2026-05-12T02:00:00+08:00", 4 * 3600),
            make_session("2026-05-12T22:00:00+08:00", "2026-05-13T02:00:00+08:00", 4 * 3600),
        ];
        // 10, 11, 12, 13 四天连续
        assert_eq!(compute_longest_streak(&sessions), 4);
    }

    #[test]
    fn test_longest_streak_empty() {
        let sessions: Vec<LiveSession> = vec![];
        assert_eq!(compute_longest_streak(&sessions), 0);
    }

    #[test]
    fn test_longest_streak_single() {
        let sessions = vec![
            make_session("2026-05-10T10:00:00+08:00", "2026-05-10T12:00:00+08:00", 2 * 3600),
        ];
        assert_eq!(compute_longest_streak(&sessions), 1);
    }

    // ── compute_average_start_minutes ─────────────────────

    #[test]
    fn test_avg_start_minutes() {
        let sessions = vec![
            make_session("2026-05-10T10:00:00+08:00", "2026-05-10T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-11T14:30:00+08:00", "2026-05-11T16:00:00+08:00", 2 * 3600),
        ];
        let avg = compute_average_start_minutes(&sessions);
        assert_eq!(avg, Some((10 * 60 + 14 * 60 + 30) / 2)); // (600 + 870) / 2 = 735
        assert_eq!(avg, Some(735));
    }

    #[test]
    fn test_avg_start_minutes_empty() {
        let sessions: Vec<LiveSession> = vec![];
        assert_eq!(compute_average_start_minutes(&sessions), None);
    }

    // ── fmt_hours ─────────────────────────────────────────

    #[test]
    fn test_fmt_hours() {
        assert_eq!(fmt_hours(3600), "1.0h");
        assert_eq!(fmt_hours(5400), "1.5h");
        assert_eq!(fmt_hours(10800), "3.0h");
        assert_eq!(fmt_hours(0), "0.0h");
    }

    // ── fmt_average_start_time ────────────────────────────

    #[test]
    fn test_fmt_avg_time() {
        assert_eq!(fmt_average_start_time(600), "10:00");
        assert_eq!(fmt_average_start_time(735), "12:15");
        assert_eq!(fmt_average_start_time(0), "00:00");
        assert_eq!(fmt_average_start_time(1439), "23:59");
    }

    // ── filter_sessions_in_range ──────────────────────────

    #[test]
    fn test_filter_sessions_in_range() {
        let sessions = vec![
            make_session("2026-05-10T10:00:00+08:00", "2026-05-10T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-15T10:00:00+08:00", "2026-05-15T12:00:00+08:00", 2 * 3600),
            make_session("2026-05-20T10:00:00+08:00", "2026-05-20T12:00:00+08:00", 2 * 3600),
        ];
        let start = NaiveDate::from_ymd_opt(2026, 5, 12).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 5, 18).unwrap();
        let filtered = filter_sessions_in_range(&sessions, start, end);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].start, "2026-05-15T10:00:00+08:00");
    }

    /// 跨日场次：结束日在范围内的也应被过滤到
    #[test]
    fn test_filter_sessions_cross_midnight() {
        let sessions = vec![
            make_session("2026-05-09T23:00:00+08:00", "2026-05-10T02:00:00+08:00", 3 * 3600),
        ];
        let start = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 5, 10).unwrap();
        let filtered = filter_sessions_in_range(&sessions, start, end);
        assert_eq!(filtered.len(), 1);
    }

    // ── get_today_sessions ────────────────────────────────

    #[test]
    fn test_get_today_sessions_empty() {
        let sessions: Vec<LiveSession> = vec![];
        let result = get_today_sessions(&sessions);
        assert!(result.is_empty());
    }

    // ── compute_weekday_distribution ──────────────────────

    #[test]
    fn test_weekday_distribution() {
        // 2026-05-11 是周一(0)
        let sessions = vec![
            make_session("2026-05-11T10:00:00+08:00", "2026-05-11T12:00:00+08:00", 2 * 3600),
        ];
        let dist = compute_weekday_distribution(&sessions.iter().collect::<Vec<_>>());
        assert_eq!(dist[0], 120); // 周一 120 min
    }
}
