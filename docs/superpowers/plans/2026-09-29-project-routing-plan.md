# Project Routing（按项目路由供应商）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Claude Code 在不同项目目录发出的请求经 cc-switch 本地代理时路由到各自绑定的供应商；未绑定请求行为与现状完全一致。

**Architecture:** 客户端在项目 `.claude/settings.local.json` 注入 `X-CC-Project` header（percent-encode 路径）→ 代理共享入口 `RequestContext::new` 提取并解码 → `ProviderRouter::select_providers_for_request` 每请求直查 SQLite `project_routes` 表并对候选链做纯函数重排（软绑定）→ forwarder 消费重排后的有序列表，仅"全局切换判定"排除绑定请求。归因字段（project_dir / project_routed）随请求上下文落库到 `proxy_request_logs` 并透传到前端使用统计。

**Tech Stack:** Tauri 2 + Rust（rusqlite、tokio、axum HeaderMap）后端 `src-tauri/`；React 18 + TypeScript + TanStack Query + Radix UI + i18next 前端 `src/`；测试 cargo test（内嵌 `#[cfg(test)]`，先例 `provider_router.rs`）/ vitest（`pnpm test:unit`）。

**Spec:** `/Users/dev/Project/cc-switch/docs/superpowers/specs/2026-09-29-project-routing-design.md`（Revised-1，唯一权威需求来源；spec 附录 A 行号均已核实）

## Global Constraints

- 首期仅 `claude` app_type 参与项目路由；表结构预留 `app_type` 列（恒 `'claude'`）
- **不改** `src-tauri/src/proxy/handlers.rs:157` 的 `select_providers("claude-desktop")` 调用点（spec §5 细则 6）
- **不改** forwarder 供应商选择与重试循环，唯一例外是 4 处 `should_switch` 判定排除绑定请求（spec §5 细则 3/7）
- **不得绕过**"select_providers 只在 `RequestContext::new` 调用一次"约束（HalfOpen 名额单次消耗，`handler_context.rs:137-138` 注释即警戒线，spec §5 细则 5）
- percent-encode：UTF-8 字节级，保留集 `[A-Za-z0-9/._~-]`，其余 `%XX` **大写**十六进制；decode 遇非法序列 → `None` → 按无项目标识处理，不报错（spec §3.2）
- `UsageSemantic` 语义哈希**不含** project 字段（跟随 `vision_routed` 既有处理，spec §10 风险 1 已核实）
- **禁止新增任何 crate 依赖**（含 dev-dependencies）：属性测试用手写确定性 LCG（`serial_test`/`tempfile`/`uuid` 已在依赖中）
- 不复用 `proxy_live_backup` 表；备份走新建 `settings_local_backup` 表（spec §4.3）
- 30 天活跃过滤硬编码 `STALE_DAYS: i64 = 30`；前端轮询 `3000ms` 条件轮询（spec §10 开放问题 1/3 取值）
- 注释用中文，沿用所在文件的注释密度与风格
- **本计划不含 git commit 步骤**：执行阶段的提交节奏由主控代理统一决定（用户全局规范：未经用户要求不做 git 提交）
- 命令行命令均从仓库根 `/Users/dev/Project/cc-switch` 执行；Rust 测试统一用 `--manifest-path src-tauri/Cargo.toml`（包名 `cc-switch`，lib 名 `cc_switch_lib`）

## Review Focus

以下 5 类输入/失败模式 spec 有要求但分散，各任务测试必须钉死：

1. **header 值含非法 percent 序列**（用户手改文件、其他工具注入坏 `%`）→ decode 返回 `None` → 静默回默认策略，绝不 panic → T3 属性测试性质 3、T7 `extract_project_dir` 非法用例
2. **settings.local.json 损坏 JSON / 写失败** → 解析失败按空对象重建最小结构；文件写失败时 DB 行保留、`sync_status` 变 `out_of_sync` 供一键修复 → T4 测试"损坏 JSON 重建"与"写失败后 DB 保留"
3. **供应商删除后的悬空绑定**（含跨 app_type 同 id 的误删风险）→ 级联删除必须精确匹配 `(provider_id, app_type)` → T2 测试"跨 app_type 同 id 不误删"
4. **绑定者熔断 Open + failover 开启** → 绑定者让位公共队列且 `routed=false`（归因不误标）→ T6 矩阵用例"Open 让位"
5. **前后端字段序列化漂移**（`projectDir` camelCase 遗漏）→ 后端 DTO `#[serde(rename_all="camelCase")]` + 前端类型同步 + T13 `pnpm typecheck` 全量把关

---

### Task 1: 数据库 Schema —— project_routes / settings_local_backup 建表 + 日志表加列 + v20→v21 迁移

**Files:**
- Modify: `src-tauri/src/database/schema.rs`（`create_tables_on_conn` 末尾追加两表，`proxy_request_logs` 建表语句 `:199-214` 尾部加两列，dispatch 循环 `:540-580` 加 arm，`migrate_v19_to_v20` `:1637` 后加 `migrate_v20_to_v21`）
- Modify: `src-tauri/src/database/mod.rs:56`（`SCHEMA_VERSION: i32 = 20` → `21`）
- Test: `src-tauri/src/database/tests.rs`（追加；复用现有 `get_column_info` helper 与 `schema_migration_sets_user_version_when_missing` 的老库构造先例）

**Interfaces:**
- Consumes: 现有 `create_tables_on_conn(conn: &Connection)`（schema.rs:24）、`Self::add_column_if_missing(conn, table, col, ddl)`、`Self::table_exists(conn, table)`、`Self::set_user_version(conn, i32)`
- Produces: DB 内存在表 `project_routes(id, project_path, app_type, provider_id, updated_at, UNIQUE(project_path, app_type))` 与 `settings_local_backup(project_path PK, content, updated_at)`；`proxy_request_logs` 含 `project_dir TEXT`、`project_routed INTEGER NOT NULL DEFAULT 0`；`user_version == 21`。后续 T2/T4/T8 依赖此结构

- [ ] **Step 1: 写失败测试**（追加到 `src-tauri/src/database/tests.rs`）

测试名与断言（照现有测试的 `Connection` + `NamedTempFile` 风格）：

```rust
// 1. 新库：user_version == 21，两张新表列齐（裸 Connection 三步，仿 schema_migration_sets_user_version_when_missing
//    先例 tests.rs:187——Database::memory() 只走 create_tables 不走迁移，user_version 恒 0，不能用它断言版本号）
fn fresh_db_has_project_routes_table_and_version_21()
//   Connection::open_in_memory → Database::create_tables_on_conn → Database::apply_schema_migrations_on_conn
//   → 断言 get_user_version == 21；
//   get_column_info(conn, "project_routes", "app_type") 存在；
//   settings_local_backup 的 content 列存在
// 1b. memory() 路径：表与列齐（不断言版本号）
fn memory_db_has_project_tables_and_columns()
//   Database::memory() → table_exists(project_routes / settings_local_backup) 为真 +
//   proxy_request_logs 的 project_dir / project_routed 两列存在（get_column_info）
// 2. UNIQUE 约束：INSERT 两条同 (project_path, app_type) 不同 id → 第二条 Err（约束冲突）
fn project_routes_unique_project_app()
// 3. 日志表新列（新库路径）：project_dir / project_routed 两列存在于 proxy_request_logs
fn fresh_request_logs_have_project_columns()
// 4. 老库迁移：手写 v20 库（旧 DDL 无两列 + 已有一行日志数据 + set_user_version(20)）
//    → 经 Database 打开触发迁移 → 列补齐、旧行保留且 project_routed==0、project_dir IS NULL、version==21
fn v20_db_migrates_to_v21_keeping_rows()
```

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fresh_db_has_project_routes_table_and_version_21`
Expected: FAIL（`project_routes` 表不存在 / user_version 为 20）

- [ ] **Step 3: 实现**

1. `mod.rs:56`：`pub(crate) const SCHEMA_VERSION: i32 = 21;`
2. `create_tables_on_conn` 末尾追加（编号注释顺延现有序号；SQL 逐字取自 spec §4.1/§4.3）：
   ```sql
   CREATE TABLE IF NOT EXISTS project_routes (
     id TEXT PRIMARY KEY,
     project_path TEXT NOT NULL,
     app_type TEXT NOT NULL DEFAULT 'claude',
     provider_id TEXT NOT NULL,
     updated_at INTEGER NOT NULL,
     UNIQUE(project_path, app_type)
   );
   CREATE TABLE IF NOT EXISTS settings_local_backup (
     project_path TEXT PRIMARY KEY,
     content      TEXT NOT NULL,
     updated_at   INTEGER NOT NULL
   );
   ```
3. `proxy_request_logs` 建表语句（schema.rs:213 `vision_routed INTEGER NOT NULL DEFAULT 0` 之后）追加 `, project_dir TEXT, project_routed INTEGER NOT NULL DEFAULT 0`
4. 新增迁移（完全仿 `migrate_v19_to_v20` :1637 的 `table_exists` + `add_column_if_missing` 模式）：
   ```rust
   fn migrate_v20_to_v21(conn: &Connection) -> Result<(), AppError>
   // proxy_request_logs 加 project_dir TEXT / project_routed INTEGER NOT NULL DEFAULT 0
   ```
5. dispatch 循环 `19 => {...}` 之后加：
   ```rust
   20 => {
       log::info!("迁移数据库从 v20 到 v21（请求日志项目归因列）");
       Self::migrate_v20_to_v21(conn)?;
       Self::set_user_version(conn, 21)?;
   }
   ```

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib database::tests`
Expected: PASS（`test result: ok. 0 failed`）

