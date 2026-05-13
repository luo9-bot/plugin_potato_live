// src/core.rs

use luo9_sdk::bus::Bus;
use luo9_sdk::payload::*;

use crate::{handle_group_msg, handle_task_event, live};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_main() {
    let msg_sub = Bus::topic("luo9_message").subscribe().unwrap();
    let task_sub = Bus::topic("luo9_task").subscribe().unwrap();
    let ver_sub = Bus::topic("luo9_version").subscribe().unwrap();

    let msg_topic = Bus::topic("luo9_message");
    let task_topic = Bus::topic("luo9_task");
    let ver_topic = Bus::topic("luo9_version");

    live::init();    

    loop {
        // ── 消息 ──
        if let Some(json) = msg_topic.pop(msg_sub) {
            if let Some(BusPayload::Message(msg)) = BusPayload::parse(&json) {
                match msg.message_type {
                    MsgType::Group => {
                        handle_group_msg(msg.group_id.unwrap_or(0), msg.user_id, &msg.message);
                    }
                    _ => {}
                }
            }
        }
        // ── 定时任务事件 ──
        if let Some(json) = task_topic.pop(task_sub) {
            handle_task_event(&json);
        }

        // ── 版本查询 ──
        if let Some(json) = ver_topic.pop(ver_sub) {
            if luo9_sdk::version::is_version_query(&json) {
                luo9_sdk::version::reply_version(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            }
        }

        // 短暂让出 CPU，避免空转
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
