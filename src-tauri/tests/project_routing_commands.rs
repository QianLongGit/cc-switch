//! 项目路由三命令的集成测试（计划 T9 / spec §6.1 第 2 项）。
//!
//! 被测对象为命令层可测核心函数（#[tauri::command] 只是带 State 的薄壳）：
//!   - `build_project_list`：scanner 三源合并 + DB 绑定 + 同步状态三态
//!   - `set_route`：先 DB 后文件双写（文件失败 DB 行保留，供修复兜底）
//!   - `clear_route`：DB 行删 + 文件目标行删（双清幂等）
//!
//! 全部数据落 TempDir（home fixture + 真实项目目录 + settings.local.json），
//! `Database::memory()` 独立建库，绝不触碰真实 ~/.claude 与用户目录。

use cc_switch_lib::{
    build_project_list, clear_route, set_route, AppError, Database, ProjectRouteInfo, Provider,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 一天的秒数（活跃度刻度）
const DAY: i64 = 86_400;

// ===================================================================
// fixture 构造器：临时 home + 真实项目目录 + claude.json 铺源
// ===================================================================

/// 临时 home：add_project 记录项目路径与活跃时间，write_claude_json 一次落盘。
struct Fixture {
    home: TempDir,
    projects: BTreeMap<String, serde_json::Value>,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            home: TempDir::new().unwrap(),
            projects: BTreeMap::new(),
        }
    }

    /// 真实 mkdir 项目目录（满足 scanner 的"目录存在"过滤）并登记
    /// lastSessionModified（毫秒 epoch，now 由调用方注入便于断言精确值）
    fn add_project(&mut self, name: &str, now: i64, age_days: i64) -> String {
        let p = self.home.path().join(name);
        fs::create_dir_all(&p).unwrap();
        let lsm_ms = (now - age_days * DAY) * 1000;
        self.projects.insert(
            p.to_str().unwrap().to_string(),
            json!({ "allowedTools": [], "lastSessionModified": lsm_ms }),
        );
        p.to_str().unwrap().to_string()
    }

    /// 把登记的 projects 写成合法 ~/.claude.json（毫秒时间戳主源）
    fn write_claude_json(&self) {
        let doc = json!({ "numStartups": 1, "projects": self.projects });
        fs::write(
            self.home.path().join(".claude.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();
    }
}

/// 构造最小供应商行（测试夹具，仅 id / name 生效）——与 T2 DAO 测试同款
fn seed_provider(db: &Database, id: &str) {
    db.save_provider(
        "claude",
        &Provider::with_id(id.to_string(), format!("Provider {id}"), json!({}), None),
    )
    .unwrap();
}

/// 按路径取列表项，缺失即 panic（断言辅助）
fn find<'v>(list: &'v [ProjectRouteInfo], path: &str) -> &'v ProjectRouteInfo {
    list.iter()
        .find(|p| p.project_path == path)
        .unwrap_or_else(|| panic!("未找到项目 {path}"))
}

/// `<project>/.claude/settings.local.json` 的完整路径
fn settings_file(project: &Path) -> PathBuf {
    project.join(".claude").join("settings.local.json")
}

/// 读取并解析 settings.local.json（必须已是合法 JSON），取
/// env.ANTHROPIC_CUSTOM_HEADERS 字符串值（缺失按空串）
fn headers_of(project: &Path) -> String {
    let text = fs::read_to_string(settings_file(project)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v.get("env")
        .and_then(|e| e.get("ANTHROPIC_CUSTOM_HEADERS"))
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string()
}

/// 独立谓词：行是否为 X-CC-Project 目标行。刻意不复用实现侧判定
/// （断言与实现同源会掩盖实现错误，T4 测试同款约定）
fn is_target_line(line: &str) -> bool {
    line.trim()
        .split_once(':')
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("x-cc-project"))
}

fn target_line_count(headers: &str) -> usize {
    headers.lines().filter(|l| is_target_line(l)).count()
}

// ===================================================================
// build_project_list：扫描 + 绑定 + 同步状态合并
// ===================================================================

