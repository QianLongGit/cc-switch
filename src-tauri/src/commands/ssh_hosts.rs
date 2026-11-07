#![allow(non_snake_case)]
//! SSH 远程主机管理命令：配置文件主机枚举、连通性测试、依赖探测。
//!
//! 三者都是对 [`crate::ssh_config`] 与 [`crate::session_manager::ssh`] 传输层的
//! 薄封装，逻辑集中在被封装侧，本模块只做阻塞转异步与错误文案映射。

use std::time::Duration;

use crate::session_manager::ssh::{run_remote, ssh_binary_available};
use crate::settings::SshHostConfig;
use crate::ssh_config::{load_ssh_config_hosts, SshConfigHost};

/// 列出 `~/.ssh/config` 中的主机条目（通配与 Match 段已过滤）。
/// 读不到配置文件时返回空清单而非报错——该能力「有则展示」。
#[tauri::command]
pub async fn list_ssh_config_hosts() -> Result<Vec<SshConfigHost>, String> {
    Ok(load_ssh_config_hosts())
}

/// 测试单台主机连通性：远端执行 `echo ok`，15 秒内返回即认为可达。
/// 失败文案来自 [`crate::session_manager::ssh::SshError`] 的中文 Display，直接给 UI 展示。
#[tauri::command]
pub async fn test_ssh_host(host: SshHostConfig) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        match run_remote(&host, "echo ok", Duration::from_secs(15)) {
            Ok(_) => Ok(true),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| format!("连通性测试任务失败: {e}"))?
}

/// 探测本机 ssh 可执行文件是否存在（不发起网络连接）。
#[tauri::command]
pub fn check_ssh_availability() -> bool {
    ssh_binary_available()
}
