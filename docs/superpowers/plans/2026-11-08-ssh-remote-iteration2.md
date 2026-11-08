# SSH 远程会话二期迭代实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现设计文档定义的四项能力：SSH 设置表单样式对齐、远程正文搜索（rg/grep 双路径）、远程会话复制恢复命令、远程用量统计（增量镜像架构）。

**Architecture:** 搜索走「远端 rg/grep 批量扫描 → 命中文件拉回本地解析 → 与本地索引搜索同构的 ContentHit」；恢复命令在 `rebind_remote_meta` 生成 ssh 包装串点亮既有复制按钮；用量统计镜像本机增量链路（清单 diff → 增量字节拉取 → 本地复用解析入库 SQLite，rollup 加 host 维度）。全部新命令参数 `hostId: Option<String>` 缺省 = 本机 = 现状零变化。

**Tech Stack:** Tauri 2.8.2 + Rust（tokio、rusqlite）+ React 18 + TanStack Query。不新增 Rust/npm 依赖。

**Spec:** `docs/superpowers/specs/2026-11-08-ssh-remote-iteration2-design.md`

## Global Constraints

- 本机行为零回归：`list_sessions` / `search_session_content` / usage 五命令（`get_usage_summary`、`get_usage_summary_by_app`、`get_provider_stats`、`get_model_stats`、`get_request_logs`）加 `hostId: Option<String>` 缺省 = 本机 = 现状完全一致
- 远端命令注入防护：搜索词与路径经 `escape_remote_path` 单引号转义（`'`→`'\''`），命令用 `--` 分隔符
- 13 个本地 scanner 解析与扫描逻辑零修改（一期既有例外继续：`parse_session_lines` 可见性等）
- 不存储 SSH 密码/口令；`identity_file` 仅路径引用
- 迁移遵循既有形态：`SCHEMA_VERSION` 26→27（`src-tauri/src/database/mod.rs:58`），rollup 重建照 `migrate_v10_to_v11`（schema.rs:1502-1551）RENAME/CREATE/INSERT/DROP 模式
- i18n 五语言全量补齐（zh / zh-TW / en / ja / tr）
- Rust 测试遵循既有惯例：`serial_test` 全局锁、`UserEnvGuard`、`SshSettingsSandbox`（`CC_SWITCH_TEST_HOME`）；localhost 集成冒烟环境探测自动 skip
- 注释用中文，ASC2 分块风格

## Review Focus

1. **rg/grep 无匹配 exit 1 被当远端错误**（搜不存在的词必现报错）→ Task 3 退出码映射测试：exit 0/1 成功（1=空结果）、≥2 透传 `SshError::Remote`、255 连接错误；禁止 `; :` 归零
2. **文件路径含冒号致 `path:lineno:line` 解析歧义** → Task 3 解析器测试：从右定位第一个 `:<纯数字>:` 段；含冒号路径/行内容正确切分
3. **搜索词含单引号/空格/`--` 前缀串** → Task 3 命令构造测试：`escape_remote_path` 转义 + `--` 分隔后词序不变
4. **首扫中断后重开导致重复计数** → Task 8 幂等测试：中途取消后 force 续扫，入库行数不翻倍（INSERT OR IGNORE + request_id 稳定 + 游标推进原子性）
5. **远程数据污染本机统计**（rollup 30 天剪枝合并、主机删除后残留）→ Task 6 迁移测试（host_id 入主键、本机行 host_id=''）+ Task 8 过滤测试（本机 `NOT LIKE 'ssh:%'` / 远程 `LIKE 'ssh:<id>%'`）

---

### Task 1: SSH 主机设置表单样式对齐

**Files:**
- Modify: `src/components/settings/SshHostsSection.tsx`
- Test: `src/components/settings/SshHostsSection.test.tsx`

**Interfaces:**
- Consumes: 既有公共组件 DialogHeader/Footer（自带 `px-6`）、`NewLayoutDialog.tsx:122` 的 `px-6 py-5` body 惯例、`CodexFormFields.tsx:1735` 的 `grid-cols-[1fr_140px]` 端口窄列先例
- Produces: 无（纯样式，无接口变化）

- [ ] **Step 1: 写失败测试（样式类名断言）**

