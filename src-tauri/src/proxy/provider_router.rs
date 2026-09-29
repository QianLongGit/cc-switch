//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{AllowResult, CircuitBreaker, CircuitBreakerConfig};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Codex Official requests carry the selected account's native Authorization
/// header. Reusing that request against another account card would cross the
/// account boundary, so these cards must never participate in provider retry.
pub(crate) fn provider_supports_failover(app_type: &str, provider: &Provider) -> bool {
    app_type != AppType::Codex.as_str()
        || !crate::proxy::providers::is_codex_official_provider(provider)
}

/// 项目绑定路由的重排结果
pub(crate) struct ProjectRoutePlan {
    /// 按尝试顺序排列的候选链
    pub candidates: Vec<Provider>,
    /// 绑定命中且绑定者位于候选链首位（本请求实际出口由绑定决定，供归因与全局切换排除使用）
    pub routed: bool,
}

/// 纯函数：项目绑定语义矩阵重排（spec §5）
///
/// - bound=None：base 原样透传（现状语义）
/// - failover 关 + 绑定：单路由 [绑定者]，忽略 bound_available（跟随 failover 关分支不查熔断的现状）
/// - failover 开 + 绑定者健康：绑定者置首，其余公共队列依序保留（去重，软绑定降级）
/// - failover 开 + 绑定者熔断 Open：让位公共队列 base 原样（救场的对偶：绑定者也不可用时不顶替）
pub(crate) fn apply_project_binding(
    base: Vec<Provider>,
    bound: Option<Provider>,
    failover_enabled: bool,
    bound_available: bool,
) -> ProjectRoutePlan {
    match bound {
        None => ProjectRoutePlan {
            candidates: base,
            routed: false,
        },
        Some(p) if !failover_enabled => ProjectRoutePlan {
            candidates: vec![p],
            routed: true,
        },
        Some(p) if bound_available => {
            let id = p.id.clone();
            let mut v = vec![p];
            v.extend(base.into_iter().filter(|q| q.id != id));
            ProjectRoutePlan {
                candidates: v,
                routed: true,
            }
        }
        Some(_) => ProjectRoutePlan {
            candidates: base,
            routed: false,
        },
    }
}

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 选择可用的供应商（支持故障转移）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;
        let current_id = AppType::from_str(app_type)
            .ok()
            .and_then(|app_enum| {
                crate::settings::get_effective_current_provider(&self.db, &app_enum)
                    .ok()
                    .flatten()
            })
            .or_else(|| self.db.get_current_provider(app_type).ok().flatten());
        let current_provider = current_id
            .as_deref()
            .map(|id| self.db.get_provider_by_id(id, app_type))
            .transpose()?
            .flatten();

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled
            && current_provider
                .as_ref()
                .is_some_and(|provider| !provider_supports_failover(app_type, provider))
        {
            // A selected Codex Official account is an explicit account choice.
            // Keep it as a single route even if an old failover setting remains
            // enabled; retrying would reuse its inbound token for another card.
            total_providers = 1;
            result.push(current_provider.expect("checked above"));
        } else if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };
                if !provider_supports_failover(app_type, &provider) {
                    continue;
                }
                total_providers += 1;

                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

                if breaker.is_available().await {
                    result.push(provider);
                } else {
                    circuit_open_count += 1;
                }
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            if let Some(current) = current_provider {
                total_providers = 1;
                result.push(current);
            }
        }

        if result.is_empty() {
            if total_providers > 0 && circuit_open_count == total_providers {
                log::warn!("[{app_type}] [FO-004] 所有供应商均已熔断");
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    /// 为单次请求选择候选供应商链（项目绑定路由入口，spec §5）
    ///
    /// - `project_path=None` 或 `app_type` 非 claude：行为与 `select_providers` 完全一致
    ///   （逐分支回退现状，细则 8；Codex Official 强制单路由分支由其内部原样保留）
    /// - 救场分支（细则 1）：failover 开 + 绑定者健康 + 公共队列全熔断/为空
    ///   （`select_providers` 返回 Err）→ 候选链 = [绑定者]，绑定请求不收到该 Err
    /// - 返回 routed：绑定命中且绑定者位于候选链首位（细则 7 的全局切换排除标记由此而来）
    pub async fn select_providers_for_request(
        &self,
        app_type: &str,
        project_path: Option<&str>,
    ) -> Result<(Vec<Provider>, bool), AppError> {
        // 1. 现状候选链：Result 暂存不立即上抛——公共队列全熔断的 Err 留给救场判定（细则 1）
        let base: Result<Vec<Provider>, AppError> = self.select_providers(app_type).await;

        // 2. 非 claude / 无项目标识：原样透传（Ok → routed=false，Err 直接上抛）
        let Some(project_path) = project_path.filter(|_| app_type == AppType::Claude.as_str())
        else {
            return base.map(|v| (v, false));
        };

        // 3. 绑定行缺失（项目从未绑定）→ 原样透传；DB Err 同样按无绑定降级（热路径不阻断），留告警可观测
        let Some(bound_id) = self
            .db
            .find_route(project_path, app_type)
            .inspect_err(|e| {
                log::warn!("[{app_type}] project_routes 查询失败，按无绑定降级走全局队列: {e}");
            })
            .ok()
            .flatten()
        else {
            return base.map(|v| (v, false));
        };

        // 4. 防御：绑定供应商已删（悬空行）→ 原样透传（spec §8 边界 1 的兜底路径）
        //    注意与步骤 3 的差异化设计：find_route 查不到行是可降级语义（无绑定），.ok() 吞掉
        //    DB Err 一并按"无绑定"回退；而这里供应商 id 存在却取不到实体，若把 DB Err 也吞掉，
        //    数据库故障会被伪装成"绑定悬空"静默走全局队列——故 Err 直接上抛不可降级。
        let Some(bound) = self.db.get_provider_by_id(&bound_id, app_type)? else {
            return base.map(|v| (v, false));
        };

        // 5. failover 开关每请求直查 DB（细则 2；select_providers 内部那次读不可复用，重读一次与现有模式一致）
        let failover_enabled = self
            .db
            .get_proxy_config_for_app(app_type)
            .await
            .map(|config| config.auto_failover_enabled)
            .unwrap_or(false);

        // 6. 绑定者熔断可用性：仅 failover 开时查询（failover 关分支不查熔断，跟随现状）；
        //    is_available 只读状态、不消耗 HalfOpen 名额（与队列过滤处的同类用法一致）
        let bound_available = if failover_enabled {
            self.get_or_create_circuit_breaker(&format!("{app_type}:{bound_id}"))
                .await
                .is_available()
                .await
        } else {
            true
        };

        // 7. 统一走纯函数重排：Err（公共队列全熔断/无供应商）视为空候选链进入救场判定，
        //    纯函数产出空候选链时上抛原 Err（绑定者也不可用 → 保持现状错误语义）
        match base {
            Ok(candidates) => {
                let plan = apply_project_binding(
                    candidates,
                    Some(bound),
                    failover_enabled,
                    bound_available,
                );
                Ok((plan.candidates, plan.routed))
            }
            Err(e) => {
                let plan = apply_project_binding(
                    Vec::new(),
                    Some(bound),
                    failover_enabled,
                    bound_available,
                );
                if plan.candidates.is_empty() {
                    Err(e)
                } else {
                    Ok((plan.candidates, plan.routed))
                }
            }
        }
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    /// 注意：调用方必须在请求结束后通过 `record_result()` 释放 HalfOpen 名额，
    /// 否则会导致该 Provider 长时间无法进入探测状态。
    pub async fn allow_provider_request(&self, provider_id: &str, app_type: &str) -> AllowResult {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.allow_request().await
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
    }

    /// 仅释放 HalfOpen permit，不影响健康统计（neutral 接口）
    ///
    /// 用于整流器等场景：请求结果不应计入 Provider 健康度，
    /// 但仍需释放占用的探测名额，避免 HalfOpen 状态卡死
    pub async fn release_permit_neutral(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
    ) {
        if !used_half_open_permit {
            return;
        }
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.release_half_open_permit();
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 更新指定应用已创建熔断器的配置（热更新）
    pub async fn update_app_configs(&self, app_type: &str, config: CircuitBreakerConfig) {
        let prefix = format!("{app_type}:");
        let breakers = self.circuit_breakers.read().await;
        for (key, breaker) in breakers.iter() {
            if key.starts_with(&prefix) {
                breaker.update_config(config.clone()).await;
            }
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = key.split(':').next().unwrap_or("claude");

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    fn managed_codex_official(id: &str, account_id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some(account_id.to_string()),
            }),
            ..Default::default()
        });
        provider
    }

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn codex_official_current_stays_single_route_when_failover_is_stale() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, official.id);
    }

    #[tokio::test]
    #[serial]
    async fn stale_codex_official_queue_entries_are_not_retry_targets() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let current = Provider::with_id(
            "third-party".to_string(),
            "Third Party".to_string(),
            json!({}),
            None,
        );
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &current).unwrap();
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &current.id).unwrap();
        db.add_to_failover_queue("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback"]
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude").await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_release_permit_neutral_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed);
        assert!(first.used_half_open_permit);

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed);

        // 使用 release_permit_neutral 释放名额（不影响健康统计）
        router
            .release_permit_neutral("a", "claude", first.used_half_open_permit)
            .await;

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed);
        assert!(third.used_half_open_permit);
    }

    // ===================================================================
    // 项目绑定路由纯函数矩阵（spec §5 四象限 × 熔断状态；无 DB，直接验证 apply_project_binding）
    // ===================================================================

    fn plain_provider(id: &str) -> Provider {
        Provider::with_id(id.to_string(), id.to_string(), json!({}), None)
    }

    fn ids(providers: &[Provider]) -> Vec<&str> {
        providers.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn matrix_no_binding_returns_base_verbatim() {
        // 无绑定：failover 开/关两态均原样透传（现状语义不变）
        let base = vec![plain_provider("a"), plain_provider("b")];
        for failover in [true, false] {
            let plan = apply_project_binding(base.clone(), None, failover, true);
            assert_eq!(ids(&plan.candidates), vec!["a", "b"]);
            assert!(!plan.routed);
        }
    }

    #[test]
    fn matrix_failover_off_bound_single_route() {
        // failover 关 + 绑定：单路由 [绑定者]，跳过熔断（bound_available=false 仍 [p]，
        // 跟随 select_providers failover 关分支不查熔断的现状）
        let plan = apply_project_binding(
            vec![plain_provider("a"), plain_provider("b")],
            Some(plain_provider("p")),
            false,
            false,
        );
        assert_eq!(ids(&plan.candidates), vec!["p"]);
        assert!(plan.routed);
    }

    #[test]
    fn matrix_failover_on_bound_available_prepends() {
        // failover 开 + 绑定者健康且不在公共队列：置首，其余队列依序保留（软绑定）
        let plan = apply_project_binding(
            vec![plain_provider("a"), plain_provider("b")],
            Some(plain_provider("p")),
            true,
            true,
        );
        assert_eq!(ids(&plan.candidates), vec!["p", "a", "b"]);
        assert!(plan.routed);
    }

    #[test]
    fn matrix_failover_on_bound_open_yields_to_queue() {
        // failover 开 + 绑定者熔断 Open：让位公共队列原样，routed=false（归因不误标）
        let plan = apply_project_binding(
            vec![plain_provider("a"), plain_provider("b")],
            Some(plain_provider("p")),
            true,
            false,
        );
        assert_eq!(ids(&plan.candidates), vec!["a", "b"]);
        assert!(!plan.routed);
    }

    #[test]
    fn matrix_bound_in_queue_deduped() {
        // 绑定者已在公共队列：去重后置首（恰出现一次）
        let plan = apply_project_binding(
            vec![plain_provider("a"), plain_provider("p"), plain_provider("b")],
            Some(plain_provider("p")),
            true,
            true,
        );
        assert_eq!(ids(&plan.candidates), vec!["p", "a", "b"]);
        assert!(plan.routed);
    }

    #[test]
    fn matrix_halfopen_treated_as_available() {
        // HalfOpen 与 Closed 同属 is_available()=true，入参即 bound_available=true 置首；
        // 两态在纯函数层为同一参数组合，无需分别构造（注释钉死语义来源）
        let plan = apply_project_binding(
            vec![plain_provider("a")],
            Some(plain_provider("p")),
            true,
            true,
        );
        assert_eq!(ids(&plan.candidates), vec!["p", "a"]);
        assert!(plan.routed);
    }

    #[test]
    fn matrix_bound_healthy_rescues_when_queue_all_open() {
        // 救场语义（spec §5 细则 1）：公共队列全熔断/为空（base=[]）+ 绑定者健康 + failover 开
        // → 候选链 [绑定者]；DB 集成对应 select_providers 返回 Err 的救场路径
        let plan = apply_project_binding(vec![], Some(plain_provider("p")), true, true);
        assert_eq!(ids(&plan.candidates), vec!["p"]);
        assert!(plan.routed);
    }

    // ===================================================================
    // select_providers_for_request DB 集成（spec §9 路由重排的端到端链路 + 边界 1 两路径）
    // ===================================================================

    /// 启用 claude 的自动故障转移（仿现有测试的 proxy_config 更新套路）
    async fn enable_failover(db: &Database) {
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();
    }

    /// sort_index 控制故障转移队列序的供应商
    fn queue_provider(id: &str, sort: usize) -> Provider {
        let mut p = Provider::with_id(id.to_string(), id.to_string(), json!({}), None);
        p.sort_index = Some(sort);
        p
    }

    #[tokio::test]
    #[serial]
    async fn request_with_binding_prepends_bound_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 队列序 [a, b]
        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        db.insert_or_update_route("/p", "claude", "b").unwrap();

        let (providers, routed) = ProviderRouter::new(db)
            .select_providers_for_request("claude", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["b", "a"]);
        assert!(routed);
    }

    #[tokio::test]
    #[serial]
    async fn request_without_header_matches_legacy_behavior() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        let router = ProviderRouter::new(db);
        let (providers, routed) = router
            .select_providers_for_request("claude", None)
            .await
            .unwrap();
        let legacy = router.select_providers("claude").await.unwrap();
        assert_eq!(ids(&providers), ids(&legacy));
        assert!(!routed);
    }

    #[tokio::test]
    #[serial]
    async fn route_row_missing_falls_back_to_default() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        // 绑定行缺失（项目从未绑定）→ 与现状一致
        let (providers, routed) = ProviderRouter::new(db)
            .select_providers_for_request("claude", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["a", "b"]);
        assert!(!routed);
    }

    #[tokio::test]
    #[serial]
    async fn provider_deleted_cascade_falls_back() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        db.insert_or_update_route("/p", "claude", "b").unwrap();
        // T2 级联：删供应商同一事务清绑定行（spec §8 边界 1 路径二）
        db.delete_provider("claude", "b").unwrap();

        let (providers, routed) = ProviderRouter::new(db)
            .select_providers_for_request("claude", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["a"]);
        assert!(!routed);
    }

    #[tokio::test]
    #[serial]
    async fn non_claude_app_type_ignores_header_path() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let current = plain_provider("codex-a");
        db.save_provider("codex", &current).unwrap();
        db.set_current_provider("codex", "codex-a").unwrap();
        // failover 关（默认）：仅当前供应商

        // 非 claude app_type：项目标识不介入，逐分支回退现状（spec §5 细则 8）
        let (providers, routed) = ProviderRouter::new(db)
            .select_providers_for_request("codex", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["codex-a"]);
        assert!(!routed);
    }

    #[tokio::test]
    #[serial]
    async fn request_all_circuit_open_err_rescued_by_healthy_bound() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 1 次失败即熔断；长超时保证稳定 Open 不进 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 3600,
            ..Default::default()
        })
        .await
        .unwrap();

        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        // c 不在公共队列：显式指定优先于队列资格（spec §5 细则 1）
        db.save_provider("claude", &plain_provider("c")).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        let router = ProviderRouter::new(db.clone());
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();
        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        db.insert_or_update_route("/p", "claude", "c").unwrap();

        // 公共队列全熔断：select_providers 本会 Err，绑定者健康 → 救场 [c]，
        // 绑定请求不收到 AllProvidersCircuitOpen（spec §5 细则 1 的 Err 介入时序）
        let (providers, routed) = router
            .select_providers_for_request("claude", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["c"]);
        assert!(routed);
    }

    #[tokio::test]
    #[serial]
    async fn request_no_providers_configured_rescued_by_healthy_bound() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // failover 开 + 故障转移队列全空：select_providers 本会走
        // Err(NoProvidersConfigured)（total_providers=0 的另一错误变体，非全熔断）
        db.save_provider("claude", &plain_provider("c")).unwrap();
        enable_failover(&db).await;

        db.insert_or_update_route("/p", "claude", "c").unwrap();

        // 绑定者健康 → 救场 [c]，绑定请求不收到 NoProvidersConfigured（细则 1）
        let (providers, routed) = ProviderRouter::new(db)
            .select_providers_for_request("claude", Some("/p"))
            .await
            .unwrap();
        assert_eq!(ids(&providers), vec!["c"]);
        assert!(routed);
    }

    #[tokio::test]
    #[serial]
    async fn request_all_circuit_open_bound_unavailable_reraises_err() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 3600,
            ..Default::default()
        })
        .await
        .unwrap();

        db.save_provider("claude", &queue_provider("a", 1)).unwrap();
        db.save_provider("claude", &queue_provider("b", 2)).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();
        enable_failover(&db).await;

        let router = ProviderRouter::new(db.clone());
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();
        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 绑定者 a 同样熔断：救场不成立 → 上抛原 Err（绑定者也不可用时保持现状错误语义）
        db.insert_or_update_route("/p", "claude", "a").unwrap();
        let result = router
            .select_providers_for_request("claude", Some("/p"))
            .await;
        assert!(matches!(result, Err(AppError::AllProvidersCircuitOpen)));
    }
}
