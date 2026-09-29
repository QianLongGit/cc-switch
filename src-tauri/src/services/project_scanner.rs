//! 项目探测：三源合并扫描（spec §3.3）。
//!
//! 本模块只做文件系统遍历 + JSON 解析，无 DB 依赖；根目录（home）与当前
//! 时间（now）均由调用方注入，保证测试完全由 fixture 目录驱动、不依赖
//! 真实 ~/.claude 状态。三源职责：
//!
//!   1. ~/.claude.json 的 projects 键 —— 权威真实路径清单（主源），
//!      值对象的 lastSessionModified 提供活跃时间（真实数据为毫秒 epoch）
//!   2. ~/.claude/projects/*/ 内的 *.jsonl —— cwd 补充源 + 文件 mtime 活跃度；
//!      无 jsonl 的陈旧残留目录不作为来源
//!   3. ~/.claude/sessions/*.json —— 当前活跃会话标记（cwd + status）
//!
//! 输出统一过滤（项目目录不存在 / 活跃度过旧默认 30 天）后按活跃时间降序；
//! claude.json 缺失或损坏时靠 jsonl 源降级工作。

use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::path::Path;

// ============================================================
// 常量与数据结构
// ============================================================

/// 活跃度过旧阈值（天）：last_active_at 距 now 超过 30 天的项目不入列
pub(crate) const STALE_DAYS: i64 = 30;

/// 视为"活跃会话"的 status 值（真实环境实测 busy/idle，spec 另列 active/running）
const ACTIVE_STATUSES: [&str; 3] = ["active", "running", "busy"];

/// 毫秒/秒级 epoch 分界：毫秒 epoch 自 2001-09 起超过此值，秒级需到 33658 年
const MS_EPOCH_THRESHOLD: i64 = 1_000_000_000_000;

/// 扫描产物：单个项目的探测结果（供命令层合并绑定信息后下发前端）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedProject {
    pub project_path: String,
    pub basename: String,
    /// epoch 秒，各提供时间戳的源取最大
    pub last_active_at: i64,
    /// sessions 源 status 为活跃值 → true
    pub has_active_session: bool,
}

/// 合并中间态：三源按 project_path 归并的活跃时间与活跃会话标记
#[derive(Default)]
struct ProjEntry {
    last_active_at: i64,
    has_active_session: bool,
}

impl ProjEntry {
    fn merge_time(&mut self, t: i64) {
        self.last_active_at = self.last_active_at.max(t);
    }
}

// ============================================================
// 入口：三源扫描 → 合并 → 过滤排序
// ============================================================

/// 扫描 home 下三源并产出项目清单。
/// home = 用户主目录（~/.claude.json 与 ~/.claude/ 的父目录）；now 注入便于测试。
pub fn scan_projects(home: &Path, now: i64) -> Vec<ScannedProject> {
    let claude_dir = home.join(".claude");
    let mut merged: HashMap<String, ProjEntry> = HashMap::new();
    scan_claude_json(&home.join(".claude.json"), &mut merged);
    scan_jsonl_dirs(&claude_dir.join("projects"), &mut merged);
    scan_sessions(&claude_dir.join("sessions"), &mut merged);
    finish(merged, now)
}

// ============================================================
// 源 1：~/.claude.json —— 权威路径清单（主源）
// ============================================================

/// 解析 projects 键：键 = 真实路径，值对象读 lastSessionModified。
/// 文件缺失或损坏时静默返回，靠 jsonl 源降级（spec §8 边界 6）。
fn scan_claude_json(path: &Path, merged: &mut HashMap<String, ProjEntry>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(projects) = doc.get("projects").and_then(|v| v.as_object()) else {
        return;
    };
    for (dir, meta) in projects {
        let t = meta
            .get("lastSessionModified")
            .and_then(parse_epoch_value)
            .unwrap_or(0);
        merged.entry(dir.clone()).or_default().merge_time(t);
    }
}

/// 解析时间戳字段：数值或字符串数字均接受；毫秒级归一化为秒
/// （实测 lastSessionModified / startedAt 均为毫秒 epoch）
fn parse_epoch_value(v: &serde_json::Value) -> Option<i64> {
    let raw = v
        .as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))?;
    Some(if raw > MS_EPOCH_THRESHOLD {
        raw / 1000
    } else {
        raw
    })
}