vitest 断言 AddHostDialog / EditHostDialog 渲染结果：body 容器含 `px-6`；user/port 行为 `grid-cols-[1fr_140px]`；手动 tab 底部按钮区为 DialogFooter（role/结构断言）；DialogContent 不再是 `max-w-lg`（改 `max-w-md`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run src/components/settings/SshHostsSection.test.tsx`
Expected: FAIL（新断言不满足）

- [ ] **Step 3: 实施样式修改**

按设计 §4.1：(a) AddHostDialog 的 Tabs/列表/表单区（现 L395-508）与 EditHostDialog 表单区（现 L542-552）外包一层 `<div className="px-6">`（底距用既有 TabsContent/Footer 边距，不加额外 pb，防止与 Footer 的 `pb-5` 叠加）；(b) user/port 双列 `grid grid-cols-2 gap-4` → `grid grid-cols-[1fr_140px] gap-4`；(c) 手动 tab 手写按钮区 `<div className="flex justify-end gap-2 pt-1">` → 迁入 DialogFooter（与导入 tab L479 同构，两 tab 共用一个 Footer 或各自 Footer 均可，取布局不跳变的方案）；(d) 导入列表行 `px-2.5 py-2` → `px-2.5 py-1.5`（密度收紧）；(e) 两对话框 `max-w-lg` → `max-w-md`。

- [ ] **Step 4: 跑测试确认通过 + 既有测试回归**

Run: `npx vitest run src/components/settings/SshHostsSection.test.tsx src/components/shell/Sidebar.test.tsx`
Expected: PASS（全部）

- [ ] **Step 5: Commit**

```bash
git add src/components/settings/SshHostsSection.tsx src/components/settings/SshHostsSection.test.tsx
git commit -m "style(settings): align SSH host dialogs with app form conventions"
```

---

### Task 2: 远程恢复命令生成

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`（rebind_remote_meta 及新函数）
- Modify: `src-tauri/src/session_manager/mod.rs`（rebind 调用点传 host config）
- Modify: `src/components/sessions/reader/SessionReaderHeader.tsx:192-209`（cd 恢复项远程隐藏）
- Test: `src-tauri/src/session_manager/ssh.rs` 内 `#[cfg(test)]`、`src/components/sessions/reader/SessionReaderHeader.test.tsx`

**Interfaces:**
- Consumes: `SshHostConfig { id, label, host, user: Option<String>, port: Option<u16>, identity_file: Option<String> }`（settings.rs:212-222）、`escape_remote_path(&str) -> String`（ssh.rs:82）、`SessionMeta { session_id, project_dir, resume_command, provider_id, source_path }`
- Produces:
  - `pub(crate) fn build_remote_resume_command(host: &SshHostConfig, provider_id: &str, session_id: &str, project_dir: Option<&str>) -> Option<String>`——`ssh [-p N] [-i key] -t [user@]host 'cd <dir> && <内层>'`；provider 非 claude/codex 返回 None（MVP 范围）；project_dir 为 None 时仅内层；内层：claude `claude --resume <id>`、codex `codex resume <id>`
  - `rebind_remote_meta` 签名改为 `(mut meta: SessionMeta, host: &SshHostConfig, remote_path: &str) -> SessionMeta`（host_id 从 `host.id` 取；`resume_command` 由 `build_remote_resume_command` 填充）

- [ ] **Step 1: 写失败测试（Rust 纯函数组合矩阵）**

