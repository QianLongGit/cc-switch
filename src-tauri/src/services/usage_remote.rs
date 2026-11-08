//! SSH 远程主机的用量统计同步引擎（设计 §4.4，架构镜像本机增量链路）。
//!
//! 远端只做「清单 + 字节区间」两个原语；解析、去重、入库全在本地复用
//! Task 7 的段解析纯函数与既有 upsert，单一口径。三层结构：
//!
//! ```text
//! IO 注入（RemoteUsageIo）── 生产 SshUsageIo（ssh.rs 原语 + ControlMaster 复用）
//!                          └─ 单测 FakeRemote（内存假远端，零 ssh 依赖）
//! 引擎（sync_host_usage_with_io）── 清单 diff 三态 + 消失态 + 90 天窗口
//!                                   + 32MB 上限 + 取消 + per-host 串行
//! 入库（claude/codex 分支）── data_source = "ssh:<hostId>"，rollup 按
//!                             data_source 前缀分账到 host_id 列（v27）
//! ```
//!
//! 游标键 = `ssh:<hostId>/<远端路径>` 源路径形态，与本机游标天然隔离；
//! 已入库数据永不删除（对齐本机「文件消失只清游标」语义）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::parser::SESSION_REQUEST_ID_PREFIX;
use crate::services::session_usage::{
    claude_tail_fingerprint, load_sync_cursors, parse_claude_usage_segment,
    update_claude_sync_state_on_conn, upsert_session_log_entry_on_conn, ClaudeSegmentSeed,
    SessionPricingCache, SyncCursor, CHAIN_SEED_BYTES, TAIL_FINGERPRINT_BYTES,
};
use crate::services::session_usage_codex::{
    insert_codex_session_entry_on_conn, parse_codex_usage_segment, thread_id_from_filename,
    update_codex_remote_sync_state_on_conn, CodexSegmentContext, CODEX_THREAD_REQUEST_ID_PREFIX,
};
use crate::session_manager::ssh::{
    self, list_files_command, parse_home_dirs, parse_list_output, prefer_plain_entries,
    read_full_command, read_range_command, run_remote, RemoteFileEntry,
};
use crate::settings::SshHostConfig;

/* ===== 常量 ===== */

/// 首扫的 mtime 窗口（用户裁定 90 天）：更早的文件永不拉取、永不建游标。
/// 清单里的 mtime 为 Unix 毫秒。
const REMOTE_USAGE_MTIME_WINDOW_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// 单文件拉取的字节上限（与本地全文防护 / `enforce_fetch_limit` 同口径 32MB）：
/// 超限跳过并计数，单文件失败不毁整个首扫。
const REMOTE_USAGE_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// 字节区间 / 全文拉取超时：与扫描链路大文件口径一致（120s）
const REMOTE_USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// 远端目录解析 / 清单命令超时：与 `session_manager::REMOTE_DIRS_TIMEOUT` /
/// `REMOTE_LIST_TIMEOUT` 同口径
const REMOTE_USAGE_DIRS_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_USAGE_LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// 轻同步单轮自动纳入的无游标新文件数上限：防异常大目录把每次自动刷新
/// 变成全量首扫；超限文件保持 pending（peek 可见），后续轮次继续消化
const REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT: usize = 20;
/// 轻同步单轮自动纳入的新文件累计字节上限（语义同上，字节维度）
const REMOTE_LIGHT_SYNC_NEW_BYTES_LIMIT: u64 = 50 * 1024 * 1024;

/* ===== 进度载荷 ===== */

/// 远程用量同步进度（事件 `remote-usage-sync-status` 的载荷，serde camelCase）。
///
/// `totalFiles` 是本轮流扫的清单文件总数（含无变化快速跳过的）；
/// `skipped` 是窗口外 / 超限 / 轻同步单轮限额外的无游标文件 / 拉取或解析
/// 失败的文件计数（限额外文件保持 pending，peek 可见、后续轮次消化）；
/// `doneBytes` 是本轮实际拉回的字节数。peek 复用同一结构表达「待同步
/// 文件数 / 待拉总字节」（`totalFiles` / `doneBytes`），不拉取不写库。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteUsageSyncStatus {
    pub done_files: usize,
    pub total_files: usize,
    pub done_bytes: u64,
    pub skipped: usize,
}

/* ===== IO 注入抽象 ===== */

/// 引擎的远端 IO 面：恰好四个原语，生产与单测共用同一引擎内核。
///
/// 单测用内存假远端（无 ssh 依赖、可精确断言拉取区间），生产实现走
/// ssh.rs 传输层（ControlMaster 复用、40MB stdout 上限、退出码分类）。
pub(crate) trait RemoteUsageIo {
    /// 远端 $HOME 下的三个会话根目录（claude / codex sessions / archived）
    fn home_dirs(&mut self) -> Result<Vec<String>, AppError>;
    /// 各目录的 `size\tmtime\tpath` TSV 清单
    fn list_files(&mut self, dirs: &[String]) -> Result<Vec<RemoteFileEntry>, AppError>;
    /// 字节区间拉取（未压缩文件专用）
    fn read_range(&mut self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, AppError>;
    /// 全文拉取（zst 归档专用；大小上限由引擎把关）
    fn read_full(&mut self, path: &str) -> Result<Vec<u8>, AppError>;
}

/* ===== 生产实现：SSH 传输 ===== */

/// [`RemoteUsageIo`] 的 SSH 生产实现：对 ssh.rs 原语的薄封装，
/// 错误文案带主机 label 供 UI 直接展示。
struct SshUsageIo {
    host: SshHostConfig,
}

impl RemoteUsageIo for SshUsageIo {
    fn home_dirs(&mut self) -> Result<Vec<String>, AppError> {
        let out = run_remote(
            &self.host,
            &ssh::home_dirs_command(),
            REMOTE_USAGE_DIRS_TIMEOUT,
        )
        .map_err(|e| AppError::Config(format!("SSH 主机「{}」: {e}", self.host.label)))?;
        Ok(parse_home_dirs(&out))
    }

    fn list_files(&mut self, dirs: &[String]) -> Result<Vec<RemoteFileEntry>, AppError> {
        let out = run_remote(
            &self.host,
            &list_files_command(dirs),
            REMOTE_USAGE_LIST_TIMEOUT,
        )
        .map_err(|e| AppError::Config(format!("SSH 主机「{}」: {e}", self.host.label)))?;
        Ok(parse_list_output(&out))
    }

