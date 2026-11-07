pub mod cache;
pub mod content;
#[cfg(windows)]
pub(crate) mod hermes_wsl;
pub mod model;
pub mod providers;
pub mod search;
mod search_revision;
pub mod ssh;
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

/// 远端目录解析命令超时（连接建立 + printf，亚秒级完成）
const REMOTE_DIRS_TIMEOUT: Duration = Duration::from_secs(15);
/// 远端清单命令超时（大目录 `find` 遍历较慢，放宽）
const REMOTE_LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// 单批头尾拉取超时
const REMOTE_BATCH_TIMEOUT: Duration = Duration::from_secs(60);
/// 头尾拉取批大小上限（spec §5.4：单批 ≤ 50 个文件）
const REMOTE_HEAD_TAIL_BATCH: usize = 50;

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
/// 3. 分批（≤50）头尾帧协议拉取 → provider 解析（复用各 provider 的
///    行解析原语）→ 覆盖远程标识字段
/// 4. 排序 + 写入 `SshScanRegistry` 清单（validate / fingerprint / 删除共用）
///
/// 任一 ssh 命令失败整体报错；成功即 claude 与 codex 双 provider 完整。
pub fn scan_remote_sessions(
    host: &crate::settings::SshHostConfig,
) -> Result<SessionScan, String> {
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

    // ── 3. 分批拉取头尾并解析 ──
    let mut sessions = scan_claude_remote(host, &claude_files)?;
    sessions.extend(scan_codex_remote(host, &codex_files)?);
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

/// claude 远程解析：head/tail 行 + 64 KiB 字节尾窗（relocated 记录在末尾窗口内）
fn scan_claude_remote(
    host: &crate::settings::SshHostConfig,
    files: &[ssh::RemoteFileEntry],
) -> Result<Vec<SessionMeta>, String> {
    let requests: Vec<ssh::HeadTailRequest> = files
        .iter()
        .map(|entry| ssh::remote_head_tail_request("claude", entry))
        .collect();
    let data = fetch_head_tail_batches(host, &requests)?;

    // 缓存键经 PathBuf → to_string_lossy 往返（Windows 会翻转斜杠），解析闭包
    // 用同构键查条目；远程标识字段始终取条目里的原始远端字符串
    let by_key: HashMap<String, &ssh::RemoteFileEntry> = files
        .iter()
        .map(|entry| (Path::new(&entry.path).to_string_lossy().into_owned(), entry))
        .collect();
    let cache_files: Vec<(PathBuf, Option<SystemTime>, u64)> = files
        .iter()
        .map(|entry| {
            (
                PathBuf::from(&entry.path),
                ssh::mtime_to_system_time(entry.mtime_ms),
                entry.size,
            )
        })
        .collect();

    let host_id = host.id.clone();
    let parse = |path: &Path| -> std::io::Result<Option<SessionMeta>> {
        let Some(entry) = by_key.get(path.to_string_lossy().as_ref()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "清单条目缺失",
            ));
        };
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
        Ok(claude::parse_session_lines(
            path,
            file.head.clone(),
            file.tail.clone(),
            relocated,
        )
        .map(|meta| ssh::rebind_remote_meta(meta, &host_id, &entry.path)))
    };

    Ok(remote_parse_cache(&host.id).scan_remote(cache_files, parse))
}

/// codex 远程解析：未压缩走 head/tail 行；压缩形态用 zst 前缀解出的头部行，
/// 最后活跃时间以清单 mtime 兜底（线程标题走既有 fallback，远端 state.db 不可读）
fn scan_codex_remote(
    host: &crate::settings::SshHostConfig,
    files: &[ssh::RemoteFileEntry],
) -> Result<Vec<SessionMeta>, String> {
    let requests: Vec<ssh::HeadTailRequest> = files
        .iter()
        .map(|entry| ssh::remote_head_tail_request("codex", entry))
        .collect();
    let data = fetch_head_tail_batches(host, &requests)?;

    let by_key: HashMap<String, &ssh::RemoteFileEntry> = files
        .iter()
        .map(|entry| (Path::new(&entry.path).to_string_lossy().into_owned(), entry))
        .collect();
    let cache_files: Vec<(PathBuf, Option<SystemTime>, u64)> = files
        .iter()
        .map(|entry| {
            (
                PathBuf::from(&entry.path),
                ssh::mtime_to_system_time(entry.mtime_ms),
                entry.size,
            )
        })
        .collect();

    let host_id = host.id.clone();
    let parse = |path: &Path| -> std::io::Result<Option<SessionMeta>> {
        let Some(entry) = by_key.get(path.to_string_lossy().as_ref()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "清单条目缺失",
            ));
        };
        let Some(file) = data.get(&entry.path) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "头尾数据缺失",
            ));
        };
        let lines = if entry.path.ends_with(".jsonl.zst") {
            let prefix = file.tail_bytes.as_deref().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "压缩前缀缺失")
            })?;
            let text = ssh::decode_zst_prefix(prefix)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            codex::SessionLines {
                head: text
                    .lines()
                    .take(codex::HEAD_LINES)
                    .map(str::to_string)
                    .collect(),
                tail: Vec::new(),
                fallback_last_active: Some(entry.mtime_ms),
            }
        } else {
            codex::SessionLines {
                head: file.head.clone(),
                tail: file.tail.clone(),
                fallback_last_active: None,
            }
        };
        Ok(codex::parse_session_lines(path, lines, &HashMap::new())
            .map(|meta| ssh::rebind_remote_meta(meta, &host_id, &entry.path)))
    };

    Ok(remote_parse_cache(&host.id).scan_remote(cache_files, parse))
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
