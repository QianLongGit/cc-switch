//! settings.local.json 的项目 header 编辑器（spec §6.1 第 3 项）。
//!
//! 职责：在 `<project>/.claude/settings.local.json` 的
//! `env.ANTHROPIC_CUSTOM_HEADERS` 多行值中**仅**增删替换 `X-CC-Project` 行
//! （行级规则全部复用 [`crate::proxy::project_header`]，杜绝两套判定漂移），
//! 用户自有的其他 header 行与其他顶层键 / 其他 env 键原样保留。
//!
//! - **读取容错**：文件不存在 → 空对象起步新建最小结构；损坏 JSON / 结构不符
//!   → 按空对象重建，但覆盖前原文先入库备份（spec §4.3 `settings_local_backup`，
//!   每项目单份 REPLACE，仅保留最近一次写前快照）
//! - **原子写**：复用 [`crate::config::atomic_write`]（temp + rename，自动创建
//!   `.claude` 父目录），半写状态不落盘
//! - **同步状态**：DB 绑定 vs 文件实读的三态对比（spec §7.1 第 3 项真值表）

use crate::config::atomic_write;
use crate::database::Database;
use crate::error::AppError;
use crate::proxy::project_header::{
    extract_x_cc_project, percent_decode_project_path, percent_encode_project_path,
    replace_x_cc_project,
};
use std::fs;
use std::path::{Path, PathBuf};

// ===================================================================
// 三态同步状态（spec §7.1 第 3 项真值表）
// ===================================================================

/// 项目绑定的同步状态（Rust 侧对比，随 `list_projects` 返回前端）。
///
/// | DB 绑定 | 文件状态                  | sync_status    |
/// |---------|---------------------------|----------------|
/// | 有      | 目标行存在且值与绑定一致    | `synced`       |
/// | 有      | 文件缺失 / 无目标行 / 值不一致（含 decode 非法） | `out_of_sync` |
/// | 无      | 文件有目标行               | `orphan_header` |
/// | 无      | 无目标行                   | `synced`       |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Synced,
    OutOfSync,
    OrphanHeader,
}

impl SyncStatus {
    /// 前端契约：序列化为 `"synced"` / `"out_of_sync"` / `"orphan_header"`。
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncStatus::Synced => "synced",
            SyncStatus::OutOfSync => "out_of_sync",
            SyncStatus::OrphanHeader => "orphan_header",
        }
    }
}

// ===================================================================
// 公开操作（供 T9 命令层调用）
// ===================================================================

/// 绑定 → 写入 `<project_path>/.claude/settings.local.json` 的
/// `env.ANTHROPIC_CUSTOM_HEADERS` 中的 `X-CC-Project` 行。
///
/// 文件不存在 / 损坏 JSON → 按空对象重建（损坏时原文先备份）；已存在文件
/// 每次写前备份（REPLACE，spec §4.3）。
pub fn set_project_header(project_path: &str, db: &Database) -> Result<(), AppError> {
    let path = settings_local_path(project_path);
    let (mut value, raw) = load_settings_json(&path);
    // 已存在文件（含损坏原文）每次写前快照入库，每项目单份 REPLACE（spec §4.3）
    if let Some(raw) = raw {
        db.save_settings_local_backup(project_path, &raw)?;
    }
    let new_headers = replace_x_cc_project(
        &headers_of(&value),
        Some(&percent_encode_project_path(project_path)),
    );
    write_custom_headers(&mut value, &path, &new_headers)
}

/// 移除 `X-CC-Project` 目标行（文件不存在 → `Ok` 直接返回）；非目标行逐字节保留。
///
/// 写前同样备份现存原文（与 [`set_project_header`] 复用同一 helper，保持
/// "每次写前快照"语义一致）；`env.ANTHROPIC_CUSTOM_HEADERS` 删空时保留键
/// （值变空串，最小侵入）。
pub fn remove_project_header(project_path: &str, db: &Database) -> Result<(), AppError> {
    let path = settings_local_path(project_path);
    if !path.exists() {
        return Ok(());
    }
    let (mut value, raw) = load_settings_json(&path);
    let headers = headers_of(&value);
    if extract_x_cc_project(&headers).is_none() {
        // 无目标行可删（含损坏 JSON 重建出的空对象）：不动用户文件，最小侵入
        return Ok(());
    }
    // 与 set 同一快照语义：真实写入前备份现存原文
    if let Some(raw) = raw {
        db.save_settings_local_backup(project_path, &raw)?;
    }
    let new_headers = replace_x_cc_project(&headers, None);
    write_custom_headers(&mut value, &path, &new_headers)
}