---

### Task 2: DAO 层 —— project_routes / settings_local_backup DAO + delete_provider 级联事务化

**Files:**
- Create: `src-tauri/src/database/dao/project_routes.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`（`pub mod project_routes;`，按字母序插入）
- Modify: `src-tauri/src/database/dao/providers.rs:379`（`delete_provider` 事务化 + 级联）
- Test: `src-tauri/src/database/dao/project_routes.rs` 内嵌 `#[cfg(test)] mod tests`（仿 `provider_router.rs:294` 的 `TempHome` + `Database::memory()` + `#[serial]` 先例；TempHome 定义在 provider_router tests 内，本文件重新声明一份等价物或直接 `Database::memory()`——DAO 不触 settings 时无需 TempHome，直接用 `Database::memory()`）

**Interfaces:**
- Consumes: T1 的表结构；`lock_conn!` 宏（`database/mod.rs:65`）；`uuid::Uuid::new_v4()`（依赖已有，providers id 先例）
- Produces（全部为 `impl Database` 方法，供 T4/T6/T9 调用）:
  ```rust
  pub fn insert_or_update_route(&self, project_path: &str, app_type: &str, provider_id: &str) -> Result<(), AppError>
  pub fn delete_route(&self, project_path: &str, app_type: &str) -> Result<(), AppError>
  pub fn find_route(&self, project_path: &str, app_type: &str) -> Result<Option<String>, AppError>   // Some(provider_id)
  pub fn list_routes(&self, app_type: &str) -> Result<Vec<(String, String)>, AppError>               // (project_path, provider_id)
  pub fn delete_routes_by_provider(&self, provider_id: &str, app_type: &str) -> Result<(), AppError>
  pub(crate) fn save_settings_local_backup(&self, project_path: &str, content: &str) -> Result<(), AppError>
  pub(crate) fn get_settings_local_backup(&self, project_path: &str) -> Result<Option<String>, AppError>
  ```

- [ ] **Step 1: 写失败测试**（内嵌于 `dao/project_routes.rs`）

```rust
// 用例清单（均 Database::memory()）：
fn insert_then_find_roundtrip()                // 插入 → find_route == Some(pid)
fn upsert_overwrites_same_project_app()        // 同 (path, app) 二次插入 → provider_id 更新为新值（不报 UNIQUE 错）
fn delete_route_removes_row()                  // 删除 → find_route == None；重复删除 Ok（幂等）
fn list_routes_returns_all_for_app()           // 两条 + 一条 app_type="codex" → list_routes("claude") 仅 2 条
fn backup_upsert_keeps_latest()                // save 两次 → get 返回第二次内容
fn delete_provider_cascades_routes()           // 绑定 a→p1 → delete_provider("claude","p1") → find_route==None 且 providers 行已删
fn delete_provider_same_id_other_app_kept()    // claude 与 codex 各有 id="p1" 供应商 → 删 claude 的 → codex 的路由行保留（Review Focus 3）
```

> 假绿防护：先完成 Files 中 `dao/mod.rs` 的模块注册（`pub mod project_routes;`）再跑测试；cargo test 过滤词 0 匹配时退出码仍为 0（假绿），跑前用 `rg -c "insert_then_find_roundtrip" src-tauri/src/database/dao/project_routes.rs` 确认测试函数已存在。

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml insert_then_find_roundtrip`
Expected: FAIL（编译错误 `no method named insert_or_update_route`）

- [ ] **Step 3: 实现**

1. `dao/project_routes.rs`：全部走 `INSERT OR REPLACE INTO project_routes VALUES (?, ?, ?, ?, ?)`（id 每次 `uuid::Uuid::new_v4().to_string()`，updated_at 用 `chrono::Utc::now().timestamp()`，chrono 已在依赖中）；`find_route` 用 `query_row ... OptionalExtension`；`delete_routes_by_provider` 为 `DELETE FROM project_routes WHERE provider_id = ?1 AND app_type = ?2`；备份两函数为 `INSERT OR REPLACE INTO settings_local_backup`（spec §4.3 每项目单份）
2. `dao/mod.rs` 加 `pub mod project_routes;`
3. `providers.rs:379` `delete_provider` 改事务（仿同文件 `set_current_provider` :389 的事务模式）：
   ```rust
   let mut conn = lock_conn!(self.conn);
   let tx = conn.transaction()...;
   tx.execute("DELETE FROM project_routes WHERE provider_id = ?1 AND app_type = ?2", params![id, app_type])...;
   tx.execute("DELETE FROM providers WHERE id = ?1 AND app_type = ?2", params![id, app_type])...;
   tx.commit()...;
   ```

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib dao::project_routes`
Expected: PASS，且 `cargo test --manifest-path src-tauri/Cargo.toml --lib dao` 全绿（delete_provider 改动未破坏 providers 既有测试）

---

### Task 3: 纯函数层 —— proxy/project_header.rs（percent-encode/decode + X-CC-Project 行操作）

**Files:**
- Create: `src-tauri/src/proxy/project_header.rs`
- Modify: `src-tauri/src/proxy/mod.rs`（`pub(crate) mod project_header;`，按现有字母序，`pub mod provider_router;` :24 之前）
- Test: 内嵌 `#[cfg(test)] mod tests`（纯函数，无 DB、无 async）

**Interfaces:**
- Consumes: 无（叶子模块）
- Produces（编码与匹配规范 spec §3.2 定死，写入端 T4 与读取端 T7 共用）:
  ```rust
  /// 保留 [A-Za-z0-9/._~-]，其余 UTF-8 字节编码为 %XX（大写）
  pub(crate) fn percent_encode_project_path(path: &str) -> String
  /// 非法 % 转义或非 UTF-8 字节序列 → None（按无项目标识处理，不报错）
  pub(crate) fn percent_decode_project_path(value: &str) -> Option<String>
  /// 从 ANTHROPIC_CUSTOM_HEADERS 多行值提取首个目标行的值（trim 后）；无目标行 → None。
  /// 行规则：按 \n 拆行、逐行 trim、按首个 ':' 分割、名字段 ASCII 大小写不敏感全等 "x-cc-project"
  pub(crate) fn extract_x_cc_project(headers_value: &str) -> Option<String>
  /// Some(e)：替换全部目标行为单行 "X-CC-Project: {e}"；None：删除全部目标行。
  /// 非目标行逐字节保留；输出各 行 以 \n join 且行尾统一补一个 \n；全空时返回空串
  pub(crate) fn replace_x_cc_project(headers_value: &str, encoded: Option<&str>) -> String
  ```

- [ ] **Step 1: 写失败测试**

固定用例 + 属性测试两段。属性测试用手写确定性 LCG（禁新依赖，Global Constraints）：

