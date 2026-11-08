pub mod cache;
pub mod content;
#[cfg(windows)]
pub(crate) mod hermes_wsl;
pub mod model;
pub mod providers;
pub mod search;
mod search_revision;
pub mod ssh;
#[cfg(all(test, unix))]
mod ssh_localtest;
pub mod terminal;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

// 其余模型类型从 `session_manager::model` 引用
pub use model::SessionMessage;
use providers::{
    agy, claude, codex, dsh, gemini, grokbuild, hermes, kimi, mcode, omp, openclaw, opencode, pi,
    utils::FileParseCache,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionRequest {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionOutcome {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let scan = scan_sessions_with_status();
    let sessions = scan.sessions.clone();
    *RECENT_SCAN.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), scan));
    sessions
}

// The index request follows list_sessions. Reuse that scan once, including its
// completeness flags; enabling from a cached page or a queued rerun scans afresh.
static RECENT_SCAN: LazyLock<Mutex<Option<(Instant, SessionScan)>>> =
    LazyLock::new(|| Mutex::new(None));
const SCAN_REUSE_WINDOW: Duration = Duration::from_secs(5);

pub(crate) fn scan_for_index() -> SessionScan {
    take_recent_scan(&RECENT_SCAN, Instant::now()).unwrap_or_else(scan_sessions_with_status)
}

fn take_recent_scan(
    slot: &Mutex<Option<(Instant, SessionScan)>>,
    now: Instant,
) -> Option<SessionScan> {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .filter(|(created, _)| now.saturating_duration_since(*created) <= SCAN_REUSE_WINDOW)
        .map(|(_, scan)| scan)
}

pub(crate) struct SessionScan {
    pub sessions: Vec<SessionMeta>,
    pub complete_providers: HashSet<String>,
}

pub(crate) fn scan_sessions_with_status() -> SessionScan {
    use providers::utils::ScanStatus;
    type Scanner = fn(&mut ScanStatus) -> Vec<SessionMeta>;
    let scanners: [(&str, Scanner); 13] = [
        ("codex", codex::scan_sessions_with_status),
        ("claude", claude::scan_sessions_with_status),
        ("opencode", opencode::scan_sessions_with_status),
        ("openclaw", openclaw::scan_sessions_with_status),
        ("gemini", gemini::scan_sessions_with_status),
        ("hermes", hermes::scan_sessions_with_status),
        ("grokbuild", grokbuild::scan_sessions_with_status),
        ("pi", pi::scan_sessions_with_status),
        ("omp", omp::scan_sessions_with_status),
        ("mcode", mcode::scan_sessions_with_status),
        // Kimi Code、DeepSeek Harness、Antigravity 的扫描还不报告读取失败，按完整处理
        ("kimi", |_| kimi::scan_sessions()),
        ("dsh", |_| dsh::scan_sessions()),
        ("agy", |_| agy::scan_sessions()),
    ];
    let mut scan = SessionScan {
        sessions: Vec::new(),
        complete_providers: HashSet::new(),
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = scanners
            .into_iter()
            .map(|(provider, scanner)| {
                (
                    provider,
                    scope.spawn(move || {
                        let mut status = ScanStatus::default();
                        let sessions = scanner(&mut status);
                        (sessions, status)
                    }),
                )
            })
            .collect();
        for (provider, handle) in handles {
            // A failed worker is not evidence that all its sessions were deleted.
            if let Ok((sessions, status)) = handle.join() {
                scan.sessions.extend(sessions);
                if !status.incomplete {
                    scan.complete_providers.insert(provider.to_string());
                }
            }
        }
    });
    scan.sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });
    scan
}

/* ===== SSH 远程扫描链路（spec §5.4）===== */

/// 远端目录解析命令超时（连接建立 + printf，亚秒级完成）；搜索链路的
/// home_dirs 往返同口径复用
pub(crate) const REMOTE_DIRS_TIMEOUT: Duration = Duration::from_secs(15);
/// 远端清单命令超时（大目录 `find` 遍历较慢，放宽）
const REMOTE_LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// 单批头尾拉取超时
const REMOTE_BATCH_TIMEOUT: Duration = Duration::from_secs(60);
/// 头尾拉取批大小上限（spec §5.4：单批 ≤ 50 个文件）
const REMOTE_HEAD_TAIL_BATCH: usize = 50;
/// 远端删除命令超时（单文件 rm 亚秒级，留足连接建立余量）
const REMOTE_DELETE_TIMEOUT: Duration = Duration::from_secs(30);

