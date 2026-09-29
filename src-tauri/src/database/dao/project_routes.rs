// ===================================================================
// 项目路由与 settings.local.json 备份 DAO（spec §4.1 / §4.3）
// ===================================================================
//
// project_routes 维护「项目路径 → 供应商」软绑定，供代理路由层每请求直查
// （不做内存缓存）；settings_local_backup 存 settings.local.json 修改前的
// 原文快照，主键即 project_path，天然每项目单份。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::{params, OptionalExtension};

impl Database {
    // ===================================================================
    // project_routes：项目 → 供应商软绑定
    // ===================================================================

    /// 写入 / 覆盖一条绑定（UNIQUE(project_path, app_type) 冲突时整行替换）。
    ///
    /// id 每次生成新 uuid（REPLACE 语义下旧行已删，无外键引用无需保 id 稳定）。
    pub fn insert_or_update_route(
        &self,
        project_path: &str,
        app_type: &str,
        provider_id: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO project_routes (id, project_path, app_type, provider_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                project_path,
                app_type,
                provider_id,
                chrono::Utc::now().timestamp(),
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 删除一条绑定；行不存在时静默成功（幂等）。
    pub fn delete_route(&self, project_path: &str, app_type: &str) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM project_routes WHERE project_path = ?1 AND app_type = ?2",
            params![project_path, app_type],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 查某项目的绑定供应商 id；未绑定 → `None`。路由层每请求直查本方法（spec §5 细则 2）。
    pub fn find_route(
        &self,
        project_path: &str,
        app_type: &str,
    ) -> Result<Option<String>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT provider_id FROM project_routes WHERE project_path = ?1 AND app_type = ?2",
            params![project_path, app_type],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| AppError::Database(e.to_string()))
    }

    /// 列出某 app_type 的全部绑定 `(project_path, provider_id)`，按路径排序保证确定性。
    pub fn list_routes(&self, app_type: &str) -> Result<Vec<(String, String)>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT project_path, provider_id FROM project_routes
                 WHERE app_type = ?1 ORDER BY project_path ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let routes = stmt
            .query_map(params![app_type], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AppError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(routes)
    }