// ============================================================
// 源 2：~/.claude/projects/*/ 的 *.jsonl —— cwd 补充 + mtime 活跃度
// ============================================================

/// 遍历 projects 下各目录，仅处理内含 jsonl 的目录（无 jsonl 的陈旧残留
/// 目录不作为来源）；cwd 取首条含该字段的行（首行常为 queue-operation
/// 等无 cwd 的元数据行），活跃度取 jsonl 文件 mtime。
fn scan_jsonl_dirs(projects_dir: &Path, merged: &mut HashMap<String, ProjEntry>) {
    let Ok(entries) = fs::read_dir(projects_dir) else {
        return;
    };
    for dir in entries.flatten() {
        let dir_path = dir.path();
        if !dir_path.is_dir() {
            continue;
        }
        let Some(jsonl) = first_jsonl_in_dir(&dir_path) else {
            continue;
        };
        let Some(cwd) = read_first_cwd(&jsonl) else {
            continue;
        };
        let mtime = jsonl_mtime(&jsonl);
        merged.entry(cwd).or_default().merge_time(mtime);
    }
}

/// 目录内按文件名排序取首个 .jsonl（排序保证多文件时的确定性）
fn first_jsonl_in_dir(dir: &Path) -> Option<std::path::PathBuf> {
    let mut jsonls: Vec<_> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    jsonls.sort();
    jsonls.into_iter().next()
}

/// 逐行向后扫描，返回首条含 string cwd 字段的行；读取错误或解析失败的行跳过
fn read_first_cwd(jsonl: &Path) -> Option<String> {
    let file = fs::File::open(jsonl).ok()?;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
            return Some(cwd.to_string());
        }
    }
    None
}

fn jsonl_mtime(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================
// 源 3：~/.claude/sessions/*.json —— 当前活跃会话标记
// ============================================================

/// 读各会话文件的 cwd 与 status（仅 .json，.key 等其他文件忽略）；
/// status 为活跃值时打标记，startedAt（毫秒）一并参与活跃度取最大
fn scan_sessions(sessions_dir: &Path, merged: &mut HashMap<String, ProjEntry>) {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) else {
            continue;
        };
        let e = merged.entry(cwd.to_string()).or_default();
        if let Some(t) = v.get("startedAt").and_then(parse_epoch_value) {
            e.merge_time(t);
        }
        let active = v
            .get("status")
            .and_then(|s| s.as_str())
            .map_or(false, |s| ACTIVE_STATUSES.contains(&s));
        if active {
            e.has_active_session = true;
        }
    }
}

// ============================================================
// 收尾：过滤（目录存在 + 未过旧）→ basename → 活跃时间降序
// ============================================================

fn finish(merged: HashMap<String, ProjEntry>, now: i64) -> Vec<ScannedProject> {
    let mut list: Vec<ScannedProject> = merged
        .into_iter()
        .filter(|(p, e)| Path::new(p).exists() && now - e.last_active_at <= STALE_DAYS * 86_400)
        .map(|(p, e)| ScannedProject {
            basename: basename_of(&p),
            project_path: p,
            last_active_at: e.last_active_at,
            has_active_session: e.has_active_session,
        })
        .collect();
    list.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    list
}

