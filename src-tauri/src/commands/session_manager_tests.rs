use super::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[test]
#[serial_test::serial]
fn borrowed_session_responses_preserve_the_json_and_channel_contract() {
    let _home = crate::commands::startup_tests::TestHome::new();
    let root = crate::config::get_claude_settings_path()
        .parent()
        .unwrap()
        .join("projects/runtime");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("runtime.jsonl");
    let record = json!({"type":"user", "uuid":"u1", "sessionId":"runtime",
        "message":{"role":"user", "content":"你好\n\"quoted\" 😀"}});
    std::fs::write(&path, format!("{record}\n")).unwrap();
    let source = path.to_string_lossy().into_owned();
    let expected =
        serde_json::to_value(session_manager::load_messages("claude", &source).unwrap()).unwrap();
    assert_eq!(expected.as_array().unwrap().len(), 1);

    let response =
        tauri::async_runtime::block_on(get_session_messages("claude".into(), source.clone()))
            .unwrap();
    assert_eq!(response.deserialize::<Value>().unwrap(), expected);

    let chunks = Arc::new(Mutex::new(Vec::<Value>::new()));
    let received = Arc::clone(&chunks);
    let channel = Channel::new(move |body| {
        received
            .lock()
            .unwrap()
            .push(body.deserialize::<Value>().unwrap());
        Ok(())
    });
    tauri::async_runtime::block_on(stream_session_messages("claude".into(), source, channel))
        .unwrap();
    let chunks = chunks.lock().unwrap();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0]["type"], "header");
    assert_eq!(chunks[0]["total"], 1);
    assert!(chunks[0]["parseMs"].is_number());
    assert_eq!(
        chunks[1],
        json!({"type":"messages", "start":0, "messages":expected})
    );
    assert_eq!(chunks[2]["type"], "done");
    assert!(chunks[2]["payloadBytes"].as_u64().unwrap() > 0);
}

#[test]
fn failed_session_stream_still_sends_an_error_packet() {
    let chunks = Arc::new(Mutex::new(Vec::<Value>::new()));
    let received = Arc::clone(&chunks);
    let channel = Channel::new(move |body| {
        received
            .lock()
            .unwrap()
            .push(body.deserialize::<Value>().unwrap());
        Ok(())
    });
    let error = tauri::async_runtime::block_on(stream_session_messages(
        "unknown".into(),
        "missing".into(),
        channel,
    ))
    .unwrap_err();
    assert_eq!(
        *chunks.lock().unwrap(),
        [json!({"type":"error", "message":error})]
    );
}

#[test]
#[serial_test::serial]
fn search_session_content_routes_remote_host_id() {
    let _home = crate::commands::startup_tests::TestHome::new();

    // hostId 缺省：本机路径零变化（TestHome 下无索引 → 空 hits，命令仍 Ok）
    let local =
        tauri::async_runtime::block_on(search_session_content("needle".into(), None, None, None))
            .unwrap();
    assert!(local.hits.is_empty());

    // hostId 指向不存在的主机：可读错误（与 load_messages 的远程文案一致）
    let missing = tauri::async_runtime::block_on(search_session_content(
        "needle".into(),
        None,
        None,
        Some("ghost-host".into()),
    ))
    .unwrap_err();
    assert_eq!(missing, "SSH 主机不存在或已移除");

    // hostId 存在但从未扫描（registry miss）：空结果而非错误
    let original = crate::settings::get_settings();
    let mut with_host = original.clone();
    with_host.ssh_hosts = Some(vec![crate::settings::SshHostConfig {
        id: "search-miss-host".to_string(),
        label: "search-miss-host".to_string(),
        host: "example.invalid".to_string(),
        user: None,
        port: None,
        identity_file: None,
    }]);
    crate::settings::update_settings(with_host).expect("注入搜索测试主机");
    let remote = tauri::async_runtime::block_on(search_session_content(
        "needle".into(),
        None,
        None,
        Some("search-miss-host".into()),
    ))
    .unwrap();
    assert!(remote.hits.is_empty());
    assert_eq!(
        remote.status,
        session_manager::search::IndexStatus::default()
    );
    crate::settings::update_settings(original).expect("恢复 settings");
}
