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
//! - [`list_files_command`] / [`parse_list_output`]：远端列表命令与 TSV 解析
//! - [`head_tail_batch_command`] / [`parse_head_tail_output`]：批量头尾帧协议（哨兵标记分帧）
//! - [`decode_zst_prefix`]：zst 压缩前缀的截断容忍解压
//! - [`read_full_command`] / [`read_range_command`] / [`delete_file_command`]：全文 / 区间 / 删除命令
//! - [`session_id_matches_stem`] / [`ensure_deletable`]：远程删除的 stem 防误删校验
//!   与清单白名单判定（由 `mod.rs::delete_remote_session` 编排）
//! - [`fetch_to_temp_file`]：全文拉取落临时文件（路径生成与上限检查拆出
//!   [`temp_file_path`] / [`enforce_fetch_limit`] 纯函数，便于确定性测试）
//! - [`home_dirs_command`] / [`parse_home_dirs`] / [`classify_remote_provider`] /
//!   [`prefer_plain_entries`] / [`remote_head_tail_request`] / [`rebind_remote_meta`]：
//!   远程扫描链路的清单归类与请求组装纯函数（由 `mod.rs::scan_remote_sessions` 编排）
//! - [`build_remote_resume_command`]：远程会话的可复制恢复命令生成
//!   （`ssh -t ... 'cd <dir> && <内层>'`，用户手动执行口径，无自动执行参数）
//! - [`run_remote_with_codes`]：[`run_remote`] 的退出码变体——code 命中
//!   accepted 集合同样成功（搜索路径 grep/rg 无匹配 exit 1 = 空结果），
//!   255 恒为连接错误；共享执行体 [`spawn_and_collect`]，既有原语不动
//! - [`build_search_command`] / [`parse_search_output`] / [`detect_remote_rg`]：
//!   远程正文搜索原语（rg 优先 / grep 降级、`path:lineno:text` 单一解析
//!   器、rg 可用性 per-host 进程内缓存）

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

use crate::settings::SshHostConfig;

use super::SessionMeta;

/* ===== 常量 ===== */

/// stdout 采集上限（40MB，防御大输出拖垮内存）。口径：语义上限 32MB
/// （`content::MAX_TEXT_BYTES` 全文）+ 远程图片 20MB 级读取 + 余量，单一
/// 常量取其上界。上限语义：读线程 `take` 满 40MB 后关闭管道读端 → 远端
/// ssh 继续写 stdout 收 SIGPIPE 非零退出 → [`SshError::Remote`] 显式报错
/// （而非静默截断）；极端时序下恰好在读满时正常收尾，后续
/// [`enforce_fetch_limit`] 的 32MB 二次上限仍会在语义处拒绝。
const MAX_STDOUT_BYTES: u64 = 40 * 1024 * 1024;
/// stderr 采集上限（4KB，仅用于错误文案）
const MAX_STDERR_BYTES: u64 = 4096;
/// `try_wait` 轮询间隔
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// ssh 连接建立超时（秒）；spec 全局约束：ssh 命令统一 ConnectTimeout=8
const CONNECT_TIMEOUT_SECS: &str = "8";
/// 连接复用的存活时长；Windows OpenSSH 不支持，仅非 Windows 启用
const CONTROL_PERSIST: &str = "10m";
/// 压缩文件拉取的压缩字节前缀上限（256KiB），本地截断解压取头部行
const ZSTD_PREFIX_BYTES: u64 = 256 * 1024;

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
    /// 本地侧失败（大小上限拦截、临时文件落盘失败等），文案即最终用户提示
    #[error("{0}")]
    Local(String),
}

/* ===== 路径转义 ===== */

/// POSIX 单引号转义：整体包一层单引号，内部 `'` 替换为 `'\''`。
/// 空串返回 `''`，保证语义为空参数而非缺参数。
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
///   `ControlPath=<tmp 短目录>/cm-%C` / `ControlPersist=10m`），
///   复用目录惰性创建；创建失败或路径越出 unix 域 socket 安全区
///   （超长 / 空格 / 非 ASCII，见 [`control_path_safe`]）则降级为不复用
///   （可用性优先）
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
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"));

    // 测试开关命中时跳过连接复用（直连）；生产恒为 None 走复用。
    // 目录超长 / 含空格或非 ASCII 时同样降级直连——安全判定内聚在
    // [`control_socket_dir`]（先判后建，降级不落盘）
    #[cfg(not(windows))]
    let control_path = if mux_disabled_for_test() {
        None
    } else {
        control_socket_dir()
    };
    #[cfg(not(windows))]
    if let Some(control_path) = control_path {
        cmd.arg("-o").arg("ControlMaster=auto");
        cmd.arg("-o")
            .arg(format!("ControlPath={}/cm-%C", control_path.display()));
        cmd.arg("-o")
            .arg(format!("ControlPersist={CONTROL_PERSIST}"));
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

/// 纯推导 socket 目录路径（无副作用）：`/tmp/cc-switch-ssh-<USER|LOGNAME|"user">`。
///
/// build 侧创建与 localtest 点杀（`ssh_localtest.rs::shutdown_control_masters`）
/// 共用同一推导，保证 socket 位置与清理目标永不漂移。
#[cfg(not(windows))]
pub(crate) fn control_socket_dir_path() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "user".to_string());
    PathBuf::from("/tmp").join(format!("cc-switch-ssh-{user}"))
}

/// 返回连接复用 socket 目录并惰性创建；不可用（Windows / 创建失败 /
/// 路径越出 unix 域安全区）返回 None（调用方降级为不复用）。
///
/// 目录刻意选 `/tmp` 下按用户隔离的短路径而非应用配置目录——配置目录可能被
/// override 到深层路径（线上事故：`unix_listener: path "..." too long for
/// Unix domain socket`），叠加 `cm-` + `%C` 展开的 40 字符 hash + ssh 追加的
/// 随机后缀后会超出 unix 域 socket `sun_path` 的 104 字节上限，导致复用
/// 初始化整体失败。`/tmp` 全局可写，故目录权限收紧到 700 防 socket 被其他
/// 用户探测接管。安全判定先于落盘：降级路径不残留空目录。
#[cfg(not(windows))]
fn control_socket_dir() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = control_socket_dir_path();
    if !control_path_safe(&dir) {
        return None;
    }
    std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
        .ok()
        .map(|_| dir)
}

/// Windows 无 ControlMaster 复用，无需 socket 目录
#[cfg(windows)]
fn control_socket_dir() -> Option<PathBuf> {
    None
}

/// socket 路径展开长度上界（字节）：unix 域 `sun_path` 上限取各平台最小值
/// （macOS 104 / Linux 108）再留余量
#[cfg(not(windows))]
const CONTROL_PATH_MAX: usize = 100;

/// 判定 socket 目录是否仍处于 unix 域 socket 路径安全区。
///
/// 展开上界 = 目录字节数 + `/`(1) + `cm-`(3) + `%C` 展开的 hash(40) +
/// ssh 追加的随机后缀余量(20)。超长会在 mux 初始化时报
/// `unix_listener: path too long` 使整体连接失败；空格会被 `ControlPath=`
/// 的单参数值截断，非 ASCII 在部分 locale 下同理——命中任一即不安全，
/// 调用方应降级为直连（牺牲复用、功能完整）。
#[cfg(not(windows))]
fn control_path_safe(dir: &Path) -> bool {
    let projected = dir.as_os_str().len() + 1 + 3 + 40 + 20;
    projected <= CONTROL_PATH_MAX
        && dir
            .as_os_str()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_whitespace())
}

/* ===== 测试钩子：ControlMaster 禁用开关 ===== */

/// 测试专用全局开关：置 true 时 [`build_ssh_command`] 跳过连接复用三参数。
///
/// 动机：macOS 上从多线程进程 spawn 的 ssh 走 mux（unix socket 传 fd）时
/// `sendmsg` 偶发 `EMSGSIZE`（`mm_send_fd: sendmsg(1): Message too long`），
/// libtest 默认并发的多线程语境可触发——localhost 冒烟测试
/// （`ssh_localtest.rs`）置 true 直连 localhost 以免疫该偶发类别；CM 三参数
/// 的命令形态由 `build_adds_control_master_triple_on_unix` 单测覆盖，
/// 生产代码不受影响。
#[cfg(all(test, not(windows)))]
pub(crate) static DISABLE_CONTROL_MASTER_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 读取测试开关（生产构建恒 false，编译期消除）
#[cfg(all(not(windows), test))]
fn mux_disabled_for_test() -> bool {
    DISABLE_CONTROL_MASTER_FOR_TEST.load(std::sync::atomic::Ordering::Relaxed)
}

/// 生产构建占位：恒 false
#[cfg(all(not(windows), not(test)))]
fn mux_disabled_for_test() -> bool {
    false
}

/* ===== 进程执行 ===== */

/// 非零退出的错误文案：stderr 非空用 stderr，否则给退出码数字
///（`run_command` 与 [`resolve_exit_aware`] 共用，口径不分叉）。
fn exit_detail(stderr: &str, code: Option<i32>) -> String {
    if stderr.is_empty() {
        format!("退出码 {}", code.unwrap_or(-1))
    } else {
        stderr.to_string()
    }
}