/// 远程解析缓存：按 hostId 隔离。不同主机的远端绝对路径可能完全相同
/// （都是 `/root/...`），共用实例会跨主机串缓存；与各 provider 的本地
/// `PARSE_CACHE` 也互不共享——本地扫描每轮整表重建，会驱逐远程条目。
static REMOTE_PARSE_CACHES: LazyLock<Mutex<HashMap<String, Arc<FileParseCache>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取（或首次创建）某主机的远程解析缓存；Arc 克隆后立刻释放全局锁，
/// 并发的多主机扫描互不阻塞。
fn remote_parse_cache(host_id: &str) -> Arc<FileParseCache> {
    REMOTE_PARSE_CACHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(host_id.to_string())
        .or_insert_with(|| Arc::new(FileParseCache::new()))
        .clone()
}

/// 扫描 SSH 远程主机的 claude / codex 会话（spec §5.4）：
///
/// 1. 一条命令解析远端 HOME 下的三个会话根目录
/// 2. `find` 清单 → 按 provider 归类 → `.jsonl`/`.jsonl.zst` 共存判定
/// 3. 两阶段增量：`(mtime, len)` 命中缓存的文件直接复用（零 ssh 拉取），
///    仅为 miss 文件分批（≤50）头尾帧协议拉取 → 单次 `scan_remote` 解析
///    （claude/codex 共表回填，不互相驱逐）→ 覆盖远程标识字段
/// 4. 排序 + 写入 `SshScanRegistry` 清单（validate / fingerprint / 删除共用）
///
/// 任一 ssh 命令失败整体报错；成功即 claude 与 codex 双 provider 完整。
pub fn scan_remote_sessions(host: &crate::settings::SshHostConfig) -> Result<SessionScan, String> {
    // ── 1. 远端 HOME 下的三个会话根目录 ──
    let dirs_raw = ssh::run_remote(host, &ssh::home_dirs_command(), REMOTE_DIRS_TIMEOUT)
        .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;
    let dirs = ssh::parse_home_dirs(&dirs_raw);
    if dirs.len() < 3 {
        return Err(format!(
            "SSH 主机「{}」: 远端目录解析异常，期望 3 个目录，实际 {} 个",
            host.label,
            dirs.len()
        ));
    }
    let (claude_dir, codex_dirs) = (dirs[0].clone(), dirs[1..].to_vec());

    // ── 2. 会话文件清单 → 归类 + 共存判定 ──
    let list_raw = ssh::run_remote(host, &ssh::list_files_command(&dirs), REMOTE_LIST_TIMEOUT)
        .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;
    let mut claude_files = Vec::new();
    let mut codex_files = Vec::new();
    for entry in ssh::parse_list_output(&list_raw) {
        // 帧协议按行分帧：含换行的远端路径无法安全入批，fail-closed 跳过
        if entry.path.contains('\n') {
            log::warn!("跳过含换行的远端路径: {:?}", entry.path);
            continue;
        }
        match ssh::classify_remote_provider(&entry.path, &claude_dir, &codex_dirs) {
            // claude 本地扫描只收 .jsonl（agent/journal 等已在远端无对应形态）
            Some("claude") if entry.path.ends_with(".jsonl") => claude_files.push(entry),
            Some("codex") => codex_files.push(entry),
            _ => {}
        }
    }
    let codex_files = ssh::prefer_plain_entries(codex_files);

    // ── 3. 两阶段增量：命中集划分 → 仅为 miss 拉取 → 单次扫描 ──
    // claude 与 codex 必须共用一次 scan_remote：缓存表每轮按本次 files 整表
    // 重建，拆成两次调用会互相驱逐对方的条目，永远 100% miss
    let cache = remote_parse_cache(&host.id);
    let cache_files: Vec<(PathBuf, Option<SystemTime>, u64)> = claude_files
        .iter()
        .chain(codex_files.iter())
        .map(remote_cache_file)
        .collect();
    let fresh = cache.cached_fresh(&cache_files);
    let requests = build_missing_requests(&claude_files, &codex_files, &fresh);
    let data = fetch_head_tail_batches(host, &requests)?;

    // 缓存键经 PathBuf → to_string_lossy 往返（Windows 会翻转斜杠），解析闭包
    // 用同构键查条目；远程标识字段始终取条目里的原始远端字符串
    let claude_by_key: HashMap<String, &ssh::RemoteFileEntry> = claude_files
        .iter()
        .map(|entry| (Path::new(&entry.path).to_string_lossy().into_owned(), entry))
        .collect();
    let codex_by_key: HashMap<String, &ssh::RemoteFileEntry> = codex_files
        .iter()
        .map(|entry| (Path::new(&entry.path).to_string_lossy().into_owned(), entry))
        .collect();
    let mut sessions = cache.scan_remote(cache_files, |path: &Path| {
        let key = path.to_string_lossy();
        if let Some(entry) = claude_by_key.get(key.as_ref()) {
            return parse_claude_entry(path, entry, &data, host);
        }
        if let Some(entry) = codex_by_key.get(key.as_ref()) {
            return parse_codex_entry(path, entry, &data, host);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "清单条目缺失",
        ))
    });

    sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    // ── 4. 清单登记（validate / fingerprint / 删除按路径查询）──
    let manifest = claude_files
        .iter()
        .chain(codex_files.iter())
        .map(|entry| {
            (
                entry.path.clone(),
                ssh::RemoteFileMeta {
                    len: entry.size,
                    mtime_ms: entry.mtime_ms,
                },
            )
        })
        .collect();
    ssh::SshScanRegistry::global().replace_manifest(&host.id, manifest);

    Ok(SessionScan {
        sessions,
        complete_providers: HashSet::from(["claude".to_string(), "codex".to_string()]),
    })
}