`build_remote_resume_command` 断言：全参数（user+port+identity+project_dir）→ `ssh -p 50051 -i /key -t guqiu@host 'cd '\''/data/proj'\'' && claude --resume <id>'` 形态（单引号转义逐字符断言）；无 project_dir → 无 `cd` 前缀；project_dir 含单引号 → `'\''` 转义；provider 为 gemini → None；命令串不含 `BatchMode`/`ControlMaster`/`ConnectTimeout`/`ControlPath`（断言）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib ssh::tests::build_remote_resume`（模块路径按实际）
Expected: FAIL（函数不存在）

- [ ] **Step 3: 实现 build_remote_resume_command + rebind 改造**

纯函数拼装；`rebind_remote_meta` 调用它填充 `resume_command`；调用点（mod.rs scan_remote_sessions 链路）改传 `&host`。既有 rebind 测试同步更新（resume_command 从 None 断言改为按 provider 期望值断言）。

- [ ] **Step 4: 跑测试确认通过 + 模块回归**

Run: `cargo test -p cc-switch --lib session_manager` 
Expected: PASS

- [ ] **Step 5: 前端——阅读器「复制进入目录并恢复」远程隐藏**

`SessionReaderHeader.tsx:192-209` 的 `cd` DropdownMenuItem 加条件：`session.projectDir && command && !isRemoteSession(session.sourcePath)`（`isRemoteSession` 已有，utils.ts:73）。vitest：远程会话（sourcePath `ssh:` 前缀）下该项不渲染、其余恢复方式菜单仍渲染。

- [ ] **Step 6: 跑前端测试 + Commit**

Run: `npx vitest run src/components/sessions/reader/SessionReaderHeader.test.tsx src/components/sessions`
Expected: PASS

```bash
git add -A src-tauri/src/session_manager src/components/sessions
git commit -m "feat(sessions): generate copyable ssh resume command for remote sessions"
```

---

### Task 3: ssh.rs 搜索原语层（rg/grep 双路径 + 退出码 + 输出解析）

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`
- Test: `ssh.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `run_command` 内部执行路径（ssh.rs:277 附近，仅 exit 0 成功 / 255 连接 / 其余 Remote）、`escape_remote_path`
- Produces:
  - `pub(crate) fn run_remote_with_codes(host: &SshHostConfig, cmd: &str, timeout: Duration, accepted: &[i32]) -> Result<Vec<u8>, SshError>`——退出码在 `accepted` 集合内返回 stdout；255 恒为连接错误；其余 `SshError::Remote`（复用 run_command 的进程执行体，加 accepted 参数的最小变体，既有 `run_remote` 不动）
  - `pub(crate) fn build_search_command(pattern: &str, files: &[String], use_rg: bool) -> String`——rg: `rg -i -n -F --no-heading --with-filename --no-ignore -m 5 -- <p> <f…>`；grep: `grep -inFH -m 5 -- <p> <f…>`；pattern 与 files 均经 `escape_remote_path`
  - `pub(crate) fn parse_search_output(stdout: &[u8]) -> Vec<RemoteSearchLine>`——`RemoteSearchLine { path: String, line_no: u64, text: String }`；行格式 `path:lineno:text`，**从右向左定位第一个 `:<纯数字>:` 段**切分（容忍路径与文本含冒号）
  - `pub(crate) fn detect_remote_rg(host: &SshHostConfig) -> bool`——`command -v rg` 探测，结果存进程内 `RwLock<HashMap<String, bool>>`（per-host 缓存；探测失败=false 并缓存，降级 grep）

- [ ] **Step 1: 写失败测试**

四组单测：(a) `build_search_command` 双路径参数串逐字断言 + pattern 含空格/单引号/`--foo` 前缀的转义断言 + 空文件列表返回空命令；(b) `parse_search_output`：常规多行、含冒号路径（`/a:b/c.jsonl:42:{"x":1}`）、无 `:` 行忽略、超长行（1MB）保留、空输入返回空；(c) 退出码映射逻辑（对 `run_remote_with_codes` 的分类函数做表驱动断言：accepted 含 1 时 exit 1 → Ok(空)；exit 2 不在 accepted → Remote；255 → 连接错误——进程级测试用 localhost 冒烟覆盖，单测测映射函数）；(d) `detect_remote_rg` 缓存命中不重复探测（用本地 `command -v` 真跑一次验证 true/false 两态）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib ssh::tests::search` 
Expected: FAIL

- [ ] **Step 3: 实现四个函数**

- [ ] **Step 4: 跑测试确认通过 + ssh 模块回归**