/// 三态对比（spec §7.1 第 3 项真值表，语义见 [`SyncStatus`]）。
///
/// `bound=Some(pp)`：文件目标行存在且 percent_decode 后 == pp → `Synced`；
/// 否则（含文件缺失 / 无目标行 / 值不一致 / decode 非法）→ `OutOfSync`。
/// `bound=None`：文件有目标行 → `OrphanHeader`；否则 `Synced`。
pub fn read_sync_status(project_path: &str, bound: Option<&str>) -> SyncStatus {
    let (value, _) = load_settings_json(&settings_local_path(project_path));
    let file_encoded = extract_x_cc_project(&headers_of(&value));
    match bound {
        Some(pp) => match file_encoded.as_deref().and_then(percent_decode_project_path) {
            // 目标行值 decode 后与绑定全等才 synced；值不一致 / decode 非法 /
            // 文件缺失 / 无目标行一律 out_of_sync（警示 + 一键修复）
            Some(decoded) if decoded == pp => SyncStatus::Synced,
            _ => SyncStatus::OutOfSync,
        },
        // 无绑定：文件残留目标行 → 孤儿 header（提示可清除，不自动删）
        None if file_encoded.is_some() => SyncStatus::OrphanHeader,
        None => SyncStatus::Synced,
    }
}

// ===================================================================
// 私有辅助
// ===================================================================

/// `<project_path>/.claude/settings.local.json` 的完整路径。
fn settings_local_path(project_path: &str) -> PathBuf {
    Path::new(project_path).join(".claude").join("settings.local.json")
}

/// 读取并容错解析 settings.local.json，返回（对象, 已存在文件的原文）。
///
/// - 文件不存在 / 读失败 → (`{}`, None)
/// - 解析成功且顶层为对象、`env`（若存在）为对象 → (原对象, Some(原文))
/// - 损坏 JSON / 顶层非对象 / `env` 非对象 → (`{}`, Some(原文))——结构不符与
///   损坏同道处理，但原文照常返回，由调用方在覆盖前入库备份（spec §4.3）
fn load_settings_json(path: &Path) -> (serde_json::Value, Option<String>) {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // 读失败（含不存在）按空对象起步；有绑定时 read_sync_status 自然报
        // out_of_sync，可经一键修复重建
        Err(_) => return (serde_json::json!({}), None),
    };
    let well_formed = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .filter(|v| {
            v.as_object().is_some_and(|obj| {
                obj.get("env").map_or(true, |e| e.is_object())
            })
        });
    match well_formed {
        Some(v) => (v, Some(raw)),
        None => (serde_json::json!({}), Some(raw)),
    }
}

/// 取文档当前 `env.ANTHROPIC_CUSTOM_HEADERS` 值（缺失 / 非字符串按空串）。
fn headers_of(value: &serde_json::Value) -> String {
    value
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_CUSTOM_HEADERS"))
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string()
}