/// 执行并采集（[`run_command`] 与 [`run_remote_with_codes`] 的共享执行体）：
/// 双线程并发读 stdout（截 40MB，超限语义见 [`MAX_STDOUT_BYTES`]）/ stderr
/// （截 4KB），`try_wait` 轮询直到退出或超时，超时 kill 整个进程组。
///
/// 返回 `(退出状态, stdout, stderr 文本)`——超时时状态为 `None`（由调用方
/// 各自映射 [`SshError::Timeout`]）；启动失败在此层返回 Err
/// （NotFound → SshUnavailable，其余 → ConnectOrAuth）。
fn spawn_and_collect(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(Option<std::process::ExitStatus>, Vec<u8>, String), SshError> {
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
    Ok((status, stdout, stderr))
}

/// 执行命令并取回 stdout。
///
/// 退出码映射：255 → [`SshError::ConnectOrAuth`]，其余非零 → [`SshError::Remote`]。
pub(crate) fn run_command(cmd: Command, timeout: Duration) -> Result<Vec<u8>, SshError> {
    let (status, stdout, stderr) = spawn_and_collect(cmd, timeout)?;
    match status {
        None => Err(SshError::Timeout),
        Some(status) if status.success() => Ok(stdout),
        Some(status) => {
            // ssh 约定：255 表示连接/认证层失败，其余非零为远端命令失败
            let detail = exit_detail(&stderr, status.code());
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
pub(crate) fn run_remote(
    host: &SshHostConfig,
    remote_cmd: &str,
    timeout: Duration,
) -> Result<Vec<u8>, SshError> {
    run_command(build_ssh_command(host, remote_cmd), timeout)
}

/// 退出码感知的结果装配（[`run_remote_with_codes`] 的映射核心，纯函数，
/// 表驱动单测目标）：
///
/// - exit 0（success）或 code 命中 `accepted` → `Ok(stdout)`——搜索路径的
///   grep / rg 无匹配 exit 1 = 空结果语义，属正常而非错误
/// - exit 255 恒为连接/认证错误——判定**优先于** accepted，防调用方把
///   连接层失败误当「可接受退出码」吞成空结果
/// - 其余非零（含信号终止无 code）→ [`SshError::Remote`] 真错误透传
///   （权限 / 文件消失等，禁止 `; :` 归零静默）
fn resolve_exit_aware(
    stdout: Vec<u8>,
    success: bool,
    code: Option<i32>,
    accepted: &[i32],
    stderr: &str,
) -> Result<Vec<u8>, SshError> {
    if code == Some(255) {
        return Err(SshError::ConnectOrAuth(exit_detail(stderr, code)));
    }
    if success || code.is_some_and(|c| accepted.contains(&c)) {
        return Ok(stdout);
    }
    Err(SshError::Remote(exit_detail(stderr, code)))
}

/// [`run_remote`] 的退出码变体：退出码在 `accepted` 集合内（含 0）同样
/// 视为成功并返回 stdout。仅搜索路径使用（无匹配非错误的退出码语义），
/// 既有 [`run_remote`] 原语不动。
pub(crate) fn run_remote_with_codes(
    host: &SshHostConfig,
    remote_cmd: &str,
    timeout: Duration,
    accepted: &[i32],
) -> Result<Vec<u8>, SshError> {
    let (status, stdout, stderr) = spawn_and_collect(build_ssh_command(host, remote_cmd), timeout)?;
    let Some(status) = status else {
        return Err(SshError::Timeout);
    };
    resolve_exit_aware(stdout, status.success(), status.code(), accepted, &stderr)
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileMeta {
    pub len: u64,
    pub mtime_ms: i64,
}

/// SSH 远端扫描清单（进程内存单例）：`hostId → { 路径 → 元数据 }`。
///
/// 扫描任务整体写入（[`SshScanRegistry::replace_manifest`]），
/// validate / fingerprint / 删除会话按路径查询与摘除。
pub(crate) struct SshScanRegistry {
    /// 各主机的文件清单
    manifests: RwLock<HashMap<String, HashMap<String, RemoteFileMeta>>>,
}

static GLOBAL: LazyLock<SshScanRegistry> = LazyLock::new(|| SshScanRegistry {
    manifests: RwLock::new(HashMap::new()),
});

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

    /// 取某主机的整份清单（远程搜索按清单圈定 grep 文件集）；
    /// 该主机从未扫描过 → None（调用方据此返回空结果）。
    pub(crate) fn manifest(&self, host_id: &str) -> Option<HashMap<String, RemoteFileMeta>> {
        self.lock_read().get(host_id).cloned()
    }

    /// 摘除单条条目（删除远端会话成功后调用）；返回条目原本是否存在
    pub(crate) fn remove_entry(&self, host_id: &str, path: &str) -> bool {
        self.lock_write()
            .get_mut(host_id)
            .is_some_and(|entries| entries.remove(path).is_some())
    }

    /// 取读锁；锁中毒时直接恢复数据继续用（清单为可重建的缓存态，无不变量需保护）
    fn lock_read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<String, HashMap<String, RemoteFileMeta>>> {
        self.manifests
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 取写锁；中毒恢复策略同 [`Self::lock_read`]
    fn lock_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<String, HashMap<String, RemoteFileMeta>>> {
        self.manifests
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/* ===== 远端原语：列表 ===== */

/// 远端会话文件条目：`find -printf` TSV 的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ms: i64,
}

/// 构造远端列表命令：`for` 循环对各目录 `find` 出 `.jsonl` / `.jsonl.zst` 文件，
/// 按 `size\t%T@\tpath` TSV 输出；`2>/dev/null` 吞掉目录不存在的远端噪音，
/// 末尾 `; :` 归零退出码——`find` 对缺失目录退出 1，只装 claude 未装 codex
/// 的主机（`~/.codex/*` 不存在）不应让整条命令以失败收场。
///
/// 远端 find 兼容性：`-printf` 是 GNU 扩展，BSD find（macOS 远端主机）不
/// 认——报错被 `2>/dev/null` 吞掉后输出为空，会静默漏掉全部会话。因此
/// GNU 输出为空时回退 `stat` 组装（GNU `stat -c` 优先、BSD `stat -f` 兜底，
/// 秒级 mtime 由 [`parse_mtime_ms`] 的整秒形态兼容）；GNU 可用的远端不走
/// 回退，毫秒精度不受影响。
/// 空 slice 早退为 shell no-op（`:`），避免生成 `for d in ; do ...` 病态命令。
pub(crate) fn list_files_command(dirs: &[String]) -> String {
    if dirs.is_empty() {
        return ":".to_string();
    }
    let quoted = dirs
        .iter()
        .map(|d| escape_remote_path(d))
        .collect::<Vec<_>>()
        .join(" ");
    let gnu = format!(
        "for d in {quoted}; do find \"$d\" -type f \\( -name '*.jsonl' -o -name '*.jsonl.zst' \\) -printf '%s\\t%T@\\t%p\\n' 2>/dev/null; done"
    );
    // stat 格式串直接嵌真实 TAB 字符（Rust `\t`）：BSD stat 不解释 `\t` 转义，
    // GNU stat 两种都认——真 TAB 是唯一双平台稳妥形态
    let bsd = format!(
        "for d in {quoted}; do find \"$d\" -type f \\( -name '*.jsonl' -o -name '*.jsonl.zst' \\) -exec sh -c 'for f do stat -c \"%s\t%Y\t%n\" -- \"$f\" 2>/dev/null || stat -f \"%z\t%m\t%N\" \"$f\"; done' sh {{}} + 2>/dev/null; done"
    );
    format!("o=$({gnu}); if [ -n \"$o\" ]; then printf '%s\\n' \"$o\"; else {bsd}; fi; :")
}

/// 解析列表命令的 TSV 输出为条目列表。
///
/// 逐行容错——空行 / 非 UTF-8 / 字段不足 / `size` 或 mtime 非法的行整行跳过；
/// 路径列取第 3 字段起（路径内含 `\t` 不再被切分）。
pub(crate) fn parse_list_output(out: &[u8]) -> Vec<RemoteFileEntry> {
    let mut entries = Vec::new();
    for line in out.split(|&b| b == b'\n') {
        // 非 UTF-8 行：路径已损坏，无拯救价值，整行丢弃
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let mut fields = text.splitn(3, '\t');
        let (Some(size), Some(mtime_ms), Some(path)) = (
            fields.next().and_then(|s| s.parse::<u64>().ok()),
            fields.next().and_then(parse_mtime_ms),
            fields.next(),
        ) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        entries.push(RemoteFileEntry {
            path: path.to_string(),
            size,
            mtime_ms,
        });
    }
    entries
}

/// 解析 GNU find `%T@`（`秒.纳秒` 浮点文本）为 Unix 毫秒：
/// 小数取前 3 位（纳秒→毫秒）不足右补零；负时间戳按 `秒×1000 − 毫秒` 折算。
fn parse_mtime_ms(raw: &str) -> Option<i64> {
    let (secs_raw, frac_raw) = raw.split_once('.').unwrap_or((raw, ""));
    // 符号取自原串（"-0.5" 的秒解析为 0 会丢符号）："−秒.毫秒" 按负方向折算
    let negative = raw.starts_with('-');
    let secs: i64 = secs_raw.parse().ok()?;
    let mut ms_digits = String::with_capacity(3);
    for c in frac_raw.chars() {
        if !c.is_ascii_digit() {
            return None;
        }
        if ms_digits.len() < 3 {
            ms_digits.push(c);
        }
    }
    while ms_digits.len() < 3 {
        ms_digits.push('0');
    }
    let ms: i64 = ms_digits.parse().ok()?;
    Some(if negative {
        secs.saturating_mul(1000).saturating_sub(ms)
    } else {
        secs.saturating_mul(1000).saturating_add(ms)
    })
}

/* ===== 远端原语：头尾帧协议 ===== */

/// 单文件头尾拉取请求。未压缩按需声明 head 行 / tail 行 / 字节尾窗；
/// 压缩文件忽略行参数，仅拉取前 [`ZSTD_PREFIX_BYTES`] 压缩字节（本地截断解压）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadTailRequest {
    pub path: String,
    pub compressed: bool,
    pub head_lines: usize,
    pub tail_lines: usize,
    pub tail_bytes: u64,
}

/// 帧解析后的单文件数据：head/tail 为行数组；
/// `tail_bytes` 为 C 段原始字节（未压缩 = 尾窗，压缩 = 压缩前缀，由调用方区分用途）。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HeadTailData {
    pub head: Vec<String>,
    pub tail: Vec<String>,
    pub tail_bytes: Option<Vec<u8>>,
}

/// 请求声明的标记段种类，命令生成与解析对账共用（压缩文件恒为 C 段）。
fn wanted_kinds(req: &HeadTailRequest) -> Vec<char> {
    if req.compressed {
        return vec!['C'];
    }
    let mut kinds = Vec::with_capacity(3);
    if req.head_lines > 0 {
        kinds.push('H');
    }
    if req.tail_lines > 0 {
        kinds.push('T');
    }
    if req.tail_bytes > 0 {
        kinds.push('C');
    }
    kinds
}

/// 标记行文本：`<<<{sentinel}:{H|T|C}:{path}`（H=head 行 / T=tail 行 / C=原始字节段）。
fn marker_line(sentinel: &str, kind: char, path: &str) -> String {
    format!("<<<{sentinel}:{kind}:{path}")
}

/// 结束标记行文本（整批最后输出；缺失即判定输出截断）。
fn end_marker_line(sentinel: &str) -> String {
    format!("<<<{sentinel}:END")
}

/// 生成标记打印语句：`printf '\n%s\n' '<标记行>'`。
///
/// 前置 `\n` 保证标记必起新行——上一段输出（含二进制尾窗）即便未以换行结尾，
/// 分帧依然成立；解析侧的对应契约是「每段末尾剥掉恰好一个换行」。
fn marker_statement(marker: &str) -> String {
    format!("printf '\\n%s\\n' {}", escape_remote_path(marker))
}

/// 构造批量头尾拉取命令（一条复合命令回传全部文件，批次大小由调用方控制在 ≤50）。
///
/// 每文件按 [`wanted_kinds`] 声明打印标记行 + `head -n` / `tail -n` / `tail -c`；
/// 压缩文件仅 `head -c` 于 C 标记；末尾统一打印结束标记供截断判定。
pub(crate) fn head_tail_batch_command(batch: &[HeadTailRequest], sentinel: &str) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(batch.len() * 2 + 1);
    for req in batch {
        let path = req.path.as_str();
        if req.compressed {
            parts.push(marker_statement(&marker_line(sentinel, 'C', path)));
            parts.push(format!(
                "head -c {ZSTD_PREFIX_BYTES} {}",
                escape_remote_path(path)
            ));
            continue;
        }
        if req.head_lines > 0 {
            parts.push(marker_statement(&marker_line(sentinel, 'H', path)));
            parts.push(format!(
                "head -n {} {}",
                req.head_lines,
                escape_remote_path(path)
            ));
        }
        if req.tail_lines > 0 {
            parts.push(marker_statement(&marker_line(sentinel, 'T', path)));
            parts.push(format!(
                "tail -n {} {}",
                req.tail_lines,
                escape_remote_path(path)
            ));
        }
        if req.tail_bytes > 0 {
            parts.push(marker_statement(&marker_line(sentinel, 'C', path)));
            parts.push(format!(
                "tail -c {} {}",
                req.tail_bytes,
                escape_remote_path(path)
            ));
        }
    }
    parts.push(marker_statement(&end_marker_line(sentinel)));
    parts.join("\n")
}

/// 解析帧协议输出为 `path → 数据` 映射。
///
/// - 仅「整行等于本批次期望标记 / 结束标记」的行视为分界，且只在行首匹配；
///   内容中形似标记的行（未知 path、行中位置、未声明的段种类）一律保持为内容
/// - 每段末尾剥掉恰好一个换行（对应标记 printf 的前置 `\n`）；
///   C 段保留原始字节不做行切分
/// - 结束标记缺失或其后仍有残留 → Err；声明的段未出现 → Err（输出被截断）
pub(crate) fn parse_head_tail_output(
    out: &[u8],
    sentinel: &str,
    batch: &[HeadTailRequest],
) -> Result<HashMap<String, HeadTailData>, String> {
    // 期望标记行 → (段种类, path)；仅命中此集合的行才可能成为分界
    let mut expected: HashMap<Vec<u8>, (char, String)> = HashMap::new();
    for req in batch {
        for kind in wanted_kinds(req) {
            expected.insert(
                marker_line(sentinel, kind, &req.path).into_bytes(),
                (kind, req.path.clone()),
            );
        }
    }
    let end_line = end_marker_line(sentinel).into_bytes();

    // 逐行扫描字节流记录分界（行首 = 流首或 \n 之后）：
    // (行首, 行尾不含换行, 分界归属；None = 结束标记)
    type LineBound = (usize, usize, Option<(char, String)>);
    let mut bounds: Vec<LineBound> = Vec::new();
    let mut pos = 0;
    while pos < out.len() {
        let line_end = out[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(out.len(), |i| pos + i);
        let section = if &out[pos..line_end] == end_line.as_slice() {
            Some(None)
        } else {
            expected
                .get(&out[pos..line_end])
                .map(|(k, p)| Some((*k, p.clone())))
        };
        if let Some(section) = section {
            bounds.push((pos, line_end, section));
        }
        pos = line_end + 1;
    }

    // 截断判定：最后一个分界必须是结束标记，且其后不允许残留内容
    match bounds.last() {
        Some(&(_, end, None)) if out.len() <= end + 1 => {}
        _ => return Err("输出被截断：缺少结束标记".to_string()),
    }

    // 相邻分界之间切段并归属；首个分界之前的内容（连接噪声）忽略
    let mut result: HashMap<String, HeadTailData> = batch
        .iter()
        .map(|r| (r.path.clone(), HeadTailData::default()))
        .collect();
    let mut found: Vec<(char, String)> = Vec::new();
    for pair in bounds.windows(2) {
        let (prev_end, prev_section) = (&pair[0].1, &pair[0].2);
        let Some((kind, path)) = prev_section else {
            continue;
        };
        let content = &out[prev_end + 1..pair[1].0];
        // 剥掉段末恰好一个合成换行，还原真实内容
        let body = content.strip_suffix(b"\n").unwrap_or(content);
        found.push((*kind, path.clone()));
        let data = result
            .get_mut(path.as_str())
            .expect("分界 path 必来自批次标记集合");
        match kind {
            'H' => data.head.extend(split_lines(body)),
            'T' => data.tail.extend(split_lines(body)),
            _ => data.tail_bytes = Some(body.to_vec()),
        }
    }

    // 完整性对账：请求声明的每段都必须出现，缺失即中途截断
    for req in batch {
        for kind in wanted_kinds(req) {
            if !found.contains(&(kind, req.path.clone())) {
                return Err(format!("输出被截断：{} 缺少 {kind} 段", req.path));
            }
        }
    }
    Ok(result)
}

/// 段体字节 → 行数组（有损 UTF-8 转换；末尾换行切出的空尾串丢弃）
fn split_lines(body: &[u8]) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(body)
        .split('\n')
        .map(|s| s.to_string())
        .collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/* ===== 远端原语：zst 前缀解压 ===== */

/// 流式解压 zst 字节前缀，容忍尾部不完整 frame。
///
/// 截断 frame 触发 `io::Error` 时，已解出的字节 >0 即视为成功返回前缀；
/// 完全无法解压（非 zst 数据 / 空输入）→ Err。输出按有损 UTF-8 转字符串。
pub(crate) fn decode_zst_prefix(bytes: &[u8]) -> Result<String, String> {
    let mut decoder = zstd::stream::read::Decoder::new(bytes)
        .map_err(|e| format!("zstd 解码器初始化失败: {e}"))?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if out.is_empty() => return Err(format!("zstd 解压失败: {e}")),
            // 截断 frame：已解出前缀即成功
            Err(_) => break,
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/* ===== 远程扫描链路：清单归类与请求组装 ===== */

/// 远程头尾拉取的行窗口（与 claude / codex 本地扫描的 head/tail 行数一致）
const REMOTE_HEAD_LINES: usize = 10;
const REMOTE_TAIL_LINES: usize = 30;
/// claude 远程额外拉取的字节尾窗：relocated 记录散布在最后 64 KiB 内，
/// 与 `claude.rs::RELOCATED_TAIL_BYTES` 对齐
const REMOTE_CLAUDE_TAIL_WINDOW: u64 = 64 * 1024;
/// 清单 TSV 尾缀（stem = 路径去掉该尾缀）
const PLAIN_SUFFIX: &str = ".jsonl";
const COMPRESSED_SUFFIX: &str = ".jsonl.zst";

/// 一条命令回传远端 HOME 下的三个会话根目录（claude 一个 + codex 两个），
/// 供 [`parse_home_dirs`] 解析后再喂 [`list_files_command`]。
pub(crate) fn home_dirs_command() -> String {
    "printf '%s\\n' \"$HOME/.claude/projects\" \"$HOME/.codex/sessions\" \"$HOME/.codex/archived_sessions\"".to_string()
}

/// 解析 [`home_dirs_command`] 输出：按行收非空绝对路径；空行 / 相对路径 /
/// 非 UTF-8 行丢弃。输出顺序即 claude / codex sessions / codex archived。
pub(crate) fn parse_home_dirs(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == b'\n')
        .filter_map(|line| {
            let text = std::str::from_utf8(line).ok()?;
            text.strip_prefix('/')
                .map(|rest| format!("/{rest}"))
                .filter(|dir| dir != "/")
        })
        .collect()
}

/// 按目录前缀归类远端文件路径：claude 目录下 → `"claude"`，codex 目录任一下
/// → `"codex"`，都不属于 → `None`（跳过）。前缀必须落在目录边界（后跟 `/`），
/// `…/projects-evil` 不得误判进 `…/projects`。
pub(crate) fn classify_remote_provider(
    path: &str,
    claude_dir: &str,
    codex_dirs: &[String],
) -> Option<&'static str> {
    let under = |dir: &str| {
        path.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    if under(claude_dir) {
        return Some("claude");
    }
    if codex_dirs.iter().any(|dir| under(dir)) {
        return Some("codex");
    }
    None
}

/// 清单级 `.jsonl` / `.jsonl.zst` 共存判定（Codex 压缩过渡期两种形态并存）：
/// 同 stem 两者都有时保留未压缩、丢弃 zst；仅 zst 时保留 zst。顺序保持清单原序。
///
/// 这是清单版判定，不复用 `content.rs::codex_source_or_sibling`——后者判断
/// 本地文件存在性，语义不适用于远端清单。
pub(crate) fn prefer_plain_entries(entries: Vec<RemoteFileEntry>) -> Vec<RemoteFileEntry> {
    let plain_keys: HashSet<String> = entries
        .iter()
        .filter(|e| e.path.ends_with(PLAIN_SUFFIX))
        .map(|e| path_key_without_suffix(&e.path).to_string())
        .collect();
    entries
        .into_iter()
        .filter(|e| {
            !e.path.ends_with(COMPRESSED_SUFFIX)
                || !plain_keys.contains(path_key_without_suffix(&e.path))
        })
        .collect()
}

/// 按 provider 组装单文件头尾请求：
/// - claude：head 10 行 / tail 30 行 / 64 KiB 字节尾窗（relocated 解析）
/// - codex 未压缩：head 10 行 / tail 30 行
/// - codex 压缩：仅 C 段压缩前缀（行参数被帧协议忽略）
pub(crate) fn remote_head_tail_request(provider: &str, entry: &RemoteFileEntry) -> HeadTailRequest {
    let compressed = entry.path.ends_with(COMPRESSED_SUFFIX);
    match (provider, compressed) {
        ("claude", _) => HeadTailRequest {
            path: entry.path.clone(),
            compressed: false,
            head_lines: REMOTE_HEAD_LINES,
            tail_lines: REMOTE_TAIL_LINES,
            tail_bytes: REMOTE_CLAUDE_TAIL_WINDOW,
        },
        (_, true) => HeadTailRequest {
            path: entry.path.clone(),
            compressed: true,
            head_lines: 0,
            tail_lines: 0,
            tail_bytes: 0,
        },
        _ => HeadTailRequest {
            path: entry.path.clone(),
            compressed: false,
            head_lines: REMOTE_HEAD_LINES,
            tail_lines: REMOTE_TAIL_LINES,
            tail_bytes: 0,
        },
    }
}

/// 远端文件名 stem：文件名去 `.jsonl` / `.jsonl.zst` 尾缀（`/p/x.jsonl` →
/// `x`）；两种尾缀都不是返回 `None`。用作远程会话的 `session_id`。
pub(crate) fn remote_session_stem(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(COMPRESSED_SUFFIX)
        .or_else(|| name.strip_suffix(PLAIN_SUFFIX))
}

/// 全路径去尾缀（保留目录）：`.jsonl` / `.jsonl.zst` 共存判定的分组 key——
/// 不同目录的同名文件是两个会话，不得因文件名相同被误判为并存形态。
fn path_key_without_suffix(path: &str) -> &str {
    path.strip_suffix(COMPRESSED_SUFFIX)
        .or_else(|| path.strip_suffix(PLAIN_SUFFIX))
        .unwrap_or(path)
}

/// 清单 mtime（Unix 毫秒）→ `SystemTime` 供解析缓存的指纹比对；负时间戳
/// 无法表达为 `SystemTime`，归 `None`（每轮恒定，缓存键仍稳定）。
pub(crate) fn mtime_to_system_time(ms: i64) -> Option<SystemTime> {
    let ms = u64::try_from(ms).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(ms))
}

/* ===== 远程恢复命令 ===== */

/// 内层 resume 命令（按 provider 重新生成，不沿用本机 meta 里的旧值——
/// 那是本机路径语义）：claude `claude --resume <id>`、codex `codex resume <id>`，
/// 与 claude.rs / codex.rs 本地扫描同式两行；其它 provider 返回 `None`。
fn inner_resume_command(provider_id: &str, session_id: &str) -> Option<String> {
    match provider_id {
        "claude" => Some(format!("claude --resume {session_id}")),
        "codex" => Some(format!("codex resume {session_id}")),
        _ => None,
    }
}

/// 生成可复制到本机终端执行的远程恢复命令：
/// `ssh [-p N] [-i key] -t [user@]host 'cd <dir> && <内层>'`。
///
/// - 与 [`build_ssh_command`] 口径相反：**不含** BatchMode / ConnectTimeout /
///   ControlMaster 等自动执行参数——用户手动执行场景，无需免密探测与连接复用
/// - `project_dir` 存在时拼 `cd` 前缀（远端绝对路径）；缺失时仅内层命令
/// - 目录与整条远端命令均经 [`escape_remote_path`] 单引号转义
/// - provider 非 claude / codex 返回 `None`（MVP 范围，无内层命令可拼）
pub(crate) fn build_remote_resume_command(
    host: &SshHostConfig,
    provider_id: &str,
    session_id: &str,
    project_dir: Option<&str>,
) -> Option<String> {
    let inner = inner_resume_command(provider_id, session_id)?;
    let remote_cmd = match project_dir {
        Some(dir) => format!("cd {} && {inner}", escape_remote_path(dir)),
        None => inner,
    };
    let target = host
        .user
        .as_deref()
        .map(|user| format!("{user}@{}", host.host))
        .unwrap_or_else(|| host.host.clone());
    let mut prefix = String::from("ssh");
    if let Some(port) = host.port {
        prefix.push_str(&format!(" -p {port}"));
    }
    if let Some(key) = host.identity_file.as_deref() {
        prefix.push_str(&format!(" -i {key}"));
    }
    Some(format!(
        "{prefix} -t {target} {}",
        escape_remote_path(&remote_cmd)
    ))
}

/// 远程会话标识覆盖：`session_id` = 文件 stem、`source_path` =
/// `ssh:<hostId><远端路径>`（与 [`parse_ssh_source`] 的解析形态及 registry
/// 清单 key 严格一致）；`resume_command` 按 [`build_remote_resume_command`]
/// 重新生成为完整可复制 ssh 命令（本机语义的旧值不沿用）；解析产出的
/// 其余字段原样保留。
pub(crate) fn rebind_remote_meta(
    mut meta: SessionMeta,
    host: &SshHostConfig,
    remote_path: &str,
) -> SessionMeta {
    if let Some(stem) = remote_session_stem(remote_path) {
        meta.session_id = stem.to_string();
    }
    // 远端路径自带前导 `/`，拼接时去重避免 `ssh:h1//root/...` 双斜杠形态
    let rel = remote_path.strip_prefix('/').unwrap_or(remote_path);
    meta.source_path = Some(format!("ssh:{}/{rel}", host.id));
    // 内层 session_id 用覆盖后的远端 stem；project_dir 是解析保留的远端路径
    meta.resume_command = build_remote_resume_command(
        host,
        &meta.provider_id,
        &meta.session_id,
        meta.project_dir.as_deref(),
    );
    meta
}

/* ===== 远端原语：全文 / 区间 / 删除 ===== */

/// `cat '<path>'`：全文拉取（大小上限由调用方把关）。
pub(crate) fn read_full_command(path: &str) -> String {
    format!("cat {}", escape_remote_path(path))
}

/// `tail -c +<offset+1> '<path>' | head -c <len>`：字节区间拉取
///（0-based offset 换算为 `tail -c` 的 1-based 起点）。仅适用未压缩文件。
pub(crate) fn read_range_command(path: &str, offset: u64, len: u64) -> String {
    format!(
        "tail -c +{} {} | head -c {}",
        offset.saturating_add(1),
        escape_remote_path(path),
        len
    )
}

/// `rm -f -- '<path>'`：删除远端文件（路径白名单由调用方把关）。
pub(crate) fn delete_file_command(path: &str) -> String {
    format!("rm -f -- {}", escape_remote_path(path))
}

/* ===== 远程删除：stem 校验与白名单判定（Task 10）===== */

/// 删除前的防误删校验（纯函数）：`session_id` 必须与远端文件 stem 完全一致
/// ——stem 复用 [`remote_session_stem`]（即 `rebind_remote_meta` 生成远程
/// `session_id` 的同一来源，校验与生成同源，杜绝算法漂移缝隙）；空 id 直接
/// 拒绝（隐藏文件 stem 为空的退化形态）。
pub(crate) fn session_id_matches_stem(session_id: &str, remote_path: &str) -> bool {
    !session_id.is_empty()
        && remote_session_stem(remote_path).is_some_and(|stem| stem == session_id)
}

/// 删除白名单判定（纯函数）：registry lookup 命中 → 放行；未命中 → 拒绝并
/// 要求先刷新——清单里没有的路径不允许 `rm`，防止删除未经扫描确认的文件。
pub(crate) fn ensure_deletable(lookup_result: Option<RemoteFileMeta>) -> Result<(), String> {
    lookup_result
        .map(|_| ())
        .ok_or_else(|| "请先刷新该主机的会话列表".to_string())
}

/* ===== 远程块读取与图片字节（Task 9）===== */

/// zst 全解压后的内存取窗（纯函数）：`[offset, min(offset+len, 总长))`；
/// offset 超过总长返回空切片。[`read_block`] 压缩路径的窗口语义与此严格一致。
pub(crate) fn zst_window(decompressed: &[u8], offset: u64, len: u64) -> &[u8] {
    let total = decompressed.len() as u64;
    let start = offset.min(total) as usize;
    let end = offset.saturating_add(len).min(total) as usize;
    &decompressed[start..end]
}

/// 远端块内容读取（块全文分页的后端，`content::read_remote_jsonl_line` 调用）：
/// - 未压缩：[`read_range_command`] 区间拉取；offset 超文件尾时远端自然返回短 / 空输出
/// - 压缩（`.jsonl.zst`）：zst 帧无法远端随机定位——`cat` 全量压缩字节
///   （上限与本地全文防护同口径 32MB）→ 本地全解压 → [`zst_window`] 内存取窗
pub(crate) fn read_block(
    host: &SshHostConfig,
    remote_path: &str,
    offset: u64,
    len: u64,
    compressed: bool,
) -> Result<Vec<u8>, SshError> {
    if !compressed {
        return run_remote(
            host,
            &read_range_command(remote_path, offset, len),
            FETCH_FULL_TIMEOUT,
        );
    }
    let raw = run_remote(host, &read_full_command(remote_path), FETCH_FULL_TIMEOUT)?;
    enforce_fetch_limit(raw.len() as u64, super::content::MAX_TEXT_BYTES)?;
    let decompressed = zstd::stream::decode_all(raw.as_slice())
        .map_err(|e| SshError::Local(format!("zstd 解压失败: {e}")))?;
    Ok(zst_window(&decompressed, offset, len).to_vec())
}

/// 拉取远端文件全文字节（[`fetch_to_temp_file`] 的内存版，不落盘）：
/// 超过 `max_bytes` 直接拒绝。远程图片等二进制小文件读取用。
pub(crate) fn read_remote_bytes(
    host: &SshHostConfig,
    remote_path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, SshError> {
    let bytes = run_remote(host, &read_full_command(remote_path), FETCH_FULL_TIMEOUT)?;
    enforce_fetch_limit(bytes.len() as u64, max_bytes)?;
    Ok(bytes)
}

/* ===== 远程正文读取：全文拉取落临时文件 ===== */

/// 全文拉取超时：与扫描链路大文件口径（`REMOTE_LIST_TIMEOUT` 120s）一致
const FETCH_FULL_TIMEOUT: Duration = Duration::from_secs(120);

/// 远端文件名的扩展名：`.jsonl.zst` 双段整体保留（codex 压缩形态），
/// 其余取最后一段；无扩展名 / 隐藏文件（stem 为空）/ 空段 → 空串。
fn remote_extension(remote_path: &str) -> String {
    let name = remote_path.rsplit('/').next().unwrap_or(remote_path);
    if name
        .strip_suffix(".jsonl.zst")
        .is_some_and(|stem| !stem.is_empty())
    {
        return "jsonl.zst".to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_string(),
        _ => String::new(),
    }
}

/// 临时文件路径生成（纯函数；uuid 由调用方注入，便于确定性测试）：
/// `<配置目录>/cache/ssh-tmp/<hostId>/<uuid>.<原扩展名>`，
/// 无扩展名时文件名仅 uuid；host_id 作子目录实现跨主机隔离。
pub(crate) fn temp_file_path(host_id: &str, remote_path: &str, uuid: &str) -> PathBuf {
    let ext = remote_extension(remote_path);
    let file_name = if ext.is_empty() {
        uuid.to_string()
    } else {
        format!("{uuid}.{ext}")
    };
    crate::config::get_app_config_dir()
        .join("cache")
        .join("ssh-tmp")
        .join(host_id)
        .join(file_name)
}

/// 拉回字节数的大小上限检查：超限文案与本地全文防护
/// （`content::TOO_LARGE_TEXT`，同一常量）逐字一致，本地与远程提示不分叉。
pub(crate) fn enforce_fetch_limit(len: u64, max_bytes: u64) -> Result<(), SshError> {
    if len > max_bytes {
        return Err(SshError::Local(super::content::TOO_LARGE_TEXT.to_string()));
    }
    Ok(())
}

/// 拉取远端文件全文并落盘临时文件，返回本地路径喂给 provider 的 `&Path`
/// 解析入口（`mod.rs::load_messages` 与 Task 9 区间读取复用）。
///
/// - `cat` 全文 + 120s 超时（与扫描链路大文件口径一致）
/// - 字节数超 `max_bytes` → Err（文案与本地 32MB 防护一致）
/// - 写入 `cache/ssh-tmp/<hostId>/<uuid>.<原扩展名>`，目录惰性创建；
///   调用方用后自行删除（失败仅告警，不影响解析结果）
pub(crate) fn fetch_to_temp_file(
    host: &SshHostConfig,
    remote_path: &str,
    max_bytes: u64,
) -> Result<PathBuf, SshError> {
    let bytes = run_remote(host, &read_full_command(remote_path), FETCH_FULL_TIMEOUT)?;
    enforce_fetch_limit(bytes.len() as u64, max_bytes)?;
    let dest = temp_file_path(&host.id, remote_path, &uuid::Uuid::new_v4().to_string());
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SshError::Local(format!("创建 SSH 临时目录失败: {e}")))?;
    }
    std::fs::write(&dest, &bytes)
        .map_err(|e| SshError::Local(format!("写入 SSH 临时文件失败: {e}")))?;
    Ok(dest)
}

/* ===== 远端原语：正文搜索（Task 3）===== */

/// rg 可用性探测命令：POSIX shell 内建 `command -v`，rg 在远端 PATH 时
/// exit 0，缺失时 exit 1——单次轻量往返（独立常量供本地 sh 真跑验证语义）
const RG_PROBE_COMMAND: &str = "command -v rg";
/// 探测超时：单次轻量往返口径（连接超时 8s + 余量）
const RG_DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// rg 可用性的进程内 per-host 缓存（hostId → 可用与否）。首次探测后缓存，
/// 探测失败（连接错误 / 超时等）同样缓存 false——降级 grep 后不再重复
/// 探测（重复探测每次搜索都多付一次 ssh 往返，取舍为缓存到底）。
static REMOTE_RG_CACHE: LazyLock<RwLock<HashMap<String, bool>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// 单条远程搜索命中：rg / grep 统一的 `path:lineno:text` 输出行解析产物。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteSearchLine {
    pub path: String,
    pub line_no: u64,
    pub text: String,
}

/// 构造远程正文搜索命令（rg 优先 / grep 降级，参数形态设计 §4.2 逐字锁定）：
///
/// - rg：`-i` 忽略大小写、`-n` 行号、`-F` 固定串防正则元字符、
///   `--no-heading` 行式输出、`--with-filename` 强制文件名前缀（单文件时
///   rg 默认省略，与多文件批混跑输出会不同构）、`--no-ignore` 防 .gitignore
///   干扰、`-m 5` 每文件截断命中数防超长输出
/// - grep：`-inFH -m 5` 一组短参等价覆盖（`-H` 强制文件名前缀，与 rg 的
///   `--with-filename` 对齐）
/// - pattern 与 files 均经 [`escape_remote_path`] 单引号转义；`--` 双保险
///   防 pattern 以 `-` 开头被解析为选项（单引号不防选项解析，`--` 才防）
/// - 空文件列表返回空命令（调用方据空串跳过执行）
pub(crate) fn build_search_command(pattern: &str, files: &[String], use_rg: bool) -> String {
    if files.is_empty() {
        return String::new();
    }
    let quoted_files = files
        .iter()
        .map(|f| escape_remote_path(f))
        .collect::<Vec<_>>()
        .join(" ");
    let quoted_pattern = escape_remote_path(pattern);
    if use_rg {
        format!("rg -i -n -F --no-heading --with-filename --no-ignore -m 5 -- {quoted_pattern} {quoted_files}")
    } else {
        format!("grep -inFH -m 5 -- {quoted_pattern} {quoted_files}")
    }
}

/// 解析 rg / grep 统一的 `path:lineno:text` 输出（两命令形态输出同构，
/// 单一解析器）。
///
/// 逐行容错——无分隔段 / path 为空 / 非 UTF-8 的行整行跳过。
pub(crate) fn parse_search_output(stdout: &[u8]) -> Vec<RemoteSearchLine> {
    let mut lines = Vec::new();
    for raw in stdout.split(|&b| b == b'\n') {
        // 非 UTF-8 行：内容已损坏，无拯救价值，整行丢弃（对齐
        // parse_list_output 的容错口径）
        let Ok(text) = std::str::from_utf8(raw) else {
            continue;
        };
        if let Some(parsed) = split_search_line(text) {
            lines.push(parsed);
        }
    }
    lines
}

/// 切分单条 `path:lineno:text` 行：**从左向右**定位第一个 `:<纯数字>:` 段。
///
/// 路径与正文都可含冒号，行号段是唯一「冒号夹纯数字」锚点。扫描方向取
/// 从左向右：真实会话行（claude / codex JSONL）的正文段几乎必含 ISO 8601
/// 时间戳（`10:00:00` 内含 `:00:` 锚形态），从右向左会把行号错锚到时间戳
/// 上、path 被截成残段；而路径段自身含 `:<纯数字>:` 连续段（如
/// `abc:42:def.jsonl`）远比时间戳罕见——两害相权，从左向右在真实数据上
/// 系统性正确（`/a:b/c.jsonl:42:{...}` 同样锚定 `:42:` 而非路径里的 `:b`）。
/// 切分点均落在 `:` / ASCII 数字边界，UTF-8 字符边界天然安全。
fn split_search_line(line: &str) -> Option<RemoteSearchLine> {
    let bytes = line.as_bytes();
    // 正序枚举冒号下标，检查其后是否紧跟「纯数字(≥1 位) + 冒号」
    for i in 0..bytes.len() {
        if bytes[i] != b':' {
            continue;
        }
        // 数字段结束位置：首个非 ASCII 数字处（None = 数字延伸到行尾，
        // 缺收尾冒号，不构成锚段，继续向右找）
        let Some(digits_end) = (i + 1..bytes.len()).find(|&j| !bytes[j].is_ascii_digit()) else {
            continue;
        };
        if digits_end > i + 1 && bytes[digits_end] == b':' {
            let path = &line[..i];
            if path.is_empty() {
                return None; // 路径为空 = 噪声行（如连接 banner）
            }
            // 纯数字段保证可 parse；防御性 ok() 兜底超大数字
            let line_no = line[i + 1..digits_end].parse().ok()?;
            return Some(RemoteSearchLine {
                path: path.to_string(),
                line_no,
                text: line[digits_end + 1..].to_string(),
            });
        }
    }
    None
}

/// 探测远端 rg 可用性：`command -v rg` 单次往返，结果按 hostId 进程内
/// 缓存（[`REMOTE_RG_CACHE`]）。exit 0 → true；缺失 exit 1 / 连接失败 /
/// 超时 → false 并同样缓存，调用方降级 grep 后不再重复探测。
pub(crate) fn detect_remote_rg(host: &SshHostConfig) -> bool {
    {
        let cache = REMOTE_RG_CACHE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(&host.id) {
            return *hit;
        }
    }
    let available = run_remote(host, RG_PROBE_COMMAND, RG_DETECT_TIMEOUT).is_ok();
    REMOTE_RG_CACHE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(host.id.clone(), available);
    available
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    // #[serial]：这两个断言 CM 三参数的单测与 ssh_localtest 冒烟共享同一把
    // serial 全局锁——冒烟窗口内 DISABLE_CONTROL_MASTER_FOR_TEST=true，libtest
    // 默认并发调度到此处会读到 true 而 CM 参数缺失（flag 默认值无法覆盖
    // 并行窗口，须互斥）
    #[test]
    #[serial]
    fn build_contains_common_options_and_user_at_host() {
        // 锁定注入用户名：目录安全上限仅 36 字节，宿主机超长 USER 会触发
        // 直连降级使 ControlPersist 断言失败
        let _guard = UserEnvGuard::new(Some("dev"));
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

    /// 测试窗口内重写 USER / 清空 LOGNAME，drop 时恢复原值。
    ///
    /// [`control_socket_dir`] 按 USER → LOGNAME → "user" 次序取用户名，注入
    /// 需同时清掉 LOGNAME 才能锁定分支；`#[serial]` 已保证窗口不与其他
    /// env 敏感测试交叠。
    struct UserEnvGuard {
        user: Option<std::ffi::OsString>,
        logname: Option<std::ffi::OsString>,
    }

    impl UserEnvGuard {
        fn new(user: Option<&str>) -> Self {
            let guard = UserEnvGuard {
                user: std::env::var_os("USER"),
                logname: std::env::var_os("LOGNAME"),
            };
            match user {
                Some(v) => std::env::set_var("USER", v),
                None => std::env::remove_var("USER"),
            }
            std::env::remove_var("LOGNAME");
            guard
        }
    }

    impl Drop for UserEnvGuard {
        fn drop(&mut self) {
            match self.user.take() {
                Some(v) => std::env::set_var("USER", v),
                None => std::env::remove_var("USER"),
            }
            match self.logname.take() {
                Some(v) => std::env::set_var("LOGNAME", v),
                None => std::env::remove_var("LOGNAME"),
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn build_adds_control_master_triple_on_unix() {
        // 锁定注入用户名，避免宿主机超长 USER 触发降级使三参数缺失
        let _guard = UserEnvGuard::new(Some("dev"));
        let cmd = build_ssh_command(&host(Some("dev"), None, None), "true");
        let argv = args(&cmd);
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        // ControlPath 落在 /tmp 下按用户隔离的短 socket 目录（规避 unix 域
        // sun_path 长度上限），且惰性创建该目录
        let cp_idx = argv
            .iter()
            .position(|a| a.starts_with("ControlPath="))
            .unwrap();
        let cp = &argv[cp_idx];
        assert!(cp.contains("cm-%C"), "ControlPath 应使用 cm-%C: {cp}");
        let dir = cp
            .trim_start_matches("ControlPath=")
            .trim_end_matches("/cm-%C");
        assert!(
            dir.starts_with("/tmp/cc-switch-ssh-"),
            "应使用 /tmp 短前缀 socket 目录: {dir}"
        );
        assert!(
            std::path::Path::new(dir).is_dir(),
            "目录应被惰性创建: {dir}"
        );
    }

    /// 降级回归：目录展开后超出 sun_path 安全区时必须省略复用三参数。
    ///
    /// 注入超长 USER 使 socket 目录长度越界，模拟配置目录被 override 到
    /// 深路径的线上场景——若不降级，mux 初始化将报
    /// `unix_listener: path too long` 导致整体连接失败。
    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn build_omits_control_master_triple_when_dir_too_long() {
        let long_user = "u".repeat(80);
        let _guard = UserEnvGuard::new(Some(&long_user));
        let cmd = build_ssh_command(&host(Some("dev"), None, None), "true");
        let argv = args(&cmd);
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlMaster=")),
            "目录超长时应省略 ControlMaster: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlPath=")),
            "目录超长时应省略 ControlPath: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlPersist=")),
            "目录超长时应省略 ControlPersist: {argv:?}"
        );
        // 安全判定先于落盘：降级路径不得残留空 socket 目录。
        // 前置清残留：/tmp 跨运行持久，先建后判的历史版本可能留有同名目录
        let leaked = PathBuf::from(format!("/tmp/cc-switch-ssh-{long_user}"));
        let _ = std::fs::remove_dir_all(&leaked);
        assert!(!leaked.exists(), "降级路径不应创建 socket 目录");
        // 直连降级下基础参数与目的地不受影响（功能完整）
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"dev@example.com".to_string()));
    }

    /* ===== socket 目录与路径安全 ===== */

    #[cfg(not(windows))]
    #[test]
    fn control_path_safe_accepts_short_tmp_dir() {
        assert!(control_path_safe(Path::new("/tmp/cc-switch-ssh-dev")));
    }

    #[cfg(not(windows))]
    #[test]
    fn control_path_safe_rejects_long_dir() {
        // 80 字符目录：80 + 1('/') + 3("cm-") + 40(hash) + 20(后缀余量) > 100
        let long = PathBuf::from("a".repeat(80));
        assert!(!control_path_safe(&long));
    }

    #[cfg(not(windows))]
    #[test]
    fn control_path_safe_rejects_dir_with_whitespace() {
        assert!(!control_path_safe(Path::new("/tmp/cc switch")));
    }

    #[cfg(not(windows))]
    #[test]
    fn control_path_safe_rejects_non_ascii_dir() {
        assert!(!control_path_safe(Path::new("/tmp/cc-切换")));
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn control_socket_dir_uses_short_tmp_path_per_user() {
        let _guard = UserEnvGuard::new(Some("cctest"));
        let dir = control_socket_dir().expect("应返回 socket 目录");
        assert_eq!(dir, PathBuf::from("/tmp/cc-switch-ssh-cctest"));
        assert!(dir.is_dir(), "目录应被惰性创建: {dir:?}");
        // /tmp 全局可写，目录必须收紧到 700 防 socket 被其他用户探测接管
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "socket 目录权限应为 700");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn control_socket_dir_falls_back_to_generic_user() {
        let _guard = UserEnvGuard::new(None);
        let dir = control_socket_dir().expect("应返回 fallback 目录");
        assert_eq!(dir, PathBuf::from("/tmp/cc-switch-ssh-user"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ===== 进程执行 ===== */

    #[test]
    fn run_captures_stdout() {
        let mut cmd = std::process::Command::new("/bin/echo");
        cmd.arg("ok");
        let out = run_command(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out, b"ok\n");
    }

    /// stdout 上限回归（16MB→40MB 提升）：17MB 输出落在旧上限之上、新上限
    /// 之下，必须完整回传——旧值下读线程 take 满关管道会让 cat 收 SIGPIPE
    /// 报 Remote。本地 `/bin/cat` 直测传输层，无需 ssh 环境。
    #[test]
    fn run_captures_stdout_over_old_16mb_cap() {
        let dir = tempfile::Builder::new()
            .prefix("ccsw-stdout-cap-")
            .tempdir()
            .expect("创建 tempdir");
        let big = vec![b'x'; 17 * 1024 * 1024];
        let file = dir.path().join("big.bin");
        std::fs::write(&file, &big).unwrap();
        let mut cmd = std::process::Command::new("/bin/cat");
        cmd.arg(&file);
        let out = run_command(cmd, Duration::from_secs(30)).expect("17MB 应在 40MB 上限内完整回传");
        assert_eq!(out.len(), big.len(), "字节数应完整无截断");
        assert_eq!(out, big, "内容应逐字节一致");
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
        assert!(SshError::ConnectOrAuth("x".into())
            .to_string()
            .contains("连接"));
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
        assert!(
            reg.lookup(host, "/a.jsonl").is_none(),
            "旧清单条目应被全量替换"
        );
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

    /* ===== 远端原语：列表 ===== */

    #[test]
    fn list_cmd_quotes_dirs_and_uses_find_printf() {
        let cmd = list_files_command(&[
            "/home/u/.claude/projects".to_string(),
            "/home/u/.codex/sessions".to_string(),
        ]);
        // GNU 主路径：毫秒精度 -printf TSV
        assert!(cmd.contains(
            "o=$(for d in '/home/u/.claude/projects' '/home/u/.codex/sessions'; do \
             find \"$d\" -type f \\( -name '*.jsonl' -o -name '*.jsonl.zst' \\) \
             -printf '%s\\t%T@\\t%p\\n' 2>/dev/null; done)"
        ));
        // GNU 输出为空时的 BSD find + stat 回退（GNU stat -c 优先、BSD stat -f 兜底）；
        // stat 格式串用真实 TAB 字符（BSD stat 不解释 `\t` 转义）
        assert!(cmd.contains("if [ -n \"$o\" ]"));
        assert!(cmd.contains("stat -c \"%s\t%Y\t%n\" -- \"$f\""));
        assert!(cmd.contains("stat -f \"%z\t%m\t%N\" \"$f\""));
        // 整体退出码归零
        assert!(cmd.ends_with("; :"), "命令应以 no-op 收尾: {cmd}");
    }

    /// 缺失目录不得让清单命令以非零退出收场（只装 claude 未装 codex 的主机）
    #[test]
    fn list_cmd_missing_dirs_exit_zero_via_local_sh() {
        let missing = std::env::temp_dir()
            .join("cc-switch-no-such-dir")
            .to_str()
            .unwrap()
            .to_string();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_files_command(&[missing]))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "缺失目录应吞掉退出码: {} / stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "无匹配文件应无清单输出");
    }

    /// BSD find（macOS 远端）真跑回退路径：无 `-printf` 的 find 上 GNU 段输出
    /// 为空 → stat 组装的 TSV 仍可被 [`parse_list_output`] 解析（含整秒 mtime）
    #[test]
    fn list_cmd_bsd_fallback_via_local_sh() {
        let dir = std::env::temp_dir().join(format!("cc-switch-ls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "line1\n").unwrap();

        let dir_s = dir.to_str().unwrap().to_string();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_files_command(std::slice::from_ref(&dir_s)))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "sh 执行失败: {} / stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );

        let entries = parse_list_output(&out.stdout);
        assert_eq!(entries.len(), 1, "应恰好列出一个夹具文件: {entries:?}");
        assert_eq!(entries[0].path, format!("{dir_s}/a.jsonl"));
        assert_eq!(entries[0].size, 6, "字节数应来自 stat 的 size");
        // 本机为 BSD find：必然走回退段，mtime 为整秒（无小数部分）
        assert!(
            entries[0].mtime_ms >= 1_700_000_000_000,
            "mtime 应为现代时间戳毫秒值"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_cmd_escapes_embedded_single_quote() {
        let cmd = list_files_command(&["/dir with'quote".to_string()]);
        // 单引号路径转义后拼入两段（GNU 主路径与 BSD 回退）
        assert!(cmd.contains("for d in '/dir with'\\''quote'; do "));
        assert_eq!(cmd.matches("'/dir with'\\''quote'").count(), 2);
    }

    #[test]
    fn parse_list_reads_tsv_with_float_mtime() {
        let entries = parse_list_output(
            b"123\t1700000000.123456789\t/p/a.jsonl\n456\t1700000000.5\t/p/b.jsonl",
        );
        assert_eq!(
            entries,
            vec![
                RemoteFileEntry {
                    path: "/p/a.jsonl".to_string(),
                    size: 123,
                    mtime_ms: 1700000000123,
                },
                RemoteFileEntry {
                    path: "/p/b.jsonl".to_string(),
                    size: 456,
                    mtime_ms: 1700000000500,
                },
            ]
        );
    }

    #[test]
    fn parse_list_skips_bad_lines_and_keeps_good_ones() {
        let mut out = Vec::new();
        out.extend_from_slice(b"\n"); // 空行
        out.extend_from_slice(b"garbage-without-tabs\n"); // 无 TSV 字段
        out.extend_from_slice(b"abc\t1.0\t/p/bad1.jsonl\n"); // size 非数字
        out.extend_from_slice(b"789\tnot-a-time\t/p/bad2.jsonl\n"); // mtime 非法
        out.extend_from_slice(b"9\t100.5\t/\xffbad.jsonl\n"); // 非 UTF-8 行
        out.extend_from_slice(b"77\t1700000000\t/p/last.jsonl"); // 无尾换行 + 整秒时间
        assert_eq!(
            parse_list_output(&out),
            vec![RemoteFileEntry {
                path: "/p/last.jsonl".to_string(),
                size: 77,
                mtime_ms: 1700000000000,
            }]
        );
    }

    #[test]
    fn parse_list_negative_mtime_folds_below_epoch() {
        let entries = parse_list_output(b"5\t-1.25\t/p/a.jsonl\n6\t-0.5\t/p/b.jsonl\n");
        assert_eq!(entries[0].mtime_ms, -1250);
        // "-0.5"：秒部 "-0" 解析为 0，符号由原串保留
        assert_eq!(entries[1].mtime_ms, -500);
    }

    /* ===== 远端原语：头尾帧协议 ===== */

    /// 构造头尾请求
    fn ht_req(
        path: &str,
        compressed: bool,
        head_lines: usize,
        tail_lines: usize,
        tail_bytes: u64,
    ) -> HeadTailRequest {
        HeadTailRequest {
            path: path.to_string(),
            compressed,
            head_lines,
            tail_lines,
            tail_bytes,
        }
    }

    /// 模拟远端帧输出：标记语句产出 = 前置 `\n` + 标记行 + 段内容
    fn push_frame(out: &mut Vec<u8>, marker: &str, content: &[u8]) {
        out.extend_from_slice(b"\n");
        out.extend_from_slice(marker.as_bytes());
        out.extend_from_slice(b"\n");
        out.extend_from_slice(content);
    }

    #[test]
    fn head_tail_cmd_uncompressed_emits_h_t_c_sections() {
        let cmd = head_tail_batch_command(&[ht_req("/p/a.jsonl", false, 10, 30, 65536)], "S");
        assert_eq!(
            cmd,
            "printf '\\n%s\\n' '<<<S:H:/p/a.jsonl'\nhead -n 10 '/p/a.jsonl'\n\
             printf '\\n%s\\n' '<<<S:T:/p/a.jsonl'\ntail -n 30 '/p/a.jsonl'\n\
             printf '\\n%s\\n' '<<<S:C:/p/a.jsonl'\ntail -c 65536 '/p/a.jsonl'\n\
             printf '\\n%s\\n' '<<<S:END'"
        );
    }

    #[test]
    fn head_tail_cmd_compressed_only_c_section() {
        // 压缩文件：即使误传行参数也只拉压缩前缀（解析侧同规则对账）
        let cmd = head_tail_batch_command(&[ht_req("/p/c.jsonl.zst", true, 5, 5, 5)], "S");
        assert_eq!(
            cmd,
            "printf '\\n%s\\n' '<<<S:C:/p/c.jsonl.zst'\nhead -c 262144 '/p/c.jsonl.zst'\n\
             printf '\\n%s\\n' '<<<S:END'"
        );
    }

    #[test]
    fn head_tail_cmd_escapes_paths_and_omits_zero_sections() {
        let cmd = head_tail_batch_command(
            &[
                ht_req("/p/it's.jsonl", false, 1, 0, 0),
                ht_req("/p/b.jsonl", false, 0, 2, 0),
            ],
            "S",
        );
        // 单引号路径转义后拼入标记与命令
        assert!(cmd.contains("printf '\\n%s\\n' '<<<S:H:/p/it'\\''s.jsonl'"));
        assert!(cmd.contains("head -n 1 '/p/it'\\''s.jsonl'"));
        // 声明为 0 的段不出现
        assert!(!cmd.contains("<<<S:T:/p/it's.jsonl"));
        assert!(!cmd.contains("<<<S:C:/p/it's.jsonl"));
        assert!(!cmd.contains("<<<S:H:/p/b.jsonl"));
        assert!(cmd.contains("tail -n 2 '/p/b.jsonl'"));
    }

    #[test]
    fn parse_head_tail_multi_file_multi_section() {
        let batch = vec![
            ht_req("/p/a.jsonl", false, 2, 1, 8),
            ht_req("/p/b.jsonl", false, 1, 1, 0),
            ht_req("/p/c.jsonl.zst", true, 0, 0, 0),
        ];
        let mut out = Vec::new();
        push_frame(&mut out, "<<<S:H:/p/a.jsonl", b"L1\nL2"); // 末行无换行，靠下一标记的前置 \n 补齐
        push_frame(&mut out, "<<<S:T:/p/a.jsonl", b""); // 空段（文件行数不足时的形态）
        push_frame(&mut out, "<<<S:C:/p/a.jsonl", b"ab\ncd"); // 二进制尾窗：含 \n 且不以 \n 结尾
        push_frame(&mut out, "<<<S:H:/p/b.jsonl", b"B1\n");
        push_frame(&mut out, "<<<S:T:/p/b.jsonl", b"B2\n");
        push_frame(&mut out, "<<<S:C:/p/c.jsonl.zst", b"\x28\xb5\x2f\xfdclip"); // 伪压缩字节
        push_frame(&mut out, "<<<S:END", b"");

        let map = parse_head_tail_output(&out, "S", &batch).unwrap();
        let a = &map["/p/a.jsonl"];
        assert_eq!(a.head, ["L1", "L2"]);
        assert!(a.tail.is_empty(), "空段应解析为零行");
        assert_eq!(a.tail_bytes.as_deref(), Some(b"ab\ncd".as_slice()));
        let b = &map["/p/b.jsonl"];
        assert_eq!(b.head, ["B1"]);
        assert_eq!(b.tail, ["B2"]);
        assert_eq!(b.tail_bytes, None, "未声明尾窗的文件不应有 C 段");
        let c = &map["/p/c.jsonl.zst"];
        assert!(c.head.is_empty() && c.tail.is_empty());
        assert_eq!(
            c.tail_bytes.as_deref(),
            Some(b"\x28\xb5\x2f\xfdclip".as_slice())
        );
    }

    #[test]
    fn parse_head_tail_fake_markers_stay_content() {
        // 内容中形似标记的行（未知 path / 行中位置 / 未声明的段种类）不得干扰归属
        let batch = vec![
            ht_req("/p/a.jsonl", false, 5, 0, 0),
            ht_req("/p/b.jsonl", false, 5, 0, 0),
        ];
        let mut out = Vec::new();
        push_frame(
            &mut out,
            "<<<S:H:/p/a.jsonl",
            b"L1\n<<<S:H:/p/other.jsonl\nmid<<<S:T:/p/b.jsonl\n<<<S:C:/p/b.jsonl\nL2",
        );
        push_frame(&mut out, "<<<S:H:/p/b.jsonl", b"B1\n");
        push_frame(&mut out, "<<<S:END", b"");

        let map = parse_head_tail_output(&out, "S", &batch).unwrap();
        assert_eq!(
            map["/p/a.jsonl"].head,
            [
                "L1",
                "<<<S:H:/p/other.jsonl",
                "mid<<<S:T:/p/b.jsonl",
                "<<<S:C:/p/b.jsonl",
                "L2",
            ]
        );
        assert_eq!(map["/p/b.jsonl"].head, ["B1"]);
    }

    #[test]
    fn parse_head_tail_missing_end_marker_errors() {
        let batch = vec![ht_req("/p/a.jsonl", false, 2, 0, 0)];
        let mut out = Vec::new();
        push_frame(&mut out, "<<<S:H:/p/a.jsonl", b"L1\n");
        // 输出在结束标记前被截断
        assert!(parse_head_tail_output(&out, "S", &batch).is_err());
        // 完全空输出同样按截断报错
        assert!(parse_head_tail_output(b"", "S", &batch).is_err());
    }

    #[test]
    fn parse_head_tail_declared_section_missing_errors() {
        // 结束标记在场，但声明的 T 段从未出现（中途截断的另一形态）
        let batch = vec![ht_req("/p/a.jsonl", false, 2, 2, 0)];
        let mut out = Vec::new();
        push_frame(&mut out, "<<<S:H:/p/a.jsonl", b"L1\n");
        push_frame(&mut out, "<<<S:END", b"");
        let err = parse_head_tail_output(&out, "S", &batch).unwrap_err();
        assert!(err.contains("截断"), "错误应说明输出被截断: {err}");
    }

    #[test]
    fn parse_head_tail_ignores_leading_noise() {
        let batch = vec![ht_req("/p/a.jsonl", false, 1, 0, 0)];
        let mut out = Vec::new();
        out.extend_from_slice(b"connection banner noise\n");
        push_frame(&mut out, "<<<S:H:/p/a.jsonl", b"L1\n");
        push_frame(&mut out, "<<<S:END", b"");
        let map = parse_head_tail_output(&out, "S", &batch).unwrap();
        assert_eq!(map["/p/a.jsonl"].head, ["L1"]);
    }

    #[test]
    fn head_tail_roundtrip_via_local_sh() {
        // 用本机 sh 真跑生成的命令，验证标记/换行契约与解析器闭环
        // （套件既有测试已依赖 /bin/sh，此处沿用同一前提）
        let dir = std::env::temp_dir().join(format!("cc-switch-ht-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("a.jsonl");
        let bin = dir.join("c.bin");
        std::fs::write(&jsonl, "h1\nh2\nm3\nm4\nt5\nt6\n").unwrap();
        std::fs::write(&bin, b"\x00\x01\x02\n\xff").unwrap();
        let (jsonl_s, bin_s) = (jsonl.to_str().unwrap(), bin.to_str().unwrap());

        let batch = vec![
            ht_req(jsonl_s, false, 2, 2, 4),
            ht_req(bin_s, true, 0, 0, 0),
        ];
        let cmd_text = head_tail_batch_command(&batch, "Sx7");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd_text)
            .output()
            .unwrap();
        assert!(out.status.success(), "sh 执行失败: {out:?}");

        let map = parse_head_tail_output(&out.stdout, "Sx7", &batch).unwrap();
        let a = &map[jsonl_s];
        assert_eq!(a.head, ["h1", "h2"]);
        assert_eq!(a.tail, ["t5", "t6"]);
        // tail -c 4 取到含换行的二进制尾窗，字节原样保留
        assert_eq!(a.tail_bytes.as_deref(), Some(b"\nt6\n".as_slice()));
        let c = &map[bin_s];
        assert!(c.head.is_empty() && c.tail.is_empty());
        assert_eq!(
            c.tail_bytes.as_deref(),
            Some(b"\x00\x01\x02\n\xff".as_slice())
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /* ===== 远端原语：zst 前缀解压 ===== */

    /// 测试内压缩样例
    fn zstd_compress(data: &str) -> Vec<u8> {
        use std::io::Write;
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        enc.write_all(data.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    /// 生成 `line{from}\n...line{to}\n` 文本
    fn numbered_lines(from: usize, to: usize) -> String {
        (from..=to).map(|i| format!("line{i}\n")).collect()
    }

    #[test]
    fn zst_prefix_full_roundtrip() {
        let text = numbered_lines(1, 3);
        assert_eq!(decode_zst_prefix(&zstd_compress(&text)).unwrap(), text);
    }

    #[test]
    fn zst_prefix_truncated_keeps_complete_frames() {
        // 双 frame 拼接后从中间截断：完整前 frame 全量解出，残缺后帧不产出内容
        let frame_a = zstd_compress(&numbered_lines(1, 10));
        let frame_b = zstd_compress(&numbered_lines(11, 20));
        let mut truncated = frame_a.clone();
        truncated.extend_from_slice(&frame_b[..frame_b.len() / 2]);
        assert_eq!(
            decode_zst_prefix(&truncated).unwrap(),
            numbered_lines(1, 10),
            "截断帧应保留完整前缀"
        );
    }

    #[test]
    fn zst_prefix_garbage_or_empty_errors() {
        assert!(decode_zst_prefix(b"\x00\x01\x02plain-garbage").is_err());
        assert!(decode_zst_prefix(b"").is_err());
    }

    /* ===== 远端原语：全文 / 区间 / 删除 ===== */

    #[test]
    fn read_full_and_delete_quote_paths() {
        assert_eq!(read_full_command("/p/a b.jsonl"), "cat '/p/a b.jsonl'");
        assert_eq!(
            read_full_command("/p/it's.jsonl"),
            "cat '/p/it'\\''s.jsonl'"
        );
        assert_eq!(delete_file_command("/p/a.jsonl"), "rm -f -- '/p/a.jsonl'");
        assert_eq!(
            delete_file_command("/p/x'y.jsonl"),
            "rm -f -- '/p/x'\\''y.jsonl'"
        );
    }

    #[test]
    fn read_range_uses_one_based_tail_offset() {
        assert_eq!(
            read_range_command("/p/a.jsonl", 10, 100),
            "tail -c +11 '/p/a.jsonl' | head -c 100"
        );
    }

    /* ===== 远程删除：stem 校验与白名单判定（Task 10）===== */

    /// stem 校验：普通 `.jsonl` / `.jsonl.zst` 双段剥扩展名后须与 session_id 完全一致
    #[test]
    fn stem_check_accepts_plain_and_zst_with_matching_id() {
        assert!(session_id_matches_stem(
            "abc",
            "/home/u/.claude/projects/p/abc.jsonl"
        ));
        // .jsonl.zst 双段整体剥除（与 remote_extension 的双段口径互逆）
        assert!(session_id_matches_stem(
            "abc",
            "/h/.codex/sessions/2026/abc.jsonl.zst"
        ));
        // 多段文件名取完整 stem（同 Path::file_stem 语义）
        assert!(session_id_matches_stem("a.b", "/p/a.b.jsonl"));
    }

    /// stem 校验拒绝面：id 与 stem 不等、路径尾斜杠 / 无扩展名 / 隐藏文件（stem 空）
    #[test]
    fn stem_check_rejects_mismatch_or_degenerate_paths() {
        // id 与 stem 不等（含 id 自带扩展名、大小写差异、空 id）
        assert!(!session_id_matches_stem("abc.jsonl", "/p/abc.jsonl"));
        assert!(!session_id_matches_stem("ABC", "/p/abc.jsonl"));
        assert!(!session_id_matches_stem("", "/p/abc.jsonl"));
        // 路径退化形态：尾斜杠（无文件名）/ 无扩展名 / 隐藏文件（stem 为空）
        assert!(!session_id_matches_stem("dir", "/p/dir/"));
        assert!(!session_id_matches_stem("abc", "/p/abc"));
        assert!(!session_id_matches_stem("jsonl", "/p/.jsonl"));
        assert!(!session_id_matches_stem("jsonl.zst", "/p/.jsonl.zst"));
        // 空 id 遇隐藏文件的空 stem：两端同为空串也拒绝（退化形态不放行）
        assert!(!session_id_matches_stem("", "/p/.jsonl"));
    }

    /// 白名单判定：清单命中放行，未命中给出「先刷新」文案
    #[test]
    fn ensure_deletable_allows_hit_and_rejects_miss() {
        assert!(ensure_deletable(Some(meta(1, 0))).is_ok());
        assert_eq!(
            ensure_deletable(None).unwrap_err(),
            "请先刷新该主机的会话列表"
        );
    }

    /* ===== 远程块读取：zst 内存取窗（Task 9）===== */

    #[test]
    fn zst_window_slices_decompressed_bytes() {
        let text = numbered_lines(1, 100);
        let bytes = text.as_bytes();
        let total = bytes.len() as u64;
        // 头窗 / 中窗与原文切片逐字节一致
        assert_eq!(zst_window(bytes, 0, 10), &bytes[..10]);
        assert_eq!(zst_window(bytes, 20, 15), &bytes[20..35]);
        // len 超过解压尾：截到文件尾
        assert_eq!(
            zst_window(bytes, total - 3, 1000),
            &bytes[bytes.len() - 3..]
        );
        // offset 恰好落在文件尾：空窗（不越界）
        assert!(zst_window(bytes, total, 10).is_empty());
        // offset 超过解压尾：空窗
        assert!(zst_window(bytes, total + 1, 10).is_empty());
    }

    #[test]
    fn zst_window_matches_original_after_full_decode() {
        // read_block 压缩路径核心语义：压缩字节 → 全解压 → 内存取窗 == 原文对应切片
        let text = numbered_lines(1, 50);
        let decompressed =
            zstd::stream::decode_all(zstd_compress(&text).as_slice()).expect("decode");
        assert_eq!(zst_window(&decompressed, 6, 6), b"line2\n");
        assert_eq!(
            zst_window(&decompressed, 0, decompressed.len() as u64),
            text.as_bytes()
        );
    }

    /* ===== 远程正文读取：临时文件路径与上限（Task 8）===== */

    #[test]
    fn temp_path_keeps_double_segment_extension() {
        let p = temp_file_path("h1", "/u/.codex/sessions/2026/rollout.jsonl.zst", "uid-1");
        // `.jsonl.zst` 双段扩展整体保留：文件名 = <uuid>.jsonl.zst
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "uid-1.jsonl.zst");
    }

    #[test]
    fn temp_path_plain_or_missing_extension() {
        assert_eq!(
            temp_file_path("h1", "/u/x.jsonl", "uid-2")
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "uid-2.jsonl"
        );
        // 无扩展名：文件名仅 uuid
        assert_eq!(
            temp_file_path("h1", "/u/readme", "uid-3")
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "uid-3"
        );
        // 隐藏文件（stem 为空）与点结尾的空扩展段都不算扩展名
        assert_eq!(
            temp_file_path("h1", "/u/.hidden", "uid-4")
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "uid-4"
        );
        assert_eq!(
            temp_file_path("h1", "/u/a.", "uid-5")
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "uid-5"
        );
    }

    #[test]
    fn temp_path_isolates_hosts_under_ssh_tmp_cache() {
        let root = crate::config::get_app_config_dir()
            .join("cache")
            .join("ssh-tmp");
        let a = temp_file_path("host-a", "/u/x.jsonl", "uid");
        let b = temp_file_path("host-b", "/u/x.jsonl", "uid");
        // 根目录固定在配置目录 cache/ssh-tmp，host_id 作子目录跨主机隔离
        assert!(a.starts_with(&root));
        assert!(b.starts_with(&root));
        assert!(a.parent().unwrap().ends_with("host-a"));
        assert!(b.parent().unwrap().ends_with("host-b"));
    }

    #[test]
    fn fetch_limit_boundary_and_message_match_local() {
        const MAX: u64 = 32 * 1024 * 1024;
        // 恰好等于上限：放行（与本地 read_limited 的 `>` 判定同口径）
        assert!(enforce_fetch_limit(MAX, MAX).is_ok());
        // 超一字节：拒绝，文案与本地 32MB 防护逐字一致（同一常量）
        let err = enforce_fetch_limit(MAX + 1, MAX).unwrap_err();
        assert_eq!(
            err.to_string(),
            crate::session_manager::content::TOO_LARGE_TEXT
        );
    }

    /* ===== 远程扫描链路（Task 7）===== */

    /// 构造远端清单条目
    fn rfe(path: &str, size: u64, mtime_ms: i64) -> RemoteFileEntry {
        RemoteFileEntry {
            path: path.to_string(),
            size,
            mtime_ms,
        }
    }

    #[test]
    fn list_cmd_empty_dirs_is_noop() {
        // 空 slice 早退为 shell no-op，避免生成 `for d in ; do ...` 病态命令
        assert_eq!(list_files_command(&[]), ":");
    }

    #[test]
    fn parse_mtime_saturates_on_huge_seconds() {
        // 秒数溢出 i64 毫秒域时饱和而非 panic（far-future 时间戳防御）
        assert_eq!(parse_mtime_ms("9300000000000000.5").unwrap(), i64::MAX);
        assert_eq!(parse_mtime_ms("-9300000000000000.5").unwrap(), i64::MIN);
    }

    #[test]
    fn home_dirs_command_prints_three_dirs_under_home() {
        assert_eq!(
            home_dirs_command(),
            "printf '%s\\n' \"$HOME/.claude/projects\" \"$HOME/.codex/sessions\" \"$HOME/.codex/archived_sessions\""
        );
    }

    #[test]
    fn parse_home_dirs_keeps_absolute_lines_only() {
        let out = b"/root/.claude/projects\n\nrelative/x\n/root/.codex/sessions\n/root/.codex/archived_sessions\n";
        assert_eq!(
            parse_home_dirs(out),
            vec![
                "/root/.claude/projects".to_string(),
                "/root/.codex/sessions".to_string(),
                "/root/.codex/archived_sessions".to_string(),
            ]
        );
        assert!(parse_home_dirs(b"").is_empty());
        // 非 UTF-8 行丢弃
        assert_eq!(parse_home_dirs(b"/a\xff\n/b\n"), vec!["/b".to_string()]);
    }

    #[test]
    fn classify_matches_dir_prefix_with_slash_boundary() {
        let claude_dir = "/root/.claude/projects".to_string();
        let codex_dirs = vec![
            "/root/.codex/sessions".to_string(),
            "/root/.codex/archived_sessions".to_string(),
        ];
        assert_eq!(
            classify_remote_provider(
                "/root/.claude/projects/p-abc/s.jsonl",
                &claude_dir,
                &codex_dirs
            ),
            Some("claude")
        );
        assert_eq!(
            classify_remote_provider(
                "/root/.codex/sessions/2026/rl.jsonl",
                &claude_dir,
                &codex_dirs
            ),
            Some("codex")
        );
        assert_eq!(
            classify_remote_provider(
                "/root/.codex/archived_sessions/rl.jsonl.zst",
                &claude_dir,
                &codex_dirs
            ),
            Some("codex")
        );
        // 前缀相似但非目录边界（projects-evil）与无关路径都不得误判
        assert_eq!(
            classify_remote_provider(
                "/root/.claude/projects-evil/s.jsonl",
                &claude_dir,
                &codex_dirs
            ),
            None
        );
        assert_eq!(
            classify_remote_provider("/opt/other/s.jsonl", &claude_dir, &codex_dirs),
            None
        );
    }

    #[test]
    fn prefer_plain_drops_zst_when_both_present() {
        let entries = vec![
            rfe("/p/a.jsonl.zst", 1, 1),
            rfe("/p/a.jsonl", 2, 2),
            rfe("/p/b.jsonl.zst", 3, 3),
        ];
        let kept = prefer_plain_entries(entries);
        // 同 stem 并存 → 未压缩优先；仅 zst → 保留；顺序保持清单原序
        assert_eq!(
            kept,
            vec![rfe("/p/a.jsonl", 2, 2), rfe("/p/b.jsonl.zst", 3, 3)]
        );
    }

    #[test]
    fn prefer_plain_keeps_same_name_files_in_different_dirs() {
        // 不同目录的同名文件是两个会话，不得因文件名相同被误判为并存形态
        let entries = vec![
            rfe("/p/sessions/2026/rollout-a.jsonl.zst", 1, 1),
            rfe("/p/archived/rollout-a.jsonl", 2, 2),
        ];
        let kept = prefer_plain_entries(entries);
        assert_eq!(kept.len(), 2, "跨目录同名文件都应保留");
    }

    #[test]
    fn head_tail_request_shapes_per_provider() {
        let claude = remote_head_tail_request("claude", &rfe("/p/a.jsonl", 1, 1));
        assert_eq!(
            claude,
            HeadTailRequest {
                path: "/p/a.jsonl".to_string(),
                compressed: false,
                head_lines: 10,
                tail_lines: 30,
                tail_bytes: 64 * 1024,
            }
        );
        let codex_plain = remote_head_tail_request("codex", &rfe("/p/a.jsonl", 1, 1));
        assert_eq!(codex_plain.tail_bytes, 0, "codex 未压缩不需要字节尾窗");
        let codex_zst = remote_head_tail_request("codex", &rfe("/p/a.jsonl.zst", 1, 1));
        assert_eq!(
            codex_zst,
            HeadTailRequest {
                path: "/p/a.jsonl.zst".to_string(),
                compressed: true,
                head_lines: 0,
                tail_lines: 0,
                tail_bytes: 0,
            }
        );
    }

    #[test]
    fn session_stem_strips_both_suffixes() {
        assert_eq!(remote_session_stem("/p/rollout-1.jsonl"), Some("rollout-1"));
        assert_eq!(
            remote_session_stem("/p/rollout-1.jsonl.zst"),
            Some("rollout-1")
        );
        assert_eq!(remote_session_stem("/p/readme"), None);
    }

    #[test]
    fn mtime_converts_to_system_time_and_negative_to_none() {
        assert_eq!(mtime_to_system_time(0), Some(SystemTime::UNIX_EPOCH));
        assert_eq!(
            mtime_to_system_time(1500),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1500))
        );
        // 负时间戳（1969 前）无法表达为 SystemTime，归 None；缓存键每轮恒定即可
        assert_eq!(mtime_to_system_time(-500), None);
    }

    /* ===== 远程恢复命令生成（Task 2）===== */

    /// 全参数主机（user + port + identity），锁定完整形态的期望值
    fn resume_host() -> SshHostConfig {
        SshHostConfig {
            id: "host-1".to_string(),
            label: "构建机".to_string(),
            host: "host".to_string(),
            user: Some("guqiu".to_string()),
            port: Some(50051),
            identity_file: Some("/key".to_string()),
        }
    }

    #[test]
    fn build_remote_resume_full_options_shape() {
        // 全参数 + cd 前缀：逐字符锁定（含两层单引号转义），复制即可粘贴执行
        let cmd =
            build_remote_resume_command(&resume_host(), "claude", "sess-9", Some("/data/proj"))
                .unwrap();
        assert_eq!(
            cmd,
            "ssh -p 50051 -i /key -t guqiu@host \
             'cd '\\''/data/proj'\\'' && claude --resume sess-9'"
        );
    }

    #[test]
    fn build_remote_resume_without_project_dir_skips_cd_prefix() {
        // project_dir 缺失：仅内层命令，无 cd 前缀；codex 内层为 `codex resume`
        let cmd = build_remote_resume_command(&resume_host(), "codex", "roll-1", None).unwrap();
        assert_eq!(
            cmd,
            "ssh -p 50051 -i /key -t guqiu@host 'codex resume roll-1'"
        );
        assert!(!cmd.contains("cd "), "无目录时不得拼 cd 前缀");
    }

    #[test]
    fn build_remote_resume_minimal_host_omits_optional_parts() {
        // 仅 host（无 user/port/identity）：无 -p / -i / user@ 前缀
        let cmd = build_remote_resume_command(&host(None, None, None), "claude", "s", Some("/p"))
            .unwrap();
        assert_eq!(
            cmd,
            "ssh -t example.com 'cd '\\''/p'\\'' && claude --resume s'"
        );
    }

    #[test]
    fn build_remote_resume_escapes_single_quote_in_dir() {
        // 目录含单引号 → 目录层 `'\''`；含引号的远端命令整体再包一层单引号
        // （本机 shell 剥外层 → 远端收到 `cd '/data/it'\''s' && ...` 再剥一层）
        let cmd =
            build_remote_resume_command(&resume_host(), "claude", "s", Some("/data/it's")).unwrap();
        assert_eq!(
            cmd,
            "ssh -p 50051 -i /key -t guqiu@host \
             'cd '\\''/data/it'\\''\\'\\'''\\''s'\\'' && claude --resume s'"
        );
    }

    #[test]
    fn build_remote_resume_unsupported_provider_returns_none() {
        // MVP 范围：仅 claude / codex 有内层命令，其它 provider 无恢复命令
        assert_eq!(
            build_remote_resume_command(&resume_host(), "gemini", "g", Some("/p")),
            None
        );
        assert_eq!(
            build_remote_resume_command(&resume_host(), "opencode", "o", None),
            None
        );
    }

    #[test]
    fn build_remote_resume_omits_batch_and_control_flags() {
        // 用户手动执行场景：不带免密探测与连接复用参数（与 build_ssh_command 口径相反）
        let cmd = build_remote_resume_command(&resume_host(), "claude", "s", Some("/p")).unwrap();
        for flag in [
            "BatchMode",
            "ControlMaster",
            "ConnectTimeout",
            "ControlPath",
        ] {
            assert!(!cmd.contains(flag), "恢复命令不应包含 {flag}");
        }
    }

    #[test]
    fn rebind_overrides_identity_fields_keeps_rest() {
        let meta = SessionMeta {
            provider_id: "claude".to_string(),
            session_id: "ignored".to_string(),
            title: Some("标题".to_string()),
            summary: None,
            project_dir: Some("/work".to_string()),
            created_at: Some(1),
            last_active_at: Some(2),
            source_path: Some("/root/x.jsonl".to_string()),
            resume_command: Some("claude --resume x".to_string()),
        };
        let rebound = rebind_remote_meta(meta, &host(None, None, None), "/root/x.jsonl");
        // session_id = 文件 stem；source_path = ssh:<hostId><远端路径>（单斜杠，
        // 与 parse_ssh_source 的解析形态、registry 清单 key 严格一致）
        assert_eq!(rebound.session_id, "x");
        assert_eq!(
            rebound.source_path.as_deref(),
            Some("ssh:test-host/root/x.jsonl")
        );
        // resume_command 按 provider 重新生成完整 ssh 命令（本机旧值不沿用）
        assert_eq!(
            rebound.resume_command.as_deref(),
            Some("ssh -t example.com 'cd '\\''/work'\\'' && claude --resume x'")
        );
        // 解析产出字段原样保留
        assert_eq!(rebound.title.as_deref(), Some("标题"));
        assert_eq!(rebound.project_dir.as_deref(), Some("/work"));
        assert_eq!(rebound.created_at, Some(1));
        assert_eq!(rebound.last_active_at, Some(2));
    }

    /* ===== 远端原语：正文搜索（Task 3）===== */

    /// 构造带独特 id 的搜索测试主机：缓存类测试按 host.id 键控全局态，
    /// 独立 id 避免并行互扰（同 registry 测试模式）；host 指向不可免密
    /// 地址——若实现未命中缓存真发探测，结果必为 false，与预填值矛盾
    fn search_host(id: &str) -> SshHostConfig {
        SshHostConfig {
            id: id.to_string(),
            label: "搜索测试".to_string(),
            host: "example.com".to_string(),
            user: None,
            port: None,
            identity_file: None,
        }
    }

    /// 构造单元素文件列表的便捷形态
    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    // —— (a) build_search_command：双路径参数串逐字锁定 + 转义 + 空文件 ——

    #[test]
    fn build_search_rg_shape_is_exact() {
        // rg 参数串逐字锁定（设计 §4.2 指定形态）；pattern 与 files 均单引号转义
        let cmd = build_search_command("error", &files(&["/f1.jsonl", "/f2.jsonl"]), true);
        assert_eq!(
            cmd,
            "rg -i -n -F --no-heading --with-filename --no-ignore -m 5 -- \
             'error' '/f1.jsonl' '/f2.jsonl'"
        );
    }

    #[test]
    fn build_search_grep_shape_is_exact() {
        // grep 降级形态：`-inFH -m 5` 一组短参等价覆盖 rg 各长参
        let cmd = build_search_command("error", &files(&["/f1.jsonl", "/f2.jsonl"]), false);
        assert_eq!(cmd, "grep -inFH -m 5 -- 'error' '/f1.jsonl' '/f2.jsonl'");
    }

    #[test]
    fn build_search_escapes_pattern_and_files() {
        // pattern 含空格 / 单引号 / `--foo` 选项伪装：整体单引号包裹 +
        // 内嵌单引号 `'\''`；`--` 双保险防 pattern 被解析为选项
        // （单引号不防选项解析，`--` 才防）。文件路径含空格同理转义
        let tricky = "--foo bar'baz";
        let rg_cmd = build_search_command(tricky, &files(&["/p a/x.jsonl"]), true);
        assert_eq!(
            rg_cmd,
            "rg -i -n -F --no-heading --with-filename --no-ignore -m 5 -- \
             '--foo bar'\\''baz' '/p a/x.jsonl'"
        );
        let grep_cmd = build_search_command(tricky, &files(&["/p a/x.jsonl"]), false);
        assert_eq!(
            grep_cmd,
            "grep -inFH -m 5 -- '--foo bar'\\''baz' '/p a/x.jsonl'"
        );
    }

    #[test]
    fn build_search_empty_files_yields_empty_command() {
        // 空文件列表返回空命令（调用方据空串跳过执行），两路径同口径
        assert_eq!(build_search_command("p", &[], true), "");
        assert_eq!(build_search_command("p", &[], false), "");
    }

    // —— (b) parse_search_output：单一解析器，容错口径 ——

    #[test]
    fn parse_search_reads_regular_lines() {
        let out = b"/p/a.jsonl:3:first hit\n/p/b.jsonl:17:second line text\n";
        assert_eq!(
            parse_search_output(out),
            vec![
                RemoteSearchLine {
                    path: "/p/a.jsonl".to_string(),
                    line_no: 3,
                    text: "first hit".to_string(),
                },
                RemoteSearchLine {
                    path: "/p/b.jsonl".to_string(),
                    line_no: 17,
                    text: "second line text".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_search_tolerates_colons_in_path_and_text() {
        // 设计 §4.2 样例行：路径含冒号时行号锚定 `:42:`（路径后的
        // `:<纯数字>:` 段），路径与 JSON 正文完整保留
        let mut out = b"/a:b/c.jsonl:42:{\"x\":1}\n".to_vec();
        // 正文含冒号（时间字面量）：`:30` 后非冒号，不构成锚段，行号仍取 `:7:`
        out.extend_from_slice(b"/p/q.jsonl:7:logged at 12:30 ok\n");
        let lines = parse_search_output(&out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].path, "/a:b/c.jsonl");
        assert_eq!(lines[0].line_no, 42);
        assert_eq!(lines[0].text, "{\"x\":1}");
        assert_eq!(lines[1].path, "/p/q.jsonl");
        assert_eq!(lines[1].line_no, 7);
        assert_eq!(lines[1].text, "logged at 12:30 ok");
    }

    #[test]
    fn parse_search_anchors_lineno_before_timestamps_in_body() {
        // 真实会话行正文必含 ISO 8601 时间戳（`10:00:00` 内含 `:00:` 锚形态）：
        // 行号必须锚定路径后的 `:3:`，不得错锚到时间戳上把 path 截成残段
        // （localhost 全链路冒烟暴露的真实形态回归）
        let out = b"/home/u/.claude/projects/p/s.jsonl:3:{\"type\":\"user\",\"timestamp\":\"2026-11-08T10:00:00Z\",\"content\":\"zephyr body\"}\n";
        let lines = parse_search_output(out);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].path, "/home/u/.claude/projects/p/s.jsonl");
        assert_eq!(lines[0].line_no, 3);
        assert!(lines[0].text.starts_with("{\"type\":\"user\""));
        assert!(lines[0].text.contains("10:00:00Z"));
    }

    #[test]
    fn parse_search_skips_lines_without_numbered_segment() {
        let mut out = Vec::new();
        out.extend_from_slice(b"garbage without colon\n"); // 无冒号
        out.extend_from_slice(b"/p/a.jsonl:abc:text\n"); // 行号段非纯数字
        out.extend_from_slice(b":7:empty-path\n"); // 路径为空（噪声行）
        out.extend_from_slice(b"/p/\xffbad.jsonl:9:text\n"); // 非 UTF-8 行
        out.extend_from_slice(b"/p/ok.jsonl:5:real match\n"); // 唯一合法行
        assert_eq!(
            parse_search_output(&out),
            vec![RemoteSearchLine {
                path: "/p/ok.jsonl".to_string(),
                line_no: 5,
                text: "real match".to_string(),
            }]
        );
    }

    #[test]
    fn parse_search_keeps_megabyte_line_intact() {
        // jsonl 单行 JSON 可达 MB 级：解析器不得截断或跳过（-m 已在命令侧
        // 截断命中数，这里只保护行内容本身完整透传）
        let huge = "x".repeat(1024 * 1024);
        let mut out = format!("/p/big.jsonl:9:{huge}").into_bytes();
        out.push(b'\n');
        let lines = parse_search_output(&out);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_no, 9);
        assert_eq!(lines[0].text.len(), 1024 * 1024);
        assert!(lines[0].text.starts_with('x') && lines[0].text.ends_with('x'));
    }

    #[test]
    fn parse_search_empty_input_returns_empty() {
        assert!(parse_search_output(b"").is_empty());
        // 纯换行切出的空行同样忽略
        assert!(parse_search_output(b"\n\n").is_empty());
    }

    // —— (c) run_remote_with_codes 退出码语义 ——

    /// 表驱动期望分类
    #[derive(Debug, Clone, Copy)]
    enum ExitExpect {
        Ok,
        Remote,
        Connect,
    }

    #[test]
    fn exit_mapping_is_table_driven() {
        // (success, code, accepted) → 期望分类：
        // exit 0 / accepted 命中 → Ok；255 恒连接层；其余非零与无 code → Remote
        let table: &[(bool, Option<i32>, &[i32], ExitExpect)] = &[
            (true, Some(0), &[0, 1], ExitExpect::Ok),
            (false, Some(1), &[0, 1], ExitExpect::Ok), // grep 无匹配 = 空结果
            (false, Some(1), &[1], ExitExpect::Ok),    // 最小 accepted 集
            (false, Some(2), &[0, 1], ExitExpect::Remote), // 真错误透传
            (false, Some(255), &[0, 1], ExitExpect::Connect),
            // 255 即使被误放进 accepted 也不得吞成空结果（连接层恒优先）
            (false, Some(255), &[0, 1, 255], ExitExpect::Connect),
            // 信号终止无 code：无法归入连接层，按远端失败透传
            (false, None, &[0, 1], ExitExpect::Remote),
            // 常规成功与 accepted 内容无关
            (true, Some(0), &[], ExitExpect::Ok),
        ];
        for &(success, code, accepted, expect) in table {
            let result = resolve_exit_aware(b"payload".to_vec(), success, code, accepted, "详情");
            match expect {
                ExitExpect::Ok => assert_eq!(
                    result.expect("应成功并回传 stdout"),
                    b"payload".to_vec(),
                    "success={success} code={code:?} accepted={accepted:?}"
                ),
                ExitExpect::Remote => assert!(
                    matches!(result, Err(SshError::Remote(_))),
                    "应分类为 Remote: success={success} code={code:?} accepted={accepted:?}"
                ),
                ExitExpect::Connect => assert!(
                    matches!(result, Err(SshError::ConnectOrAuth(_))),
                    "应分类为 ConnectOrAuth: success={success} code={code:?} accepted={accepted:?}"
                ),
            }
        }
    }

    #[test]
    fn run_with_codes_process_level_via_local_sh() {
        // 进程级退出码路径：本地 sh 真实退出 → spawn_and_collect 采集 +
        // 映射装配的完整组合（与 run_remote_with_codes 同一条代码路径，
        // 仅免 ssh 环境的等价冒烟）
        // (脚本, accepted, 期望 Ok 与否, 期望的具体错误类)
        let cases: &[(&str, &[i32], bool, Option<&str>)] = &[
            ("exit 0", &[0, 1], true, None),
            ("exit 1", &[0, 1], true, None), // 无匹配语义 → Ok
            ("exit 2", &[0, 1], false, Some("remote")), // 真错误 → Remote
            ("exit 255", &[0, 1], false, Some("connect")), // 连接层 → ConnectOrAuth
        ];
        for &(script, accepted, expect_ok, expect_kind) in cases {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", script]);
            let (status, stdout, stderr) =
                spawn_and_collect(cmd, Duration::from_secs(5)).expect("采集应成功");
            let status = status.expect("本地 sh 必然正常退出");
            let result =
                resolve_exit_aware(stdout, status.success(), status.code(), accepted, &stderr);
            assert_eq!(
                result.is_ok(),
                expect_ok,
                "脚本 {script} 的结果与期望不符: {result:?}"
            );
            if let Some(kind) = expect_kind {
                let err = result.expect_err("期望错误");
                match kind {
                    "remote" => assert!(matches!(err, SshError::Remote(_)), "{script}: {err:?}"),
                    _ => assert!(
                        matches!(err, SshError::ConnectOrAuth(_)),
                        "{script}: {err:?}"
                    ),
                }
            }
        }
    }

    // —— (d) detect_remote_rg：探测语义 + per-host 缓存 ——

    #[test]
    fn detect_rg_probe_semantics_via_local_sh() {
        // 探测命令真跑（本地 sh，免远端环境）：本机 rg 在 PATH（macOS 开发
        // 环境）→ exit 0 → true 态；缺失二进制 → exit 1 → false 态。
        // run 语义：Ok → true / 非零 → false，与 detect_remote_rg 的映射一致
        let mut ok_probe = std::process::Command::new("/bin/sh");
        ok_probe.arg("-c").arg(RG_PROBE_COMMAND);
        let ok = run_command(ok_probe, Duration::from_secs(5));
        assert!(ok.is_ok(), "本机 command -v rg 应 exit 0: {:?}", ok.err());

        let mut miss_probe = std::process::Command::new("/bin/sh");
        miss_probe
            .arg("-c")
            .arg("command -v cc-switch-definitely-missing-bin");
        let miss = run_command(miss_probe, Duration::from_secs(5));
        assert!(miss.is_err(), "缺失二进制应 exit 1 → false 态");
        assert!(
            matches!(miss, Err(SshError::Remote(_))),
            "exit 1 应为 Remote 而非连接错误: {miss:?}"
        );
    }

    #[test]
    fn detect_rg_cache_hit_returns_cached_without_probe() {
        // 预填 true / false 两态：host 指向不可免密的 example.com，若实现
        // 未命中缓存真发探测 → 连接/认证失败必得 false，与预填 true 矛盾
        // ——断言成立即证明缓存命中且零 ssh 往返
        let host_true = search_host("t-rg-cache-true");
        let host_false = search_host("t-rg-cache-false");
        {
            let mut cache = REMOTE_RG_CACHE
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.insert("t-rg-cache-true".to_string(), true);
            cache.insert("t-rg-cache-false".to_string(), false);
        }
        assert!(detect_remote_rg(&host_true), "预填 true 应命中缓存直接返回");
        assert!(
            !detect_remote_rg(&host_false),
            "预填 false 应命中缓存直接返回"
        );
        // 命中后条目仍在缓存中（未被探测路径覆写）
        let cache = REMOTE_RG_CACHE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(cache.get("t-rg-cache-true"), Some(&true));
        assert_eq!(cache.get("t-rg-cache-false"), Some(&false));
    }
}