Run: `cargo test -p cc-switch --lib session_manager::ssh`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/session_manager/ssh.rs
git commit -m "feat(ssh): add rg/grep search primitives with exit-code semantics"
```

---

### Task 4: 后端远程正文搜索集成

**Files:**
- Modify: `src-tauri/src/session_manager/search.rs`（新增 `search_remote`）
- Modify: `src-tauri/src/commands/session_manager.rs:49-64`（`search_session_content` 加 `hostId`）
- Test: `search.rs` 单测 + `ssh_localtest.rs` localhost 冒烟扩展

**Interfaces:**
- Consumes: Task 3 四函数；`SshScanRegistry::global()` 清单；`parse_ssh_source`；`fetch_to_temp_file`（32MB 上限）；`load_messages`（ssh 源既有支持）；`ContentHit/ContentSnippet/ContentSearchResult/IndexStatus`（serde camelCase 原样）；`DEFAULT_LIMIT = 200`
- Produces:
  - `pub fn search_remote(query: &str, host: &SshHostConfig, providers: Option<Vec<String>>, limit: usize) -> ContentSearchResult`——无清单（registry miss）时返回空 hits + `IndexStatus::default()`
  - 命令层：`search_session_content(query, providerIds, limit, hostId: Option<String>)`——`hostId` 有值 → 读 settings 取 `SshHostConfig`（不存在返回 Err「SSH 主机不存在或已移除」）→ `search_remote`；缺省 → 既有 `search` 零变化

- [ ] **Step 1: 写失败测试（search_remote 聚合单测，不依赖真 ssh）**

对 `search_remote` 的聚合层（把「远端扫描」抽象为可注入的 `scan_fn` 或对内部纯函数 `aggregate_hits(lines: Vec<RemoteSearchLine>, fetch: impl Fn(&str) -> Result<Vec<SessionMessage>>, limit) -> Vec<ContentHit>` 直接测）：每会话 ≤3 snippets、matchCount 总数、`message_index` 恒为 0、fetch 失败（超 32MB）时该会话跳过但不出现在 panic、高亮区间为本地大小写不敏感子串扫描（对 ASCII 与非 ASCII 各一例）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib search::remote` 
Expected: FAIL

- [ ] **Step 3: 实现 search_remote + 命令分流**

数据流按设计 §4.2：registry 清单 → 未压缩 `.jsonl` 过滤（zst 跳过）+ providers 过滤（按远端路径前缀对应 provider 根目录）→ 批 50 `build_search_command` + `run_remote_with_codes(host, cmd, timeout, &[0, 1])` → `parse_search_output` → 命中文件 `fetch_to_temp_file` + `load_messages` → snippet 定位（解析后消息文本上找命中行所属消息；行文本与 grep 行内容对齐——按行首 JSON 摘要或消息内容包含匹配）→ `ContentHit { provider_id, source_path: ssh 源路径, snippets, match_count }`。

- [ ] **Step 4: localhost 集成冒烟扩展（真实 grep/rg 全链路）**

`ssh_localtest.rs` 加用例：tempdir 造两个 jsonl（一含关键词、一不含）→ 按既有 FixtureGuard 模式走 registry 注入 → `search_remote` → 断言命中文件/snippet 文本；再搜不存在词 → 断言空结果**非错误**（覆盖 Review Focus #1 真实进程路径）。

- [ ] **Step 5: 跑全部相关测试**

Run: `cargo test -p cc-switch --lib search ssh_localtest`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/session_manager/search.rs src-tauri/src/commands/session_manager.rs src-tauri/src/session_manager/ssh_localtest.rs
git commit -m "feat(sessions): remote content search via rg/grep with local snippet mapping"
```

---

### Task 5: 前端远程搜索接线

**Files:**
- Modify: `src/lib/api/sessions.ts:67-73`（searchContent 加 hostId）
- Modify: `src/hooks/useSessionContentSearch.ts`（hook 加 hostId + key 扩维 + 远程跳过本机索引同步）
- Modify: `src/lib/query/sessions.ts` 或该 hook 所在 key 工厂（`sessionContentKeys.search` 扩维）
- Modify: `src/components/sessions/SessionManagerPage.tsx`（向 hook 传 hostId）
- Test: `src/hooks/useSessionContentSearch.test.ts`（或既有测试文件）+ `SessionManagerPage.test.tsx` 回归

**Interfaces:**
- Consumes: Task 4 命令层 `hostId` 参数；`sessionKeys.list` 扩维先例（sessions.ts:25）
- Produces: `searchContent(query: string, providerIds?: string[], hostId?: string)`；`useSessionContentSearch({ query, enabled, listIsSuccess, listUpdatedAt, providerIds, hostId })`

- [ ] **Step 1: 写失败测试**

vitest：(a) hostId 变化时 search query key 不同（`sessionContentKeys.search(q, providers, "h1")` ≠ `...("h2")`，缺省 `"local"` 占位对齐 sessions 先例）；(b) hostId 非空时 invoke 参数含 hostId；(c) hostId 非空时 `listUpdatedAt` 变化**不**触发 `sync_session_index`（本机索引同步跳过）。

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run src/hooks/useSessionContentSearch.test.ts`
Expected: FAIL