```rust
// ===== 固定用例 =====
fn encode_keeps_unreserved_and_uppercases_hex()   // "/a b中文" → "/a%20b%E4%B8%AD%E6%96%87"；'~','.','_','-' 保留
fn decode_roundtrip_fixed()                        // decode(encode("/Users/dev/项目 A")) == Some(原值)
fn decode_rejects_bad_sequences()                  // "%", "A%2", "%zz", "%E4%B8"（截断）, "%FF%FE"（非 UTF-8）→ 全部 None
fn extract_matches_case_insensitive_first_colon()  // "X-API-Key: k\nx-cc-project: %2Fp" → Some("%2Fp")；
                                                   // "X-CC-Project: a: b"（值含冒号）→ Some("a: b")
fn extract_none_when_absent()
fn replace_merges_multiple_target_lines()          // 两行目标行 + 一行 "X-Api-Key: k" → 输出恰 1 目标行且 k 行逐字节保留
fn replace_none_removes_all_targets()              // 同上输入 + None → 无目标行，k 行保留
fn replace_appends_when_absent()                   // 只有 k 行 → 输出 k 行在前、目标行在末尾
fn replace_keeps_cr_bytes()                        // 原行尾 \r 保留（split('\n') 后行内容含 \r，join 还原）

// ===== 属性测试（spec §9 settings_local_writer 类别的前半：编码往返 / 非法回退 / 多行合并）=====
struct Lcg(u64);  // xorshift64*：next() -> u64
fn rand_string(rng: &mut Lcg, len: usize) -> String  // 从字符表随机拼：ASCII 字母数字 + "/._~-%:\n\r\t " + 中文池 ["项目","开","发","文","件"]

#[test] fn prop_encode_decode_roundtrip()          // 500 轮：decode(encode(s)) == Some(s)，且 encode 输出仅匹配 ^[A-Za-z0-9/._~%]*$、%XX 大写
#[test] fn prop_replace_preserves_other_lines()    // 500 轮：随机构造多行值（含随机目标行数量 0..3）→ replace(v, Some(e))：
                                                   //   (a) 输出目标行恰 1 行且值 == e；(b) 除目标行外其余行逐字节与输入一致（按行对比）；
                                                   //   (c) replace(replace(v, Some(e1)), Some(e2)) == replace(v, Some(e2))
#[test] fn prop_replace_none_idempotent()          // replace(replace(v, None), None) == replace(v, None) 且 extract == None
```

> 假绿防护：先完成 Files 中 `proxy/mod.rs` 的模块注册（`pub(crate) mod project_header;`）再跑测试；cargo test 过滤词 0 匹配时退出码仍为 0（假绿），跑前用 `rg -c "prop_encode_decode_roundtrip" src-tauri/src/proxy/project_header.rs` 确认测试函数已存在。

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml project_header`
Expected: FAIL（编译错误：模块/函数不存在）

- [ ] **Step 3: 实现**（`src-tauri/src/proxy/project_header.rs`）

- encode：`path.as_bytes()` 逐字节，保留集命中输出字符，否则 `format!("%{byte:02X}")`
- decode：手写状态机扫 `%XX`（两位必须是 hex，非法即整体 `None`），产出 `Vec<u8>` 后 `String::from_utf8(...).ok()`
- extract：`split('\n')` → `trim` → `split_once(':')` → `name.trim().eq_ignore_ascii_case("x-cc-project")` → 首个命中返回 `value.trim().to_string()`
- replace：`split('\n')` 收集非目标行（判定同 extract 的行级条件），`Some` 时 push `format!("X-CC-Project: {e}")`；全部行 `join("\n")`，结果非空则补尾 `\n`，空则返回 `""`
- 文件头注释用中文说明"写入端与读取端共同遵守的编码契约（spec §3.2），定死规则以杜绝两端漂移"

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml project_header`
Expected: PASS（固定用例 + 3 条属性测试全绿）

---

### Task 4: settings_local_writer —— settings.local.json 编辑器 + 写前备份 + 同步状态

**Files:**
- Create: `src-tauri/src/services/settings_local_writer.rs`
- Modify: `src-tauri/src/services/mod.rs`（`pub mod settings_local_writer;`，按字母序）
- Test: 内嵌 `#[cfg(test)] mod tests`（`tempfile::TempDir` + `Database::memory()`）

**Interfaces:**
- Consumes: T2 的 `save_settings_local_backup` / `get_settings_local_backup`；T3 的 `percent_encode_project_path` / `extract_x_cc_project` / `replace_x_cc_project`；`crate::config::atomic_write`（config.rs:383，temp + rename）
- Produces（供 T9 命令层调用）:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SyncStatus { Synced, OutOfSync, OrphanHeader }   // 序列化为 "synced"/"out_of_sync"/"orphan_header"（手写 as_str）

  /// 绑定 → 写入 <project_path>/.claude/settings.local.json 的 env.ANTHROPIC_CUSTOM_HEADERS。
  /// 文件不存在/损坏 JSON → 按空对象重建（损坏时原文先备份）；已存在文件每次写前备份（REPLACE，spec §4.3）
  pub fn set_project_header(project_path: &str, db: &Database) -> Result<(), AppError>

  /// 移除目标行（文件不存在 → Ok 直接返回）；非目标行逐字节保留
  pub fn remove_project_header(project_path: &str, db: &Database) -> Result<(), AppError>

  /// 三态对比（spec §7.1 第 3 项真值表）：
  /// bound=Some(pp)：文件目标行存在且 percent_decode 后 == pp → Synced；否则 OutOfSync
  /// bound=None：文件有目标行 → OrphanHeader；否则 Synced
  pub fn read_sync_status(project_path: &str, bound: Option<&str>) -> SyncStatus
  ```

- [ ] **Step 1: 写失败测试**

```rust
// 用例清单（settings 路径 = temp_dir/.claude/settings.local.json，db = Database::memory()）：
fn set_creates_minimal_structure_when_missing()     // 不存在 → set → 文件为合法 JSON，
                                                   //   env.ANTHROPIC_CUSTOM_HEADERS == "X-CC-Project: <encoded>\n"
fn set_preserves_user_other_headers()               // 预置 {"env":{"ANTHROPIC_CUSTOM_HEADERS":"X-Api-Key: k\n"},
                                                   //   "permissions":{...}} → set → X-Api-Key 行逐字节保留、permissions 键保留
fn set_merges_duplicate_target_lines()              // 预置两行目标行 → set → 输出恰 1 行目标行
fn set_backs_up_existing_file_once_per_write()      // 预置原文 → set → get_settings_local_backup == Some(原文)；
                                                   //   再改一次 → 备份 == 第一次 set 后的内容（每次写前快照）
fn set_rebuilds_corrupted_json()                    // 预置 "not json{" → set → 文件重建为最小结构，备份 == 损坏原文
fn remove_deletes_only_target_line()                // 预置混合行 → remove → 目标行消失、其他行不变
fn remove_ok_when_file_missing()
fn sync_status_synced_out_of_sync_orphan()           // 三态各一断言（含"文件行值与绑定不一致 → OutOfSync"、
                                                    //   "decode 非法 → OutOfSync"（Review Focus 1 的文件侧））
fn write_failure_keeps_db_row()                     // 绑定已写 DB（模拟 T9 先 DB 后文件序）→ 目标目录只读/指向不存在盘符路径
                                                    //   → set Err → DB 行仍在（由 T9 的 sync_status 兜底修复）
```

> 假绿防护：先完成 Files 中 `services/mod.rs` 的模块注册（`pub mod settings_local_writer;`）再跑测试；cargo test 过滤词 0 匹配时退出码仍为 0（假绿），跑前用 `rg -c "set_creates_minimal_structure_when_missing" src-tauri/src/services/settings_local_writer.rs` 确认测试函数已存在。

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings_local_writer`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

- 内部辅助 `fn load_settings_json(path: &Path) -> (serde_json::Value, Option<String>)`：返回（对象，已存在文件的原文 `Option<String>`；损坏时原文也返回用于备份，对象为 `json!({})`）
- `set`：load → 有原文则 `db.save_settings_local_backup(path_str, 原文)` → `env.ANTHROPIC_CUSTOM_HEADERS`（缺省空串）→ `replace_x_cc_project(v, Some(&percent_encode_project_path(project_path)))` 写回 → `serde_json::to_string_pretty` → `atomic_write`
- `remove`：文件存在才处理；`replace_x_cc_project(v, None)` 写回；写前同样备份现存原文（与 `set` 复用同一 helper，保持"每次写前快照"语义一致）；`env` 下 `ANTHROPIC_CUSTOM_HEADERS` 值为空串时保留键（最小侵入）
- `read_sync_status`：`extract_x_cc_project` + `percent_decode_project_path` 对比（decode 非法按不一致处理）
- 注意 `atomic_write` 会 `create_dir_all` 父目录（config.rs:383 现状），`.claude` 目录自动创建

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings_local_writer`
Expected: PASS

---

### Task 5: project_scanner —— 三源合并探测

**Files:**
- Create: `src-tauri/src/services/project_scanner.rs`
- Modify: `src-tauri/src/services/mod.rs`（`pub mod project_scanner;`）
- Test: 内嵌 `#[cfg(test)] mod tests`（`tempfile::TempDir` fixture 三源 + `now` 注入）