/// 清单条目 → 解析缓存文件键：`(PathBuf, mtime SystemTime, len)`。
/// 负 mtime 无法表达为 `SystemTime`，归 `None`（每轮恒定，缓存键仍稳定）。
fn remote_cache_file(entry: &ssh::RemoteFileEntry) -> (PathBuf, Option<SystemTime>, u64) {
    (
        PathBuf::from(&entry.path),
        ssh::mtime_to_system_time(entry.mtime_ms),
        entry.size,
    )
}

/// fetch 决策（两阶段增量的第一阶段）：剔除 `(mtime, len)` 仍命中缓存的
/// 文件（`fresh` 来自 [`FileParseCache::cached_fresh`]），只为 miss 的文件
/// 构造头尾拉取请求。命中轮输出空集 → 不发起任何 ssh 拉取（spec §5.4）。
fn build_missing_requests(
    claude_files: &[ssh::RemoteFileEntry],
    codex_files: &[ssh::RemoteFileEntry],
    fresh: &HashSet<PathBuf>,
) -> Vec<ssh::HeadTailRequest> {
    let mut requests = Vec::new();
    let push_missing = |entries: &[ssh::RemoteFileEntry],
                        provider: &str,
                        requests: &mut Vec<ssh::HeadTailRequest>| {
        for entry in entries {
            if !fresh.contains(Path::new(&entry.path)) {
                requests.push(ssh::remote_head_tail_request(provider, entry));
            }
        }
    };
    push_missing(claude_files, "claude", &mut requests);
    push_missing(codex_files, "codex", &mut requests);
    requests
}

/// claude 单条远程解析：head/tail 行 + 64 KiB 字节尾窗（relocated 记录在
/// 末尾窗口内）→ [`claude::parse_session_lines`] → 覆盖远程标识字段。
/// 头尾数据缺失按读取失败处理：Err 不进缓存、下一轮重试。
fn parse_claude_entry(
    path: &Path,
    entry: &ssh::RemoteFileEntry,
    data: &HashMap<String, ssh::HeadTailData>,
    host: &crate::settings::SshHostConfig,
) -> std::io::Result<Option<SessionMeta>> {
    let Some(file) = data.get(&entry.path) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "头尾数据缺失",
        ));
    };
    let relocated = file
        .tail_bytes
        .as_deref()
        .and_then(|window| claude::parse_relocated_from_tail_bytes(window, entry.size));
    Ok(
        claude::parse_session_lines(path, file.head.clone(), file.tail.clone(), relocated)
            .map(|meta| ssh::rebind_remote_meta(meta, host, &entry.path)),
    )
}

/// codex 单条远程解析：未压缩走 head/tail 行；压缩形态用 zst 前缀解出的
/// 头部行，最后活跃时间以清单 mtime 兜底（负 mtime 归 `None`，对齐本地
/// `SystemTime` 语义）；线程标题走既有 fallback（远端 state.db 不可读）。
fn parse_codex_entry(
    path: &Path,
    entry: &ssh::RemoteFileEntry,
    data: &HashMap<String, ssh::HeadTailData>,
    host: &crate::settings::SshHostConfig,
) -> std::io::Result<Option<SessionMeta>> {
    let Some(file) = data.get(&entry.path) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "头尾数据缺失",
        ));
    };
    let lines = if entry.path.ends_with(".jsonl.zst") {
        let prefix = file
            .tail_bytes
            .as_deref()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "压缩前缀缺失"))?;
        let text = ssh::decode_zst_prefix(prefix)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        codex::SessionLines {
            head: text
                .lines()
                .take(codex::HEAD_LINES)
                .map(str::to_string)
                .collect(),
            tail: Vec::new(),
            fallback_last_active: (entry.mtime_ms >= 0).then_some(entry.mtime_ms),
        }
    } else {
        codex::SessionLines {
            head: file.head.clone(),
            tail: file.tail.clone(),
            fallback_last_active: None,
        }
    };
    Ok(codex::parse_session_lines(path, lines, &HashMap::new())
        .map(|meta| ssh::rebind_remote_meta(meta, host, &entry.path)))
}