#[test]
fn build_list_merges_scan_and_binding() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let bound = fx.add_project("projA", now, 2);
    let unbound = fx.add_project("projB", now, 5);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    seed_provider(&db, "p1");

    // 绑定走被测核心：先 DB 后文件（文件落地后 sync 应为 synced）
    set_route(&db, &bound, "p1").unwrap();

    let list = build_project_list(&db, fx.home.path(), now).unwrap();

    // 活跃时间降序：projA(2 天前) 在前
    let a = find(&list, &bound);
    assert_eq!(a.basename, "projA");
    assert_eq!(a.last_active_at, now - 2 * DAY, "lsm 毫秒应归一化为秒");
    assert!(!a.has_active_session);
    assert_eq!(a.bound_provider_id.as_deref(), Some("p1"));
    assert_eq!(a.bound_provider_name.as_deref(), Some("Provider p1"));
    assert!(a.bound_provider_valid, "绑定的供应商存在 → valid");
    assert_eq!(a.sync_status, "synced", "set 已写文件 → synced");

    // 未绑定项目：无绑定信息、无孤儿行 → synced / valid
    let b = find(&list, &unbound);
    assert_eq!(b.bound_provider_id, None);
    assert_eq!(b.bound_provider_name, None);
    assert!(b.bound_provider_valid, "未绑定不构成失效");
    assert_eq!(b.sync_status, "synced");

    assert_eq!(list.len(), 2, "仅扫描源项目入列");
}

#[test]
fn build_list_marks_out_of_sync_and_orphan() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let pp = fx.add_project("projA", now, 1);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    seed_provider(&db, "p1");
    set_route(&db, &pp, "p1").unwrap();

    // 有绑定 + 文件被手动删（spec §8 边界 2）→ out_of_sync
    fs::remove_file(settings_file(Path::new(&pp))).unwrap();
    let list = build_project_list(&db, fx.home.path(), now).unwrap();
    let a = find(&list, &pp);
    assert_eq!(a.bound_provider_id.as_deref(), Some("p1"), "DB 行仍在");
    assert_eq!(a.sync_status, "out_of_sync", "绑定在文件缺 → out_of_sync");

    // 解除绑定后文件又残留目标行 → orphan_header（不参与路由，仅提示）
    clear_route(&db, &pp).unwrap();
    let encoded = pp.clone(); // TempDir 内纯 ASCII 安全路径，percent-encode 后不变
    fs::write(
        settings_file(Path::new(&pp)),
        format!(r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-CC-Project: {encoded}\n"}}}}"#),
    )
    .unwrap();
    let list = build_project_list(&db, fx.home.path(), now).unwrap();
    let a = find(&list, &pp);
    assert_eq!(a.bound_provider_id, None, "clear 后无绑定");
    assert_eq!(a.sync_status, "orphan_header", "无绑定 + 文件残留目标行 → orphan_header");
}

#[test]
fn build_list_invalid_provider_flagged() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let pp = fx.add_project("projA", now, 1);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    seed_provider(&db, "p1");
    set_route(&db, &pp, "p1").unwrap();

    // 删供应商（级联清行）后直调 DAO 重新指向已删 p1，构造悬空绑定
    // （§8 边界 9"已失效"态：不静默清空展示，交由用户改选或清空）
    db.delete_provider("claude", "p1").unwrap();
    db.insert_or_update_route(&pp, "claude", "p1").unwrap();

    let list = build_project_list(&db, fx.home.path(), now).unwrap();
    let a = find(&list, &pp);
    assert_eq!(a.bound_provider_id.as_deref(), Some("p1"), "悬空绑定仍透出 id");
    assert_eq!(a.bound_provider_name, None, "供应商已删 → name 缺失");
    assert!(!a.bound_provider_valid, "悬空绑定 → valid=false（已失效态）");
}

// ===================================================================
// set_route：DB + 文件双写
// ===================================================================