**Interfaces:**
- Consumes: 无 DB 依赖（纯文件系统 + JSON 解析）
- Produces（供 T9 调用）:
  ```rust
  pub(crate) const STALE_DAYS: i64 = 30;

  #[derive(Debug, Clone, serde::Serialize)]
  #[serde(rename_all = "camelCase")]
  pub struct ScannedProject {
      pub project_path: String,
      pub basename: String,
      pub last_active_at: i64,       // epoch 秒，三源取最大
      pub has_active_session: bool,  // sessions 源 status 为活跃 → true
  }

  /// home = 用户主目录（~/.claude.json 与 ~/.claude/ 的父目录）；now 注入便于测试 30 天过滤
  pub fn scan_projects(home: &std::path::Path, now: i64) -> Vec<ScannedProject>
  ```

- [ ] **Step 1: 写失败测试**

fixture 构造器：`fn fixture(home: &TempDir) -> Builder` 在 `home/.claude.json`、`home/.claude/projects/<slug>/xx.jsonl`（每行 JSON 含 `cwd`）、`home/.claude/sessions/<id>.json`（`cwd` + `status`）三处铺数据：

```rust
fn merges_three_sources_taking_max_activity()   // 项目 A 只在 claude.json（lastSessionModified=T1）+ sessions 活跃
                                                //   → last_active_at 取最大、has_active_session=true
fn jsonl_cwd_supplies_missing_projects()        // claude.json 损坏（写入非法 JSON）→ jsonl 的 cwd 项目仍入列（spec §8 边界 6）
fn missing_claude_json_degrades_gracefully()    // claude.json 不存在 → 同上仍入列
fn nonexistent_project_dir_filtered()           // claude.json 指向 /nonexistent/path → 不入列（spec §8 边界 7）
fn stale_project_filtered_29d_kept()            // last_active = now-31d → 剔除；now-29d → 保留
fn dir_without_jsonl_not_a_source()             // ~/.claude/projects/空目录（无 jsonl）→ 不作为补充源项目
fn chinese_path_basename()                      // "/Users/x/我的 项目" → basename=="项目"、路径原样
fn sorted_by_last_active_desc()
```

