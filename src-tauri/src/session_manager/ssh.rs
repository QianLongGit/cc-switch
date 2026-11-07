//! SSH 远程会话传输层：命令构造、远端路径转义、进程执行与源路径解析。
//!
//! 仅依赖标准库 + Task 1 的 [`SshHostConfig`]：
//!
//! - [`build_ssh_command`]：构造 `ssh` 命令（BatchMode / ConnectTimeout / 连接复用三参数）
//! - [`escape_remote_path`]：POSIX 单引号转义，供远端命令内嵌路径
//! - [`run_command`]：双线程读 stdout/stderr + `try_wait` 轮询 + 超时 kill
//! - [`run_remote`]：组合上述两者，完成一次远端命令执行
//! - [`parse_ssh_source`]：解析 `ssh:<hostId>/<远端绝对路径>` 源路径
//! - [`SshScanRegistry`]：远端扫描清单的进程内存单例

use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::settings::SshHostConfig;

/* ===== 常量 ===== */

/// stdout 采集上限（16MB，防御大输出拖垮内存；会话文件远小于此）
const MAX_STDOUT_BYTES: u64 = 16 * 1024 * 1024;
/// stderr 采集上限（4KB，仅用于错误文案）
const MAX_STDERR_BYTES: u64 = 4096;
/// `try_wait` 轮询间隔
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// ssh 连接建立超时（秒）；spec 全局约束：ssh 命令统一 ConnectTimeout=8
const CONNECT_TIMEOUT_SECS: &str = "8";
/// 连接复用的存活时长；Windows OpenSSH 不支持，仅非 Windows 启用
const CONTROL_PERSIST: &str = "10m";

/* ===== 错误分类 ===== */

/// SSH 传输层错误，按「连接层 / 远端层 / 超时 / 缺依赖」四类区分，
/// 供上层映射为用户可读提示。
#[derive(Debug, Error)]
pub(crate) enum SshError {
    /// ssh 退出码 255：连接拒绝、DNS 失败、认证失败等
    #[error("SSH 连接或认证失败: {0}")]
    ConnectOrAuth(String),
    /// 远端命令自身失败（非 255 的非零退出码）
    #[error("远端命令执行失败: {0}")]
    Remote(String),
    #[error("SSH 连接超时")]
    Timeout,
    #[error("系统未安装 ssh")]
    SshUnavailable,
}

/* ===== 路径转义 ===== */

/// POSIX 单引号转义：整体包一层单引号，内部 `'` 替换为 `'\''`。
/// 空串返回 `''`，保证语义为空参数而非缺参数。
// 后续任务构造远端命令时接线，届时移除
#[allow(dead_code)]
pub(crate) fn escape_remote_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 2);
    out.push('\'');
    for c in p.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/* ===== 二进制探测 ===== */

/// 遍历 `PATH` 各段查找 ssh 可执行文件（Windows 找 `ssh.exe`），不引入新依赖。
// 后续任务接线 UI 层后移除
#[allow(dead_code)]
pub(crate) fn ssh_binary_available() -> bool {
    let name = if cfg!(windows) { "ssh.exe" } else { "ssh" };
    let separator = if cfg!(windows) { ';' } else { ':' };
    std::env::var("PATH")
        .map(|paths| {
            paths
                .split(separator)
                .filter(|dir| !dir.is_empty())
                .any(|dir| is_executable(&PathBuf::from(dir).join(name)))
        })
        .unwrap_or(false)
}

/// 判断候选路径是否为可执行文件（unix 校验执行位，Windows 仅校验存在且为文件）
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

/* ===== 命令构造 ===== */

