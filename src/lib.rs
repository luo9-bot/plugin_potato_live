// src/lib.rs

pub mod core;
pub mod live;
pub mod path;

// use luo9_sdk::Bot;
// use luo9_sdk::bus::Bus;
// use luo9_sdk::command::{Command, PrefixMode};
// use luo9_sdk::payload::*;
// use std::ffi::CString;
// use serde_json::json;

pub fn handle_group_msg(_group_id: u64, _user_id: u64, _msg: &str) {
    let _admin_qq = live::LiveMonitorConfig::admin();

    // if _user_id == admin_qq && let Some(cmd) = Command::parse(msg, "土豆", PrefixMode::Required('/')) {    
    //     let reply = |text: String| { let _ = Bot::send_group_msg(group_id, CString::new(text).unwrap()); };
    //     cmd.on("开启", |args| handle_task_start(&reply, args))
    //         .on("关闭", |args| handle_task_end(&reply, args));
    // }
}

fn handle_task_event(json: &str) {
    live::handle_task_event(json);
}