> 假绿防护：先完成 Files 中 `services/mod.rs` 的模块注册（`pub mod project_scanner;`）再跑测试；cargo test 过滤词 0 匹配时退出码仍为 0（假绿），跑前用 `rg -c "merges_three_sources_taking_max_activity" src-tauri/src/services/project_scanner.rs` 确认测试函数已存在。

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml project_scanner`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

- 源 1（主源）：`home/.claude.json` 解析 `projects` 对象，键=路径，值对象读 `lastSessionModified`（数值或字符串数字，容错缺省 0）
- 源 2（补充）：遍历 `home/.claude/projects/*/`，仅目录内存在 `*.jsonl` 的才作为来源；读首个 jsonl 的首行 `cwd` 字段，活跃度取该 jsonl 文件 `mtime`（`std::fs::metadata`，epoch 秒）
- 源 3（活跃标记）：遍历 `home/.claude/sessions/*.json`，读 `cwd` 与 `status`；`status` 为活跃值（`"active"` / `"running"`，与 fixture 断言一致即可，缺省不活跃）
- 合并：`HashMap<String, (i64, bool)>` 取 max / or；过滤 `Path::new(p).exists()` 且 `now - last_active <= STALE_DAYS*86400`
- 输出按 `last_active_at` 降序；`basename` 用 `Path::file_name`（中文路径正常）
- 全量扫描即满足性能（实测 64ms / 3s，spec §3.3）；**不做** mtime 增量缓存（可选项，YAGNI）

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml project_scanner`
Expected: PASS

---

### Task 6: provider_router —— 候选链重排纯函数 + select_providers_for_request

**Files:**
- Modify: `src-tauri/src/proxy/provider_router.rs`
- Test: 内嵌 `#[cfg(test)] mod tests` 追加（现有 tests 模块 :294 起）

**Interfaces:**
- Consumes: T2 的 `find_route`；现有 `select_providers`（:45）、`get_or_create_circuit_breaker`（:255）、`db.get_provider_by_id`、`db.get_proxy_config_for_app`
- Produces（供 T7 调用）:
  ```rust
  pub(crate) struct ProjectRoutePlan {
      pub candidates: Vec<Provider>,
      pub routed: bool,   // 绑定命中且绑定者位于候选链首位（本请求实际出口由绑定决定）
  }

  /// 纯函数：语义矩阵重排（spec §5）。bound_available 在 failover_enabled=false 时被忽略（跟随 :113 现状不查熔断）
  pub(crate) fn apply_project_binding(
      base: Vec<Provider>,            // 现有 select_providers 结果（无绑定语义，熔断已过滤）
      bound: Option<Provider>,        // 绑定供应商完整对象
      failover_enabled: bool,
      bound_available: bool,          // 绑定者熔断 is_available()（Closed/HalfOpen=true, Open=false）
  ) -> ProjectRoutePlan

  impl ProviderRouter {
      /// project_path=None 或 app_type!="claude" 时行为与 select_providers 完全一致（逐分支回退，spec §5 细则 8）
      /// 救场分支（spec §5 细则 1）：failover 开 + 绑定者健康 + 公共队列全熔断/为空（select_providers 返回 Err）
      /// → 候选链=[绑定者]。该分支在 select_providers 的 Err 上抛之前介入——绑定重排不能只包裹在其成功返回之后，
      /// 需处理 Err 情形下绑定者健康的救场路径（实现见 Step 7 第 1/7 步）
      pub async fn select_providers_for_request(
          &self, app_type: &str, project_path: Option<&str>,
      ) -> Result<(Vec<Provider>, bool), AppError>
  }
  ```

- [ ] **Step 1: 写失败测试 —— 纯函数矩阵（spec §9 路由重排单测；无 DB，四象限 × 熔断 × 不在队列 × 行缺失）**

```rust
fn matrix_no_binding_returns_base_verbatim()          // failover 开/关 × bound=None → base 原样、routed=false（两断言）
fn matrix_failover_off_bound_single_route()           // bound=Some(p)+failover 关 → [p]、routed=true（跳过熔断：bound_available=false 仍 [p]）
fn matrix_failover_on_bound_available_prepends()      // base=[a,b] + bound=p(不在队列)+available → [p,a,b]、routed=true
fn matrix_failover_on_bound_open_yields_to_queue()    // bound=p + !bound_available → base 原样、routed=false（Review Focus 4）
fn matrix_bound_in_queue_deduped()                    // base=[a,p,b] + bound=p + available → [p,a,b]（p 恰一次，首位）
fn matrix_halfopen_treated_as_available()             // bound_available=true 即置首（Closed/HalfOpen 同参覆盖，注释说明）
fn matrix_bound_healthy_rescues_when_queue_all_open() // base=[]（公共队列全熔断/为空的救场输入）+ bound=p + failover 开 +
                                                      //   available → [p]、routed=true（救场语义，spec §5 细则 1；
                                                      //   DB 集成对应 select_providers Err 路径，见 Step 7 第 7 步）
```

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.tomn apply_project_binding`
Expected: FAIL（函数不存在）

- [ ] **Step 3: 实现纯函数**

```rust
match bound {
    None => ProjectRoutePlan { candidates: base, routed: false },
    Some(p) if !failover_enabled => ProjectRoutePlan { candidates: vec![p], routed: true },
    Some(p) if bound_available => {
        let id = p.id.clone();
        let mut v = vec![p];
        v.extend(base.into_iter().filter(|q| q.id != id));
        ProjectRoutePlan { candidates: v, routed: true }
    }
    Some(_) => ProjectRoutePlan { candidates: base, routed: false },
}
```

- [ ] **Step 4: 跑绿（纯函数）**

Run: `cargo test --manifest-path src-tauri/Cargo.toml apply_project_binding matrix`
Expected: PASS

- [ ] **Step 5: 写失败测试 —— DB 集成（`select_providers_for_request`；仿现有 `test_failover_enabled_uses_queue_order_ignoring_current` :408 的建库套路）**

```rust
// 语义矩阵四象限中两条有 DB 依赖的端到端链路 + 边界 1 两路径（spec §9 DAO 级联类别的路由侧）：
async fn request_with_binding_prepends_bound_provider()      // 建库：providers a,b + queue [a,b] + failover 开 +
                                                             //   db.insert_or_update_route("/p","claude","b")
                                                             //   → select_providers_for_request("claude", Some("/p")) == ([b,a], true)
async fn request_without_header_matches_legacy_behavior()    // None → (select_providers 结果, false)，且与 select_providers 直接调用逐 id 相等
async fn route_row_missing_falls_back_to_default()           // 无绑定行 → (base, false)
async fn provider_deleted_cascade_falls_back()               // 绑定后 db.delete_provider("claude", "b")（T2 级联）
                                                             //   → (base, false)（spec §8 边界 1 路径二）
async fn non_claude_app_type_ignores_header_path()           // app_type="codex" + project_path=Some → (base, false)
```

- [ ] **Step 6: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml request_with_binding_prepends_bound_provider`
Expected: FAIL（方法不存在）

- [ ] **Step 7: 实现 `select_providers_for_request`**

1. `let base = self.select_providers(app_type).await;`（**Result 暂存，此步不 `?` 上抛**——公共队列全熔断的 Err 留给第 7 步救场判定）
2. `project_path` 为 `None` 或 `app_type != "claude"` → base 原样透传：`Ok(v)` → `Ok((v, false))`，`Err` 直接上抛（**Codex Official 强制单路由分支 :72-81 由 select_providers 内部原样保留**，绑定不介入）
3. `bound_id = self.db.find_route(project_path, app_type).ok().flatten()`；`None` → base 原样透传（Ok → `(v, false)`，Err 上抛）
4. `bound = self.db.get_provider_by_id(bound_id, app_type)?`；`None`（防御：供应商已删）→ base 原样透传（同上）
5. `failover_enabled = self.db.get_proxy_config_for_app(app_type).await.map(|c| c.auto_failover_enabled).unwrap_or(false)`（每请求直查 DB，spec §5 细则 2；select_providers 内部那次读不可复用，重读一次与现有模式一致）
6. `bound_available`：仅 `failover_enabled` 时查 `self.get_or_create_circuit_breaker(&format!("{app_type}:{bound_id}")).is_available().await`（**查询不消耗 HalfOpen 名额**，与 :106 现状同类用法）；否则 `true`
7. base 统一走 `apply_project_binding`：`Ok(v)` 直接传入；`Err(e)`（公共队列全熔断 AllProvidersCircuitOpen / 无供应商 NoProvidersConfigured）**视为 `vec![]` 传入**（救场分支在 Err 上抛之前介入，spec §5 细则 1：failover 开 + 绑定者健康 → 候选链=[绑定者]，绑定请求不收到该 Err）；纯函数产出空候选链时 → `Err(e)` 原样上抛（绑定者也不可用 → 仍按现状返回全部熔断错误，兼容 spec §5 错误语义）；正常产出 → `Ok((plan.candidates, plan.routed))`

- [ ] **Step 8: 跑绿 + 回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::provider_router`
Expected: PASS（新用例 + 现有 8 个路由测试全绿，证明现状语义未破坏）

---

### Task 7: handler_context + forwarder 集成 —— 信号提取 / 上下文字段 / 全局切换排除

**Files:**
- Modify: `src-tauri/src/proxy/handler_context.rs`（struct :35-78 加两字段；`new` :93-183 内提取信号并替换 :139-141 调用；`create_forwarder` :232-250 传新参；新增模块级纯函数 `extract_project_dir`）
- Modify: `src-tauri/src/proxy/forwarder.rs`（struct :158-190 加字段；`new` :240 加尾部参数；新增模块级纯函数 `should_switch_to_provider`；4 处判定 :561 / :664 / :810 / :974 替换）
- Test: 两文件内嵌 tests 追加（`handler_context.rs:307`、`forwarder.rs:3840` 两个现有 tests 模块）

**Interfaces:**
- Consumes: T3 的 `percent_decode_project_path`；T6 的 `select_providers_for_request`
- Produces（T8/T9 依赖）:
  ```rust
  // handler_context.rs
  pub struct RequestContext {
      // ...现有字段...
      pub project_dir: Option<String>,  // header 解出的合法项目路径；无 header / decode 失败 / 非 claude → None
      pub project_routed: bool,         // "经项目绑定路由"标记（spec §5 细则 7）
  }
  /// 纯函数：仅 claude app_type 且 header 合法时返回 Some(decode 后路径)
  pub(crate) fn extract_project_dir(headers: &axum::http::HeaderMap, app_type_str: &str) -> Option<String>

  // forwarder.rs
  /// 绑定路由的请求永不触发全局"当前供应商"切换（spec §5 细则 7）
  pub(crate) fn should_switch_to_provider(
      current_provider_id_at_start: &str, provider_id: &str, project_routed: bool,
  ) -> bool   // !project_routed && current != provider_id
  ```

- [ ] **Step 1: 写失败测试**

```rust
// handler_context.rs tests（仿 :307 现有纯函数测试风格，无需 ProxyState）：
fn extract_project_dir_from_valid_header()        // HeaderMap 插入 "x-cc-project": "/Users/x/%E9%A1%B9%E7%9B%AE"（axum HeaderMap 大小写不敏感）
                                                 //   + app_type_str="claude" → Some("/Users/x/项目")
fn extract_project_dir_none_when_decode_fails()   // 值 "%zz" → None（Review Focus 1）
fn extract_project_dir_none_when_header_absent()
fn extract_project_dir_none_for_non_claude()      // app_type_str="codex" + 合法 header → None

// forwarder.rs tests：
fn should_switch_true_only_for_unbound_drift()    // (current="a", provider="b", routed=false) → true（现状语义）
fn should_switch_false_when_same_provider()       // ("a","a",false) → false
fn should_switch_never_when_project_routed()      // ("a","b",true) → false（Review Focus 6 —— 4 处调用点统一走本函数）
```

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml extract_project_dir && cargo test --manifest-path src-tauri/Cargo.toml should_switch`
Expected: FAIL（函数不存在）

- [ ] **Step 3: 实现 handler_context 侧**

1. `extract_project_dir`：`app_type_str == "claude"` 时 `headers.get("x-cc-project").and_then(|v| v.to_str().ok()).and_then(percent_decode_project_path)`，否则 `None`
2. `new()` 内（:139 调用点替换，**时序不变**——仍在 RequestContext 创建时调用一次）：
   ```rust
   let project_dir = extract_project_dir(headers, app_type_str);
   let (providers, project_routed) = state.provider_router
       .select_providers_for_request(app_type_str, project_dir.as_deref())
       .await
       .map_err(|e| /* 与现有 :143-149 相同的 match 映射 */)?;
   ```
3. `RequestContext` 加 `project_dir` / `project_routed` 字段，`new` 尾部构造时填入（`vision_routed: false` 同款位置）
4. `create_forwarder` 的 `RequestForwarder::new(...)` 调用在 `max_retries` 之后追加 `self.project_routed` 实参

- [ ] **Step 4: 实现 forwarder 侧**

1. 模块级纯函数 `should_switch_to_provider`（签名见 Interfaces）
2. `RequestForwarder` struct 加 `/// 请求是否经项目绑定路由（绑定请求不改写全局 current，spec §5 细则 7）` + `project_routed: bool`
3. `RequestForwarder::new` 参数列表尾部追加 `project_routed: bool` 并存入 struct
4. 4 处判定（:561-562 / :664-666 / :810-812 / :974-976）统一替换为：
   ```rust
   let should_switch = should_switch_to_provider(
       &self.current_provider_id_at_start, &provider.id, self.project_routed);
   ```
   （模块级纯函数直接调用，与 Interfaces 声明一致，无 `self.` 接收者；若 4 处调用形式因借用细节略有差异，保持"统一调纯函数"不变）

