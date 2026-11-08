//! 使用统计相关命令

use std::sync::atomic::AtomicBool;

use crate::error::AppError;
use crate::services::model_pricing::{ModelPricingInfo, ModelsDevSyncConfig, ModelsDevSyncState};
use crate::services::usage_remote::{self, RemoteUsageSyncStatus};
use crate::services::usage_stats::*;
use crate::settings::SshHostConfig;
use crate::store::AppState;
use tauri::{Emitter, State};

/* ===== SSH 远程作用域（设计 §4.4）===== */

/// hostId 字符集校验：hostId 会直接拼进聚合 SQL 的 LIKE / 等值子句
/// （[`host_data_source_filter`]），只放行应用生成的 UUID 安全字符
/// （字母数字与连字符），杜绝引号与 LIKE 通配符。
fn validate_host_id(host_id: &str) -> Result<(), AppError> {
    if host_id.is_empty()
        || !host_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AppError::InvalidInput("无效的 SSH 主机标识".to_string()));
    }
    Ok(())
}

/// 在主机清单里按 id 查找（纯函数，供命令层与测试共用）。
fn find_remote_host(
    hosts: Option<Vec<SshHostConfig>>,
    host_id: &str,
) -> Result<SshHostConfig, AppError> {
    validate_host_id(host_id)?;
    hosts
        .and_then(|hosts| hosts.into_iter().find(|host| host.id == host_id))
        .ok_or_else(|| AppError::InvalidInput("SSH 主机不存在或已移除".to_string()))
}

/// 从全局 settings 解析 hostId 对应的主机配置。
fn resolve_remote_host(host_id: &str) -> Result<SshHostConfig, AppError> {
    find_remote_host(crate::settings::get_settings().ssh_hosts, host_id)
}

/// hostId 有值的查询命令共用前置：解析主机 → 轻同步（清单 diff + 增量，
/// 无变化零流量）→ 返回聚合用的 host 维度。同步失败透传（前端展示错误态
/// 并可重试），远程自动刷新链路由此内建：前端定时 invalidate → 查询重跑
/// → 命令层轻同步。
///
/// 轻同步以闭包注入（[`prepare_remote_scope`] 提供生产实现），测试注入
/// 计数闭包断言「hostId=Some 必触发轻同步」。
fn prepare_remote_scope_with(
    db: &crate::database::Database,
    hosts: Option<Vec<SshHostConfig>>,
    host_id: &str,
    light_sync: impl FnOnce(&crate::database::Database, &SshHostConfig) -> Result<(), AppError>,
) -> Result<String, AppError> {
    let host = find_remote_host(hosts, host_id)?;
    light_sync(db, &host)?;
    Ok(host.id)
}

/// [`prepare_remote_scope_with`] 的生产形态：读全局 settings + 轻同步走
/// 远程用量引擎（force=false，取消标志只读全局）。
fn prepare_remote_scope(db: &crate::database::Database, host_id: &str) -> Result<String, AppError> {
    prepare_remote_scope_with(
        db,
        crate::settings::get_settings().ssh_hosts,
        host_id,
        |db, host| {
            let cancel = AtomicBool::new(false);
            usage_remote::sync_host_usage(db, host, false, &mut |_| {}, &cancel).map(|_| ())
        },
    )
}

/// 数据库操作放到阻塞线程池里跑：同步命令在 Tauri 2 里跑在主线程上。
/// 不仅聚合扫描会卡窗口，启动定价同步等小查询等待会话同步持有的锁时也会。
async fn run_db_query<T, F>(state: &State<'_, AppState>, query: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&crate::database::Database) -> Result<T, AppError> + Send + 'static,
{
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || query(&db))
        .await
        .map_err(|error| AppError::Message(format!("用量查询任务失败: {error}")))?
}