/// 分批（≤[`REMOTE_HEAD_TAIL_BATCH`]）执行头尾帧协议拉取；sentinel 每批
/// 随机生成，文件内容中出现的固定哨兵文本无法伪装成行首分界。
fn fetch_head_tail_batches(
    host: &crate::settings::SshHostConfig,
    requests: &[ssh::HeadTailRequest],
) -> Result<HashMap<String, ssh::HeadTailData>, String> {
    let mut all = HashMap::new();
    for batch in requests.chunks(REMOTE_HEAD_TAIL_BATCH) {
        let sentinel = uuid::Uuid::new_v4().simple().to_string();
        let cmd = ssh::head_tail_batch_command(batch, &sentinel);
        let out = ssh::run_remote(host, &cmd, REMOTE_BATCH_TIMEOUT)
            .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;
        let map = ssh::parse_head_tail_output(&out, &sentinel, batch)
            .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;
        all.extend(map);
    }
    Ok(all)
}

pub fn load_messages(provider_id: &str, source_path: &str) -> Result<Vec<SessionMessage>, String> {
    if provider_id == "mcode" {
        return mcode::load_messages(source_path);
    }
    // SSH 远程源：拉原始字节落临时文件，转交对应 provider 的本地解析入口
    if source_path.starts_with("ssh:") {
        return load_remote_messages(provider_id, source_path);
    }
    // SQLite sessions use a "sqlite:" prefixed source_path
    if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        return opencode::load_messages_sqlite(source_path);
    }
    if provider_id == "hermes" && source_path.starts_with("sqlite:") {
        return hermes::load_messages_sqlite(source_path);
    }

    let path = Path::new(source_path);
    match provider_id {
        "codex" => codex::load_messages(path),
        "claude" => claude::load_messages(path),
        "opencode" => opencode::load_messages(path),
        "openclaw" => openclaw::load_messages(path),
        "gemini" => gemini::load_messages(path),
        "agy" => agy::load_messages(path),
        "grokbuild" => grokbuild::load_messages(path),
        "hermes" => hermes::load_messages(path),
        "pi" => pi::load_messages(path),
        "kimi" => kimi::load_messages(path),
        "omp" => omp::load_messages(path),
        "dsh" => dsh::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

/// `ssh:` 源的正文读取（spec §5.4）：解析源 → settings 查主机 → ssh 全文
/// 拉取落临时文件 → 按 provider 走本地解析入口 → 无论成败删除临时文件。
///
/// 分支保持薄：合法性已由 `content::validate_source` 三层校验前置保证，
/// 这里仅做防御性兜底（绕过 validate 的直接调用、设置漂移时给出可读错误）。
/// 注意入参 `raw` 即完整 `ssh:` 源（`load_transcript` 的 load 闭包传
/// `load_path()` 的原样返回），不得二次拼接前缀。
fn load_remote_messages(provider_id: &str, raw: &str) -> Result<Vec<SessionMessage>, String> {
    let (host_id, remote_path) =
        ssh::parse_ssh_source(raw).ok_or_else(|| "无效的 SSH 会话源".to_string())?;
    let host = ssh_host_by_id(&host_id).ok_or_else(|| "SSH 主机不存在或已移除".to_string())?;
    let temp = ssh::fetch_to_temp_file(&host, &remote_path, content::MAX_TEXT_BYTES)
        .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;

    let parsed = match provider_id {
        "claude" => claude::load_messages(&temp),
        "codex" => codex::load_messages(&temp),
        other => Err(format!("SSH 远程会话不支持该 provider: {other}")),
    };
    // 临时文件用后即删（uuid 命名不会复用，残留仅占磁盘）；删除失败仅告警
    if let Err(e) = std::fs::remove_file(&temp) {
        log::warn!("删除 SSH 临时文件失败 {}: {e}", temp.display());
    }
    parsed
}

/// 按 id 查设置中的 SSH 主机配置（找不到返回 None，由调用方给出可读错误）
fn ssh_host_by_id(host_id: &str) -> Option<crate::settings::SshHostConfig> {
    crate::settings::get_settings()
        .ssh_hosts?
        .iter()
        .find(|host| host.id == host_id)
        .cloned()
}

/// 一次读取的结果：解析结果、是否命中缓存、耗时（含校验与指纹）
pub struct LoadedTranscript {
    pub transcript: Arc<cache::Transcript>,
    pub cached: bool,
    pub parse_ms: u64,
}

/// 校验 `source_path` 归属后读取会话，未变化的源直接用 [`cache::TranscriptCache`]。
/// 解析仍走上面的 [`load_messages`] 分发。
pub fn load_transcript(provider_id: &str, source_path: &str) -> Result<LoadedTranscript, String> {
    let started = Instant::now();
    let source = content::validate_source(provider_id, source_path)?;
    let fingerprint = cache::source_fingerprint(&source)
        .map_err(|e| format!("Failed to read session source: {e}"))?;
    let key = (source.provider_id.clone(), source.raw.clone());
    let (transcript, cached) = cache::global().get_or_load(key, fingerprint, || {
        load_messages(&source.provider_id, &source.load_path()).map(cache::Transcript::new)
    })?;
    Ok(LoadedTranscript {
        transcript,
        cached,
        parse_ms: started.elapsed().as_millis() as u64,
    })
}

/// 删除成功后同时移除该会话的解析缓存（§5.2）
pub fn delete_session(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
) -> Result<bool, String> {
    // SSH 远程源：走清单白名单 + rm 链路（缓存清理在远端分支内完成）
    if source_path.starts_with("ssh:") {
        return delete_remote_session(provider_id, session_id, source_path);
    }
    let deleted = delete_session_source(provider_id, session_id, source_path)?;
    if deleted {
        cache::global().remove(provider_id, source_path);
        search::forget_source(provider_id, source_path);
    }
    Ok(deleted)
}

fn delete_session_source(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
) -> Result<bool, String> {
    if provider_id == "dsh" {
        return Err(
            "Delete this session in DeepSeek Harness to keep its native history consistent".into(),
        );
    }
    if provider_id == "agy" {
        return Err(
            "Delete this session in Antigravity to keep its binary store and summaries consistent"
                .into(),
        );
    }
    if provider_id == "mcode" {
        return Err(
            "Delete this session in MCode so its runtime state and history are removed together"
                .into(),
        );
    }
    // SQLite sessions bypass the file-based deletion path
    if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        return opencode::delete_session_sqlite(session_id, source_path);
    }
    if provider_id == "hermes" && source_path.starts_with("sqlite:") {
        return hermes::delete_session_sqlite(session_id, source_path);
    }

    let roots = provider_roots(provider_id)?;
    delete_session_with_roots(provider_id, session_id, Path::new(source_path), &roots)
}