    /// 按供应商清理绑定行（精确匹配 `(provider_id, app_type)`，跨 app_type 同 id 不误删）。
    ///
    /// 供应商删除的级联清理在 [`crate::database::dao::providers::Database::delete_provider`]
    /// 的事务内以同款 SQL 直接执行（事务无法跨 `&self` 方法传递）；本方法供上层
    /// 需要独立清理时使用。
    pub fn delete_routes_by_provider(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM project_routes WHERE provider_id = ?1 AND app_type = ?2",
            params![provider_id, app_type],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    // ===================================================================
    // settings_local_backup：settings.local.json 写前快照
    // ===================================================================

    pub(crate) fn save_settings_local_backup(
        &self,
        project_path: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO settings_local_backup (project_path, content, updated_at)
             VALUES (?1, ?2, ?3)",
            params![project_path, content, chrono::Utc::now().timestamp()],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    #[allow(dead_code)] // 读取侧（T11 修复 UI）尚未接入，先行落地 DAO；接入后移除本行
    pub(crate) fn get_settings_local_backup(
        &self,
        project_path: &str,
    ) -> Result<Option<String>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT content FROM settings_local_backup WHERE project_path = ?1",
            params![project_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| AppError::Database(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use crate::database::Database;
    use crate::provider::Provider;

    /// 构造最小供应商行（测试夹具，仅 id / name 生效）
    fn test_provider(id: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            format!("Provider {id}"),
            serde_json::json!({}),
            None,
        )
    }

    // -----------------------------------------------------------------
    // project_routes：CRUD
    // -----------------------------------------------------------------

    #[test]
    fn insert_then_find_roundtrip() {
        let db = Database::memory().expect("memory db");
        db.insert_or_update_route("/Users/dev/proj", "claude", "p1")
            .expect("insert route");

        let bound = db
            .find_route("/Users/dev/proj", "claude")
            .expect("find route");
        assert_eq!(bound.as_deref(), Some("p1"), "插入后应能按 (path, app) 查回 provider_id");
    }

    #[test]
    fn upsert_overwrites_same_project_app() {
        let db = Database::memory().expect("memory db");
        db.insert_or_update_route("/Users/dev/proj", "claude", "p1")
            .expect("first insert");
        // 同 (project_path, app_type) 二次写入不应触发 UNIQUE 冲突，而是覆盖
        db.insert_or_update_route("/Users/dev/proj", "claude", "p2")
            .expect("upsert overwrite");

        let bound = db
            .find_route("/Users/dev/proj", "claude")
            .expect("find route");
        assert_eq!(bound.as_deref(), Some("p2"), "upsert 后 provider_id 应更新为新值");
    }

    #[test]
    fn delete_route_removes_row() {
        let db = Database::memory().expect("memory db");
        db.insert_or_update_route("/Users/dev/proj", "claude", "p1")
            .expect("insert route");

        db.delete_route("/Users/dev/proj", "claude")
            .expect("delete route");
        let bound = db
            .find_route("/Users/dev/proj", "claude")
            .expect("find after delete");
        assert!(bound.is_none(), "删除后应查不到绑定行");

        // 幂等：重复删除不报错
        db.delete_route("/Users/dev/proj", "claude")
            .expect("delete again is idempotent");
    }

    #[test]
    fn list_routes_returns_all_for_app() {
        let db = Database::memory().expect("memory db");
        db.insert_or_update_route("/p/a", "claude", "p1")
            .expect("insert a");
        db.insert_or_update_route("/p/b", "claude", "p2")
            .expect("insert b");
        db.insert_or_update_route("/p/c", "codex", "p3")
            .expect("insert c (other app)");

        let routes = db.list_routes("claude").expect("list claude routes");
        assert_eq!(
            routes,
            vec![
                ("/p/a".to_string(), "p1".to_string()),
                ("/p/b".to_string(), "p2".to_string()),
            ],
            "list_routes(claude) 应仅返回 claude 的两条（按 project_path 排序）"
        );
    }

    // -----------------------------------------------------------------
    // settings_local_backup：写前快照
    // -----------------------------------------------------------------

    #[test]
    fn backup_upsert_keeps_latest() {
        let db = Database::memory().expect("memory db");
        db.save_settings_local_backup("/Users/dev/proj", "first snapshot")
            .expect("save first backup");
        db.save_settings_local_backup("/Users/dev/proj", "second snapshot")
            .expect("save second backup (REPLACE)");

        let got = db
            .get_settings_local_backup("/Users/dev/proj")
            .expect("get backup");
        assert_eq!(
            got.as_deref(),
            Some("second snapshot"),
            "每项目单份，应仅保留最近一次快照"
        );

        let missing = db
            .get_settings_local_backup("/Users/dev/other")
            .expect("get missing backup");
        assert!(missing.is_none(), "未备份过的项目应返回 None");
    }

    // -----------------------------------------------------------------
    // delete_provider 级联清理（spec §4.1：同事务删除关联路由行）
    // -----------------------------------------------------------------

    #[test]
    fn delete_provider_cascades_routes() {
        let db = Database::memory().expect("memory db");
        db.save_provider("claude", &test_provider("p1"))
            .expect("save p1");
        // p2 无任何绑定行，验证无路由行的供应商删除不受级联影响
        db.save_provider("claude", &test_provider("p2"))
            .expect("save p2");

        db.insert_or_update_route("/Users/dev/proj", "claude", "p1")
            .expect("bind proj to p1");

        db.delete_provider("claude", "p1").expect("delete p1");

        let bound = db
            .find_route("/Users/dev/proj", "claude")
            .expect("find route after cascade");
        assert!(bound.is_none(), "供应商删除后其绑定行应级联消失");
        let gone = db
            .get_provider_by_id("p1", "claude")
            .expect("query p1");
        assert!(gone.is_none(), "供应商行本身已删");

        // 无路由行的 provider 删除照常成功且不产生副作用
        db.delete_provider("claude", "p2").expect("delete p2 without routes");
    }

    #[test]
    fn delete_provider_same_id_other_app_kept() {
        let db = Database::memory().expect("memory db");
        // providers 主键是 (id, app_type)：claude 与 codex 可各有一行 id="p1"
        db.save_provider("claude", &test_provider("p1"))
            .expect("save claude p1");
        db.save_provider("codex", &test_provider("p1"))
            .expect("save codex p1");
        db.insert_or_update_route("/Users/dev/projC", "claude", "p1")
            .expect("bind claude route");
        db.insert_or_update_route("/Users/dev/projX", "codex", "p1")
            .expect("bind codex route");

        db.delete_provider("claude", "p1").expect("delete claude p1");

        assert!(
            db.find_route("/Users/dev/projC", "claude")
                .expect("find claude route")
                .is_none(),
            "被删供应商的 claude 路由行应级联清理"
        );
        assert_eq!(
            db.find_route("/Users/dev/projX", "codex")
                .expect("find codex route")
                .as_deref(),
            Some("p1"),
            "级联必须精确匹配 (provider_id, app_type)，codex 的同名 id 路由行不误删"
        );
        assert!(
            db.get_provider_by_id("p1", "codex")
                .expect("query codex p1")
                .is_some(),
            "codex 侧同名 id 供应商行不受影响"
        );
    }
}