/// 获取使用量汇总。`hostId` 有值时先轻同步该远程主机再按 host 维度聚合；
/// 缺省本机视图（排除一切 `ssh:` 来源行），行为与既有完全一致。
#[tauri::command]
pub async fn get_usage_summary(
    state: State<'_, AppState>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    app_type: Option<String>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<UsageSummary, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_usage_summary(
            start_date,
            end_date,
            app_type.as_deref(),
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取单个会话的使用量汇总（会话阅读页头部）
#[tauri::command]
pub async fn get_session_usage_summary(
    state: State<'_, AppState>,
    app_type: String,
    session_id: String,
) -> Result<UsageSummary, AppError> {
    run_db_query(&state, move |db| {
        db.get_session_usage_summary(&app_type, &session_id)
    })
    .await
}

/// 获取按 app_type 拆分的使用量汇总（hostId 语义同 [`get_usage_summary`]）
#[tauri::command]
pub async fn get_usage_summary_by_app(
    state: State<'_, AppState>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<Vec<UsageSummaryByApp>, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_usage_summary_by_app(
            start_date,
            end_date,
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取每日趋势（hostId 语义同 [`get_usage_summary`]）
#[tauri::command]
pub async fn get_usage_trends(
    state: State<'_, AppState>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    app_type: Option<String>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<Vec<DailyStats>, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_daily_trends(
            start_date,
            end_date,
            app_type.as_deref(),
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取最早有用量记录的日期（本地 `YYYY-MM-DD`），没有记录时为 null
/// （hostId 语义同 [`get_usage_summary`]）
#[tauri::command]
pub async fn get_usage_first_date(
    state: State<'_, AppState>,
    app_type: Option<String>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<Option<String>, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_first_usage_date(
            app_type.as_deref(),
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取 Provider 统计（hostId 语义同 [`get_usage_summary`]）
#[tauri::command]
pub async fn get_provider_stats(
    state: State<'_, AppState>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    app_type: Option<String>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<Vec<ProviderStats>, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_provider_stats(
            start_date,
            end_date,
            app_type.as_deref(),
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取模型统计（hostId 语义同 [`get_usage_summary`]）
#[tauri::command]
pub async fn get_model_stats(
    state: State<'_, AppState>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    app_type: Option<String>,
    provider_name: Option<String>,
    model: Option<String>,
    hostId: Option<String>,
) -> Result<Vec<ModelStats>, AppError> {
    run_db_query(&state, move |db| {
        let host_filter = match hostId.as_deref() {
            Some(host_id) => Some(prepare_remote_scope(db, host_id)?),
            None => None,
        };
        db.get_model_stats(
            start_date,
            end_date,
            app_type.as_deref(),
            provider_name.as_deref(),
            model.as_deref(),
            host_filter.as_deref(),
        )
    })
    .await
}

/// 获取请求日志列表。`hostId` 有值时先轻同步再过滤（hostId 与 filters
/// 各自独立：filters 不含 host 维度，避免前端两处传参的口径漂移）
#[tauri::command]
pub async fn get_request_logs(
    state: State<'_, AppState>,
    filters: LogFilters,
    page: u32,
    page_size: u32,
    hostId: Option<String>,
) -> Result<PaginatedLogs, AppError> {
    run_db_query(&state, move |db| {
        let mut filters = filters;
        match hostId.as_deref() {
            Some(host_id) => {
                filters.host_id = Some(prepare_remote_scope(db, host_id)?);
            }
            None => filters.host_id = None,
        }
        db.get_request_logs(&filters, page, page_size)
    })
    .await
}

/// 获取单个请求详情
#[tauri::command]
pub async fn get_request_detail(
    state: State<'_, AppState>,
    request_id: String,
) -> Result<Option<RequestLogDetail>, AppError> {
    run_db_query(&state, move |db| db.get_request_detail(&request_id)).await
}

/// 获取模型定价列表
#[tauri::command]
pub async fn get_model_pricing(
    state: State<'_, AppState>,
) -> Result<Vec<ModelPricingInfo>, AppError> {
    run_db_query(&state, get_model_pricing_blocking).await
}

fn get_model_pricing_blocking(
    db: &crate::database::Database,
) -> Result<Vec<ModelPricingInfo>, AppError> {
    log::info!("获取模型定价列表");
    db.ensure_model_pricing_seeded()?;
    crate::services::model_pricing::sync_local_model_pricing(db)?;

    let conn = crate::database::lock_conn!(db.conn);

    // 检查表是否存在
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='model_pricing'",
            [],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )
        .unwrap_or(false);

    if !table_exists {
        log::error!("model_pricing 表不存在,可能需要重启应用以触发数据库迁移");
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT model_id, display_name, input_cost_per_million, output_cost_per_million,
                cache_read_cost_per_million, cache_creation_cost_per_million
         FROM model_pricing
         ORDER BY display_name",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ModelPricingInfo {
            model_id: row.get(0)?,
            display_name: row.get(1)?,
            input_cost_per_million: row.get(2)?,
            output_cost_per_million: row.get(3)?,
            cache_read_cost_per_million: row.get(4)?,
            cache_creation_cost_per_million: row.get(5)?,
        })
    })?;

    let mut pricing = Vec::new();
    for row in rows {
        pricing.push(row?);
    }

    log::info!("成功获取 {} 条模型定价数据", pricing.len());
    Ok(pricing)
}

/// 更新模型定价
#[tauri::command]
pub async fn update_model_pricing(
    state: State<'_, AppState>,
    model_id: String,
    display_name: String,
    input_cost: String,
    output_cost: String,
    cache_read_cost: String,
    cache_creation_cost: String,
) -> Result<(), AppError> {
    run_db_query(&state, move |db| {
        crate::services::model_pricing::update_model_pricing(
            db,
            ModelPricingInfo {
                model_id,
                display_name,
                input_cost_per_million: input_cost,
                output_cost_per_million: output_cost,
                cache_read_cost_per_million: cache_read_cost,
                cache_creation_cost_per_million: cache_creation_cost,
            },
        )?;
        Ok(())
    })
    .await
}

/// 批量更新模型定价（models.dev 自动同步仅触发一次历史成本回填）
#[tauri::command]
pub async fn update_model_pricing_batch(
    state: State<'_, AppState>,
    entries: Vec<ModelPricingInfo>,
) -> Result<usize, AppError> {
    run_db_query(&state, move |db| {
        crate::services::model_pricing::update_model_pricing_batch(db, entries)
    })
    .await
}

#[tauri::command]
pub async fn get_models_dev_sync_config(
    state: State<'_, AppState>,
) -> Result<ModelsDevSyncState, AppError> {
    run_db_query(
        &state,
        crate::services::model_pricing::get_models_dev_sync_state,
    )
    .await
}

#[tauri::command]
pub async fn save_models_dev_sync_config(
    state: State<'_, AppState>,
    config: ModelsDevSyncConfig,
) -> Result<(), AppError> {
    run_db_query(&state, move |db| {
        crate::services::model_pricing::save_models_dev_sync_config(db, config)
    })
    .await
}

#[tauri::command]
pub async fn record_models_dev_sync_result(
    state: State<'_, AppState>,
    synced_at: Option<i64>,
    error: Option<String>,
) -> Result<(), AppError> {
    run_db_query(&state, move |db| {
        crate::services::model_pricing::record_models_dev_sync_result(db, synced_at, error)
    })
    .await
}

/// 拉取 models.dev 数据，走后端代理感知 HTTP 客户端，避免 WebView 直连在代理环境下超时
#[tauri::command]
pub async fn fetch_models_dev() -> Result<String, String> {
    crate::services::model_pricing::fetch_models_dev().await
}

/// 检查 Provider 使用限额
#[tauri::command]
pub async fn check_provider_limits(
    state: State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<crate::services::usage_stats::ProviderLimitStatus, AppError> {
    run_db_query(&state, move |db| {
        db.check_provider_limits(&provider_id, &app_type)
    })
    .await
}

/// 删除模型定价
#[tauri::command]
pub async fn delete_model_pricing(
    state: State<'_, AppState>,
    model_id: String,
) -> Result<(), AppError> {
    run_db_query(&state, move |db| {
        crate::services::model_pricing::delete_model_pricing(db, &model_id)?;
        log::info!("已删除模型定价: {model_id}");
        Ok(())
    })
    .await
}

/// 同步远程主机用量（手动首扫 / 增量同步，设计 §4.4）。
///
/// `force=true` 走首扫（拉取 90 天窗口内全部无游标文件）；每处理完一个文件
/// emit `remote-usage-sync-status` 进度事件（载荷 [`RemoteUsageSyncStatus`]）。
/// 入口重置取消标志（上一轮取消的残留不拦本轮）；取消或单文件失败都不毁
/// 整轮，返回已推进的部分进度。
#[tauri::command]
pub async fn sync_remote_usage<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
    hostId: String,
    force: bool,
) -> Result<RemoteUsageSyncStatus, AppError> {
    let host = resolve_remote_host(&hostId)?;
    usage_remote::reset_cancel_flag();
    let db = state.db.clone();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cancel = AtomicBool::new(false);
        usage_remote::sync_host_usage(
            &db,
            &host,
            force,
            &mut |status| {
                // emit 失败只影响进度展示，不影响同步本身
                let _ = app.emit("remote-usage-sync-status", status);
            },
            &cancel,
        )
    })
    .await
    .map_err(|error| AppError::Message(format!("远程用量同步任务失败: {error}")))?
}