- [ ] **Step 3: 实现三处接线**

api 透传；hook 签名与 queryKey 扩维；`SessionManagerPage` 把 `effectiveHostId` 传入 hook；本机索引同步 useEffect 加 `!hostId` 条件。

- [ ] **Step 4: 跑测试 + 回归**

Run: `npx vitest run src/hooks src/components/sessions`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/lib src/hooks src/components/sessions
git commit -m "feat(sessions): wire remote host dimension into content search hook"
```

---

### Task 6: rollup host 维度迁移（SCHEMA_VERSION 26→27）

**Files:**
- Modify: `src-tauri/src/database/mod.rs:58`（SCHEMA_VERSION = 27）
- Modify: `src-tauri/src/database/schema.rs`（`migrate_v26_to_v27` + 注册 arm；建表语句同步加列）
- Modify: `src-tauri/src/services/session_usage_codex.rs:594-603`（`reset_codex_usage_on_conn` 的 rollup 清理谓词加 `AND host_id = ''`）
- Test: `schema.rs` 迁移单测

**Interfaces:**
- Consumes: `migrate_v10_to_v11`（schema.rs:1502-1551）RENAME/CREATE/INSERT/DROP 模式；`add_column_if_missing`、`set_user_version`
- Produces: `usage_daily_rollups` 新主键 `(date, app_type, provider_id, model, request_model, pricing_model, host_id)`，`host_id TEXT NOT NULL DEFAULT ''`；本机写入路径默认 ''（rollup 聚合 INSERT 的列清单加 host_id——定位 rollup 写入函数一并补列）

- [ ] **Step 1: 写失败测试（迁移前后数据保留）**

构造 v26 老库 fixture（rollup 若干行）→ 跑迁移 → 断言：行数与数值不变、`host_id` 全为 `''`、PRAGMA table_info 主键含 host_id、user_version=27；再跑 rollup 写入路径 → 本机行 host_id=''；`reset_codex_usage_on_conn` 只清 host_id='' 行（造一行 host_id='ssh:h1' 断言其存活）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib schema::tests::migrate_v26`
Expected: FAIL

- [ ] **Step 3: 实现迁移 + 写入路径补列**

arm `26 => { Self::migrate_v26_to_v27(conn)?; Self::set_user_version(conn, 27)?; }`；新表 CREATE 含 host_id 主键尾列；INSERT SELECT 显式列清单；rollup 聚合 INSERT 语句与剪枝 `rollup_and_prune` 的读取列同步加 host_id（保持本机行为：写入 ''）。

- [ ] **Step 4: 跑测试 + database 模块回归**

Run: `cargo test -p cc-switch --lib database`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/database src-tauri/src/services/session_usage_codex.rs
git commit -m "feat(db): add host_id dimension to usage rollups (v26->v27)"
```

---

### Task 7: usage 字节段续读纯函数提取（claude + codex）

**Files:**
- Modify: `src-tauri/src/services/session_usage.rs`（从 `sync_single_file`:641 抽出解析核心）
- Modify: `src-tauri/src/services/session_usage_codex.rs`（同理抽 codex 累计值解析核心）
- Test: 两文件内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `SyncCursor { last_modified, last_line_offset, last_byte_offset, last_tail_fingerprint, parser_state, last_synced_at }`（session_usage.rs:82-93）；`ParsedAssistantUsage`
- Produces:
  - `pub(crate) fn parse_claude_usage_segment(bytes: &[u8], cursor: &SyncCursor) -> (Vec<ParsedAssistantUsage>, SyncCursor)`——纯函数：输入增量字节段与旧游标，输出解析结果与新游标（含 parser_state 序列化）；`sync_single_file` 改为「seek 到 offset 读剩余字节 → 调本函数」，行为不变
  - codex 侧同构函数 `parse_codex_usage_segment`（输出 delta 所需的累计值与游标推进；`compute_delta` 复用不动）

- [ ] **Step 1: 写失败测试（分段喂入等价性）**

同一完整文件字节流：一次性喂 vs 按 2 段/3 段喂（游标接力）→ 解析出的 usage 行集合与最终游标完全一致；空段 → 空 vec + 游标不变。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib session_usage::segment`
Expected: FAIL