- [ ] **Step 5: 跑绿 + 回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::handler_context && cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::forwarder`
Expected: PASS（新测试 + 两个模块既有测试全绿）

- [ ] **Step 6: 覆盖数校验（spec §9 全局切换回归的静态保障）**

Run: `rg -c "should_switch_to_provider" src-tauri/src/proxy/forwarder.rs`
Expected: `5`（1 定义 + 4 调用点；若不等于 5，说明有判定点漏改，禁止收尾）

---

### Task 8: 归因落库 —— logger.rs + handlers.rs + usage_stats.rs

**Files:**
- Modify: `src-tauri/src/proxy/usage/logger.rs`（`RequestLog` :64-90 加两字段；INSERT 列清单 :171-179 尾部扩展；测试构造点 :292 / :334 / :540 / :844 补默认值）
- Modify: `src-tauri/src/proxy/handlers.rs`（`ClaudeUsageLog` 结构 :306 起加两字段；`prepare_claude_usage_log` :321-352 填值；`write_claude_usage_log` 及其余 RequestLog 构造点透传）
- Modify: `src-tauri/src/proxy/response_processor.rs:645`（vision_routed 同款参数链补 project 字段）
- Modify: `src-tauri/src/services/usage_stats.rs`（`RequestLogDetail` :127-163 加两字段；`row_to_request_log_detail` :176 与 27 列文档注释 :165-173 扩为 29 列；`LogFilters` :105-112 加 `project_dir`；`get_request_logs` :1560 加筛选与 SELECT 列）
- Test: `logger.rs` 内嵌 tests（:511 起）与 `usage_stats.rs` 内嵌 tests（:2370 起）追加

**Interfaces:**
- Consumes: T7 的 `RequestContext.project_dir` / `project_routed`
- Produces:
  ```rust
  // logger.rs
  pub struct RequestLog {
      // ...
      pub project_dir: Option<String>,   // 来源项目路径，NULL=未识别
      pub project_routed: bool,          // 是否因绑定改变路由
  }
  // INSERT 列序（vision_routed 之后追加）：..., created_at, vision_routed, project_dir, project_routed → ?27, ?28
  // usage_stats.rs
  pub struct RequestLogDetail {
      // ...
      #[serde(skip_serializing_if = "Option::is_none")]
      pub project_dir: Option<String>,
      pub project_routed: bool,
  }
  pub struct LogFilters {
      // ...
      pub project_dir: Option<String>,   // serde camelCase → 前端 projectDir
  }
  ```

- [ ] **Step 1: 写失败测试**

```rust
// logger.rs tests（Database::memory() 直接插 RequestLog，仿现有 :511 套路）：
fn log_request_persists_project_fields()        // project_dir=Some("/p"), project_routed=true → SQL 直查两列值正确
fn project_fields_not_in_semantic_hash()        // 同语义两条（仅 project_dir/project_routed 不同）→ 第二次 log_request
                                               //   返回 Ok 且表内仍 1 行（锁死 spec §10 风险 1）
// usage_stats.rs tests（:2370 现有套路，内存库插两行不同 project_dir）：
fn project_dir_filter_narrows_results()         // LogFilters{project_dir: Some("/a")} → 仅 /a 行；None → 全部
fn detail_row_maps_project_columns()            // 回读 RequestLogDetail.project_dir/project_routed 正确（27→29 列不乱序）
```

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml log_request_persists_project_fields`
Expected: FAIL（字段不存在，编译错误）

- [ ] **Step 3: 实现**

1. `logger.rs`：`RequestLog` 加字段；INSERT 语句 `vision_routed` 后追加 `, project_dir, project_routed` 与 `?27, ?28`；`params![...]` 尾部补 `log.project_dir, log.project_routed as i64`；`UsageSemantic` **一个字都不改**，其旁加中文注释"归因字段（vision_routed / project_*）不参与语义哈希（spec §10 风险 1）"
2. `handlers.rs`：`ClaudeUsageLog` 加 `project_dir: Option<String>` / `project_routed: bool`；`prepare_claude_usage_log` 填 `project_dir: ctx.project_dir.clone(), project_routed: ctx.project_routed`；`write_claude_usage_log` 透传到 `RequestLog`
3. **以编译错误为清单**：`cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | rg "project_dir|project_routed|RequestLog"` 逐个补齐全部构造点——预计 `response_processor.rs:645` 参数链（codex/gemini 等分支：统一传 `ctx.project_dir.clone()` / `ctx.project_routed`）与 `logger.rs` 测试构造 4 处（测试补 `project_dir: None, project_routed: false`）
4. `usage_stats.rs`：`RequestLogDetail` / `row_to_request_log_detail`（`row.get(27)?` / `row.get::<_, i64>(28)? != 0`）/ 列序文档注释扩 29 列 / `get_request_logs` 的 SELECT（:1630 `l.vision_routed` 后加 `, l.project_dir, l.project_routed`）与 `if let Some(ref pd) = filters.project_dir { conditions.push("l.project_dir = ?".to_string()); params.push(Box::new(pd.clone())); }`

- [ ] **Step 4: 跑绿 + 回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::usage::logger && cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage_stats && cargo check --manifest-path src-tauri/Cargo.toml`
Expected: PASS + `Finished` 零错误（含所有传染构造点）

---

### Task 9: 命令层 —— commands/project_routing.rs 三命令 + 注册

**Files:**
- Create: `src-tauri/src/commands/project_routing.rs`
- Modify: `src-tauri/src/commands/mod.rs`（`pub mod project_routing;` + `pub use project_routing::*;`，仿 :61-62 先例）
- Modify: `src-tauri/src/lib.rs`（invoke_handler :1388 起的列表追加三行）
- Test: `src-tauri/tests/project_routing_commands.rs`（集成测试，`Database::memory()` 为 `pub fn`，mod.rs:186；Tauri State 不入测——被测函数为非 command 的核心函数）

**Interfaces:**
- Consumes: T2 全部 DAO；T4 的 `set_project_header` / `remove_project_header` / `read_sync_status` / `SyncStatus`；T5 的 `scan_projects`；`crate::config::get_home_dir()`（config.rs:22）
- Produces（T10 前端 invoke 契约）:
  ```rust
  #[derive(serde::Serialize)]
  #[serde(rename_all = "camelCase")]
  pub struct ProjectRouteInfo {
      pub project_path: String,
      pub basename: String,
      pub last_active_at: i64,
      pub has_active_session: bool,
      pub bound_provider_id: Option<String>,
      pub bound_provider_name: Option<String>,
      pub bound_provider_valid: bool,   // false = 绑定的供应商已删（§8 边界 9"已失效"态）
      pub sync_status: String,          // "synced" | "out_of_sync" | "orphan_header"
  }

  /// 可测核心（command 只是薄壳）：scanner + DB 绑定 + 同步状态合并
  pub(crate) fn build_project_list(db: &Database, home: &std::path::Path, now: i64) -> Result<Vec<ProjectRouteInfo>, AppError>

  #[tauri::command] pub fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectRouteInfo>, AppError>
  #[tauri::command] pub fn set_project_route(state: State<'_, AppState>, project_path: String, provider_id: String) -> Result<(), AppError>
  #[tauri::command] pub fn clear_project_route(state: State<'_, AppState>, project_path: String) -> Result<(), AppError>
  ```

- [ ] **Step 1: 写失败测试**（`src-tauri/tests/project_routing_commands.rs`）

```rust
// fixture：TempDir 铺 claude.json（一个项目 /tmp/.../projA，需真实 mkdir 该目录）+ Database::memory()
fn build_list_merges_scan_and_binding()        // 建供应商 p1 → set 绑定 → build → bound_provider_id==Some("p1")、
                                               //   sync_status=="synced"（set 已写文件）
fn build_list_marks_out_of_sync_and_orphan()   // 手动删 settings.local.json → bound 仍在 → "out_of_sync"；
                                               //   clear 绑定后手动写含目标行的文件 → "orphan_header"
fn build_list_invalid_provider_flagged()       // 绑定 p1 → db.delete_provider("claude","p1")（级联清行）
                                               //   → 重新 insert_or_update_route 指向已删 p1（构造悬空）→ bound_provider_valid==false
fn set_route_double_write_db_then_file()       // set → DB 行在 + 文件目标行在；文件失败场景（project_path 指向
                                               //   不可写位置）→ Err 且 DB 行保留（Review Focus 2 闭环）
fn clear_route_removes_both()                 // clear → DB 行无 + 文件目标行无（文件不存在也 Ok）
```

- [ ] **Step 2: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test project_routing_commands`
Expected: FAIL（文件/函数不存在）

- [ ] **Step 3: 实现**

1. `build_project_list`：`scan_projects(home, now)` → `db.list_routes("claude")` 建 `HashMap<project_path, provider_id>` → 逐项：`let bound = routes.get(&sp.project_path);`（`Option<&String>`）→ `let bound_provider = bound.and_then(|pid| db.get_provider_by_id(pid, "claude").ok().flatten());` → `read_sync_status(&sp.project_path, bound.map(|_| sp.project_path.as_str()))`（第二个参数：DB 有绑定即传该路径本身，供文件行对比）→ `sync_status.as_str()` 入 DTO
2. `list_projects`：`build_project_list(&state.db, &get_home_dir(), chrono::Utc::now().timestamp())`
3. `set_project_route`：**先 DB 后文件**——`db.insert_or_update_route(&project_path, "claude", &provider_id)?` → `settings_local_writer::set_project_header(&project_path, &state.db)?`；文件失败时 Err 上抛、DB 行保留（sync_status 兜底修复，T4 测试已锁此语义）
4. `clear_project_route`：`db.delete_route(&project_path, "claude")?` → `remove_project_header(&project_path, &state.db)`
5. `commands/mod.rs` 与 `lib.rs` invoke_handler 注册（追加 `commands::list_projects, commands::set_project_route, commands::clear_project_route`）
6. 错误类型用 `Result<T, AppError>`（与 `commands/usage.rs:104` 先例一致）