/// 构造 `ssh <options> [user@]host <remote_cmd>`。
///
/// - 基础参数：`BatchMode=yes`（禁止交互挂起）、`ConnectTimeout=8`
/// - 非 Windows 追加连接复用三参数（`ControlMaster=auto` /
///   `ControlPath=<配置目录>/ssh/cm-%C` / `ControlPersist=10m`），
///   复用目录惰性创建，创建失败则降级为不复用（可用性优先）
/// - `remote_cmd` 原样作为最后一个参数传递，内嵌路径由调用方经
///   [`escape_remote_path`] 转义
pub(crate) fn build_ssh_command(host: &SshHostConfig, remote_cmd: &str) -> Command {
    let mut cmd = Command::new("ssh");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(identity) = host.identity_file.as_deref().filter(|s| !s.is_empty()) {
        cmd.arg("-i").arg(identity);
    }
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o").arg(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"));

    #[cfg(not(windows))]
    if let Some(control_path) = control_socket_dir() {
        cmd.arg("-o").arg("ControlMaster=auto");
        cmd.arg("-o").arg(format!("ControlPath={}/cm-%C", control_path.display()));
        cmd.arg("-o").arg(format!("ControlPersist={CONTROL_PERSIST}"));
    }

    let destination = match host.user.as_deref().filter(|s| !s.is_empty()) {
        Some(user) => format!("{user}@{}", host.host),
        None => host.host.clone(),
    };
    cmd.arg(destination).arg(remote_cmd);

    // GUI 应用下 spawn 不弹控制台窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    cmd
}

/// 返回连接复用 socket 目录并惰性创建；创建失败返回 None（调用方降级为不复用）。
fn control_socket_dir() -> Option<PathBuf> {
    let dir = crate::config::get_app_config_dir().join("ssh");
    std::fs::create_dir_all(&dir).ok().map(|_| dir)
}

/* ===== 进程执行 ===== */

/// 执行命令：双线程并发读 stdout（截 16MB）/ stderr（截 4KB），
/// `try_wait` 轮询直到退出或超时，超时 kill 整个进程组。
///
/// 退出码映射：255 → [`SshError::ConnectOrAuth`]，其余非零 → [`SshError::Remote`]。
pub(crate) fn run_command(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, SshError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => SshError::SshUnavailable,
            _ => SshError::ConnectOrAuth(format!("无法启动 ssh 进程: {e}")),
        })?;

    // 管道读完前线程不会退出；超时 kill 会关闭管道，读线程随之返回
    let stdout_handle = {
        let pipe = child.stdout.take().expect("stdout 已 piped");
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(MAX_STDOUT_BYTES).read_to_end(&mut bytes);
            bytes
        })
    };
    let stderr_handle = {
        let pipe = child.stderr.take().expect("stderr 已 piped");
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(MAX_STDERR_BYTES).read_to_end(&mut bytes);
            bytes
        })
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            // 超时或 try_wait 异常：kill 并回收，避免僵尸进程
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr_handle.join().unwrap_or_default())
        .trim()
        .to_string();

    match status {
        None => Err(SshError::Timeout),
        Some(status) if status.success() => Ok(stdout),
        Some(status) => {
            // ssh 约定：255 表示连接/认证层失败，其余非零为远端命令失败
            let detail = if stderr.is_empty() {
                format!("退出码 {}", status.code().unwrap_or(-1))
            } else {
                stderr
            };
            if status.code() == Some(255) {
                Err(SshError::ConnectOrAuth(detail))
            } else {
                Err(SshError::Remote(detail))
            }
        }
    }
}

/* ===== 组合入口 ===== */

/// 在指定主机上执行远端命令并取回 stdout。
// 后续任务接线数据层后移除
#[allow(dead_code)]
pub(crate) fn run_remote(
    host: &SshHostConfig,
    remote_cmd: &str,
    timeout: Duration,
) -> Result<Vec<u8>, SshError> {
    run_command(build_ssh_command(host, remote_cmd), timeout)
}

/* ===== 源路径解析 ===== */