/// 路径末段作为项目名（中文/空格路径正常；无末段的异常路径原样返回）
fn basename_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// 一天的秒数，stale 边界用例的统一刻度
    const DAY: i64 = 86_400;

    // ============================================================
    // fixture 构造器：在临时 home 下铺三源数据
    // ============================================================

    struct Fixture<'a> {
        home: &'a TempDir,
    }

    fn fixture(home: &TempDir) -> Fixture<'_> {
        Fixture { home }
    }

    impl<'a> Fixture<'a> {
        /// 真实创建项目目录（满足"目录存在"过滤）并返回其绝对路径
        fn real_dir(&self, name: &str) -> String {
            let p = self.home.path().join("workspaces").join(name);
            fs::create_dir_all(&p).unwrap();
            p.to_str().unwrap().to_string()
        }

        /// 原样落盘 ~/.claude.json（可写入损坏 JSON 测降级）
        fn write_claude_json_raw(&self, content: &str) {
            fs::write(self.home.path().join(".claude.json"), content).unwrap();
        }

        /// 写合法 ~/.claude.json 的 projects 键；lsm 为任意 Value
        /// （数值 / 字符串数字 / 毫秒时间戳均可传入）
        fn claude_json_projects(&self, entries: &[(&str, Value)]) {
            let mut projects = serde_json::Map::new();
            for (path, lsm) in entries {
                projects.insert(
                    path.to_string(),
                    json!({ "allowedTools": [], "lastSessionModified": lsm }),
                );
            }
            let doc = json!({ "numStartups": 100, "projects": projects });
            fs::write(
                self.home.path().join(".claude.json"),
                serde_json::to_string(&doc).unwrap(),
            )
            .unwrap();
        }

        /// 铺 ~/.claude/projects/<slug>/<id>.jsonl（每行一个 JSON 对象），
        /// 返回 jsonl 路径供 mtime 断言
        fn add_jsonl(&self, slug: &str, id: &str, lines: &[Value]) -> PathBuf {
            let dir = self
                .home
                .path()
                .join(".claude")
                .join("projects")
                .join(slug);
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join(format!("{id}.jsonl"));
            let body = lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&file, body).unwrap();
            file
        }

        /// 铺 ~/.claude/sessions/<id>.json（cwd + status + startedAt 毫秒）；
        /// 同时落一个同名 .key 文件，钉死"非 json 文件被忽略"
        fn add_session(&self, id: &str, cwd: &str, status: &str, started_at_ms: i64) {
            let dir = self.home.path().join(".claude").join("sessions");
            fs::create_dir_all(&dir).unwrap();
            let doc = json!({
                "pid": 100,
                "sessionId": id,
                "cwd": cwd,
                "status": status,
                "startedAt": started_at_ms,
            });
            fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string(&doc).unwrap(),
            )
            .unwrap();
            fs::write(dir.join(format!("{id}.key")), "opaque").unwrap();
        }

        /// 铺 ~/.claude/projects/<slug> 空目录（无任何 jsonl）
        fn add_empty_project_dir(&self, slug: &str) {
            fs::create_dir_all(
                self.home
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(slug),
            )
            .unwrap();
        }
    }

    /// 按路径取结果项，缺失即 panic（测试断言用）
    fn find<'v>(list: &'v [ScannedProject], path: &str) -> &'v ScannedProject {
        list.iter()
            .find(|p| p.project_path == path)
            .unwrap_or_else(|| panic!("未找到项目 {path}"))
    }

    fn jsonl_mtime(path: &std::path::Path) -> i64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    // ============================================================
    // 用例：三源合并
    // ============================================================

    #[test]
    fn merges_three_sources_taking_max_activity() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let proj_a = fx.real_dir("projA");
        let proj_b = fx.real_dir("projB");
        let proj_c = fx.real_dir("projC");

        // 源 1：projA 毫秒时间戳（10 天前），projB 字符串数字（2 天前）
        fx.claude_json_projects(&[
            (&proj_a, json!((now - 10 * DAY) * 1000)),
            (&proj_b, json!((now - 2 * DAY).to_string())),
            (&proj_c, json!(now - 5 * DAY)),
        ]);
        // 源 2：projA 的 jsonl 含 cwd（首行为无 cwd 的元数据行），mtime ≈ now
        let jsonl = fx.add_jsonl("proj-a-slug", "s1", &[
            json!({ "type": "queue-operation", "operation": "enqueue" }),
            json!({ "cwd": proj_a, "sessionId": "s1" }),
        ]);
        // 源 3：projA active、projB idle、projC busy（真实环境实测值）
        fx.add_session("1001", &proj_a, "active", (now - DAY) * 1000);
        fx.add_session("1002", &proj_b, "idle", (now - DAY) * 1000);
        fx.add_session("1003", &proj_c, "busy", (now - 3 * DAY) * 1000);

        let list = scan_projects(home.path(), now);

        // projA：三源取最大 → jsonl mtime（≈now）> sessions startedAt（now-1d）> lsm（now-10d）
        let a = find(&list, &proj_a);
        assert_eq!(a.last_active_at, jsonl_mtime(&jsonl));
        assert!(a.has_active_session);

        // projB：无 jsonl，取 lsm（字符串数字正确解析）与 startedAt 的最大值
        let b = find(&list, &proj_b);
        assert_eq!(b.last_active_at, now - DAY);
        assert!(!b.has_active_session);

        // projC：busy 视为活跃；lsm（now-5d）> startedAt（now-3d）
        let c = find(&list, &proj_c);
        assert_eq!(c.last_active_at, now - 3 * DAY);
        assert!(c.has_active_session);
    }

    #[test]
    fn jsonl_metadata_line_skipped_takes_first_cwd_line() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let proj = fx.real_dir("meta");
        // 首两行均为无 cwd 的元数据行，第三行才含 cwd —— 必须向后扫描取首条含 cwd 行
        fx.add_jsonl("meta-slug", "s9", &[
            json!({ "type": "queue-operation", "operation": "enqueue", "timestamp": "2026-09-29T08:36:37.563Z" }),
            json!({ "type": "summary", "summary": "no cwd here" }),
            json!({ "parentUuid": null, "cwd": proj, "sessionId": "s9" }),
        ]);

        let list = scan_projects(home.path(), now);
        let p = find(&list, &proj);
        assert!(p.last_active_at >= now - 60);
    }

    // ============================================================
    // 用例：降级（claude.json 损坏 / 缺失）
    // ============================================================

    #[test]
    fn jsonl_cwd_supplies_missing_projects() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let proj = fx.real_dir("projX");
        fx.write_claude_json_raw("not json{ 损坏");
        fx.add_jsonl("proj-x", "s1", &[json!({ "cwd": proj })]);

        let list = scan_projects(home.path(), now);
        let p = find(&list, &proj);
        assert!(p.last_active_at >= now - 60);
        assert!(!p.has_active_session);
    }

    #[test]
    fn missing_claude_json_degrades_gracefully() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let proj = fx.real_dir("projY");
        // 不写 claude.json
        fx.add_jsonl("proj-y", "s1", &[json!({ "cwd": proj })]);

        let list = scan_projects(home.path(), now);
        find(&list, &proj);
    }

    // ============================================================
    // 用例：过滤（目录不存在 / 陈旧 / 无 jsonl 目录）
    // ============================================================

    #[test]
    fn nonexistent_project_dir_filtered() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let ghost = home.path().join("never-created-dir");
        fx.claude_json_projects(&[(ghost.to_str().unwrap(), json!(now))]);

        let list = scan_projects(home.path(), now);
        assert!(list.is_empty());
    }

    #[test]
    fn stale_project_filtered_29d_kept() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let stale = fx.real_dir("stale-31d");
        let fresh = fx.real_dir("fresh-29d");
        fx.claude_json_projects(&[
            (&stale, json!(now - 31 * DAY)),
            (&fresh, json!((now - 29 * DAY).to_string())),
        ]);

        let list = scan_projects(home.path(), now);
        assert_eq!(STALE_DAYS, 30);
        assert!(list.iter().all(|p| p.project_path != stale)); // 31 天 → 剔除
        find(&list, &fresh); // 29 天 → 保留
    }

    #[test]
    fn dir_without_jsonl_not_a_source() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        fx.add_empty_project_dir("empty-residue");

        let list = scan_projects(home.path(), now);
        assert!(list.is_empty());
    }

    // ============================================================
    // 用例：basename 与排序
    // ============================================================

    #[test]
    fn chinese_path_basename() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let cn = fx.real_dir("我的 项目");
        fx.claude_json_projects(&[(&cn, json!(now))]);

        let list = scan_projects(home.path(), now);
        let p = find(&list, &cn);
        // basename 为路径末段全名（中文与空格逐字节保留，计划原文的期望值 "项目" 系笔误）
        assert_eq!(p.basename, "我的 项目");
        assert_eq!(p.project_path, cn);
    }

    #[test]
    fn sorted_by_last_active_desc() {
        let home = TempDir::new().unwrap();
        let fx = fixture(&home);
        let now = chrono::Utc::now().timestamp();
        let p1 = fx.real_dir("one-day-ago");
        let p2 = fx.real_dir("two-day-ago");
        let p3 = fx.real_dir("three-day-ago");
        // 故意乱序铺入
        fx.claude_json_projects(&[
            (&p3, json!(now - 3 * DAY)),
            (&p1, json!(now - DAY)),
            (&p2, json!(now - 2 * DAY)),
        ]);

        let list = scan_projects(home.path(), now);
        let paths: Vec<&str> = list.iter().map(|p| p.project_path.as_str()).collect();
        assert_eq!(paths, vec![p1.as_str(), p2.as_str(), p3.as_str()]);
    }
}