/// `ssh:` 源的删除（spec §5.4）：解析源 → settings 查主机 → stem 防误删校验 →
/// registry 白名单（清单未命中须先刷新）→ `rm -f` → 成功后摘清单条目并清本地
/// 解析缓存（与本地删除序列对齐；远端源不在本地索引，`forget_source` 幂等无害）。
///
/// 非叶子校验全部下沉为 `ssh.rs` 纯函数（[`ssh::session_id_matches_stem`] /
/// [`ssh::ensure_deletable`]），本函数仅做编排，不单独设测。
fn delete_remote_session(provider_id: &str, session_id: &str, raw: &str) -> Result<bool, String> {
    let (host_id, remote_path) =
        ssh::parse_ssh_source(raw).ok_or_else(|| "无效的 SSH 会话源".to_string())?;
    let host = ssh_host_by_id(&host_id).ok_or_else(|| "SSH 主机不存在或已移除".to_string())?;
    // 防误删：session_id 必须与远端文件 stem 一致（abc.jsonl / abc.jsonl.zst → abc）
    if !ssh::session_id_matches_stem(session_id, &remote_path) {
        return Err("会话 ID 与远端文件名不匹配".to_string());
    }
    // 白名单：只允许删除扫描清单确认过的路径
    ssh::ensure_deletable(ssh::SshScanRegistry::global().lookup(&host_id, &remote_path))?;
    ssh::run_remote(
        &host,
        &ssh::delete_file_command(&remote_path),
        REMOTE_DELETE_TIMEOUT,
    )
    .map_err(|e| format!("SSH 主机「{}」: {e}", host.label))?;
    // 删除成功：清单摘条目 + 本地解析缓存清理
    ssh::SshScanRegistry::global().remove_entry(&host_id, &remote_path);
    cache::global().remove(provider_id, raw);
    search::forget_source(provider_id, raw);
    Ok(true)
}

pub fn delete_sessions(requests: &[DeleteSessionRequest]) -> Vec<DeleteSessionOutcome> {
    collect_delete_session_outcomes(requests, |request| {
        delete_session(
            &request.provider_id,
            &request.session_id,
            &request.source_path,
        )
    })
}

