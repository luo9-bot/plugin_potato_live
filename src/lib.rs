// src/lib.rs

pub mod core;
pub mod live;
pub mod path;

use luo9_sdk::Bot;
use luo9_sdk::Msg;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;
use std::time::Instant;

static REPORT_COOLDOWN: Lazy<Mutex<HashMap<&'static str, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn handle_group_msg(group_id: u64, _user_id: u64, msg: &str) {
    let msg_trimmed = msg.trim();

    // 定时报表查询指令
    if let Some(report_type) = is_report_query(msg_trimmed) {
        // 冷却检查：每个报表类型 60 秒内不可重复触发
        {
            let mut cooldown = REPORT_COOLDOWN.lock().unwrap();
            if let Some(last) = cooldown.get(report_type) {
                if last.elapsed().as_secs() < 60 {
                    let _ = Bot::send_group_msg(group_id, CString::new("操作过于频繁，请稍后再试").unwrap());
                    return;
                }
            }
            cooldown.insert(report_type, Instant::now());
        }

        let rooms = live::LiveMonitorConfig::rooms();
        if let Some(room) = rooms.first() {
            let (result, success) = match report_type {
                "weekly" => live::handle_weekly_query(room.room_id, &room.name),
                "monthly" => live::handle_monthly_query(room.room_id, &room.name),
                "yearly" => live::handle_yearly_query(room.room_id, &room.name),
                _ => return,
            };
            if success {
                let image_msg = Msg::image(&result).build();
                let _ = Bot::send_group_msg(group_id, image_msg);
            } else {
                let _ = Bot::send_group_msg(group_id, CString::new("报表生成失败，请稍后再试").unwrap());
            }
        } else {
            let _ = Bot::send_group_msg(group_id, CString::new("暂无配置的直播间").unwrap());
        }
        return;
    }

    // // 模糊匹配"土豆今天直播了没"及其变体
    // if is_live_query(msg_trimmed) {
    //     let rooms = live::LiveMonitorConfig::rooms();
    //     if let Some(room) = rooms.first() {
    //         let (result, is_image) = live::handle_daily_query(room.room_id, &room.name);
    //         if is_image {
    //             // API 返回了图片 URL，发送图片消息
    //             let image_msg = Msg::txt(&room.name).endl()
    //                 .image(&result)
    //                 .build();
    //             let _ = Bot::send_group_msg(group_id, image_msg);
    //         } else {
    //             // 回退到文本消息
    //             let _ = Bot::send_group_msg(group_id, CString::new(result).unwrap());
    //         }
    //     } else {
    //         let msg = CString::new("暂无配置的直播间").unwrap();
    //         let _ = Bot::send_group_msg(group_id, msg);
    //     }
    //     return;
    // }
}

/// 判断是否为定时报表查询指令
/// 支持：土豆周报/月报/年报、🥔周报/月报/年报
fn is_report_query(msg: &str) -> Option<&'static str> {
    let patterns: &[(&str, &str)] = &[
        ("土豆周报", "weekly"),
        ("土豆月报", "monthly"),
        ("土豆年报", "yearly"),
        ("\u{1f954}周报", "weekly"),
        ("\u{1f954}月报", "monthly"),
        ("\u{1f954}年报", "yearly"),
    ];

    for &(pattern, report_type) in patterns {
        if msg.contains(pattern) {
            return Some(report_type);
        }
    }
    None
}

fn handle_task_event(json: &str) {
    live::handle_task_event(json);
}

// ── 单元测试 ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_report_query ────────────────────────────────────

    #[test]
    fn test_report_weekly_cn() {
        assert_eq!(is_report_query("土豆周报"), Some("weekly"));
    }

    #[test]
    fn test_report_monthly_cn() {
        assert_eq!(is_report_query("土豆月报"), Some("monthly"));
    }

    #[test]
    fn test_report_yearly_cn() {
        assert_eq!(is_report_query("土豆年报"), Some("yearly"));
    }

    #[test]
    fn test_report_weekly_emoji() {
        assert_eq!(is_report_query("\u{1f954}周报"), Some("weekly"));
    }

    #[test]
    fn test_report_monthly_emoji() {
        assert_eq!(is_report_query("\u{1f954}月报"), Some("monthly"));
    }

    #[test]
    fn test_report_yearly_emoji() {
        assert_eq!(is_report_query("\u{1f954}年报"), Some("yearly"));
    }

    #[test]
    fn test_report_no_match() {
        assert_eq!(is_report_query("土豆直播"), None);
        assert_eq!(is_report_query("你好"), None);
        assert_eq!(is_report_query(""), None);
    }
}