/// 取消进行中的远程用量同步：引擎在下一个文件边界停止并返回部分进度
/// （已入库数据与已推进游标保留，未处理文件下轮续扫）。
#[tauri::command]
pub async fn cancel_remote_usage_sync() -> Result<(), AppError> {
    usage_remote::request_cancel_remote_usage_sync();
    Ok(())
}

/// 清单 diff 单往返统计待同步文件数 / 待拉总字节（首扫预告与「有更新」
/// 提示共用）：不拉取文件内容、不写库。
#[tauri::command]
pub async fn peek_remote_usage(
    state: State<'_, AppState>,
    hostId: String,
) -> Result<RemoteUsageSyncStatus, AppError> {
    let host = resolve_remote_host(&hostId)?;
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || usage_remote::peek_host_usage(&db, &host))
        .await
        .map_err(|error| AppError::Message(format!("远程用量统计任务失败: {error}")))?
}

/// 手动触发会话日志同步
#[tauri::command]
pub async fn sync_session_usage(
    state: State<'_, AppState>,
) -> Result<crate::services::session_usage::SessionSyncResult, AppError> {
    let db = state.db.clone();
    let _guard = crate::services::session_usage::session_sync_mutex()
        .lock()
        .await;
    tauri::async_runtime::spawn_blocking(move || {
        crate::services::session_usage::sync_all_unlocked(&db)
    })
    .await
    .map_err(|error| AppError::Message(format!("会话用量同步任务失败: {error}")))
}