fn delete_session_with_roots(
    provider_id: &str,
    session_id: &str,
    source_path: &Path,
    roots: &[PathBuf],
) -> Result<bool, String> {
    let source_path = content::codex_source_or_sibling(provider_id, source_path);
    let (validated_root, validated_source) =
        content::resolve_under_roots(provider_id, &source_path, roots)?;
    match provider_id {
        "codex" => codex::delete_session(&validated_root, &validated_source, session_id),
        "claude" => claude::delete_session(&validated_root, &validated_source, session_id),
        "opencode" => opencode::delete_session(&validated_root, &validated_source, session_id),
        "openclaw" => openclaw::delete_session(&validated_root, &validated_source, session_id),
        "gemini" => gemini::delete_session(&validated_root, &validated_source, session_id),
        "grokbuild" => grokbuild::delete_session(&validated_root, &validated_source, session_id),
        "hermes" => hermes::delete_session(&validated_root, &validated_source, session_id),
        "pi" => pi::delete_session(&validated_root, &validated_source, session_id),
        "omp" => omp::delete_session(&validated_root, &validated_source, session_id),
        "kimi" => kimi::delete_session(&validated_root, &validated_source, session_id),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

fn provider_roots(provider_id: &str) -> Result<Vec<PathBuf>, String> {
    let roots = match provider_id {
        "codex" => codex::session_roots(),
        "claude" => vec![crate::config::get_claude_config_dir().join("projects")],
        "opencode" => vec![opencode::get_opencode_data_dir()],
        "openclaw" => vec![crate::openclaw_config::get_openclaw_dir().join("agents")],
        "gemini" => vec![crate::gemini_config::get_gemini_dir().join("tmp")],
        "grokbuild" => grokbuild::session_roots(),
        "hermes" => vec![crate::hermes_config::get_hermes_dir().join("sessions")],
        "pi" => pi::session_roots(),
        "kimi" => kimi::session_roots(),
        "dsh" => dsh::session_roots(),
        "agy" => agy::session_roots(),
        "omp" => omp::session_roots(),
        _ => return Err(format!("Unsupported provider: {provider_id}")),
    };

    Ok(roots)
}

fn canonicalize_existing_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("{label} not found: {}", path.display()));
    }

    path.canonicalize()
        .map_err(|e| format!("Failed to resolve {label} {}: {e}", path.display()))
}

