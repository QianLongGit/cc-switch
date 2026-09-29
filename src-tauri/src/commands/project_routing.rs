//! 项目路由命令层（spec §6.1 第 2 项 / 计划 T9）。
//!
//! 三命令均为「薄壳 command + 可测核心」结构：command 只取 State 委托核心
//! 函数，核心函数以 `&Database` + 注入 home/now 为参数，集成测试不经
//! Tauri 运行时即可全链路验证（scanner 三源合并 + DB 绑定 + 同步状态）。
//!
//! 写序约定（Review Focus 2 闭环）：
//! - set：**先 DB 后文件**——文件写失败时 Err 上抛、DB 行保留，由
//!   sync_status 的 out_of_sync 兜底修复（T4 测试已锁此语义）
//! - clear：**先 DB 后文件**——DB 行删除后移除文件目标行；文件缺失 /
//!   无目标行时静默 Ok（幂等，用户文件不动）

use crate::config::get_home_dir;
use crate::database::Database;
use crate::error::AppError;
use crate::services::project_scanner::scan_projects;
use crate::services::settings_local_writer::{read_sync_status, remove_project_header, set_project_header};
use crate::store::AppState;
use std::collections::HashMap;
use std::path::Path;
use tauri::State;

// ===================================================================
// DTO（T10 前端 invoke 契约）
// ===================================================================

/// 单条项目路由信息：扫描产物（路径 / basename / 活跃时间 / 活跃会话）
/// 合并 DB 绑定（id / 名称 / 是否有效）与文件同步三态。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRouteInfo {
    pub project_path: String,
    pub basename: String,
    pub last_active_at: i64,
    pub has_active_session: bool,
    pub bound_provider_id: Option<String>,
    pub bound_provider_name: Option<String>,
    /// false = 绑定的供应商已删（§8 边界 9"已失效"态，不静默清空展示）
    pub bound_provider_valid: bool,
    /// `"synced"` | `"out_of_sync"` | `"orphan_header"`
    pub sync_status: String,
}

// ===================================================================
// 可测核心（command 只是薄壳）
// ===================================================================

/// 扫描 home 三源并合并 DB 绑定与同步状态（list_projects 的可测核心）。
///
/// 以扫描结果为主体逐项合并：绑定行仅作为属性附加，扫描之外的项目路径
/// 不产生条目（目录已删的项目自然出列，DB 残留行由扫描过滤兜住）。
pub fn build_project_list(
    db: &Database,
    home: &Path,
    now: i64,
) -> Result<Vec<ProjectRouteInfo>, AppError> {
    let route_rows = db.list_routes("claude")?;
    let routes: HashMap<&str, &str> = route_rows
        .iter()
        .map(|(path, id)| (path.as_str(), id.as_str()))
        .collect();

    Ok(scan_projects(home, now)
        .into_iter()
        .map(|sp| {
            let bound = routes.get(sp.project_path.as_str()).copied();
            // 悬空绑定（供应商已删）：id 照透出，valid=false 交前端标"已失效"；
            // DB Err 同按失效降级（list 不因单条查询失败整体报错），留告警可观测
            let bound_provider = bound.and_then(|pid| {
                db.get_provider_by_id(pid, "claude")
                    .inspect_err(|e| {
                        log::warn!("查询绑定供应商 {pid} 失败，按已失效降级展示: {e}");
                    })
                    .ok()
                    .flatten()
            });
            let bound_provider_valid = match bound {
                Some(_) => bound_provider.is_some(),
                None => true,
            };
            // DB 有绑定即传该路径本身，供文件行值对比（bound=None 检孤儿行）
            let sync_status = read_sync_status(
                &sp.project_path,
                bound.map(|_| sp.project_path.as_str()),
            )
            .as_str()
            .to_string();
            ProjectRouteInfo {
                project_path: sp.project_path,
                basename: sp.basename,
                last_active_at: sp.last_active_at,
                has_active_session: sp.has_active_session,
                bound_provider_id: bound.map(str::to_string),
                bound_provider_name: bound_provider.map(|p| p.name),
                bound_provider_valid,
                sync_status,
            }
        })
        .collect())
}

/// 绑定项目 → 供应商（set_project_route 的可测核心，先 DB 后文件）。
///
/// 前置校验供应商存在：绑定到不存在的 id 即刻失败，不留悬空行；
/// 文件写失败时 Err 上抛、DB 行保留（sync_status 兜底修复）。
pub fn set_route(db: &Database, project_path: &str, provider_id: &str) -> Result<(), AppError> {
    if db.get_provider_by_id(provider_id, "claude")?.is_none() {
        return Err(AppError::InvalidInput(format!(
            "供应商 {provider_id} 不存在，无法绑定项目路由"
        )));
    }
    db.insert_or_update_route(project_path, "claude", provider_id)?;
    set_project_header(project_path, db)
}

/// 解除绑定（clear_project_route 的可测核心，DB 行 + 文件目标行双清）。
pub fn clear_route(db: &Database, project_path: &str) -> Result<(), AppError> {
    db.delete_route(project_path, "claude")?;
    remove_project_header(project_path, db)
}

// ===================================================================
// Tauri 命令（薄壳：State 装配 + 委托核心）
// ===================================================================

/// 获取项目路由列表（扫描 + 绑定 + 同步状态合并）
#[tauri::command]
pub fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectRouteInfo>, AppError> {
    build_project_list(&state.db, &get_home_dir(), chrono::Utc::now().timestamp())
}

/// 设置项目路由（DB + settings.local.json 双写）
#[tauri::command]
pub fn set_project_route(
    state: State<'_, AppState>,
    project_path: String,
    provider_id: String,
) -> Result<(), AppError> {
    set_route(&state.db, &project_path, &provider_id)
}

/// 清除项目路由（DB 行 + 文件目标行双清）
#[tauri::command]
pub fn clear_project_route(
    state: State<'_, AppState>,
    project_path: String,
) -> Result<(), AppError> {
    clear_route(&state.db, &project_path)
}
