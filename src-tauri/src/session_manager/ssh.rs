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
//! - [`fetch_to_temp_file`]：全文拉取落临时文件（路径生成与上限检查拆出
//!   [`temp_file_path`] / [`enforce_fetch_limit`] 纯函数，便于确定性测试）
//! - [`home_dirs_command`] / [`parse_home_dirs`] / [`classify_remote_provider`] /
//!   [`prefer_plain_entries`] / [`remote_head_tail_request`] / [`rebind_remote_meta`]：
//!   远程扫描链路的清单归类与请求组装纯函数（由 `mod.rs::scan_remote_sessions` 编排）

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

static GLOBAL: LazyLock<SshScanRegistry> =
    LazyLock::new(|| SshScanRegistry {
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

    /// 摘除单条条目（删除远端会话成功后调用）；返回条目原本是否存在
    // Task 10 接线远端删除后移除
    #[allow(dead_code)]
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

/* ===== 远端原语：列表 ===== */

/// 远端会话文件条目：`find -printf` TSV 的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ms: i64,
}

/// 构造远端列表命令：`for` 循环对各目录 `find` 出 `.jsonl` / `.jsonl.zst` 文件，
/// 按 `size\t%T@\tpath` TSV 输出；`2>/dev/null` 吞掉目录不存在等远端噪音。
/// 空 slice 早退为 shell no-op（`:`），避免生成 `for d in ; do ...` 病态命令。
pub(crate) fn list_files_command(dirs: &[String]) -> String {
    if dirs.is_empty() {
        return ":".to_string();
    }
    let quoted = dirs
        .iter()
        .map(|d| escape_remote_path(d))
        .collect::<Vec<_>>();
    format!(
        "for d in {}; do find \"$d\" -type f \\( -name '*.jsonl' -o -name '*.jsonl.zst' \\) -printf '%s\\t%T@\\t%p\\n' 2>/dev/null; done",
        quoted.join(" ")
    )
}

/// 解析列表命令的 TSV 输出为条目列表。
///
/// 逐行容错——空行 / 非 UTF-8 / 字段不足 / `size` 或 mtime 非法的行整行跳过；
/// 路径列取第 3 字段起（路径内含 `\t` 不再被切分）。
pub(crate) fn parse_list_output(out: &[u8]) -> Vec<RemoteFileEntry> {
    let mut entries = Vec::new();
    for line in out.split(|&b| b == b'\n') {
        // 非 UTF-8 行：路径已损坏，无拯救价值，整行丢弃
        let Ok(text) = std::str::from_utf8(line) else { continue };
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
    let mut bounds: Vec<(usize, usize, Option<(char, String)>)> = Vec::new();
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
        let Some((kind, path)) = prev_section else { continue };
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
    let under = |dir: &str| path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'));
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

/// 远程会话标识覆盖：`session_id` = 文件 stem、`source_path` =
/// `ssh:<hostId><远端路径>`（与 [`parse_ssh_source`] 的解析形态及 registry
/// 清单 key 严格一致）、`resume_command` 置空；解析产出的其余字段原样保留。
pub(crate) fn rebind_remote_meta(
    mut meta: SessionMeta,
    host_id: &str,
    remote_path: &str,
) -> SessionMeta {
    if let Some(stem) = remote_session_stem(remote_path) {
        meta.session_id = stem.to_string();
    }
    // 远端路径自带前导 `/`，拼接时去重避免 `ssh:h1//root/...` 双斜杠形态
    let rel = remote_path.strip_prefix('/').unwrap_or(remote_path);
    meta.source_path = Some(format!("ssh:{host_id}/{rel}"));
    meta.resume_command = None;
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
// Task 10 接线远程删除后移除
#[allow(dead_code)]
pub(crate) fn delete_file_command(path: &str) -> String {
    format!("rm -f -- {}", escape_remote_path(path))
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

    /* ===== 远端原语：列表 ===== */

    #[test]
    fn list_cmd_quotes_dirs_and_uses_find_printf() {
        let cmd = list_files_command(&[
            "/home/u/.claude/projects".to_string(),
            "/home/u/.codex/sessions".to_string(),
        ]);
        assert_eq!(
            cmd,
            "for d in '/home/u/.claude/projects' '/home/u/.codex/sessions'; do \
             find \"$d\" -type f \\( -name '*.jsonl' -o -name '*.jsonl.zst' \\) \
             -printf '%s\\t%T@\\t%p\\n' 2>/dev/null; done"
        );
    }

    #[test]
    fn list_cmd_escapes_embedded_single_quote() {
        let cmd = list_files_command(&["/dir with'quote".to_string()]);
        assert!(cmd.starts_with("for d in '/dir with'\\''quote'; do "));
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
        out.extend_from_slice(b"\n");                               // 空行
        out.extend_from_slice(b"garbage-without-tabs\n");           // 无 TSV 字段
        out.extend_from_slice(b"abc\t1.0\t/p/bad1.jsonl\n");        // size 非数字
        out.extend_from_slice(b"789\tnot-a-time\t/p/bad2.jsonl\n"); // mtime 非法
        out.extend_from_slice(b"9\t100.5\t/\xffbad.jsonl\n");       // 非 UTF-8 行
        out.extend_from_slice(b"77\t1700000000\t/p/last.jsonl");    // 无尾换行 + 整秒时间
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
        assert_eq!(read_full_command("/p/it's.jsonl"), "cat '/p/it'\\''s.jsonl'");
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
        assert_eq!(zst_window(bytes, total - 3, 1000), &bytes[bytes.len() - 3..]);
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
        let rebound = rebind_remote_meta(meta, "host-1", "/root/x.jsonl");
        // session_id = 文件 stem；source_path = ssh:<hostId><远端路径>（单斜杠，
        // 与 parse_ssh_source 的解析形态、registry 清单 key 严格一致）
        assert_eq!(rebound.session_id, "x");
        assert_eq!(
            rebound.source_path.as_deref(),
            Some("ssh:host-1/root/x.jsonl")
        );
        assert_eq!(rebound.resume_command, None, "远程会话不支持本地 resume");
        // 解析产出字段原样保留
        assert_eq!(rebound.title.as_deref(), Some("标题"));
        assert_eq!(rebound.project_dir.as_deref(), Some("/work"));
        assert_eq!(rebound.created_at, Some(1));
        assert_eq!(rebound.last_active_at, Some(2));
    }
}