#[test]
fn set_route_double_write_db_then_file() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let pp = fx.add_project("projA", now, 1);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    seed_provider(&db, "p1");

    set_route(&db, &pp, "p1").unwrap();

    // DB 侧：绑定行在
    assert_eq!(
        db.find_route(&pp, "claude").unwrap().as_deref(),
        Some("p1"),
        "set 应写入 DB 绑定行"
    );
    // 文件侧：settings.local.json 目标行在（值 == 编码后项目路径，ASCII 安全路径不变）
    let headers = headers_of(Path::new(&pp));
    assert_eq!(target_line_count(&headers), 1, "应有且仅有一行目标行");
    assert!(
        headers.lines().any(|l| is_target_line(l) && l.contains(&pp)),
        "目标行值应为项目路径本身（ASCII 安全路径编码不变）：{headers:?}"
    );

    // 文件写失败（普通文件占住项目路径 → <project>/.claude 无法创建）：
    // 先 DB 后文件序下 Err 上抛、DB 行保留，供 sync_status 修复兜底
    let blocker = fx.home.path().join("blocker");
    fs::write(&blocker, "占位文件").unwrap();
    let blocker_pp = blocker.to_str().unwrap().to_string();
    let result = set_route(&db, &blocker_pp, "p1");
    assert!(result.is_err(), "目标不可写时 set 应失败：{result:?}");
    assert_eq!(
        db.find_route(&blocker_pp, "claude").unwrap().as_deref(),
        Some("p1"),
        "文件写失败后 DB 绑定行必须保留（T4 语义闭环）"
    );
}

#[test]
fn set_route_unknown_provider_errors() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let pp = fx.add_project("projA", now, 1);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    // 不 seed 任何供应商：绑定到不存在的 id 必须前置失败

    let err = set_route(&db, &pp, "ghost").unwrap_err();
    match err {
        AppError::InvalidInput(msg) => {
            assert!(msg.contains("ghost"), "错误信息应包含供应商 id：{msg}");
        }
        other => panic!("应为 InvalidInput，实际 {other:?}"),
    }

    // 前置校验失败：不得留下任何半成品（DB 行 / settings 文件）
    assert!(
        db.find_route(&pp, "claude").unwrap().is_none(),
        "校验失败不得写 DB 行"
    );
    assert!(
        !settings_file(Path::new(&pp)).exists(),
        "校验失败不得产生 settings 文件"
    );
}

// ===================================================================
// clear_route：DB 行 + 文件目标行双清
// ===================================================================

#[test]
fn clear_route_removes_both() {
    let now = chrono::Utc::now().timestamp();
    let mut fx = Fixture::new();
    let pp = fx.add_project("projA", now, 1);
    fx.write_claude_json();
    let db = Database::memory().unwrap();
    seed_provider(&db, "p1");

    // 预置用户自有行，验证 clear 只删目标行
    fs::create_dir_all(settings_file(Path::new(&pp)).parent().unwrap()).unwrap();
    fs::write(
        settings_file(Path::new(&pp)),
        format!(r#"{{"env":{{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\nX-CC-Project: {pp}\n"}}}}"#),
    )
    .unwrap();

    clear_route(&db, &pp).unwrap();

    assert!(
        db.find_route(&pp, "claude").unwrap().is_none(),
        "clear 后 DB 绑定行应删除"
    );
    let headers = headers_of(Path::new(&pp));
    assert_eq!(target_line_count(&headers), 0, "文件目标行应删净");
    assert!(
        headers.contains("X-Api-Key: k"),
        "非目标行逐字节保留：{headers:?}"
    );

    // 幂等：文件已无目标行再 clear → Ok
    clear_route(&db, &pp).expect("重复 clear 应幂等 Ok");
    // 文件不存在的项目路径 → Ok
    let ghost = fx.home.path().join("never-exists");
    clear_route(&db, ghost.to_str().unwrap()).expect("文件不存在也应 Ok");
    assert!(
        db.find_route(ghost.to_str().unwrap(), "claude")
            .unwrap()
            .is_none()
    );
}
