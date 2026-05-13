use ini::Ini;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_yaml;
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use luo9_sdk::Bot;
use luo9_sdk::bus::Bus;
use reqwest::blocking::Client;

// ── 全局状态 ──────────────────────────────────────────────

/// 配置单例
static CONFIG: OnceCell<LiveMonitorConfig> = OnceCell::new();
/// 数据目录单例
static DATA_DIR: OnceCell<PathBuf> = OnceCell::new();

// ── 配置结构体 ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveMonitorConfig {
    /// 管理员qq号
    pub admin: u64,
    /// 监控的直播间列表
    pub rooms: Vec<LiveRoom>,
    /// 推送的群列表
    pub push_groups: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveRoom {
    pub room_id: u64,    // 直播间ID
    pub name: String,    // 主播名称
}

impl LiveMonitorConfig {
    /// 获取全局配置实例的引用
    pub fn get() -> &'static LiveMonitorConfig {
        CONFIG.get().expect("LiveMonitorConfig not initialized, call init() first")
    }
    
    /// 获取管理员 QQ
    pub fn admin() -> u64 {
        Self::get().admin
    }
    
    /// 获取推送群列表
    pub fn push_groups() -> &'static [u64] {
        &Self::get().push_groups
    }
    
    /// 获取监控的直播间列表
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
                    room_id: 123456,      // TODO: 替换为实际ID
                    name: "土豆".into(),
                }
            ],
            push_groups: vec![123456789], // TODO: 替换为实际群号
        }
    }
}

// ── 默认配置 YAML ────────────────────────────────────────

const DEFAULT_CONFIG_YAML: &str = r#"# B站直播监控配置
admin: 123456
rooms:
  - room_id: 123456          # 直播间ID
    name: "土豆"              # 主播名称
push_groups:
  - 123456789                # 推送的群号
"#;

// ── 公开初始化函数 ───────────────────────────────────────

/// 模块初始化，由 main.rs 调用
pub fn init() {
    // 来自 utils/path.rs
    let data_path = crate::path::to_absolute(
        &PathBuf::from("data").join("plugin_potato_live")
    );
    fs::create_dir_all(&data_path).ok();
    let _ = DATA_DIR.set(data_path.clone());

    // 加载或生成配置文件
    let config_path = data_path.join("config.yaml");
    if !config_path.exists() {
        fs::write(&config_path, DEFAULT_CONFIG_YAML).ok();
    }

    let config: LiveMonitorConfig = match fs::read_to_string(&config_path) {
        Ok(content) => serde_yaml::from_str(&content)
            .unwrap_or_else(|_| {
                LiveMonitorConfig::default()
            }),
        Err(_) => {
            LiveMonitorConfig::default()
        }
    };

    let _ = CONFIG.set(config);

    // 注册所有直播间的定时任务
    register_schedule_tasks();
}

// ── 定时任务注册 ─────────────────────────────────────────

fn register_schedule_tasks() {
    let config = CONFIG.get().expect("CONFIG not initialized");
    
    for room in &config.rooms {
        let req = serde_json::json!({
            "action": "schedule",
            "task_name": format!("bilibili_live_{}", room.room_id),
            "cron": "0 */1 * * * *",      // 每分钟执行
            "payload": serde_json::json!({
                "room_id": room.room_id,
                "name": room.name
            }).to_string()
        });
        let _ = Bus::topic("luo9_task_miso").publish(&req.to_string());
        tracing::info!(
            "[live_monitor] registered task for room: {} ({})",
            room.name, room.room_id
        );
    }
}

// ── 任务事件处理 ─────────────────────────────────────────

/// 处理任务系统回调
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

        // 同步调用
        check_and_notify(room_id, &name);
    }
}

// ── 核心检测逻辑 ─────────────────────────────────────────

/// 检查直播间状态并推送通知
fn check_and_notify(room_id: u64, name: &str) {
    let status = get_live_status(room_id);
    let last_is_live = read_status_from_ini(room_id);

    if status == 1 && !last_is_live {
        // 开播
        write_status_to_ini(room_id, true);
        let msg = format!("{}开播啦！", name);
        push_to_all_groups(&msg);
    } else if status == 0 && last_is_live {
        // 下播
        write_status_to_ini(room_id, false);
    }
    // 其他状态（轮播/未变化）不做处理
}

/// 从 INI 读取上次的直播状态
fn read_status_from_ini(room_id: u64) -> bool {
    let ini_path = get_ini_path(room_id);
    Ini::load_from_file(&ini_path)
        .unwrap_or_default()
        .get_from(Some("status"), "live")
        .unwrap_or("0")
        == "1"
}

/// 将当前直播状态写入 INI
fn write_status_to_ini(room_id: u64, is_live: bool) {
    let ini_path = get_ini_path(room_id);
    let mut conf = Ini::load_from_file(&ini_path).unwrap_or_default();
    conf.set_to(
        Some("status"),
        "live".to_string(),
        if is_live { "1" } else { "0" }.to_string(),
    );
    conf.write_to_file(&ini_path).unwrap();
}

/// 获取 INI 文件路径
fn get_ini_path(room_id: u64) -> PathBuf {
    DATA_DIR
        .get()
        .expect("DATA_DIR not set")
        .join(format!("live_status_{}.ini", room_id))
}

/// 调用 B站 API 获取直播状态（同步版本）
fn get_live_status(room_id: u64) -> u32 {
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
                json["data"]["live_status"].as_u64().unwrap_or(0) as u32
            }
            _ => 0,
        },
        Err(_) => 0,
    }
}

/// 推送消息到所有配置的群
fn push_to_all_groups(msg: &str) {
    let config = CONFIG.get().expect("CONFIG not initialized");
    for &group_id in &config.push_groups {
        let _ = Bot::send_group_msg(group_id, CString::new(msg).unwrap());
        // 同步不需要 sleep，如果 SDK 需要间隔可以保留
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}