fn collect_delete_session_outcomes<F>(
    requests: &[DeleteSessionRequest],
    mut deleter: F,
) -> Vec<DeleteSessionOutcome>
where
    F: FnMut(&DeleteSessionRequest) -> Result<bool, String>,
{
    requests
        .iter()
        .map(|request| match deleter(request) {
            Ok(true) => DeleteSessionOutcome {
                provider_id: request.provider_id.clone(),
                session_id: request.session_id.clone(),
                source_path: request.source_path.clone(),
                success: true,
                error: None,
            },
            Ok(false) => DeleteSessionOutcome {
                provider_id: request.provider_id.clone(),
                session_id: request.session_id.clone(),
                source_path: request.source_path.clone(),
                success: false,
                error: Some("Session was not deleted".to_string()),
            },
            Err(error) => DeleteSessionOutcome {
                provider_id: request.provider_id.clone(),
                session_id: request.session_id.clone(),
                source_path: request.source_path.clone(),
                success: false,
                error: Some(error),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /* ===== SSH 远程扫描（Task 7）===== */

    /// 构造远端清单条目
    fn remote_entry(path: &str, size: u64, mtime_ms: i64) -> ssh::RemoteFileEntry {
        ssh::RemoteFileEntry {
            path: path.to_string(),
            size,
            mtime_ms,
        }
    }

    /// 构造最小远程主机配置（无 user/port/identity，恢复命令仅 `-t <host>`）
    fn remote_host(id: &str) -> crate::settings::SshHostConfig {
        crate::settings::SshHostConfig {
            id: id.to_string(),
            label: format!("主机 {id}"),
            host: "192.0.2.10".to_string(),
            user: None,
            port: None,
            identity_file: None,
        }
    }

    /// 测试内 zstd 压缩（单帧完整数据，模拟远端压缩前缀）
    fn zst_compress(text: &str) -> Vec<u8> {
        use std::io::Write;
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn build_missing_requests_skips_fresh_entries() {
        let claude_files = vec![remote_entry("/h/.claude/projects/p/a.jsonl", 10, 1)];
        let codex_files = vec![remote_entry("/h/.codex/sessions/2026/b.jsonl.zst", 20, 2)];

        // 全部命中缓存 → 不产生任何拉取请求（命中轮零 ssh 往返）
        let fresh: HashSet<PathBuf> = [
            PathBuf::from("/h/.claude/projects/p/a.jsonl"),
            PathBuf::from("/h/.codex/sessions/2026/b.jsonl.zst"),
        ]
        .into();
        assert!(build_missing_requests(&claude_files, &codex_files, &fresh).is_empty());

        // 全部 miss → 每文件一个请求：claude 带字节尾窗、codex 压缩走 C 段
        let reqs = build_missing_requests(&claude_files, &codex_files, &HashSet::new());
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].path, "/h/.claude/projects/p/a.jsonl");
        assert_eq!(reqs[0].tail_bytes, 64 * 1024);
        assert_eq!(reqs[1].path, "/h/.codex/sessions/2026/b.jsonl.zst");
        assert!(reqs[1].compressed);

        // 部分命中 → 只为 miss 的文件构造请求
        let partial: HashSet<PathBuf> = [PathBuf::from("/h/.claude/projects/p/a.jsonl")].into();
        let reqs = build_missing_requests(&claude_files, &codex_files, &partial);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "/h/.codex/sessions/2026/b.jsonl.zst");
    }

    /// 组装层两阶段协同：首轮填充缓存后，同 meta 的第二轮 cached_fresh 全命中，
    /// build_missing_requests 输出空集——链路在该轮不发起任何头尾拉取。
    #[test]
    fn remote_scan_second_pass_fetches_nothing() {
        let cache = FileParseCache::new();
        let files = vec![
            (
                PathBuf::from("/h/a.jsonl"),
                Some(SystemTime::UNIX_EPOCH),
                10u64,
            ),
            (
                PathBuf::from("/h/b.jsonl.zst"),
                Some(SystemTime::UNIX_EPOCH),
                20u64,
            ),
        ];
        assert!(cache.cached_fresh(&files).is_empty(), "首轮全 miss");
        cache.scan_remote(files.clone(), |_| Ok(None));

        let fresh = cache.cached_fresh(&files);
        assert_eq!(fresh.len(), 2, "同 meta 二轮全命中");
        let claude_files = vec![remote_entry("/h/a.jsonl", 10, 0)];
        let codex_files = vec![remote_entry("/h/b.jsonl.zst", 20, 0)];
        assert!(build_missing_requests(&claude_files, &codex_files, &fresh).is_empty());
    }

    #[test]
    fn parse_claude_entry_rebinds_remote_identity() {
        let entry = remote_entry(
            "/h/.claude/projects/p/sess-1.jsonl",
            70_000,
            1_700_000_000_000,
        );
        let tail_window = concat!(
            "{\"type\":\"assistant\",\"cwd\":\"/start\"}\n",
            "{\"type\":\"relocated\",\"relocatedCwd\":\"/after/cd\"}\n",
        );
        let mut data = HashMap::new();
        data.insert(
            entry.path.clone(),
            ssh::HeadTailData {
                head: vec![
                    r#"{"sessionId":"inner-id","cwd":"/start","timestamp":"2026-03-06T10:00:00Z"}"#
                        .to_string(),
                ],
                tail: vec![],
                tail_bytes: Some(tail_window.as_bytes().to_vec()),
            },
        );

        let meta = parse_claude_entry(
            Path::new(&entry.path),
            &entry,
            &data,
            &remote_host("host-1"),
        )
        .unwrap()
        .expect("应解析出会话");

        // session_id 用文件 stem 而非内容里的 id；source_path 带 ssh: 前缀；
        // resume_command 重新生成为完整可复制 ssh 命令
        assert_eq!(meta.provider_id, "claude");
        assert_eq!(meta.session_id, "sess-1");
        assert_eq!(
            meta.source_path.as_deref(),
            Some("ssh:host-1/h/.claude/projects/p/sess-1.jsonl")
        );
        assert_eq!(
            meta.resume_command.as_deref(),
            Some("ssh -t 192.0.2.10 'cd '\\''/after/cd'\\'' && claude --resume sess-1'")
        );
        // 字节尾窗里的 relocated 记录覆盖起始 cwd（size > 64KiB → 首行按半截丢弃）
        assert_eq!(meta.project_dir.as_deref(), Some("/after/cd"));
    }

    #[test]
    fn parse_codex_entry_compressed_uses_zst_prefix_and_mtime() {
        let entry = remote_entry(
            "/h/.codex/sessions/2026/rollout-b.jsonl.zst",
            4_096,
            1_700_000_123_456,
        );
        let text = concat!(
            "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"inner-b\",\"cwd\":\"/tmp/project\"}}\n",
            "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello codex\"}}\n",
        );
        let mut data = HashMap::new();
        data.insert(
            entry.path.clone(),
            ssh::HeadTailData {
                head: vec![],
                tail: vec![],
                tail_bytes: Some(zst_compress(text)),
            },
        );

        let meta = parse_codex_entry(
            Path::new(&entry.path),
            &entry,
            &data,
            &remote_host("host-2"),
        )
        .unwrap()
        .expect("应解析出会话");

        assert_eq!(meta.provider_id, "codex");
        assert_eq!(meta.session_id, "rollout-b");
        assert_eq!(
            meta.source_path.as_deref(),
            Some("ssh:host-2/h/.codex/sessions/2026/rollout-b.jsonl.zst")
        );
        assert_eq!(
            meta.resume_command.as_deref(),
            Some("ssh -t 192.0.2.10 'cd '\\''/tmp/project'\\'' && codex resume rollout-b'")
        );
        // 压缩形态无 tail 行：最后活跃时间以清单 mtime 兜底
        assert_eq!(meta.last_active_at, Some(1_700_000_123_456));
        // 标题来自解压出的首条 user message（远程无 state.db 线程标题）
        assert!(meta
            .title
            .as_deref()
            .is_some_and(|t| t.contains("hello codex")));
    }

    #[test]
    fn parse_codex_entry_negative_mtime_drops_fallback() {
        // 负 mtime 无法表达为 SystemTime：对齐本地语义兜底归 None，不得落 Some(负值)
        let entry = remote_entry("/h/.codex/sessions/2026/neg.jsonl.zst", 4_096, -500);
        let text = "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"neg\",\"cwd\":\"/tmp\"}}\n";
        let mut data = HashMap::new();
        data.insert(
            entry.path.clone(),
            ssh::HeadTailData {
                head: vec![],
                tail: vec![],
                tail_bytes: Some(zst_compress(text)),
            },
        );

        let meta = parse_codex_entry(Path::new(&entry.path), &entry, &data, &remote_host("h"))
            .unwrap()
            .expect("应解析出会话");
        assert_eq!(meta.last_active_at, None, "负 mtime 不做兜底");
    }

    #[test]
    fn parse_remote_entries_error_when_data_missing() {
        // 头尾数据缺失按读取失败处理：不缓存、下一轮重试
        let entry = remote_entry("/h/a.jsonl", 1, 1);
        let empty = HashMap::new();
        assert!(
            parse_claude_entry(Path::new(&entry.path), &entry, &empty, &remote_host("h")).is_err()
        );
        assert!(
            parse_codex_entry(Path::new(&entry.path), &entry, &empty, &remote_host("h")).is_err()
        );
    }

    #[test]
    fn index_reuses_a_fresh_scan_once_and_preserves_incomplete_providers() {
        let now = Instant::now();
        let slot = Mutex::new(Some((
            now,
            SessionScan {
                sessions: Vec::new(),
                // Claude failed; Codex really has no sessions. These flags must
                // survive reuse so only Codex's old index is eligible for pruning.
                complete_providers: HashSet::from(["codex".into()]),
            },
        )));
        let scan = take_recent_scan(&slot, now).unwrap();
        assert!(scan.sessions.is_empty());
        assert_eq!(scan.complete_providers, HashSet::from(["codex".into()]));
        assert!(take_recent_scan(&slot, now).is_none());

        *slot.lock().unwrap() = Some((now, scan));
        assert!(
            take_recent_scan(&slot, now + SCAN_REUSE_WINDOW + Duration::from_secs(1)).is_none()
        );
        assert!(slot.lock().unwrap().is_none());
    }

    fn write_codex_session(path: &Path, session_id: &str) {
        std::fs::write(
            path,
            format!(
                "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/tmp/project\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello\"}}}}\n",
            ),
        )
        .expect("write source");
    }

    #[test]
    fn accepts_source_path_under_any_allowed_provider_root() {
        let active_root = tempdir().expect("active root");
        let archived_root = tempdir().expect("archived root");
        let source = archived_root.path().join("session.jsonl");
        write_codex_session(&source, "archived-session");

        let deleted = delete_session_with_roots(
            "codex",
            "archived-session",
            &source,
            &[
                active_root.path().to_path_buf(),
                archived_root.path().to_path_buf(),
            ],
        )
        .expect("delete archived session");

        assert!(deleted);
        assert!(!source.exists());
    }

    #[test]
    fn rejects_source_path_outside_provider_root() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("tempdir");
        let source = outside.path().join("session.jsonl");
        std::fs::write(&source, "{}").expect("write source");

        let err =
            delete_session_with_roots("codex", "session-1", &source, &[root.path().to_path_buf()])
                .expect_err("expected outside-root path to be rejected");

        assert!(err.contains("outside provider roots"));
    }

    #[test]
    fn rejects_missing_source_path() {
        let root = tempdir().expect("tempdir");
        let missing = root.path().join("missing.jsonl");

        let err =
            delete_session_with_roots("codex", "session-1", &missing, &[root.path().to_path_buf()])
                .expect_err("expected missing source path to fail");

        assert!(err.contains("session source not found"));
    }

    #[test]
    fn batch_delete_collects_successes_and_failures_in_order() {
        let requests = vec![
            DeleteSessionRequest {
                provider_id: "codex".to_string(),
                session_id: "s1".to_string(),
                source_path: "/tmp/s1".to_string(),
            },
            DeleteSessionRequest {
                provider_id: "claude".to_string(),
                session_id: "s2".to_string(),
                source_path: "/tmp/s2".to_string(),
            },
            DeleteSessionRequest {
                provider_id: "gemini".to_string(),
                session_id: "s3".to_string(),
                source_path: "/tmp/s3".to_string(),
            },
        ];

        let outcomes = collect_delete_session_outcomes(&requests, |request| {
            match request.session_id.as_str() {
                "s1" => Ok(true),
                "s2" => Err("boom".to_string()),
                _ => Ok(false),
            }
        });

        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].success);
        assert_eq!(outcomes[0].error, None);
        assert!(!outcomes[1].success);
        assert_eq!(outcomes[1].error.as_deref(), Some("boom"));
        assert!(!outcomes[2].success);
        assert_eq!(
            outcomes[2].error.as_deref(),
            Some("Session was not deleted")
        );
    }
}