- [ ] **Step 3: 实现提取**

以现有 `sync_single_file` / codex 解析循环为主体抽出，文件读取与入库留在外层。既有全部 session_usage 测试不改动、必须继续通过（行为锁定）。

- [ ] **Step 4: 跑测试确认通过 + services 回归**

Run: `cargo test -p cc-switch --lib services::session_usage`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/session_usage.rs src-tauri/src/services/session_usage_codex.rs
git commit -m "refactor(usage): extract byte-segment usage parsers for remote reuse"
```

---

### Task 8: 远程 usage 同步引擎 + 命令层

**Files:**
- Create: `src-tauri/src/services/usage_remote.rs`
- Modify: `src-tauri/src/commands/usage.rs`（五命令加 hostId + 两个新命令）
- Modify: `src-tauri/src/services/usage_stats.rs`（聚合 SQL 加 host 过滤子句 helper）
- Modify: `src-tauri/src/lib.rs`（命令注册）
- Test: `usage_remote.rs` 单测 + `ssh_localtest.rs` 冒烟扩展

**Interfaces:**
- Consumes: Task 6 迁移后的 rollup；Task 7 两段解析函数；ssh.rs 的 `run_remote` / `list_files_command` / `read_range_command` / `read_full_command` / `parse_ssh_source` / ControlMaster 复用；`load_sync_cursors` / `update_claude_sync_state_on_conn`（file_path 键 = `ssh:` 源路径天然隔离）；`upsert_session_log_entry_on_conn`（data_source 参数位）；`session-index-status` 的 emit 先例（commands/session_manager.rs:40-46）
- Produces:
  - `pub struct RemoteUsageSyncStatus { doneFiles: usize, totalFiles: usize, doneBytes: u64, skipped: usize }`（serde camelCase，事件名 `remote-usage-sync-status`）
  - `#[tauri::command] pub async fn sync_remote_usage<R: Runtime>(app: AppHandle<R>, hostId: String, force: bool) -> Result<RemoteUsageSyncStatus, AppError>`
  - `#[tauri::command] pub async fn cancel_remote_usage_sync() -> Result<(), AppError>`
  - `#[tauri::command] pub async fn peek_remote_usage(hostId: String) -> Result<RemoteUsageSyncStatus, AppError>`——清单 diff 单往返统计待同步文件数/总字节，不拉取不写库（首扫预告与「有更新」提示共用）
  - 五命令加 `hostId: Option<String>`：service 聚合调用前经 `host_data_source_filter(hostId) -> Option<String>`（Some → `LIKE 'ssh:<id>%'`，None → `NOT LIKE 'ssh:%'`）注入 WHERE 子句；`hostId` 指向不存在主机 → Err「SSH 主机不存在或已移除」
  - 引擎函数 `sync_host_usage(db, host, force, progress, cancel: &AtomicBool) -> Result<RemoteUsageSyncStatus, AppError>`：清单 diff（`list_files` TSV 单往返 vs 上次清单）→ 无变化且非 force 直接返回；变化/无游标文件：mtime ≥ now-90d（mtime < 者跳过不建游标）→ 未压缩 `read_range(offset, size-offset)` 喂 Task 7 纯函数入库（`data_source = ssh:<hostId>`，rollup 写入 host_id=<hostId>）；zst 全量 `read_full`；size > 32MB 或拉取失败 → skipped+1 继续；每文件完成 emit 进度；取消标志置位 → 停止并返回当前进度（游标已推进部分保留）；per-host 串行（模块级 Mutex HashMap<hostId, ()>）

- [ ] **Step 1: 写失败测试（引擎单测，scan/read 抽象注入）**

清单 diff 三态（无变化零拉取 / size 增长走增量 / 新文件走首扫）；90 天窗口（旧 mtime 文件不拉不建游标）；zst 全量路径；>32MB skipped；取消标志中途置位 → 返回部分进度且游标一致；幂等（同文件同字节二次同步 → INSERT OR IGNORE 不重复计数，覆盖 Review Focus #4）；data_source 与 rollup host_id 写值断言；host 过滤子句 SQL 形态断言（LIKE / NOT LIKE）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cc-switch --lib usage_remote` 
Expected: FAIL