- [ ] **Step 4: 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test project_routing_commands && cargo check --manifest-path src-tauri/Cargo.toml`
Expected: PASS + 零错误

---

### Task 10: 前端数据层 —— types / api / query

**Files:**
- Create: `src/types/projectRouting.ts`
- Create: `src/lib/api/projectRouting.ts`
- Create: `src/lib/query/projectRouting.ts`
- Test: `src/lib/api/projectRouting.test.ts`（vitest；mock `@tauri-apps/api/core` 的 `invoke`，锁定命令名与参数形状——这是前后端 IPC 契约的唯一自动防线，Review Focus 5）

**Interfaces:**
- Consumes: T9 的三个 Tauri 命令名与 DTO（camelCase）
- Produces（T11 消费）:
  ```ts
  export type ProjectSyncStatus = "synced" | "out_of_sync" | "orphan_header";
  export interface ProjectRouteInfo {
    projectPath: string; basename: string; lastActiveAt: number; hasActiveSession: boolean;
    boundProviderId?: string; boundProviderName?: string; boundProviderValid: boolean;
    syncStatus: ProjectSyncStatus;
  }
  export const projectRoutingApi = {
    async listProjects(): Promise<ProjectRouteInfo[]>;                      // invoke("list_projects")
    async setProjectRoute(projectPath: string, providerId: string): Promise<void>;  // invoke("set_project_route", { projectPath, providerId })
    async clearProjectRoute(projectPath: string): Promise<void>;            // invoke("clear_project_route", { projectPath })
  };
  export const projectRoutingKeys = { projects: ["projectRoutingProjects"] as const };
  export function useProjectRoutingQuery(enabled: boolean);   // refetchInterval: enabled ? 3000 : false（proxy.ts:28 条件轮询先例）
  export function useSetProjectRoute();                        // mutation → invalidate projects + toast
  export function useClearProjectRoute();                      // 同上
  ```

- [ ] **Step 1: 写失败测试**（`src/lib/api/projectRouting.test.ts`）

```ts
// vi.mock("@tauri-apps/api/core") 后断言：
// listProjects() 调 invoke("list_projects") 无参
// setProjectRoute("/p","pid") 调 invoke("set_project_route", { projectPath: "/p", providerId: "pid" })
// clearProjectRoute("/p") 调 invoke("clear_project_route", { projectPath: "/p" })
```

- [ ] **Step 2: 跑红**

Run: `pnpm test:unit projectRouting`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

- `types/projectRouting.ts`：如 Interfaces
- `lib/api/projectRouting.ts`：仿 `src/lib/api/proxy.ts` 的逐命令 async 函数模式
- `lib/query/projectRouting.ts`：仿 `src/lib/query/proxy.ts`——`useQuery({ queryKey, queryFn, refetchInterval: enabled ? 3000 : false, enabled, placeholderData: (p) => p })`；两个 mutation 仿 `useSetProxyTakeoverForApp`（invalidate + `toast` + `useTranslation`）

- [ ] **Step 4: 跑绿 + 类型检查**

Run: `pnpm test:unit projectRouting && pnpm typecheck`
Expected: PASS + tsc 零错误

---

### Task 11: 前端页面 —— ProjectRoutingPage + App 集成 + i18n 四语言

**Files:**
- Create: `src/components/projectRouting/syncStatus.ts`（页面内纯逻辑抽出，先测后写）
- Create: `src/components/projectRouting/ProjectRoutingPage.tsx`
- Modify: `src/App.tsx` 四处小改：View 类型 union（:116-130 加 `| "projectRouting"`）、`VALID_VIEWS`（:151 起追加）、`renderContent` case（:1030 switch 加分支）、头部按钮（:1694 `sessions` 按钮旁仿写一个，icon 用 lucide 现有 `FolderTree` 或 `Route`）+ 页面标题（:1342 `currentView === "sessions" && t(...)` 同款）
- Modify: `src/i18n/locales/en.json` / `ja.json` / `zh-TW.json` / `zh.json`（新增顶层 `projectRouting` 命名空间）
- Test: `src/components/projectRouting/syncStatus.test.ts`

**Interfaces:**
- Consumes: T10 的 hooks 与 `ProjectRouteInfo` 类型；现有 `useProvidersQuery`（`src/lib/query/queries.ts:56`，供应商下拉数据源，KISS 不新增 API）；Radix Select 先例 `RequestLogTable.tsx:117-138`；Table 组件族（RequestLogTable 同款 import 路径）
- Produces: 可导航的 `/projectRouting` 视图（View 字面量 `"projectRouting"`）

- [ ] **Step 1: 写失败测试**（`syncStatus.test.ts`）

```ts
// syncBadgeTone(status): "synced"→"ok"；"out_of_sync"→"warn"；"orphan_header"→"info"
// providerOptionKind(info): 无绑定→"default"；绑定有效→"bound"；boundProviderValid=false→"invalid"
// 这两个纯函数是页面上唯一有分支逻辑的单元；其余为声明式 JSX
```

- [ ] **Step 2: 跑红**

Run: `pnpm test:unit syncStatus`
Expected: FAIL

- [ ] **Step 3: 实现 `syncStatus.ts` 与 `ProjectRoutingPage.tsx`**

页面结构（单列布局，样式类对齐 RequestLogTable 的 `rounded-lg border bg-card/50` 风格）：

- 数据：`useProjectRoutingQuery(isActive)`（页面挂载即轮询 3000ms，卸载停）
- 供应商下拉数据：复用现有 providers 数据源——`useProvidersQuery("claude")`（`src/lib/query/queries.ts:56`，内部 `providersApi.getAll` → `invoke("get_providers", { app })`，后端 `commands/provider.rs:24`），取返回的 `providers` 供下拉；**不新增**供应商列表的任何新 API 封装（KISS：T10 的 `projectRoutingApi` 三命令契约不变，`src/lib/api/projectRouting.ts` 无需为此改动）
- 行：名称（`basename` + tooltip 完整 `projectPath`）/ 活跃时间（相对时间）/ 「使用中」徽标（`hasActiveSession`）/ Radix Select（"默认"=不绑定 + 供应商名列表 + `invalid` 态显示"已失效"占位且保留当前值，§8 边界 9）/ 同步状态徽标（tone 色点 + 文案）
- 交互：Select 变更 → 选"默认"调 `clearProjectRoute`，否则 `setProjectRoute`；`out_of_sync` 显示「修复」按钮 → `setProjectRoute`（按 DB 重写文件）；`orphan_header` 显示「清除」按钮 → `clearProjectRoute`（提示文案说明该 header 不参与路由，spec §7.1 真值表的界面动作列）
- 空态：无项目时展示引导文案（i18n）

- [ ] **Step 4: App.tsx 集成 + i18n**

- 四处小改逐项落位（Files 列表行号）；i18n 最小 key 集（四语言全量翻译）：
  `projectRouting.title` / `.subtitle` / `.column.project` / `.column.activity` / `.column.provider` / `.column.sync` / `.default` / `.invalid` / `.inUse` / `.sync.synced` / `.sync.outOfSync` / `.sync.orphan` / `.sync.fix` / `.sync.clear` / `.empty` / `.toast.saved` / `.toast.failed`

- [ ] **Step 5: 跑绿 + 全量回归**

Run: `pnpm test:unit syncStatus && pnpm test:unit && pnpm typecheck`
Expected: 全 PASS + tsc 零错误

---

### Task 12: 前端历史展示 —— 项目列 + 项目筛选

**Files:**
- Create: `src/components/usage/projectFilter.ts`（唯一项目清单纯函数）
- Modify: `src/types/usage.ts`（`RequestLog` :10-39 加 `projectDir?: string`；`LogFilters` :134-141 加 `projectDir?: string`）
- Modify: `src/components/usage/RequestLogTable.tsx`（筛选区 :117-138 的 statusCode Select 旁加项目 Select；`effectiveFilters` :65-73 透传 `projectDir`；表头 :156-179 与行体加"项目"列，无值显示 `—`）
- Modify: `src/i18n/locales/` 四语言 `usage.projectDir` 键
- Test: `src/components/usage/projectFilter.test.ts`

**Interfaces:**
- Consumes: T8 后端 `RequestLogDetail.projectDir`（camelCase）与 `LogFilters.project_dir`（serde camelCase → 前端 `projectDir`）；`useRequestLogs`（`src/lib/query/usage.ts:305`，filters 形状已含全部 LogFilters 字段，透传即可）
- Produces: 使用统计页项目列 + 筛选

- [ ] **Step 1: 写失败测试**（`projectFilter.test.ts`）

```ts
// uniqueProjectDirs(logs: RequestLog[]): string[] —— 取非空 projectDir 去重、按字母序；空输入 → []
// 断言含 undefined/重复路径的混合输入
```

- [ ] **Step 2: 跑红**

Run: `pnpm test:unit projectFilter`
Expected: FAIL

- [ ] **Step 3: 实现**

- `projectFilter.ts`：`uniqueProjectDirs`
- `types/usage.ts` 两接口加字段（注释说明 camelCase 对齐后端 `project_dir`）
- `RequestLogTable.tsx`：`const [projectDir, setProjectDir] = useState<string | undefined>()`；Select 选项 = `uniqueProjectDirs(logs)`（"全部" + 各路径 basename，选中值为完整路径）；`effectiveFilters` 加 `projectDir`；列渲染 `log.projectDir ?? "—"`（tooltip 完整路径）
- i18n：`usage.projectDir` 四语言

- [ ] **Step 4: 跑绿 + 回归**

Run: `pnpm test:unit projectFilter && pnpm test:unit && pnpm typecheck`
Expected: 全 PASS + tsc 零错误

---

### Task 13: 端到端验证与全量回归

**Files:**
- 无新文件；本任务是验证关口（不写产品代码）

**Interfaces:**
- Consumes: T1-T12 全部产物
- Produces: 全量绿的验证证据（测试输出/编译输出）

- [ ] **Step 1: Rust 全量**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: `test result: ok. ... 0 failed`（lib + tests/ 全部目标）

- [ ] **Step 2: Rust 编译零警告路径检查**

Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | rg -c "warning: unused|error"`
Expected: 无输出（rg 计数 0 = 无 unused 警告、无 error；若有逐条修复后重跑）