/// 把 `env.ANTHROPIC_CUSTOM_HEADERS` 置为 `new_headers` 并原子写回。
///
/// env 对象缺失时自动创建（最小结构起步）；序列化用 pretty（贴近 Claude Code
/// 自身生成的 settings 文件形态，便于用户阅读与 diff）。
fn write_custom_headers(
    value: &mut serde_json::Value,
    path: &Path,
    new_headers: &str,
) -> Result<(), AppError> {
    // load_settings_json 契约已保证顶层与 env 均为对象，此处再兜底一次，
    // 使本 helper 对任意输入安全（防御深度，零 panic 路径）
    if !value.is_object() {
        *value = serde_json::json!({});
    }
    if !value.get("env").is_some_and(|e| e.is_object()) {
        value["env"] = serde_json::json!({});
    }
    value["env"]["ANTHROPIC_CUSTOM_HEADERS"] = serde_json::json!(new_headers);
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::project_header::{extract_x_cc_project, percent_encode_project_path};
    use std::fs;
    use std::path::{Path, PathBuf};

    // ===================================================================
    // 测试辅助（全部落 TempDir，禁止触碰真实 ~/.claude）
    // ===================================================================

    /// `<project>/.claude/settings.local.json` 的完整路径。
    fn settings_file(project: &Path) -> PathBuf {
        project.join(".claude").join("settings.local.json")
    }

    /// 预置一个已存在的 settings.local.json（自动建 `.claude` 目录）。
    fn seed_settings(project: &Path, content: &str) {
        let path = settings_file(project);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }

    /// 读取并解析 settings.local.json（必须已是合法 JSON）。
    fn read_settings_json(project: &Path) -> serde_json::Value {
        let text = fs::read_to_string(settings_file(project)).unwrap();
        serde_json::from_str(&text).expect("settings.local.json 应为合法 JSON")
    }

    /// 取 JSON 的 `env.ANTHROPIC_CUSTOM_HEADERS` 字符串值（缺失按空串）。
    fn headers_value(v: &serde_json::Value) -> String {
        v.get("env")
            .and_then(|e| e.get("ANTHROPIC_CUSTOM_HEADERS"))
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// 测试独立谓词：行是否为 X-CC-Project 目标行。
    ///
    /// 刻意不复用实现的判定函数——断言与实现同源会掩盖实现侧错误。
    fn is_target_line(line: &str) -> bool {
        line.trim()
            .split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("x-cc-project"))
    }

    /// 与写入端序列化步骤严格对齐：拆行（丢末尾单个空元素）→ 过滤目标行 →
    /// 剥末尾空行（目标行间/后的空行过滤后暴露在末尾，在"无尾随换行"写入
    /// 形态下不可表达）。
    fn other_lines(headers: &str) -> Vec<String> {
        let mut lines: Vec<&str> = headers.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        let mut others: Vec<&str> = lines
            .into_iter()
            .filter(|l| !is_target_line(l))
            .collect();
        while others.last() == Some(&"") {
            others.pop();
        }
        others.into_iter().map(str::to_string).collect()
    }

    // ===================================================================
    // 固定用例（计划 Task 4 Step 1 清单）
    // ===================================================================

    #[test]
    fn set_creates_minimal_structure_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        // 中文 + 空格路径（spec §8 边界 5）：编码进 header 的链路一并覆盖
        let project = dir.path().join("我的 项目");
        let pp = project.to_str().unwrap().to_string();

        set_project_header(&pp, &db).unwrap();

        let v = read_settings_json(&project);
        assert_eq!(
            headers_value(&v),
            format!("X-CC-Project: {}", percent_encode_project_path(&pp)),
            "新建文件应为最小结构，目标行值 == 编码后的项目路径（单行无尾随换行）"
        );
        assert!(
            db.get_settings_local_backup(&pp).unwrap().is_none(),
            "原文件不存在时不应产生备份（spec §4.3：首次修改【已存在】文件前备份）"
        );
    }

    #[test]
    fn set_preserves_user_other_headers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().to_str().unwrap().to_string();
        let original =
            r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\n"},"permissions":{"allow":["Bash(ls:*)"]}}"#;
        seed_settings(dir.path(), original);

        set_project_header(&pp, &db).unwrap();

        let v = read_settings_json(dir.path());
        assert_eq!(
            headers_value(&v),
            format!(
                "X-Api-Key: k\nX-CC-Project: {}",
                percent_encode_project_path(&pp)
            ),
            "用户自有 header 行逐字节保留，目标行追加其后（末行无尾随换行）"
        );
        assert_eq!(
            v.get("permissions"),
            Some(&serde_json::json!({"allow": ["Bash(ls:*)"]})),
            "其他顶层键（permissions）原样保留"
        );
    }

    #[test]
    fn set_merges_duplicate_target_lines() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().to_str().unwrap().to_string();
        let original = r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"x-cc-project: old1\nX-Api-Key: k\nX-CC-PROJECT: old2\n"}}"#;
        seed_settings(dir.path(), original);

        set_project_header(&pp, &db).unwrap();

        let headers = headers_value(&read_settings_json(dir.path()));
        assert_eq!(
            headers.split('\n').filter(|l| is_target_line(l)).count(),
            1,
            "两行既有目标行必须合并为一：{headers:?}"
        );
        assert_eq!(
            extract_x_cc_project(&headers).as_deref(),
            Some(percent_encode_project_path(&pp).as_str()),
            "合并后的目标行值为本次写入的编码路径"
        );
        assert_eq!(
            other_lines(&headers),
            vec!["X-Api-Key: k".to_string()],
            "非目标行不变"
        );
    }

    #[test]
    fn set_backs_up_existing_file_once_per_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().to_str().unwrap().to_string();
        let original = r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\n"}}"#;
        seed_settings(dir.path(), original);

        // 第一次写：备份 == 预置原文
        set_project_header(&pp, &db).unwrap();
        assert_eq!(
            db.get_settings_local_backup(&pp).unwrap().as_deref(),
            Some(original),
            "首次修改已存在文件前，原文应入库备份"
        );
        let after_first = fs::read_to_string(settings_file(dir.path())).unwrap();

        // 第二次写：备份 == 第一次写完成后的文件内容（每次写前快照，单份 REPLACE）
        set_project_header(&pp, &db).unwrap();
        assert_eq!(
            db.get_settings_local_backup(&pp).unwrap().as_deref(),
            Some(after_first.as_str()),
            "第二次写前应快照上一次的内容（每项目单份 REPLACE）"
        );
    }

    #[test]
    fn set_rebuilds_corrupted_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().to_str().unwrap().to_string();
        seed_settings(dir.path(), "not json{");

        set_project_header(&pp, &db).unwrap();

        let v = read_settings_json(dir.path());
        assert_eq!(
            headers_value(&v),
            format!("X-CC-Project: {}", percent_encode_project_path(&pp)),
            "损坏 JSON 应按空对象重建最小结构（单行无尾随换行）"
        );
        assert_eq!(
            db.get_settings_local_backup(&pp).unwrap().as_deref(),
            Some("not json{"),
            "覆盖损坏文件前，损坏原文应先备份"
        );
    }

    #[test]
    fn remove_deletes_only_target_line() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().to_str().unwrap().to_string();
        let encoded = percent_encode_project_path(&pp);
        let original = format!(
            r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\nX-CC-Project: {encoded}\n"}},"permissions":{{}}}}"#
        );
        seed_settings(dir.path(), &original);

        remove_project_header(&pp, &db).unwrap();

        let v = read_settings_json(dir.path());
        assert_eq!(
            headers_value(&v),
            "X-Api-Key: k",
            "仅删目标行，其余行逐字节保留（末行无尾随换行）"
        );
        assert!(v.get("permissions").is_some(), "其他顶层键保留");
        assert_eq!(
            db.get_settings_local_backup(&pp).unwrap().as_deref(),
            Some(original.as_str()),
            "remove 写前同样备份现存原文（每次写前快照语义一致）"
        );
    }

    #[test]
    fn remove_ok_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let pp = dir.path().join("never-exists").to_str().unwrap().to_string();

        remove_project_header(&pp, &db).expect("文件不存在应直接 Ok 返回");
    }

    #[test]
    fn sync_status_synced_out_of_sync_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let pp = project.to_str().unwrap().to_string();
        let bound_path = "/Users/dev/绑定项目";

        // synced：文件目标行值 == 绑定路径（编码后）
        let encoded = percent_encode_project_path(bound_path);
        seed_settings(
            &project,
            &format!(r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-CC-Project: {encoded}\n"}}}}"#),
        );
        assert_eq!(
            read_sync_status(&pp, Some(bound_path)),
            SyncStatus::Synced,
            "文件行值与绑定一致 → synced"
        );

        // out_of_sync 之一：值不一致
        let other = percent_encode_project_path("/Users/dev/别的项目");
        seed_settings(
            &project,
            &format!(r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-CC-Project: {other}\n"}}}}"#),
        );
        assert_eq!(
            read_sync_status(&pp, Some(bound_path)),
            SyncStatus::OutOfSync,
            "文件行值与绑定不一致 → out_of_sync"
        );

        // out_of_sync 之二：decode 非法（坏 % 转义，Review Focus 1 文件侧）
        seed_settings(
            &project,
            r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-CC-Project: %zz\n"}}"#,
        );
        assert_eq!(
            read_sync_status(&pp, Some(bound_path)),
            SyncStatus::OutOfSync,
            "decode 非法按不一致处理 → out_of_sync"
        );

        // out_of_sync 之三：有绑定但文件无目标行
        seed_settings(
            &project,
            r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\n"}}"#,
        );
        assert_eq!(
            read_sync_status(&pp, Some(bound_path)),
            SyncStatus::OutOfSync,
            "有绑定但文件无目标行 → out_of_sync"
        );

        // orphan_header：无绑定但文件有目标行（不参与路由，仅提示可清除）
        let stray = percent_encode_project_path("/Users/dev/孤儿路径");
        seed_settings(
            &project,
            &format!(r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-CC-Project: {stray}\n"}}}}"#),
        );
        assert_eq!(
            read_sync_status(&pp, None),
            SyncStatus::OrphanHeader,
            "无绑定 + 文件有目标行 → orphan_header"
        );

        // synced：无绑定 + 文件无目标行
        seed_settings(
            &project,
            r#"{"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\n"}}"#,
        );
        assert_eq!(
            read_sync_status(&pp, None),
            SyncStatus::Synced,
            "无绑定 + 文件无目标行 → synced"
        );

        // out_of_sync 之四：有绑定但文件被手动删除（spec §8 边界 2）
        let missing = dir.path().join("missing_proj");
        let missing_pp = missing.to_str().unwrap().to_string();
        assert_eq!(
            read_sync_status(&missing_pp, Some(bound_path)),
            SyncStatus::OutOfSync,
            "有绑定 + 文件缺失 → out_of_sync（警示 + 一键修复）"
        );
        // synced：无绑定 + 文件不存在
        assert_eq!(
            read_sync_status(&missing_pp, None),
            SyncStatus::Synced,
            "无绑定 + 文件缺失 → synced"
        );
    }

    #[test]
    fn write_failure_keeps_db_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        // 用普通文件占住项目路径，使 <project>/.claude 无法创建 → 写失败（跨平台稳定）
        let blocker = dir.path().join("proj");
        fs::write(&blocker, "占位文件").unwrap();
        let pp = blocker.to_str().unwrap().to_string();

        // 模拟 T9 的「先 DB 后文件」序：绑定行已入库
        db.insert_or_update_route(&pp, "claude", "p1").unwrap();

        let result = set_project_header(&pp, &db);
        assert!(result.is_err(), "目标路径不可写时 set 应失败：{result:?}");

        assert_eq!(
            db.find_route(&pp, "claude").unwrap().as_deref(),
            Some("p1"),
            "写失败后 DB 绑定行必须保留（sync_status 兜底修复的前提）"
        );
    }

    // ===================================================================
    // 属性测试（spec §9：任意既有 settings.local.json 内容——含用户自有 env /
    // 其他顶层键 / 用户自有 CUSTOM_HEADERS 行——增删 X-CC-Project 后其余内容
    // 不变）。延续 T3 的手写确定性 LCG 风格，零新依赖。
    // ===================================================================

    /// xorshift64*：状态非零，输出乘黄金比率常数打散低位。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// 均匀取 [0, bound)。
        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// 宽表随机串（可作 JSON 字符串值）：ASCII 字母数字 + `/._~-%:\n\r\t ` + 中文池。
    fn rand_string(rng: &mut Lcg, len: usize) -> String {
        const CHARSET: &[&str] = &[
            "a", "B", "z", "0", "9", "/", ".", "_", "~", "-", "%", ":", "\n", "\r", "\t", " ",
            "2", "4", "8", "E", "F", "f", "项目", "开", "发", "文", "件",
        ];
        (0..len)
            .map(|_| CHARSET[rng.below(CHARSET.len() as u64) as usize].to_string())
            .collect()
    }

    /// 行内随机值：同宽表但**不含 `\n`**（行级结构可控），可含 `:`、`%`、`\r`、空白、中文。
    fn rand_line_value(rng: &mut Lcg) -> String {
        const LINE_CHARSET: &[&str] = &[
            "a", "B", "z", "0", "9", "/", ".", "_", "~", "-", ":", "\r", "\t", " ", "%", "2",
            "F", "x", "X", "k", "项目",
        ];
        (0..rng.below(16) + 1)
            .map(|_| LINE_CHARSET[rng.below(LINE_CHARSET.len() as u64) as usize].to_string())
            .collect()
    }

    /// 随机非目标 header 行：名字池（含前缀相似名，验证全等匹配）随机大小写变体
    /// + 随机前导空白 + 随机值。名字池任何成员 trim 后都不可能全等 `x-cc-project`。
    fn rand_other_header_line(rng: &mut Lcg) -> String {
        const NAMES: &[&str] = &[
            "X-Api-Key",
            "x-custom",
            "Anthropic-Version",
            "x-cc-projec",   // 少一个字母：前缀相似，必须不命中
            "X-CC-ProjectX", // 多一个字母：同样必须不命中
        ];
        let name: String = NAMES[rng.below(NAMES.len() as u64) as usize]
            .chars()
            .map(|c| {
                if rng.below(2) == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        let lead = ["", " ", "\t"][rng.below(3) as usize];
        format!("{lead}{name}: {}", rand_line_value(rng))
    }

    /// 随机构造既有 `ANTHROPIC_CUSTOM_HEADERS` 多行值：
    /// 0..2 行用户自有目标行（大小写变体 + 随机值）+ 0..3 行非目标行，行序随机，随机尾 `\n`。
    fn rand_existing_headers(rng: &mut Lcg) -> String {
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..rng.below(3) {
            let name: String = "x-cc-project"
                .chars()
                .map(|c| {
                    if rng.below(2) == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            lines.push(format!("{name}: {}", rand_line_value(rng)));
        }
        for _ in 0..rng.below(4) {
            lines.push(rand_other_header_line(rng));
        }
        // Fisher–Yates 洗牌打散目标行位置
        for i in (1..lines.len()).rev() {
            lines.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let mut v = lines.join("\n");
        if rng.below(2) == 1 {
            v.push('\n');
        }
        v
    }

    /// 随机既有 settings.local.json 文档：permissions + 随机顶层键 + env 内
    /// 自有 API key 与随机 CUSTOM_HEADERS。返回（文档, 随机顶层键名）。
    fn rand_existing_doc(rng: &mut Lcg) -> (serde_json::Value, String) {
        let extra_key = format!("customKey{}", rng.below(1000));
        let mut doc = serde_json::json!({
            "permissions": {"allow": [rand_string(rng, 12)]},
            "env": {
                "ANTHROPIC_API_KEY": rand_string(rng, 24),
                "ANTHROPIC_CUSTOM_HEADERS": rand_existing_headers(rng),
            }
        });
        doc[&extra_key] = serde_json::json!(rand_string(rng, 16));
        (doc, extra_key)
    }

    /// 断言辅助：除 CUSTOM_HEADERS 外，set / remove 均不得动文档的其他任何内容。
    fn assert_other_content_unchanged(
        after: &serde_json::Value,
        before: &serde_json::Value,
        extra_key: &str,
        ctx: &str,
    ) {
        assert_eq!(
            after["permissions"], before["permissions"],
            "{ctx}: permissions 键不得变动"
        );
        assert_eq!(
            after["env"]["ANTHROPIC_API_KEY"], before["env"]["ANTHROPIC_API_KEY"],
            "{ctx}: env 内用户自有键不得变动"
        );
        assert_eq!(
            after[extra_key], before[extra_key],
            "{ctx}: 用户自有顶层键不得变动"
        );
    }

    #[test]
    fn prop_set_then_remove_preserves_other_content() {
        let mut rng = Lcg(0xC0FF_EE01_BADC_0DE5);
        for round in 0..250 {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::memory().unwrap();
            // 项目名含中文 + 空格：编码链路随属性轮次反复覆盖（spec §8 边界 5）
            let project = dir.path().join(format!("项 目 {round}"));
            let pp = project.to_str().unwrap().to_string();

            let (before, extra_key) = rand_existing_doc(&mut rng);
            let original_headers =
                before["env"]["ANTHROPIC_CUSTOM_HEADERS"].as_str().unwrap().to_string();
            seed_settings(&project, &serde_json::to_string(&before).unwrap());

            // --- set：只动 X-CC-Project 行 ---
            set_project_header(&pp, &db).unwrap();
            let after = read_settings_json(&project);
            let headers_after = headers_value(&after);
            assert_other_content_unchanged(&after, &before, &extra_key, "set");
            assert!(
                !headers_after.ends_with('\n'),
                "round {round}: set 后写入形态不得带尾随换行: {headers_after:?}"
            );
            assert_eq!(
                extract_x_cc_project(&headers_after).as_deref(),
                Some(percent_encode_project_path(&pp).as_str()),
                "round {round}: set 后目标行值应恰为编码后的项目路径"
            );
            assert_eq!(
                other_lines(&headers_after),
                other_lines(&original_headers),
                "round {round}: set 后用户自有 header 行逐字节不变"
            );

            // --- remove：目标行删净，其余仍不变 ---
            remove_project_header(&pp, &db).unwrap();
            let final_v = read_settings_json(&project);
            let headers_final = headers_value(&final_v);
            assert_other_content_unchanged(&final_v, &before, &extra_key, "remove");
            assert_eq!(
                extract_x_cc_project(&headers_final),
                None,
                "round {round}: remove 后不得残留目标行"
            );
            assert_eq!(
                other_lines(&headers_final),
                other_lines(&original_headers),
                "round {round}: remove 后用户自有 header 行逐字节不变"
            );
        }
    }
}