/// 解析 `ssh:<hostId>/<远端绝对路径>` 形态的源路径，返回 `(host_id, remote_path)`。
///
/// 规则：
/// - 前缀必须是 `ssh:`
/// - 按首个 `/` 切分出 hostId 与路径，hostId 由此天然不含 `/` 且必须非空
/// - 路径必须为非空绝对路径（`/` 开头），且不含 `..` 段（防止逃逸目标目录）
///
/// 不合法返回 `None`，由上层映射为用户可读错误。
// Task 6/7 接线 validate/scan 后移除
#[allow(dead_code)]
pub(crate) fn parse_ssh_source(raw: &str) -> Option<(String /*host_id*/, String /*remote_path*/)> {
    let rest = raw.strip_prefix("ssh:")?;
    // 首个 `/` 之前是 hostId；之后（含斜杠本体）是远端绝对路径
    let (host_id, path) = rest.split_once('/')?;
    if host_id.is_empty() || path.is_empty() {
        return None;
    }
    let path = format!("/{path}");
    if path.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some((host_id.to_string(), path))
}

/* ===== 扫描清单 Registry ===== */

/// 远端文件元数据：扫描时点的字节长度与修改时间（Unix 毫秒）。
/// 供 fingerprint 判定「远端文件是否变化、缓存是否需失效」。
// Task 6/7 接线 fingerprint 后移除
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileMeta {
    pub len: u64,
    pub mtime_ms: i64,
}

/// SSH 远端扫描清单（进程内存单例）：`hostId → { 路径 → 元数据 }`。
///
/// 扫描任务整体写入（[`SshScanRegistry::replace_manifest`]），
/// validate / fingerprint / 删除会话按路径查询与摘除。
// Task 6/7 接线后移除
#[allow(dead_code)]
pub(crate) struct SshScanRegistry {
    /// 各主机的文件清单
    manifests: RwLock<HashMap<String, HashMap<String, RemoteFileMeta>>>,
}

static GLOBAL: LazyLock<SshScanRegistry> =
    LazyLock::new(|| SshScanRegistry {
        manifests: RwLock::new(HashMap::new()),
    });