- [ ] **Step 3: 实现引擎 + 命令 + 过滤子句**

- [ ] **Step 4: localhost 集成冒烟扩展**

tempdir 造 claude jsonl（含 usage 行）→ 首次 sync（force）→ 断言入库行与游标；追加字节再 sync → 断言增量行数与游标推进；`get_usage_summary(hostId=Some)` 聚合数字正确、`hostId=None` 不含该数据（覆盖 Review Focus #5）。

- [ ] **Step 5: 跑全部相关测试 + lib.rs 注册核对**

Run: `cargo test -p cc-switch --lib usage_remote ssh_localtest commands`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/services/usage_remote.rs src-tauri/src/commands/usage.rs src-tauri/src/services/usage_stats.rs src-tauri/src/lib.rs src-tauri/src/session_manager/ssh_localtest.rs
git commit -m "feat(usage): remote host usage sync with incremental mirror architecture"
```

---

### Task 9: 前端 usage 主机切换与同步 UI

**Files:**
- Modify: `src/components/usage/UsageDashboard.tsx`（filterRow:585-592 首个分隔线后挂 SessionHostMenu；页头 actions 或 filterRow 内：手动同步/首扫确认/进度/取消）
- Modify: `src/lib/query/usage.ts:44-168`（usageKeys 全系加 hostId 维度，缺省 "local" 占位）
- Modify: `src/lib/api/usage.ts`（五封装 + sync/cancel 新封装传 hostId）
- Modify: `src/hooks/useUsageAutoRefresh.ts`（远程下自动刷新仅走轻路径：invalidate 后由查询内建的清单 diff 承担，不直接调 force）
- Modify: `src/i18n/locales/{zh,zh-TW,en,ja,tr}.json`
- Test: `src/components/usage/UsageDashboard.test.tsx`（或新测试文件）

**Interfaces:**
- Consumes: Task 8 命令层与 `remote-usage-sync-status` 事件；`SessionHostMenu`（src/components/sessions/SessionHostMenu.tsx:31，props 适配：value/onChange/disabled）；listen 先例（useSessionContentSearch.ts:82-103）
- Produces: 无（UI 层）

- [ ] **Step 1: 写失败测试**

vitest：(a) 主机切换后 usageKeys 维度变化（h1 ≠ local）；(b) 首扫确认交互——无游标状态（后端 status 或专用探测）显示「首次同步 N 文件 / 约 X MB」+ 开始按钮，确认后调 `sync_remote_usage(force=true)`；(c) 进度事件渲染（mock listen emit doneFiles/totalFiles）；(d) 取消按钮调 `cancel_remote_usage_sync`；(e) 远程错误态显示分类信息 + 重试；(f) hostId 缺省时全部请求不含 hostId（本机零回归）。

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run src/components/usage`
Expected: FAIL

- [ ] **Step 3: 实现全部 UI 与接线**

首扫预告用独立轻命令 `peek_remote_usage(hostId) -> RemoteUsageSyncStatus`（清单 diff 单往返统计待同步文件数与总字节，不拉取不写库）；确认后调 `sync_remote_usage(hostId, force=true)`。i18n 五语言 key：`usage.hostSelector.*`、`usage.remoteSync.*`（首次同步预告/开始/取消/进度/跳过提示/错误）。

- [ ] **Step 4: 跑测试 + usage 前端回归**

Run: `npx vitest run src/components/usage src/hooks src/lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/components/usage src/lib src/hooks src/i18n
git commit -m "feat(usage): host switcher with first-sync flow and progress UI"
```

---

## 收尾（终审后）

- 全量回归：`cargo test -p cc-switch` + `npx vitest run`（已知环境性预存在失败除外：`omp_search_paths_include_the_bun_global_bin`、`SessionManagerPage.test.tsx` 日期敏感 flaky）
- 真机 0.165 验证（guqiu@192.168.0.165:50051）：远程搜索（rg 与 grep 双路径各验一次——临时改探测缓存强制 false）、复制恢复命令、用量统计首扫+增量
- 版本 4.2.0 + Release + DMG（沿用一期流程）