    fn read_range(&mut self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, AppError> {
        run_remote(
            &self.host,
            &read_range_command(path, offset, len),
            REMOTE_USAGE_FETCH_TIMEOUT,
        )
        .map_err(|e| AppError::Config(format!("SSH 主机「{}」: {e}", self.host.label)))
    }

    fn read_full(&mut self, path: &str) -> Result<Vec<u8>, AppError> {
        run_remote(
            &self.host,
            &read_full_command(path),
            REMOTE_USAGE_FETCH_TIMEOUT,
        )
        .map_err(|e| AppError::Config(format!("SSH 主机「{}」: {e}", self.host.label)))
    }
}

/* ===== 取消与 per-host 串行 ===== */

/// 取消标志（模块级，设计 §4.4）：`cancel_remote_usage_sync` 命令置位，
/// 引擎主循环在下一个文件边界停止并返回已推进的部分进度。
/// 手动同步入口（`sync_remote_usage` 命令）开工前重置。
static SYNC_CANCELLED: AtomicBool = AtomicBool::new(false);

/// 请求取消进行中的远程用量同步（对全部主机的同步生效——单用户 UI 场景
/// 同时只有一台主机在同步，per-host 标志不值得多出的状态复杂度）。
pub fn request_cancel_remote_usage_sync() {
    SYNC_CANCELLED.store(true, Ordering::Relaxed);
}

/// 重置取消标志：手动同步入口（`sync_remote_usage` 命令）开工前调用，
/// 上一轮取消的残留不拦本轮。
pub fn reset_cancel_flag() {
    SYNC_CANCELLED.store(false, Ordering::Relaxed);
}

/// 引擎入口统一读取的取消视图：显式取消标志优先；测试可注入自己的标志。
fn cancelled(cancel: &AtomicBool) -> bool {
    SYNC_CANCELLED.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed)
}

/// per-host 串行锁：同主机并发同步（自动轻同步撞上手动首扫）排队执行，
/// 防同主机 ssh 通道争用与游标写竞态。锁槽只增不删，HashMap 随主机数有界。
static HOST_SYNC_MUTEXES: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn host_mutex(host_id: &str) -> Arc<Mutex<()>> {
    let mut slots = HOST_SYNC_MUTEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slots
        .entry(host_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/* ===== 引擎：清单 diff + 增量拉取 ===== */

/// 远端清单按 provider 归类后的文件集（claude 只收 `.jsonl`，codex 侧
/// `.jsonl` / `.jsonl.zst` 共存时保留未压缩——与扫描链路同判定）。
#[derive(Default)]
struct RemoteUsageFiles {
    claude: Vec<RemoteFileEntry>,
    codex: Vec<RemoteFileEntry>,
}

/// 清单归类（纯函数）：目录前缀判 provider + 帧协议同款的换行路径防护 +
/// codex 共存判定。claude 本地扫描只收 `.jsonl`（远端无 agent/journal 形态）。
fn classify_usage_entries(
    entries: Vec<RemoteFileEntry>,
    claude_dir: &str,
    codex_dirs: &[String],
) -> RemoteUsageFiles {
    let mut files = RemoteUsageFiles::default();
    let mut codex_raw = Vec::new();
    for entry in entries {
        if entry.path.contains('\n') {
            log::warn!("[REMOTE-USAGE] 跳过含换行的远端路径: {:?}", entry.path);
            continue;
        }
        match ssh::classify_remote_provider(&entry.path, claude_dir, codex_dirs) {
            Some("claude") if entry.path.ends_with(".jsonl") => files.claude.push(entry),
            Some("codex") => codex_raw.push(entry),
            _ => {}
        }
    }
    files.codex = prefer_plain_entries(codex_raw);
    files
}

/// 远程游标键：`ssh:<hostId>/<远端路径>`（与远程会话 source_path 同构，
/// parse_ssh_source 可无损反解）。
fn remote_cursor_key(host_id: &str, remote_path: &str) -> String {
    format!("ssh:{}/{}", host_id, remote_path.trim_start_matches('/'))
}

/// 远程明细的 data_source 值（rollup 剪枝按此前缀分账到 host_id 列）。
fn remote_data_source(host_id: &str) -> String {
    format!("ssh:{host_id}")
}

/// 清单 diff 的变化判定（纯函数）：无游标 = 待首扫；有游标时 committed
/// 字节边界或 mtime 任一变化才需要拉取。同尺寸重写 claude 侧由段种子的
/// 指纹校验兜底（拉回后核对），codex 侧与本机一样检测不到（无指纹）。
fn file_needs_fetch(entry: &RemoteFileEntry, cursor: Option<&SyncCursor>) -> bool {
    match cursor {
        None => true,
        Some(cursor) => {
            let committed = cursor.last_byte_offset.unwrap_or(0);
            entry.size as i64 != committed || entry.mtime_ms != cursor.last_modified
        }
    }
}

/// 同步一台远程主机的用量（生产入口）：per-host 串行 + SSH IO。
///
/// `progress` 在每个文件处理完后收到一次当前进度（命令层转 emit）；
/// `cancel` 是调用方注入的额外取消通道（命令层传常量引用即可，引擎
/// 内部还会检查模块级取消标志）。
pub fn sync_host_usage(
    db: &Database,
    host: &SshHostConfig,
    force: bool,
    progress: &mut dyn FnMut(&RemoteUsageSyncStatus),
    cancel: &AtomicBool,
) -> Result<RemoteUsageSyncStatus, AppError> {
    // per-host 串行：排队等正在跑的同主机同步结束（后续那趟通常零变化）
    let serial = host_mutex(&host.id);
    let _guard = serial
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut io = SshUsageIo { host: host.clone() };
    sync_host_usage_with_io(db, &host.id, &mut io, force, progress, cancel)
}

/// [`peek_host_usage_with_io`] 的 SSH 生产入口（`peek_remote_usage` 命令）。
pub fn peek_host_usage(
    db: &Database,
    host: &SshHostConfig,
) -> Result<RemoteUsageSyncStatus, AppError> {
    let mut io = SshUsageIo { host: host.clone() };
    peek_host_usage_with_io(db, &host.id, &mut io)
}

/// 引擎内核（IO 注入版，单测目标）：清单 diff 三态 + 消失态 + 90 天窗口
/// + 32MB 上限 + 每文件进度 + 取消即停。
pub(crate) fn sync_host_usage_with_io(
    db: &Database,
    host_id: &str,
    io: &mut dyn RemoteUsageIo,
    force: bool,
    progress: &mut dyn FnMut(&RemoteUsageSyncStatus),
    cancel: &AtomicBool,
) -> Result<RemoteUsageSyncStatus, AppError> {
    // ── 1. 远端目录 + 清单（两次轻往返）──
    let dirs = io.home_dirs()?;
    if dirs.len() < 3 {
        return Err(AppError::Config(format!(
            "远端目录解析异常，期望 3 个目录，实际 {} 个",
            dirs.len()
        )));
    }
    let (claude_dir, codex_dirs) = (dirs[0].clone(), dirs[1..].to_vec());
    let files = classify_usage_entries(io.list_files(&dirs)?, &claude_dir, &codex_dirs);

    // ── 2. 本主机游标（`ssh:` 前缀键天然隔离，全表预取后过滤）──
    let prefix = format!("ssh:{host_id}/");
    let cursors: HashMap<String, SyncCursor> = load_sync_cursors(db)?
        .into_iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .collect();

    // ── 3. 消失态：上次清单有、本次无 → 删游标行，已入库数据保留 ──
    delete_vanished_cursors(db, &cursors, &files)?;

    // ── 4. 主循环：窗口 / 大小 / force 门 + 分支拉取入库 ──
    let mut status = RemoteUsageSyncStatus {
        total_files: files.claude.len() + files.codex.len(),
        ..RemoteUsageSyncStatus::default()
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_floor_ms = now_ms.saturating_sub(REMOTE_USAGE_MTIME_WINDOW_MS);
    let data_source = remote_data_source(host_id);

    let all_files: Vec<(&'static str, &RemoteFileEntry)> = files
        .claude
        .iter()
        .map(|entry| ("claude", entry))
        .chain(files.codex.iter().map(|entry| ("codex", entry)))
        .collect();

    // 轻同步自动首扫的限额账本（force 首扫不设限，拉全部窗口内文件）
    let mut auto_new_files = 0usize;
    let mut auto_new_bytes = 0u64;

    for (provider, entry) in all_files {
        if cancelled(cancel) {
            log::info!(
                "[REMOTE-USAGE] 取消请求，停止于 {}/{} 个文件",
                status.done_files,
                status.total_files
            );
            break;
        }
        // 90 天窗口：旧文件不拉不建游标（计入 skipped 供 UI 提示）
        if entry.mtime_ms < window_floor_ms {
            status.skipped += 1;
            status.done_files += 1;
            progress(&status);
            continue;
        }
        // 32MB 上限：跳过并计数
        if entry.size > REMOTE_USAGE_MAX_FILE_BYTES {
            log::warn!(
                "[REMOTE-USAGE] 文件超过 {}MB 上限，跳过: {}",
                REMOTE_USAGE_MAX_FILE_BYTES / (1024 * 1024),
                entry.path
            );
            status.skipped += 1;
            status.done_files += 1;
            progress(&status);
            continue;
        }

        let key = remote_cursor_key(host_id, &entry.path);
        let cursor = cursors.get(&key);
        // 轻同步的自动首扫门（spec §4.4 数据流第 1 条：新文件 → 自动对变化
        // 文件执行增量拉取）：无游标且窗口内、≤32MB 的新文件直接纳入本轮；
        // 单轮限额（≤20 个 / 累计 ≤50MB）防大目录把自动刷新变成全量首扫，
        // 超限的保持 pending（计入 skipped，peek 可见），手动 force 兜底
        if cursor.is_none() && !force {
            if auto_new_files >= REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT
                || auto_new_bytes.saturating_add(entry.size) > REMOTE_LIGHT_SYNC_NEW_BYTES_LIMIT
            {
                status.skipped += 1;
                status.done_files += 1;
                progress(&status);
                continue;
            }
            auto_new_files += 1;
            auto_new_bytes += entry.size;
        }

        let outcome = match provider {
            "claude" => sync_claude_remote_file(db, io, entry, cursor, &key, &data_source),
            _ => sync_codex_remote_file(db, io, entry, cursor, &key, &data_source),
        };
        match outcome {
            Ok(FileSyncOutcome::Synced { fetched_bytes }) => {
                status.done_bytes += fetched_bytes;
            }
            Ok(FileSyncOutcome::Unchanged) => {}
            Ok(FileSyncOutcome::Skipped) => status.skipped += 1,
            // 单文件拉取/解析失败：计数继续，不毁整轮（设计 §4.4）
            Err(error) => {
                log::warn!("[REMOTE-USAGE] 文件同步失败 {}: {error}", entry.path);
                status.skipped += 1;
            }
        }
        status.done_files += 1;
        progress(&status);
    }

    // 取消请求是一次性的：本轮退出（无论正常完成还是观察到取消而停止）
    // 即消费完毕，复位标志——否则残留 true 会让后续每一轮轻同步在首文件
    // 前静默 break，远程视图停更直到手动同步
    SYNC_CANCELLED.store(false, Ordering::Relaxed);
    Ok(status)
}

/// 单文件同步结果（文件级）。
#[derive(Debug, PartialEq, Eq)]
enum FileSyncOutcome {
    /// 拉取并入库（或推游标），`fetched_bytes` 为拉回字节数
    Synced { fetched_bytes: u64 },
    /// 清单 diff 无变化，零拉取
    Unchanged,
    /// 文件级跳过（截断/重写钉住、codex 父关系未解决等），计入 skipped
    Skipped,
}

/// 消失态清理：本主机游标中，远端路径不在本次清单里的删除游标行。
/// 已入库数据保留（对齐本机「文件删除只清游标」语义）。
fn delete_vanished_cursors(
    db: &Database,
    cursors: &HashMap<String, SyncCursor>,
    files: &RemoteUsageFiles,
) -> Result<(), AppError> {
    let listed: HashSet<&str> = files
        .claude
        .iter()
        .chain(files.codex.iter())
        .map(|entry| entry.path.as_str())
        .collect();
    let vanished: Vec<&String> = cursors
        .keys()
        // 键可反解出远端绝对路径（与 source_path 同构）；反解失败的键不动
        .filter(|key| {
            ssh::parse_ssh_source(key)
                .map(|(_, path)| !listed.contains(path.as_str()))
                .unwrap_or(false)
        })
        .collect();
    if vanished.is_empty() {
        return Ok(());
    }
    let conn = lock_conn!(db.conn);
    for key in vanished {
        // 只清游标：数据行保留，远程账本不因清单变化丢历史
        conn.execute("DELETE FROM session_log_sync WHERE file_path = ?1", [key])
            .map_err(|e| AppError::Database(format!("清理消失文件的游标失败: {e}")))?;
        log::info!("[REMOTE-USAGE] 远端文件已消失，清理游标（数据保留）: {key}");
    }
    Ok(())
}

/* ===== claude 分支：seed 窗口 + 段解析 + 入库 ===== */

/// 同步单个远程 claude jsonl：增量路径一次 `read_range` 同时取回
/// 「游标前窗口（链回看 + 指纹种子）+ 新增段」，窗口与段边界在本地切分。
fn sync_claude_remote_file(
    db: &Database,
    io: &mut dyn RemoteUsageIo,
    entry: &RemoteFileEntry,
    cursor: Option<&SyncCursor>,
    key: &str,
    data_source: &str,
) -> Result<FileSyncOutcome, AppError> {
    let size = entry.size;
    let mtime = entry.mtime_ms;

    let (offset, window, segment, from_start) = match cursor {
        Some(cursor) => {
            let offset = cursor.last_byte_offset.unwrap_or(0);
            if offset > size as i64 {
                // 截断：游标钉到新 EOF、不重放旧区间（本机同语义），拉新尾重算指纹
                return pin_claude_cursor(db, io, entry, key);
            }
            if offset == size as i64 && mtime == cursor.last_modified {
                return Ok(FileSyncOutcome::Unchanged);
            }
            // 增量：窗口起点回看 ≤ CHAIN_SEED_BYTES，一次往返带上窗口 + 段
            let window_start = (offset as u64).saturating_sub(CHAIN_SEED_BYTES as u64);
            let bytes = io.read_range(&entry.path, window_start, size - window_start)?;
            let split = (offset as u64 - window_start) as usize;
            let (window, segment) = bytes.split_at(split);
            // 指纹校验（远程版 read_tail_before 的对偶）：边界前 4KB 尾巴对不上
            // 说明文件被重写，钉 EOF 防已剪明细双算
            if let Some(expected) = cursor.last_tail_fingerprint {
                let tail_seed =
                    &window[window.len().saturating_sub(TAIL_FINGERPRINT_BYTES as usize)..];
                if claude_tail_fingerprint(tail_seed) != expected {
                    log::warn!(
                        "[REMOTE-USAGE] 远端文件被外部重写，游标钉至 EOF、不重放旧区间: {}",
                        entry.path
                    );
                    return pin_claude_cursor(db, io, entry, key);
                }
            }
            (offset, window.to_vec(), segment.to_vec(), window_start == 0)
        }
        None => {
            // 首扫（force）：全量段 + 空窗口（链从头积累）
            let bytes = io.read_range(&entry.path, 0, size)?;
            (0, Vec::new(), bytes, true)
        }
    };

    // ── 段解析（Task 7 纯函数）+ 入库：同一事务原子提交 ──
    let cursor_in = SyncCursor {
        last_modified: mtime,
        last_byte_offset: Some(offset),
        ..cursor.cloned().unwrap_or_default()
    };
    let seed = ClaudeSegmentSeed {
        chain_window: &window,
        chain_window_from_start: from_start,
        tail_seed: &window[window.len().saturating_sub(TAIL_FINGERPRINT_BYTES as usize)..],
    };
    let outcome = parse_claude_usage_segment(&segment, &cursor_in, &seed);
    let fetched_bytes = (window.len() + segment.len()) as u64;

    let conn = lock_conn!(db.conn);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| AppError::Database(format!("启动远程 Claude 用量事务失败: {e}")))?;
    let mut pricing_cache = SessionPricingCache::default();
    for usage in outcome.usages {
        let msg = usage.parsed;
        // 与本机同 gate：任一计费维度 > 0 即导入（放宽 gate 的理由见
        // sync_single_file 的注释，远程与本机必须同口径）
        let has_billable_tokens = msg.input_tokens > 0
            || msg.output_tokens > 0
            || msg.cache_read_tokens > 0
            || msg.cache_creation_tokens > 0;
        if !has_billable_tokens {
            continue;
        }
        let request_id = format!("{SESSION_REQUEST_ID_PREFIX}{}", msg.message_id);
        // 插入失败只跳过该条（本机同行为），不断整个文件
        if let Err(error) = upsert_session_log_entry_on_conn(
            &tx,
            &request_id,
            &msg,
            usage.timing,
            data_source,
            &mut pricing_cache,
        ) {
            log::warn!("[REMOTE-USAGE] 插入失败 ({}): {error}", msg.message_id);
        }
    }
    update_claude_sync_state_on_conn(
        &tx,
        key,
        mtime,
        outcome.cursor.last_byte_offset.unwrap_or(offset),
        outcome.cursor.last_tail_fingerprint,
    )?;
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交远程 Claude 用量事务失败: {e}")))?;

    Ok(FileSyncOutcome::Synced { fetched_bytes })
}

/// 截断 / 重写后的游标钉住：拉新 EOF 前的指纹窗、游标推进到当前大小，
/// 旧区间一概不重放（重放会把已剪明细双算进汇总）。
fn pin_claude_cursor(
    db: &Database,
    io: &mut dyn RemoteUsageIo,
    entry: &RemoteFileEntry,
    key: &str,
) -> Result<FileSyncOutcome, AppError> {
    let tail_len = TAIL_FINGERPRINT_BYTES as u64;
    let tail_start = entry.size.saturating_sub(tail_len);
    let tail = io.read_range(&entry.path, tail_start, entry.size - tail_start)?;
    let fingerprint = claude_tail_fingerprint(&tail);
    let conn = lock_conn!(db.conn);
    update_claude_sync_state_on_conn(
        &conn,
        key,
        entry.mtime_ms,
        entry.size as i64,
        Some(fingerprint),
    )?;
    Ok(FileSyncOutcome::Skipped)
}

/* ===== codex 分支：parser_state 接力 + 段解析 + 入库 ===== */

/// 同步单个远程 codex rollout：
/// - 未压缩：`read_range(committed, size - committed)` 增量段，跨段状态经
///   `parser_state` 序列化接力（`rollout_index` 传 None——构建索引需要全量
///   拉取，带基线需求的续写页 / fork 子页按跳过处理，防误算）
/// - zst 归档：`read_full` 全量拉取 + 本地解压（zst 无法区间读；归档文件
///   不变，拉一次即终态），游标的 `last_byte_offset` 记压缩 size 作 diff 基准
fn sync_codex_remote_file(
    db: &Database,
    io: &mut dyn RemoteUsageIo,
    entry: &RemoteFileEntry,
    cursor: Option<&SyncCursor>,
    key: &str,
    data_source: &str,
) -> Result<FileSyncOutcome, AppError> {
    let size = entry.size;
    let mtime = entry.mtime_ms;
    let zst = entry.path.ends_with(".jsonl.zst");

    // 文件名缺有效尾部 UUID：无法构造稳定 request_id，跳过（本机 deferred 同语义）
    let Some(root_thread_id) = thread_id_from_filename(Path::new(&entry.path)) else {
        log::warn!(
            "[REMOTE-USAGE] codex 文件名缺少有效的尾部 UUID，跳过: {}",
            entry.path
        );
        return Ok(FileSyncOutcome::Skipped);
    };

    let (segment, cursor_in, fetched_bytes) = if zst {
        // zst 归档：无变化短路（diff 基准 = 压缩 size）；变化或首扫走全量重扫
        match cursor {
            Some(cursor) if !file_needs_fetch(entry, Some(cursor)) => {
                return Ok(FileSyncOutcome::Unchanged);
            }
            _ => {
                let raw = io.read_full(&entry.path)?;
                if raw.len() as u64 > REMOTE_USAGE_MAX_FILE_BYTES {
                    log::warn!("[REMOTE-USAGE] zst 压缩字节超过上限，跳过: {}", entry.path);
                    return Ok(FileSyncOutcome::Skipped);
                }
                let bytes = zstd::stream::decode_all(raw.as_slice())
                    .map_err(|e| AppError::Config(format!("zstd 解压失败 {}: {e}", entry.path)))?;
                (bytes, SyncCursor::default(), raw.len() as u64)
            }
        }
    } else {
        match cursor {
            Some(cursor) => {
                let committed = cursor.last_byte_offset.unwrap_or(0);
                if committed > size as i64 {
                    // 截断：游标重置当首扫重解析（request_id 去重兜底，无指纹可校验）
                    let bytes = io.read_range(&entry.path, 0, size)?;
                    (bytes, SyncCursor::default(), size)
                } else if committed == size as i64 && mtime == cursor.last_modified {
                    return Ok(FileSyncOutcome::Unchanged);
                } else {
                    let bytes =
                        io.read_range(&entry.path, committed as u64, size - committed as u64)?;
                    let fetched = bytes.len() as u64;
                    let mut cursor_in = cursor.clone();
                    cursor_in.last_modified = mtime;
                    (bytes, cursor_in, fetched)
                }
            }
            None => {
                // 首扫（force）：全量段，状态从空起步
                let bytes = io.read_range(&entry.path, 0, size)?;
                (bytes, SyncCursor::default(), size)
            }
        }
    };

    // ── 段解析（Task 7 纯函数，rollout_index = None）──
    let ctx = CodexSegmentContext {
        file_path: Path::new(&entry.path),
        root_thread_id: Some(&root_thread_id),
        rollout_index: None,
    };
    let outcome = parse_codex_usage_segment(&segment, &cursor_in, &ctx);

    // 段末身份判定（跳过时游标不动，下轮 force 重扫；对齐本机 deferred 语义）
    if !outcome.state.root_meta_seen() {
        log::warn!(
            "[REMOTE-USAGE] codex 段未见 session_meta，暂缓: {}",
            entry.path
        );
        return Ok(FileSyncOutcome::Skipped);
    }
    if let Some(reason) = outcome.state.unresolved_parent_reason() {
        log::warn!("[REMOTE-USAGE] codex 文件暂缓（{reason}）: {}", entry.path);
        return Ok(FileSyncOutcome::Skipped);
    }

    // ── 入库：行号门 + request_id 稳定前缀 + 事务原子推进游标 ──
    let mut cursor_out = outcome.cursor;
    cursor_out.last_modified = mtime;
    if zst {
        // zst 的 diff 基准是清单里的压缩 size（解压长度每轮都变会永判「有变化」）
        cursor_out.last_byte_offset = Some(size as i64);
    }

    let session_thread_id = outcome
        .state
        .meta_thread_id()
        .unwrap_or(root_thread_id.as_str());
    let line_gate = cursor_in.last_line_offset;

    let conn = lock_conn!(db.conn);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| AppError::Database(format!("启动远程 Codex 用量事务失败: {e}")))?;
    let mut pricing_cache: HashMap<String, Option<crate::proxy::usage::calculator::ModelPricing>> =
        HashMap::new();
    let mut suspected_duplicates = 0u32;

    let mut insert_event = |tx: &rusqlite::Connection,
                            suffix: &str,
                            event: &crate::services::session_usage_codex::ParsedTokenEvent|
     -> Result<(), AppError> {
        let request_id = format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{root_thread_id}:{suffix}");
        // 插入失败只跳过该条（本机同行为），不断整个文件
        if let Err(error) = insert_codex_session_entry_on_conn(
            tx,
            &request_id,
            &event.delta,
            &event.model,
            Some(session_thread_id),
            event.timestamp.as_deref(),
            event.timing,
            data_source,
            &mut suspected_duplicates,
            &mut pricing_cache,
        ) {
            log::warn!("[REMOTE-USAGE] 插入失败 ({request_id}): {error}");
        }
        Ok(())
    };

    for event in &outcome.token_events {
        // 零 delta 快照没有 event_index（不占序号）；行号门挡住 parser_state
        // 兜底回退时重放的旧行——双保险，与 request_id 主键去重同向
        let Some(event_index) = event.event_index else {
            continue;
        };
        if event.line_offset <= line_gate {
            continue;
        }
        insert_event(&tx, &event_index.to_string(), event)?;
    }
    for event in &outcome.usage_record_events {
        let Some(suffix) = event.request_suffix.as_deref() else {
            continue;
        };
        if event.line_offset <= line_gate {
            continue;
        }
        insert_event(&tx, suffix, event)?;
    }

    // 无计费文件也推游标（本机同语义）：零事件自然零插入，只落游标
    update_codex_remote_sync_state_on_conn(&tx, key, &cursor_out)?;
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交远程 Codex 用量事务失败: {e}")))?;

    Ok(FileSyncOutcome::Synced { fetched_bytes })
}

/* ===== peek：待同步统计（不拉取不写库） ===== */

/// 清单 diff 单往返统计待同步文件数 / 待拉总字节：usage 页首扫预告与
/// 「有更新」提示共用。读库（游标）但不写。
pub(crate) fn peek_host_usage_with_io(
    db: &Database,
    host_id: &str,
    io: &mut dyn RemoteUsageIo,
) -> Result<RemoteUsageSyncStatus, AppError> {
    let dirs = io.home_dirs()?;
    if dirs.len() < 3 {
        return Err(AppError::Config(format!(
            "远端目录解析异常，期望 3 个目录，实际 {} 个",
            dirs.len()
        )));
    }
    let (claude_dir, codex_dirs) = (dirs[0].clone(), dirs[1..].to_vec());
    let files = classify_usage_entries(io.list_files(&dirs)?, &claude_dir, &codex_dirs);

    let prefix = format!("ssh:{host_id}/");
    let cursors: HashMap<String, SyncCursor> = load_sync_cursors(db)?
        .into_iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .collect();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_floor_ms = now_ms.saturating_sub(REMOTE_USAGE_MTIME_WINDOW_MS);

    let mut pending = 0usize;
    let mut pending_bytes = 0u64;
    for entry in files.claude.iter().chain(files.codex.iter()) {
        // 窗口外 / 超限文件与引擎同门跳过，不计入待同步
        if entry.mtime_ms < window_floor_ms || entry.size > REMOTE_USAGE_MAX_FILE_BYTES {
            continue;
        }
        let key = remote_cursor_key(host_id, &entry.path);
        let cursor = cursors.get(&key);
        if !file_needs_fetch(entry, cursor) {
            continue;
        }
        pending += 1;
        // 增量文件只算新增段；首扫候选按全量（zst 按压缩 size）
        pending_bytes += match cursor {
            Some(cursor) => entry
                .size
                .saturating_sub(cursor.last_byte_offset.unwrap_or(0).max(0) as u64),
            None => entry.size,
        };
    }

    Ok(RemoteUsageSyncStatus {
        done_files: 0,
        total_files: pending,
        done_bytes: pending_bytes,
        skipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    // SYNC_CANCELLED 是模块级全局：消费测试会翻转它，整个测试模块串行
    // 防并行用例在窗口内读到误取消
    use serial_test::serial;

    /* ===== 内存假远端 ===== */

    struct FakeFile {
        bytes: Vec<u8>,
        mtime_ms: i64,
    }

    /// 内存假远端：清单 / 区间 / 全文三原语的确定性实现，记录每次拉取
    /// 供「零拉取 / 增量区间」断言。
    struct FakeRemote {
        files: BTreeMap<String, FakeFile>,
        range_calls: Vec<(String, u64, u64)>,
        full_calls: Vec<String>,
    }

    impl FakeRemote {
        fn new() -> Self {
            Self {
                files: BTreeMap::new(),
                range_calls: Vec::new(),
                full_calls: Vec::new(),
            }
        }

        fn upsert(&mut self, path: &str, bytes: &[u8], mtime_ms: i64) {
            self.files.insert(
                path.to_string(),
                FakeFile {
                    bytes: bytes.to_vec(),
                    mtime_ms,
                },
            );
        }

        fn remove(&mut self, path: &str) {
            self.files.remove(path);
        }

        fn append(&mut self, path: &str, bytes: &[u8], mtime_ms: i64) {
            let file = self.files.get_mut(path).expect("file exists");
            file.bytes.extend_from_slice(bytes);
            file.mtime_ms = mtime_ms;
        }

        fn range_calls_for(&self, path: &str) -> Vec<(u64, u64)> {
            self.range_calls
                .iter()
                .filter(|(p, _, _)| p == path)
                .map(|(_, off, len)| (*off, *len))
                .collect()
        }
    }

    impl RemoteUsageIo for FakeRemote {
        fn home_dirs(&mut self) -> Result<Vec<String>, AppError> {
            Ok(vec![
                "/home/u/.claude/projects".to_string(),
                "/home/u/.codex/sessions".to_string(),
                "/home/u/.codex/archived_sessions".to_string(),
            ])
        }

        fn list_files(&mut self, dirs: &[String]) -> Result<Vec<RemoteFileEntry>, AppError> {
            let mut entries = Vec::new();
            for (path, file) in &self.files {
                if !dirs.iter().any(|dir| path.starts_with(dir.as_str())) {
                    continue;
                }
                entries.push(RemoteFileEntry {
                    path: path.clone(),
                    size: file.bytes.len() as u64,
                    mtime_ms: file.mtime_ms,
                });
            }
            Ok(entries)
        }

        fn read_range(&mut self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, AppError> {
            self.range_calls.push((path.to_string(), offset, len));
            let file = self
                .files
                .get(path)
                .ok_or_else(|| AppError::Config(format!("假远端缺文件: {path}")))?;
            let start = (offset as usize).min(file.bytes.len());
            let end = start.saturating_add(len as usize).min(file.bytes.len());
            Ok(file.bytes[start..end].to_vec())
        }

        fn read_full(&mut self, path: &str) -> Result<Vec<u8>, AppError> {
            self.full_calls.push(path.to_string());
            self.files
                .get(path)
                .map(|file| file.bytes.clone())
                .ok_or_else(|| AppError::Config(format!("假远端缺文件: {path}")))
        }
    }

    /* ===== fixture ===== */

    const HOST_ID: &str = "h1";

    /// 新鲜文件的 mtime 基准（一分钟前）：窗口判定对比真实时钟，固定常数
    /// 会落进「未来」或「远古」造成判定漂移
    fn fresh_mtime() -> i64 {
        chrono::Utc::now().timestamp_millis() - 60_000
    }

    fn claude_path(name: &str) -> String {
        format!("/home/u/.claude/projects/p1/{name}.jsonl")
    }

    /// 无 usage 的 user 消息填充行（把文件撑过链回看窗口，让增量区间可区分）
    fn claude_filler_line(len: usize) -> String {
        let mut line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "x" },
            "sessionId": "session-x",
            "timestamp": "2026-11-08T09:00:00Z"
        })
        .to_string();
        while line.len() < len {
            line.push('x');
        }
        line
    }

    fn claude_line(msg_id: &str, output: u32) -> String {
        format!("{}\n", claude_usage_line(msg_id, output))
    }

    fn claude_usage_line(msg_id: &str, output: u32) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": msg_id,
                "model": "claude-opus-4-8",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": output,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                },
                "stop_reason": "end_turn"
            },
            "timestamp": "2026-11-08T10:00:00Z",
            "sessionId": "session-x"
        })
        .to_string()
    }

    fn codex_path(name: &str) -> String {
        format!("/home/u/.codex/sessions/2026/11/08/{name}.jsonl")
    }

    const CODEX_THREAD_ID: &str = "00000000-0000-4000-8000-0000000000aa";

    fn codex_meta_line() -> String {
        serde_json::json!({
            "timestamp": "2026-11-08T10:00:00Z",
            "type": "session_meta",
            "payload": { "id": CODEX_THREAD_ID, "cwd": "/tmp" }
        })
        .to_string()
    }

    fn codex_token_count_line(input: u64, output: u64) -> String {
        serde_json::json!({
            "timestamp": "2026-11-08T10:00:05Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": 0,
                        "output_tokens": output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": input + output
                    }
                }
            }
        })
        .to_string()
    }

    fn cursor_of(db: &Database, key: &str) -> Option<SyncCursor> {
        load_sync_cursors(db).unwrap().remove(key)
    }

    fn rows_with_source(db: &Database, data_source: &str) -> Vec<(String, i64)> {
        let conn = db
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT request_id, output_tokens FROM proxy_request_logs
                 WHERE data_source = ?1 ORDER BY request_id",
            )
            .unwrap();
        stmt.query_map([data_source], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn sync(db: &Database, io: &mut FakeRemote, force: bool) -> RemoteUsageSyncStatus {
        let cancel = AtomicBool::new(false);
        let mut progress_calls = Vec::new();
        let status = sync_host_usage_with_io(
            db,
            HOST_ID,
            io,
            force,
            &mut |s| progress_calls.push(s.clone()),
            &cancel,
        )
        .expect("同步应成功");
        if status.total_files > 0 {
            assert!(!progress_calls.is_empty(), "每文件完成后应回调进度");
            assert_eq!(
                progress_calls.last().expect("有文件必有回调"),
                &status,
                "最后一次进度回调应等于最终状态"
            );
        }
        status
    }

    /* ===== 清单 diff 三态 ===== */

    // 无变化：首扫建游标后，第二次同步零区间拉取（清单 diff 短路）
    #[test]
    #[serial]
    fn manifest_diff_unchanged_fetches_nothing() {
        let db = Database::memory().unwrap();
        let path = claude_path("a");
        let mut io = FakeRemote::new();
        io.upsert(&path, claude_line("msg_a1", 5).as_bytes(), fresh_mtime());

        let first = sync(&db, &mut io, true);
        assert_eq!(first.done_files, 1);
        assert_eq!(first.skipped, 0);
        assert!(first.done_bytes > 0, "首扫应有拉取字节");
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1);

        let before = io.range_calls.len();
        let second = sync(&db, &mut io, false);
        assert_eq!(second.done_files, 1, "无变化文件仍计入 doneFiles");
        assert_eq!(second.done_bytes, 0, "无变化零拉取");
        assert_eq!(io.range_calls.len(), before, "不应发生任何区间拉取");
    }

    // size 增长：从游标边界增量拉取——区间起点 = committed 回看窗口（256KB），
    // 前置填充行把文件撑过窗口长度，让增量区间与「全文件重拉」可区分
    #[test]
    #[serial]
    fn manifest_diff_growth_fetches_incremental_segment() {
        let db = Database::memory().unwrap();
        let path = claude_path("b");
        let filler = claude_filler_line(300 * 1024);
        let line1 = claude_line("msg_b1", 5);
        let mut io = FakeRemote::new();
        io.upsert(
            &path,
            format!("{filler}\n{line1}").as_bytes(),
            fresh_mtime(),
        );
        sync(&db, &mut io, true);
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1);

        // 追加一条 usage 行，mtime 前进
        let line2 = claude_line("msg_b2", 7);
        io.append(&path, line2.as_bytes(), fresh_mtime() + 5_000);

        let status = sync(&db, &mut io, false);
        assert_eq!(status.skipped, 0);
        let ranges = io.range_calls_for(&path);
        let (last_off, last_len) = *ranges.last().expect("增量应发起区间拉取");
        let full_len = (filler.len() + 1 + line1.len() + line2.len()) as u64;
        assert!(last_off > 0, "增量区间起点应在回看窗口处而非文件头");
        assert!(
            last_len < full_len,
            "增量区间（回看窗口 + 新段）应小于全文件：{last_len} vs {full_len}"
        );
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 2, "增量行应入库");
    }

    // 新文件（spec §4.4 数据流第 1 条）：已同步主机的清单中出现新增文件时，
    // 轻同步（force=false）自动拉取入库并建游标——统计不再静默停留
    #[test]
    #[serial]
    fn new_file_is_auto_ingested_in_light_sync() {
        let db = Database::memory().unwrap();
        let path_a = claude_path("c-a");
        let path_b = claude_path("c-b");
        let mut io = FakeRemote::new();
        io.upsert(&path_a, claude_line("msg_c1", 5).as_bytes(), fresh_mtime());
        sync(&db, &mut io, true);
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1);

        // 已同步主机清单中出现新增文件：自动刷新 / 手动增量（均轻同步）覆盖
        io.upsert(
            &path_b,
            claude_line("msg_c2", 6).as_bytes(),
            fresh_mtime() + 1_000,
        );
        let light = sync(&db, &mut io, false);
        assert!(light.done_bytes > 0, "轻同步应拉取新增文件");
        assert_eq!(light.skipped, 0);
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 2, "新增文件应入库");
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &path_b)).is_some(),
            "轻同步为新文件建游标"
        );
    }

    // 单轮自动拉取上限：21 个无游标文件轻同步只拉 20 个，超限的保持
    // pending（skipped 计数 + 无游标），后续轮次限额释放后继续消化
    #[test]
    #[serial]
    fn light_sync_caps_new_files_per_round() {
        let db = Database::memory().unwrap();
        let mut io = FakeRemote::new();
        let mut paths = Vec::new();
        for i in 0..=REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT {
            let path = claude_path(&format!("cap-{i:02}"));
            io.upsert(
                &path,
                claude_line(&format!("msg_cap_{i}"), 5).as_bytes(),
                fresh_mtime(),
            );
            paths.push(path);
        }

        let status = sync(&db, &mut io, false);
        assert_eq!(status.skipped, 1, "超出单轮上限的新文件计入 skipped");
        assert_eq!(
            rows_with_source(&db, "ssh:h1").len(),
            REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT,
            "本轮只拉取上限个数的新文件"
        );
        let pending = &paths[REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT];
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, pending)).is_none(),
            "超限文件保持无游标（pending），等待后续轮次"
        );

        // 下一轮轻同步：已建游标的走 unchanged，剩余 pending 在限额内消化
        let next = sync(&db, &mut io, false);
        assert_eq!(next.skipped, 0);
        assert_eq!(
            rows_with_source(&db, "ssh:h1").len(),
            REMOTE_LIGHT_SYNC_NEW_FILE_LIMIT + 1
        );
    }

    // 单轮字节上限：待纳入新文件的累计字节超过 50MB 后，后续新文件本轮
    // 不拉（防异常大目录把每次自动刷新变成全量首扫）
    #[test]
    #[serial]
    fn light_sync_caps_new_file_bytes_per_round() {
        let db = Database::memory().unwrap();
        let mut io = FakeRemote::new();
        // 两个 (LIMIT/2 + 1)B 文件：第一个纳入后，第二个累计超 50MB 被挡
        let half = (REMOTE_LIGHT_SYNC_NEW_BYTES_LIMIT / 2 + 1) as usize;
        io.upsert(&claude_path("big-1"), &vec![0u8; half], fresh_mtime());
        io.upsert(&claude_path("big-2"), &vec![0u8; half], fresh_mtime());

        let status = sync(&db, &mut io, false);
        assert_eq!(status.skipped, 1, "累计字节超限的新文件计入 skipped");
        assert_eq!(status.done_bytes, half as u64, "只拉了第一个大文件");
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &claude_path("big-2"))).is_none(),
            "字节超限文件保持 pending"
        );
    }

    // 32MB 上限对轻同步同样生效：超限新文件零拉取、不建游标（与 force
    // 首扫同门，防单轮自动刷新被异常大文件拖垮）
    #[test]
    #[serial]
    fn light_sync_skips_oversize_new_file() {
        let db = Database::memory().unwrap();
        let path = claude_path("light-big");
        let mut io = FakeRemote::new();
        io.upsert(
            &path,
            &vec![0u8; (REMOTE_USAGE_MAX_FILE_BYTES + 1) as usize],
            fresh_mtime(),
        );

        let status = sync(&db, &mut io, false);
        assert_eq!(status.skipped, 1);
        assert_eq!(status.done_bytes, 0);
        assert!(io.range_calls.is_empty(), "超限文件不应发起拉取");
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &path)).is_none(),
            "超限文件不建游标"
        );
    }

    /* ===== 消失态 ===== */

    // 文件从清单消失：游标删除，已入库数据保留
    #[test]
    #[serial]
    fn vanished_file_deletes_cursor_but_keeps_rows() {
        let db = Database::memory().unwrap();
        let path = claude_path("d");
        let mut io = FakeRemote::new();
        io.upsert(&path, claude_line("msg_d1", 5).as_bytes(), fresh_mtime());
        sync(&db, &mut io, true);
        let key = remote_cursor_key(HOST_ID, &path);
        assert!(cursor_of(&db, &key).is_some());

        io.remove(&path);
        let status = sync(&db, &mut io, false);
        assert_eq!(status.done_files, 0, "清单为空");
        assert!(cursor_of(&db, &key).is_none(), "消失文件的游标应被删除");
        assert_eq!(
            rows_with_source(&db, "ssh:h1").len(),
            1,
            "已入库数据必须保留"
        );
    }

    /* ===== 90 天窗口 ===== */

    // 旧 mtime 文件：不拉取、不建游标、计 skipped
    #[test]
    #[serial]
    fn stale_mtime_file_is_never_synced() {
        let db = Database::memory().unwrap();
        let path = claude_path("old");
        let mut io = FakeRemote::new();
        io.upsert(
            &path,
            claude_line("msg_old", 5).as_bytes(),
            fresh_mtime() - REMOTE_USAGE_MTIME_WINDOW_MS - 1,
        );

        let status = sync(&db, &mut io, true);
        assert_eq!(status.skipped, 1);
        assert_eq!(status.done_bytes, 0);
        assert!(io.range_calls.is_empty(), "窗口外文件不应拉取");
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &path)).is_none(),
            "窗口外文件不建游标"
        );
    }

    /* ===== zst 全量路径 ===== */

    // zst 归档：read_full + 本地解压 + 全量解析入库，游标记压缩 size
    #[test]
    #[serial]
    fn zst_archive_uses_full_read() {
        let db = Database::memory().unwrap();
        let path =
            format!("/home/u/.codex/sessions/2026/11/08/rollout-{CODEX_THREAD_ID}.jsonl.zst");
        let plain = format!(
            "{}\n{}\n",
            codex_meta_line(),
            codex_token_count_line(100, 40)
        );
        let compressed = zstd::stream::encode_all(plain.as_bytes(), 3).unwrap();

        let mut io = FakeRemote::new();
        io.upsert(&path, &compressed, fresh_mtime());

        let status = sync(&db, &mut io, true);
        assert_eq!(status.skipped, 0, "zst 文件应正常处理: {:?}", status);
        assert_eq!(io.full_calls, vec![path.clone()], "zst 走 read_full");
        let rows = rows_with_source(&db, "ssh:h1");
        assert_eq!(rows.len(), 1, "token_count 事件应入库: {rows:?}");

        // 游标 last_byte_offset = 压缩 size（diff 基准），再同步零拉取
        let key = remote_cursor_key(HOST_ID, &path);
        let cursor = cursor_of(&db, &key).expect("zst 同步后应有游标");
        assert_eq!(cursor.last_byte_offset, Some(compressed.len() as i64));
        let again = sync(&db, &mut io, false);
        assert_eq!(again.done_bytes, 0, "zst 终态后零拉取");
        assert_eq!(io.full_calls.len(), 1);
    }

    /* ===== 32MB 上限 ===== */

    // 超过 32MB 的文件：skipped+1，不拉取
    #[test]
    #[serial]
    fn oversize_file_is_skipped() {
        let db = Database::memory().unwrap();
        let path = claude_path("big");
        let mut io = FakeRemote::new();
        // 清单 size = bytes.len()：直接分配超限字节（一次分配可接受），
        // 引擎在拉取前就按清单 size 拦截，字节不会真的传回
        io.upsert(
            &path,
            &vec![0u8; (REMOTE_USAGE_MAX_FILE_BYTES + 1) as usize],
            fresh_mtime(),
        );

        let status = sync(&db, &mut io, true);
        assert_eq!(status.skipped, 1);
        assert_eq!(status.done_bytes, 0);
        assert!(io.range_calls.is_empty(), "超限文件不应发起拉取");
    }

    /* ===== 取消 ===== */

    // 中途置位取消：立即停止，已处理文件的游标保留
    #[test]
    #[serial]
    fn cancel_midway_stops_with_partial_progress() {
        let db = Database::memory().unwrap();
        let path_a = claude_path("ca");
        let path_b = claude_path("cb");
        let mut io = FakeRemote::new();
        io.upsert(&path_a, claude_line("msg_ca", 5).as_bytes(), fresh_mtime());
        io.upsert(&path_b, claude_line("msg_cb", 5).as_bytes(), fresh_mtime());

        let cancel = AtomicBool::new(false);
        let mut seen_files = 0usize;
        let status = sync_host_usage_with_io(
            &db,
            HOST_ID,
            &mut io,
            true,
            &mut |_status| {
                // 第一个文件处理完的进度回调里置位取消
                seen_files += 1;
                if seen_files == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .expect("取消不是错误");

        assert_eq!(status.done_files, 1, "只处理了第一个文件");
        assert_eq!(status.total_files, 2);
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1, "第二个文件未入库");
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &path_a)).is_some(),
            "已处理文件的游标保留"
        );
        assert!(
            cursor_of(&db, &remote_cursor_key(HOST_ID, &path_b)).is_none(),
            "未处理文件无游标，后续可续扫"
        );
    }

    /* ===== 幂等 ===== */

    // 同文件同字节二次 force 同步：request_id 主键 + INSERT OR IGNORE 不重复计数
    #[test]
    #[serial]
    fn resync_same_bytes_is_idempotent() {
        let db = Database::memory().unwrap();
        let path = claude_path("idem");
        let mut io = FakeRemote::new();
        io.upsert(&path, claude_line("msg_idem", 5).as_bytes(), fresh_mtime());

        sync(&db, &mut io, true);
        // mtime 抬高绕过 unchanged 短路，验证幂等靠主键而非清单 diff
        io.files.get_mut(&path).unwrap().mtime_ms = fresh_mtime() + 9_000;
        sync(&db, &mut io, true);

        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1, "不重复计数");
    }

    /* ===== codex 增量 ===== */

    // codex 未压缩：追加 token_count → 增量拉取新段，parser_state 接力不重放
    #[test]
    #[serial]
    fn codex_increment_appends_events() {
        let db = Database::memory().unwrap();
        let path = codex_path(&format!("rollout-{CODEX_THREAD_ID}"));
        let part1 = format!(
            "{}\n{}\n",
            codex_meta_line(),
            codex_token_count_line(100, 40)
        );
        let mut io = FakeRemote::new();
        io.upsert(&path, part1.as_bytes(), fresh_mtime());
        sync(&db, &mut io, true);
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1);

        // 追加第二个 token_count（累计快照，delta = 与高水位之差）
        let part2 = format!("{}\n", codex_token_count_line(300, 90));
        io.append(&path, part2.as_bytes(), fresh_mtime() + 4_000);

        let status = sync(&db, &mut io, false);
        assert_eq!(status.skipped, 0, "增量同步应成功: {status:?}");
        let rows = rows_with_source(&db, "ssh:h1");
        assert_eq!(rows.len(), 2, "第二个事件增量入库: {rows:?}");

        // 游标推进到 committed 边界（两条完整记录后）
        let cursor = cursor_of(&db, &remote_cursor_key(HOST_ID, &path)).unwrap();
        assert_eq!(
            cursor.last_byte_offset,
            Some((part1.len() + part2.len()) as i64),
            "游标应推进到文件末"
        );
        assert!(
            cursor.parser_state.is_some(),
            "codex 跨段状态应随游标持久化"
        );
    }

    /* ===== data_source 写值 ===== */

    // 明细行的 data_source = ssh:<hostId>（rollup 分账依据）
    #[test]
    #[serial]
    fn rows_carry_ssh_data_source() {
        let db = Database::memory().unwrap();
        let path = claude_path("src");
        let mut io = FakeRemote::new();
        io.upsert(&path, claude_line("msg_src", 5).as_bytes(), fresh_mtime());
        sync(&db, &mut io, true);

        let conn = db
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source: String = conn
            .query_row(
                "SELECT data_source FROM proxy_request_logs WHERE request_id LIKE 'session:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "ssh:h1");
    }

    /* ===== peek ===== */

    // peek：统计待同步文件数与待拉字节，不拉取不写库
    #[test]
    #[serial]
    fn peek_counts_pending_without_fetching() {
        let db = Database::memory().unwrap();
        let path = claude_path("pk1");
        let other = claude_path("pk2");
        let mut io = FakeRemote::new();
        let body = claude_line("msg_pk1", 5);
        io.upsert(&path, body.as_bytes(), fresh_mtime());
        io.upsert(&other, claude_line("msg_pk2", 6).as_bytes(), fresh_mtime());

        // 全新状态：两个文件都是首扫候选，待拉字节 = 全量
        let before = peek_host_usage_with_io(&db, HOST_ID, &mut io).unwrap();
        assert_eq!(before.total_files, 2);
        assert!(before.done_bytes >= (body.len() as u64), "首扫候选按全量计");
        assert!(
            io.range_calls.is_empty() && io.full_calls.is_empty(),
            "peek 不拉取"
        );

        // force 同步后：无变化 → 零待同步
        sync(&db, &mut io, true);
        let after = peek_host_usage_with_io(&db, HOST_ID, &mut io).unwrap();
        assert_eq!(after.total_files, 0, "同步完成后无待同步");
        assert_eq!(after.done_bytes, 0);
    }

    // 取消标志是一次性消费（Minor-1）：观察到取消停止后标志复位，
    // 下一轮轻同步不被残留 true 拦在首文件前
    #[test]
    #[serial]
    fn cancelled_flag_is_consumed_after_sync_stops() {
        let db = Database::memory().unwrap();
        let path = claude_path("cancel-flag");
        let mut io = FakeRemote::new();
        io.upsert(&path, claude_line("msg_cf", 5).as_bytes(), fresh_mtime());

        SYNC_CANCELLED.store(true, Ordering::Relaxed);
        let cancel = AtomicBool::new(false);
        let first = sync_host_usage_with_io(&db, HOST_ID, &mut io, true, &mut |_| {}, &cancel)
            .expect("取消不是错误");
        assert_eq!(first.done_files, 0, "取消在首文件前生效");
        // 引擎退出时已消费复位：无需手动 reset，下一轮正常处理
        assert!(!SYNC_CANCELLED.load(Ordering::Relaxed));
        let second = sync_host_usage_with_io(&db, HOST_ID, &mut io, true, &mut |_| {}, &cancel)
            .expect("残留标志不应拦截下一轮");
        assert_eq!(second.done_files, 1, "下一轮应正常处理文件");
        assert_eq!(rows_with_source(&db, "ssh:h1").len(), 1);
    }
}