/// 会话日志扫描（后台定时或手动同步）最近一次完成的时间，毫秒时间戳；
/// 本次启动后还没扫过时为 `null`。
#[tauri::command]
pub fn get_session_usage_last_sync() -> Option<i64> {
    crate::services::session_usage::last_sync_completed_at()
}

/// Codex reset 成功后，无论重导是否导入新行或返回错误，都必须通知前端刷新。
/// 调用方应只在 reset 成功后调用，避免把未发生的数据变更误报为重建完成。
fn finish_codex_rebuild(
    result: Result<crate::services::session_usage::SessionSyncResult, AppError>,
) -> Result<crate::services::session_usage::SessionSyncResult, AppError> {
    crate::usage_events::notify_log_recorded();
    result
}

/// 备份数据库后，仅重建 Codex session 用量。锁覆盖 backup → reset → import
/// 整个序列，避免后台同步在清理和重导之间插入数据。
#[tauri::command]
pub async fn rebuild_codex_usage(
    state: State<'_, AppState>,
) -> Result<crate::services::session_usage::SessionSyncResult, AppError> {
    let db = state.db.clone();
    let _guard = crate::services::session_usage::session_sync_mutex()
        .lock()
        .await;
    tauri::async_runtime::spawn_blocking(move || {
        db.backup_database_file()?;
        db.reset_codex_usage()?;
        let result = crate::services::session_usage_codex::sync_codex_usage(&db);
        finish_codex_rebuild(result)
    })
    .await
    .map_err(|error| AppError::Message(format!("Codex 用量重建任务失败: {error}")))?
}

