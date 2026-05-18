// API 报告测试
// 运行: cargo test --test api_report_test -- --nocapture
// 生成的 PNG 图片将复制到 target/test_output/ 目录

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use plugin_potato_live::live;

/// 测试用的房间 ID
const TEST_ROOM_ID: u64 = 13160979;

fn get_mock_sessions_jsonl() -> Vec<&'static str> {
    vec![
        r#"{"room_id":13160979,"start":"2026-05-14T09:56:00.825296639+08:00","end":"2026-05-14T14:19:00.636183118+08:00","duration_secs":15779,"weekday":3,"start_hour":9,"end_hour":14}"#,
        r#"{"room_id":13160979,"start":"2026-05-15T10:13:00.295659940+08:00","end":"2026-05-15T15:24:00.272495576+08:00","duration_secs":18659,"weekday":4,"start_hour":10,"end_hour":15}"#,
        r#"{"room_id":13160979,"start":"2026-05-16T12:35:01.054997753+08:00","end":"2026-05-16T15:57:01.097099645+08:00","duration_secs":12120,"weekday":5,"start_hour":12,"end_hour":15}"#,
        r#"{"room_id":13160979,"start":"2026-05-18T10:15:01.035196505+08:00","end":"2026-05-18T14:04:01.059786791+08:00","duration_secs":13740,"weekday":0,"start_hour":10,"end_hour":14}"#,
        r#"{"room_id":13160979,"start":"2026-05-18T19:00:00+08:00","end":"2026-05-19T02:00:00+08:00","duration_secs":25200,"weekday":0,"start_hour":19,"end_hour":2}"#,
        r#"{"room_id":13160979,"start":"2026-04-30T22:00:00+08:00","end":"2026-05-01T03:00:00+08:00","duration_secs":18000,"weekday":3,"start_hour":22,"end_hour":3}"#,
    ]
}

fn write_sessions_file(data_dir: &PathBuf) {
    let path = data_dir.join(format!("sessions_{}.json", TEST_ROOM_ID));
    let mut file = fs::OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&path).expect("无法创建 sessions 文件");
    for line in get_mock_sessions_jsonl() {
        writeln!(file, "{}", line).expect("写入失败");
    }
    println!("  ✓ 写入 sessions 文件");
}

fn write_runtime_state_file(data_dir: &PathBuf) {
    let path = data_dir.join(format!("runtime_state_{}.json", TEST_ROOM_ID));
    fs::write(&path, r#"{"room_id":13160979,"is_live":false,"current_start":null,"consecutive_failures":0}"#)
        .expect("无法写入 runtime_state");
    println!("  ✓ 写入 runtime_state 文件");
}

fn write_config(data_dir: &PathBuf) {
    let path = data_dir.join("config.yaml");
    fs::write(&path, format!(r#"
admin: 123456
rooms:
  - room_id: {}
    name: "土豆"
push_groups:
  - 123456789
push_on_start: true
push_on_end: false
report_api_url: ""
"#, TEST_ROOM_ID)).expect("无法写入 config");
    println!("  ✓ 写入 config 文件");
}

fn save_json(name: &str, json: &serde_json::Value) {
    let out = PathBuf::from("target").join("test_output");
    fs::create_dir_all(&out).expect("无法创建 output 目录");
    let path = out.join(name);
    let pretty = serde_json::to_string_pretty(json).expect("序列化失败");
    fs::write(&path, &pretty).expect("无法写入输出文件");
    println!("  ✓ 保存 JSON: {} ({} bytes)", name, pretty.len());
}

/// 从 data/plugin_potato_live/cache/ 中找到最新匹配的 PNG 文件，复制到 target/test_output/
fn copy_latest_png(prefix: &str, dest_name: &str) {
    let cache_dir = PathBuf::from("data").join("plugin_potato_live").join("cache");
    if !cache_dir.exists() {
        return;
    }
    let mut candidates: Vec<_> = fs::read_dir(&cache_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix) && n.ends_with(".png"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort_by_key(|e| e.path().metadata().ok().and_then(|m| m.modified().ok()));

    if let Some(latest) = candidates.last() {
        let dest = PathBuf::from("target").join("test_output").join(dest_name);
        let _ = fs::copy(latest.path(), &dest);
        let size = fs::metadata(&dest).ok().map(|m| m.len()).unwrap_or(0);
        println!("  ✓ 复制图片: {} ({} bytes)", dest_name, size);
    }
}

#[test]
fn test_api_handlers() {
    println!("\n═══════════════════════════════════════");
    println!("  API Handler 测试（周/月/年）");
    println!("═══════════════════════════════════════\n");

    let data_dir = PathBuf::from("data").join("plugin_potato_live");
    fs::create_dir_all(&data_dir).expect("无法创建数据目录");

    println!("📂 准备测试数据...");
    write_sessions_file(&data_dir);
    write_runtime_state_file(&data_dir);
    // write_config(&data_dir);

    println!("\n🔧 初始化模块...");
    live::init();
    println!("  ✓ 初始化完成\n");

    // ── 周报 ──
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  📋 周报 /api/report/weekly");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let weekly: serde_json::Value = live::build_weekly_report_data(TEST_ROOM_ID, "土豆");
    save_json("weekly_report.json", &weekly);
    {
        let (path, ok) = live::handle_weekly_query(TEST_ROOM_ID, "土豆");
        if ok {
            println!("  ✅ 图片已保存到: {}", path);
            copy_latest_png("report_weekly", "weekly_report.png");
        } else {
            println!("  ⚠ API 未返回图片");
        }
    }

    // ── 月报 ──
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  📋 月报 /api/report/monthly");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let monthly = live::build_monthly_report_data(TEST_ROOM_ID, "土豆");
    save_json("monthly_report.json", &monthly);
    {
        let (path, ok) = live::handle_monthly_query(TEST_ROOM_ID, "土豆");
        if ok {
            println!("  ✅ 图片已保存到: {}", path);
            copy_latest_png("report_monthly", "monthly_report.png");
        } else {
            println!("  ⚠ API 未返回图片");
        }
    }

    // ── 年报 ──
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  📋 年报 /api/report/yearly");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let yearly = live::build_yearly_report_data(TEST_ROOM_ID, "土豆");
    save_json("yearly_report.json", &yearly);
    {
        let (path, ok) = live::handle_yearly_query(TEST_ROOM_ID, "土豆");
        if ok {
            println!("  ✅ 图片已保存到: {}", path);
            copy_latest_png("report_yearly", "yearly_report.png");
        } else {
            println!("  ⚠ API 未返回图片");
        }
    }

    println!("\n═══════════════════════════════════════");
    println!("  ✅ 测试完成");
    println!("  输出目录: target/test_output/");
    println!("═══════════════════════════════════════\n");
}