// Task 6/7 接线后移除
#[allow(dead_code)]
impl SshScanRegistry {
    /// 进程级共享清单（单例参照 `cache.rs::global()` 的 LazyLock 方案）
    pub(crate) fn global() -> &'static Self {
        &GLOBAL
    }

    /// 全量替换某主机的清单，旧清单整体丢弃
    pub(crate) fn replace_manifest(&self, host_id: &str, entries: HashMap<String, RemoteFileMeta>) {
        self.lock_write().insert(host_id.to_string(), entries);
    }

    /// 查询某主机某路径的元数据
    pub(crate) fn lookup(&self, host_id: &str, path: &str) -> Option<RemoteFileMeta> {
        self.lock_read().get(host_id)?.get(path).copied()
    }

    /// 摘除单条条目（删除远端会话成功后调用）；返回条目原本是否存在
    pub(crate) fn remove_entry(&self, host_id: &str, path: &str) -> bool {
        self.lock_write()
            .get_mut(host_id)
            .is_some_and(|entries| entries.remove(path).is_some())
    }

    /// 取读锁；锁中毒时直接恢复数据继续用（清单为可重建的缓存态，无不变量需保护）
    fn lock_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, HashMap<String, RemoteFileMeta>>> {
        self.manifests
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 取写锁；中毒恢复策略同 [`Self::lock_read`]
    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, HashMap<String, RemoteFileMeta>>> {
        self.manifests
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ===== 测试辅助 ===== */

    /// 将 Command 参数转为 Vec<String>，便于断言
    fn args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// 构造最小可用的主机配置
    fn host(user: Option<&str>, port: Option<u16>, identity: Option<&str>) -> SshHostConfig {
        SshHostConfig {
            id: "test-host".to_string(),
            label: "测试主机".to_string(),
            host: "example.com".to_string(),
            user: user.map(|s| s.to_string()),
            port,
            identity_file: identity.map(|s| s.to_string()),
        }
    }

    /* ===== 路径转义 ===== */

    #[test]
    fn escape_wraps_in_single_quotes() {
        assert_eq!(escape_remote_path("/a b.jsonl"), "'/a b.jsonl'");
    }

    #[test]
    fn escape_handles_embedded_single_quote() {
        assert_eq!(escape_remote_path("a'b"), "'a'\\''b'");
    }

    #[test]
    fn escape_empty_path_yields_empty_quotes() {
        assert_eq!(escape_remote_path(""), "''");
    }

    /* ===== 命令构造 ===== */

    #[test]
    fn build_contains_common_options_and_user_at_host() {
        let cmd = build_ssh_command(&host(Some("dev"), None, None), "cat /tmp/x");
        let argv = args(&cmd);
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ConnectTimeout=8".to_string()));
        assert!(argv.contains(&"ControlPersist=10m".to_string()));
        assert!(argv.contains(&"dev@example.com".to_string()));
        // remote_cmd 必须是最后一个参数
        assert_eq!(argv.last().unwrap(), "cat /tmp/x");
    }

    #[test]
    fn build_without_user_uses_bare_host() {
        let cmd = build_ssh_command(&host(None, None, None), "true");
        let argv = args(&cmd);
        assert!(argv.contains(&"example.com".to_string()));
        assert!(!argv.iter().any(|a| a.contains('@')));
    }

    #[test]
    fn build_includes_port_and_identity_when_present() {
        let cmd = build_ssh_command(
            &host(Some("dev"), Some(2222), Some("/Users/dev/.ssh/id_ed25519")),
            "true",
        );
        let argv = args(&cmd);
        let port_idx = argv.iter().position(|a| a == "-p").unwrap();
        assert_eq!(argv[port_idx + 1], "2222");
        let ident_idx = argv.iter().position(|a| a == "-i").unwrap();
        assert_eq!(argv[ident_idx + 1], "/Users/dev/.ssh/id_ed25519");
    }

    #[cfg(not(windows))]
    #[test]
    fn build_adds_control_master_triple_on_unix() {
        let cmd = build_ssh_command(&host(Some("dev"), None, None), "true");
        let argv = args(&cmd);
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        // ControlPath 落在应用配置目录的 ssh 子目录，且惰性创建该目录
        let cp_idx = argv
            .iter()
            .position(|a| a.starts_with("ControlPath="))
            .unwrap();
        let cp = &argv[cp_idx];
        assert!(cp.contains("cm-%C"), "ControlPath 应使用 cm-%C: {cp}");
        let dir = cp.trim_start_matches("ControlPath=").trim_end_matches("/cm-%C");
        assert!(std::path::Path::new(dir).is_dir(), "目录应被惰性创建: {dir}");
    }

    /* ===== 进程执行 ===== */

    #[test]
    fn run_captures_stdout() {
        let mut cmd = std::process::Command::new("/bin/echo");
        cmd.arg("ok");
        let out = run_command(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out, b"ok\n");
    }

    #[test]
    fn run_times_out_and_kills() {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("5");
        let err = run_command(cmd, Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, SshError::Timeout));
    }

    #[test]
    fn run_maps_exit_1_to_remote() {
        // 本机 /bin 可能无 false（macOS 精简版），用 sh 退出码更稳
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 1"]);
        let err = run_command(cmd, Duration::from_secs(5)).unwrap_err();
        match err {
            SshError::Remote(_) => {}
            other => panic!("期望 Remote，实际 {other:?}"),
        }
    }

    #[test]
    fn run_maps_exit_255_to_connect_or_auth() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 255"]);
        let err = run_command(cmd, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, SshError::ConnectOrAuth(_)));
    }

    /* ===== 错误文案 ===== */

    #[test]
    fn error_display_is_chinese() {
        assert!(SshError::ConnectOrAuth("x".into()).to_string().contains("连接"));
        assert!(SshError::Remote("x".into()).to_string().contains("远端"));
        assert!(SshError::Timeout.to_string().contains("超时"));
        assert!(SshError::SshUnavailable.to_string().contains("ssh"));
    }

    /* ===== 源路径解析 ===== */

    #[test]
    fn parse_valid_ssh_source() {
        assert_eq!(
            parse_ssh_source("ssh:h1/home/u/.claude/projects/x.jsonl"),
            Some((
                "h1".to_string(),
                "/home/u/.claude/projects/x.jsonl".to_string()
            ))
        );
    }

    #[test]
    fn parse_rejects_missing_ssh_prefix() {
        assert_eq!(parse_ssh_source("/home/u/x.jsonl"), None);
        assert_eq!(parse_ssh_source("h1/home/u/x.jsonl"), None);
    }

    #[test]
    fn parse_rejects_empty_host_id() {
        // 前缀后直接是 /（含 //）：按首个 / 切分后 hostId 为空
        assert_eq!(parse_ssh_source("ssh:/home/u"), None);
        assert_eq!(parse_ssh_source("ssh://home/u"), None);
    }

    #[test]
    fn parse_rejects_missing_path() {
        // host 后无 /：取不出绝对路径
        assert_eq!(parse_ssh_source("ssh:h1"), None);
        assert_eq!(parse_ssh_source("ssh:h1.jsonl"), None);
        // 仅根斜杠：路径为空
        assert_eq!(parse_ssh_source("ssh:h1/"), None);
    }

    #[test]
    fn parse_rejects_parent_dir_segments() {
        assert_eq!(parse_ssh_source("ssh:h1/home/u/../x.jsonl"), None);
        assert_eq!(parse_ssh_source("ssh:h1/a/b/../c.jsonl"), None);
        // 仅独立的 ".." 段被拒；".claude" 这类单点前缀合法
        assert!(parse_ssh_source("ssh:h1/home/u/.claude/x.jsonl").is_some());
    }

    /* ===== 扫描清单 Registry ===== */

    /// 构造远端文件元数据
    fn meta(len: u64, mtime_ms: i64) -> RemoteFileMeta {
        RemoteFileMeta { len, mtime_ms }
    }

    /// 测试间共享 global 单例：各测试用独立 host_id，避免并行互扰
    #[test]
    fn registry_replace_manifest_swaps_whole_list() {
        let reg = SshScanRegistry::global();
        let host = "t-replace";
        reg.replace_manifest(
            host,
            HashMap::from([("/a.jsonl".to_string(), meta(10, 100))]),
        );
        // 第二次写入应整体丢弃旧清单
        reg.replace_manifest(
            host,
            HashMap::from([("/b.jsonl".to_string(), meta(20, 200))]),
        );
        assert!(reg.lookup(host, "/a.jsonl").is_none(), "旧清单条目应被全量替换");
        assert_eq!(reg.lookup(host, "/b.jsonl"), Some(meta(20, 200)));
    }

    #[test]
    fn registry_lookup_hit_and_miss() {
        let reg = SshScanRegistry::global();
        let host = "t-lookup";
        reg.replace_manifest(
            host,
            HashMap::from([("/a.jsonl".to_string(), meta(10, 100))]),
        );
        assert_eq!(reg.lookup(host, "/a.jsonl"), Some(meta(10, 100)));
        // 路径未命中 / 主机未命中
        assert_eq!(reg.lookup(host, "/missing.jsonl"), None);
        assert_eq!(reg.lookup("t-lookup-other", "/a.jsonl"), None);
    }

    #[test]
    fn registry_remove_entry_then_lookup_misses() {
        let reg = SshScanRegistry::global();
        let host = "t-remove";
        reg.replace_manifest(
            host,
            HashMap::from([("/a.jsonl".to_string(), meta(10, 100))]),
        );
        assert!(reg.remove_entry(host, "/a.jsonl"));
        assert_eq!(reg.lookup(host, "/a.jsonl"), None);
        // 删除不存在的条目返回 false，不 panic
        assert!(!reg.remove_entry(host, "/a.jsonl"));
    }
}