/// 获取数据来源分布
#[tauri::command]
pub async fn get_usage_data_sources(
    state: State<'_, AppState>,
) -> Result<Vec<crate::services::session_usage::DataSourceSummary>, AppError> {
    run_db_query(
        &state,
        crate::services::session_usage::get_data_source_breakdown,
    )
    .await
}

/// 取 USD 对各显示币种的汇率：主源 open.er-api.com，失败退 floatrates.com。
/// 只用于金额换算显示；失败时前端保留上次缓存或手填的汇率。
#[tauri::command]
pub async fn fetch_exchange_rates(
) -> Result<crate::services::exchange_rates::FetchedExchangeRates, AppError> {
    crate::services::exchange_rates::fetch_usd_rates().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_host(id: &str) -> SshHostConfig {
        SshHostConfig {
            id: id.to_string(),
            label: id.to_string(),
            host: "example.com".to_string(),
            user: None,
            port: None,
            identity_file: None,
        }
    }

    // hostId=Some 必触发轻同步（闭包注入断言），且返回 host 维度供聚合过滤
    #[test]
    fn remote_scope_triggers_light_sync_and_returns_host_id() {
        let db = crate::database::Database::memory().unwrap();
        let hosts = Some(vec![test_host("h1"), test_host("h2")]);
        let mut called = Vec::new();
        let scope = prepare_remote_scope_with(&db, hosts, "h2", |_db, host| {
            called.push(host.id.clone());
            Ok(())
        })
        .expect("合法主机应解析成功");
        assert_eq!(
            called,
            vec!["h2".to_string()],
            "hostId 有值时必须先执行轻同步"
        );
        assert_eq!(scope, "h2", "聚合过滤维度 = 主机 id");
    }

    // hostId 不存在 / 字符集非法 → Err（不触发任何同步）
    #[test]
    fn remote_scope_rejects_unknown_or_unsafe_host_id() {
        let hosts = || Some(vec![test_host("h1")]);
        assert!(find_remote_host(hosts(), "no-such-host").is_err());
        // 引号与 LIKE 通配符直接拒绝：hostId 会拼进 SQL 子句
        assert!(find_remote_host(hosts(), "h1'--").is_err());
        assert!(find_remote_host(hosts(), "h%1").is_err());
    }

    #[test]
    fn codex_rebuild_notifies_when_reimport_is_empty() {
        crate::usage_events::take_test_notify_count();

        let result = finish_codex_rebuild(Ok(
            crate::services::session_usage::SessionSyncResult::default(),
        ))
        .expect("空重导应成功");

        assert_eq!(result.imported, 0);
        assert_eq!(crate::usage_events::take_test_notify_count(), 1);
    }

    #[test]
    fn codex_rebuild_notifies_when_reimport_fails_after_reset() {
        crate::usage_events::take_test_notify_count();

        let result = finish_codex_rebuild(Err(AppError::Message(
            "synthetic reimport failure".to_string(),
        )));

        assert!(result.is_err());
        assert_eq!(crate::usage_events::take_test_notify_count(), 1);
    }
}