- [ ] **Step 3: 前端全量**

Run: `pnpm test:unit && pnpm typecheck`
Expected: vitest 全 PASS + tsc 零错误

- [ ] **Step 4: 覆盖矩阵静态校验**

Run: `rg -c "should_switch_to_provider" src-tauri/src/proxy/forwarder.rs && rg -c "select_providers_for_request" src-tauri/src/proxy/handler_context.rs`
Expected: `5` 与 `1`（T7 Step 6 同款校验，最终关口复查）

- [ ] **Step 5: 人工端到端（spec §9 端到端类别；需真实环境，由主控/用户执行，非子代理）**

清单（可选，不阻塞计划验收）：
1. `pnpm dev` 启动应用 → 项目路由页绑定某真实项目到供应商 B（全局 current 为 A）
2. 该项目目录 `claude -p "hi"` → 使用统计新行 `projectDir` = 该项目且 provider = B
3. 另一未绑定项目 `claude -p` → 行为与升级前一致（provider = A）
4. 删除供应商 B → 绑定自动消失（级联），项目回默认策略
5. 手改该 `.claude/settings.local.json` 删掉 header → 页面 3s 内出现 `out_of_sync` → 点修复恢复
6. 绑定者上游失败（如绑定的供应商配错 key）→ 请求成功且实际 provider = 公共队列次位（软绑定降级行为级验证，spec §5 矩阵第一行"失败行为"）

---

## 附录 B：Spec 覆盖映射表

| Spec 条目 | 任务 |
|-----------|------|
| §3.2 编码与匹配规范 | T3（定义）；T4/T7（消费） |
| §3.3 三源探测 | T5 |
| §4.1 project_routes 表 + 级联 | T1（DDL）/ T2（DAO + 级联） |
| §4.2 日志表加列 SCHEMA_VERSION 21 | T1（DDL/迁移）/ T8（写入与筛选） |
| §4.3 settings_local_backup | T1（DDL）/ T2（DAO）/ T4（写入策略） |
| §5 路由矩阵 + 细则 1-9 | T6（矩阵/细则 1/2/4/9）/ T7（细则 3/5/7/8；细则 6 = 不改 handlers.rs:157，Global Constraints） |
| §6.1-1 dao/project_routes.rs | T2 |
| §6.1-2 commands/project_routing.rs | T9 |
| §6.1-3 settings_local_writer.rs | T4 |
| §6.1-4 project_scanner.rs | T5 |
| §6.2-5 provider_router.rs | T6 |
| §6.2-6 handler_context.rs | T7 |
| §6.2-7 logger.rs + usage_stats.rs | T8 |
| §6.2-8 forwarder.rs | T7 |
| §7.1 前端新 View/绑定 UI/同步徽标/数据层 | T10（数据层）/ T11（页面） |
| §7.2 历史展示 + 筛选 | T12 |
| §8 边界 1（供应商删除两路径） | T2（级联）+ T6（路由回退两路径测试） |
| §8 边界 2（文件手改/删） | T4（sync_status）+ T9（build_list）+ T11（修复 UI） |
| §8 边界 3/4（自有 header / 同名接管） | T3（逐字节保留 / 替换全部目标行）+ T4 |
| §8 边界 5（中文/空格路径） | T3（编码往返属性测试） |
| §8 边界 6/7（claude.json 损坏/目录不存在） | T5 |
| §8 边界 8（多客户端并发） | 架构天然满足（无会话状态，header 每请求携带）——无专门测试 |
| §8 边界 9（下拉"已失效"） | T9（bound_provider_valid）+ T11（invalid 态） |
| §9 路由重排单测 | T6 |
| §9 settings_local_writer 属性测试 | T3（编码/行操作属性）+ T4（文件级属性） |
| §9 全局切换回归 | T7（纯函数矩阵 + 4 处判定静态校验）+ T13 Step 4 |
| §9 DAO 级联清理 | T2 |
| §9 project_scanner fixture | T5 |
| §9 端到端 | T13 Step 5 |

## 附录 C：任务 DAG

```
T1 schema ──▶ T2 dao ──┬──▶ T4 writer ──┐
T3 project_header ─────┘──▶ T6 router ──┤
        │                              ├──▶ T9 commands ──▶ T10 fe-data ──▶ T11 fe-page
        └──────────────▶ T7 handler+forwarder                │
T5 scanner ────────────────────────────▶ T9                  │
                                         T7 ──▶ T8 logger ──┴──▶ T12 fe-history
T1..T12 ──▶ T13 e2e
```

精确依赖表（无循环）：

| 任务 | 直接依赖 |
|------|----------|
| T1 | — |
| T2 | T1 |
| T3 | — |
| T4 | T2, T3 |
| T5 | — |
| T6 | T2 |
| T7 | T3, T6 |
| T8 | T7 |
| T9 | T2, T4, T5 |
| T10 | T9 |
| T11 | T10 |
| T12 | T8 |
| T13 | T1-T12 |

可并行波次（≤4 并发约束下的调度建议）：
- 波 1：{T1, T3, T5}
- 波 2：{T2,（T3/T5 完成后无新可开）}
- 波 3：{T4, T6}
- 波 4：{T7, T9}（T9 依赖 T4+T5+T2 齐备）
- 波 5：{T8, T10}
- 波 6：{T11, T12}
- 波 7：{T13}

## 附录 D：每任务验证命令汇总

| 任务 | 验证命令（cwd = /Users/dev/Project/cc-switch） | 期望 |
|------|------------------------------------------------|------|
| T1 | `cargo test --manifest-path src-tauri/Cargo.toml --lib database::tests` | ok, 0 failed |
| T2 | `cargo test --manifest-path src-tauri/Cargo.toml --lib dao` | ok, 0 failed |
| T3 | `cargo test --manifest-path src-tauri/Cargo.toml project_header` | ok, 0 failed |
| T4 | `cargo test --manifest-path src-tauri/Cargo.toml settings_local_writer` | ok, 0 failed |
| T5 | `cargo test --manifest-path src-tauri/Cargo.toml project_scanner` | ok, 0 failed |
| T6 | `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::provider_router` | ok, 0 failed（含既有 8 测试） |
| T7 | `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::handler_context && cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::forwarder && rg -c "should_switch_to_provider" src-tauri/src/proxy/forwarder.rs` | ok + `5` |
| T8 | `cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::usage::logger && cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage_stats && cargo check --manifest-path src-tauri/Cargo.toml` | ok + Finished |
| T9 | `cargo test --manifest-path src-tauri/Cargo.toml --test project_routing_commands && cargo check --manifest-path src-tauri/Cargo.toml` | ok + Finished |
| T10 | `pnpm test:unit projectRouting && pnpm typecheck` | PASS + 零错误 |
| T11 | `pnpm test:unit syncStatus && pnpm test:unit && pnpm typecheck` | PASS + 零错误 |
| T12 | `pnpm test:unit projectFilter && pnpm test:unit && pnpm typecheck` | PASS + 零错误 |
| T13 | 附录内 5 步 | 全绿 |
