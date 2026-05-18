// src/lib.rs

pub mod core;
pub mod live;
pub mod path;

use luo9_sdk::Bot;
use luo9_sdk::Msg;
use std::ffi::CString;

pub fn handle_group_msg(group_id: u64, _user_id: u64, msg: &str) {
    let msg_trimmed = msg.trim();

    // 模糊匹配"土豆今天直播了没"及其变体
    if is_live_query(msg_trimmed) {
        let rooms = live::LiveMonitorConfig::rooms();
        if let Some(room) = rooms.first() {
            let (result, is_image) = live::handle_daily_query(room.room_id, &room.name);
            if is_image {
                // API 返回了图片 URL，发送图片消息
                let image_msg = Msg::txt(&room.name).endl()
                    .image(&result)
                    .build();
                let _ = Bot::send_group_msg(group_id, image_msg);
            } else {
                // 回退到文本消息
                let _ = Bot::send_group_msg(group_id, CString::new(result).unwrap());
            }
        } else {
            let msg = CString::new("暂无配置的直播间").unwrap();
            let _ = Bot::send_group_msg(group_id, msg);
        }
        return;
    }
}

/// 判断是否为"土豆今天直播了没"类查询指令（支持模糊匹配和常见变体）
fn is_live_query(msg: &str) -> bool {
    let msg = msg.to_lowercase();

    // 必须包含"土豆"或"potato"
    let has_name = msg.contains("土豆") || msg.contains("potato");
    if !has_name {
        return false;
    }

    // 必须包含直播相关词（支持常见变体和错别字）
    let live_keywords = ["直播", "直潘", "直插", "播了", "播没", "live", "开播", "在播"];
    let has_live = live_keywords.iter().any(|&kw| msg.contains(kw));

    // 或者包含查询意图词
    let query_keywords = ["今天", "今天有", "查", "状态", "情况", "统计", "日报", "report"];
    let has_query = query_keywords.iter().any(|&kw| msg.contains(kw));

    if !has_live && !has_query {
        return false;
    }

    // 排除明显的非查询语句（如"开启直播"、"关闭直播"）
    let exclude_keywords = ["开启", "关闭", "开播提醒", "关播提醒", "设置"];
    if exclude_keywords.iter().any(|&kw| msg.contains(kw)) {
        return false;
    }

    true
}

fn handle_task_event(json: &str) {
    live::handle_task_event(json);
}

