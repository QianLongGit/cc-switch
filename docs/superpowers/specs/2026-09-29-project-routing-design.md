# 设计文档：按项目路由供应商（Project Routing）

| 元信息 | 值 |
|--------|-----|
| 日期 | 2026-09-29 |
| 状态 | Revised-1（2026-09-29：对抗性审阅 9 项缺陷修复） |
| 作者 | cc-switch 团队（spec 子代理整理，四路研究子代理结论汇编） |
| 阶段 | 设计已批准，待实现计划拆解 |
| 涉及模块 | src-tauri（proxy / database / commands / services）、src（React 前端） |

---

## 1. 背景与目标

cc-switch 是一个 Tauri 2 桌面应用：React 18 + TypeScript 前端，Rust 后端（`src-tauri/`），SQLite 持久化（`~/.cc-switch/cc-switch.db`）。其本地代理接管 Claude Code 流量后，目前对所有请求采用统一出口策略：故障转移开启时依公共 failover 队列 P1 → P2 → ... 降级，关闭时仅使用当前供应商（`src-tauri/src/proxy/provider_router.rs:45` 现有语义）。

用户的实际使用场景是**多项目并行**：不同项目目录下的 Claude Code 会话希望走不同供应商（不同账号、不同计费主体、不同模型配额）。当前代理无法区分请求来自哪个项目。

### 目标

1. Claude Code 在不同项目目录发起的请求，经 cc-switch 本地代理时**路由到不同供应商**；未绑定项目的请求走现有默认策略（公共 failover 队列 P1 → P2 → ...），行为完全不变。
2. 全部配置动作由软件界面完成，配置写入目标项目的 `.claude/settings.local.json`（个人私有配置，天然不进 git，不污染团队仓库）。
3. 首期仅支持 `claude` app_type；数据表结构预留 `app_type` 列，供二期扩展到其他 app 类型。
4. 与现有 Profile 功能（`src-tauri/src/services/profile.rs`，手动快照 / 一键应用）明确划界：**互不复用、互不干扰**。项目路由是代理出口层的动态决策，Profile 是配置集合的手动快照机制，两者数据模型、触发路径、生命周期均独立。

## 2. 范围与非目标

### 范围内

- 项目 → 供应商绑定关系的存储、查询、删除（含级联清理）
- 客户端信号注入：项目 `.claude/settings.local.json` 写入 `X-CC-Project` header
- 代理端信号读取与路由决策（候选链重排）
- 项目探测（发现用户机器上存在哪些 Claude Code 项目）
- 请求历史归因（哪个项目发的、是否因绑定改变路由）
- 前端配置界面（新 View）与历史记录展示扩展

### 非目标

- 不支持 claude 之外的 app_type（仅预留表结构）
- 不改变全局接管机制：`~/.claude/settings.json` 的 `ANTHROPIC_BASE_URL` 仍指向 cc-switch proxy，现有接管机制不变，项目流量始终过 proxy，仅出口不同
- 不与 Profile 功能做任何集成或数据互通
- 不做路由规则的正则 / 通配符匹配（按项目路径精确匹配）
- 不做跨设备同步路由表（随 DB 备份机制自然带走，不单独处理）

## 3. 总体架构

### 3.1 数据流全景

```
【配置流：绑定 / 解绑】
┌──────────────┐   invoke    ┌─────────────────────┐
│ ProjectRouting │──────────▶│ commands/            │
│ Page (React)  │◀────────── │ project_routing.rs   │
└──────┬───────┘  列表/状态  └──────────┬──────────┘
       │ react-query 3s 条件轮询          │ DB + 文件双写
       │                                  ├─────────────▶ SQLite project_routes 表
       │                                  │               （供应商删除时级联清理）
       │                                  └─────────────▶ <项目>/.claude/settings.local.json
       │                                       settings_local_writer.rs
       │                                       env.ANTHROPIC_CUSTOM_HEADERS
       │                                       仅增删替换 X-CC-Project 行（保留用户其他 header）
       ▼
  project_scanner.rs 三源探测
  （~/.claude.json projects 键 ＋ ~/.claude/projects/*/jsonl
    ＋ ~/.claude/sessions/*.json，mtime 增量缓存为可选）

【请求流：代理路由】
Claude Code (项目 A)                          公共 failover 队列 P1→P2→...
   │ env.ANTHROPIC_CUSTOM_HEADERS:             ▲
   │   X-CC-Project: <percent-encode(A)>       │
   │  (ANTHROPIC_BASE_URL → cc-switch proxy)   │
   ▼                                           │
cc-switch proxy handler_context.rs             │
   │ 读 header → percent-decode → project_path │
   │                                           ▼
   └──▶ provider_router.select_providers_for_request(app_type, project_path)
             │ 每请求直查 SQLite project_routes
             │ 命中绑定 → 候选链重排（纯函数）
             ▼
        有序 Vec<Provider> ──▶ forwarder 循环（仅全局切换判定排除绑定请求，§5 细则 7）
                                   │
                                   ▼
                              绑定供应商（或按序降级公共队列）

【归因流：历史记录】
project_path ──随请求上下文传递──▶ logger.rs 落库
  proxy_request_logs.project_dir / project_routed
  ──▶ usage_stats.rs DTO ──▶ 前端使用统计（项目列 + 筛选）
```

### 3.2 信号机制（已实证）

- **客户端**：项目 `.claude/settings.local.json` 的 `env.ANTHROPIC_CUSTOM_HEADERS` 注入 `X-CC-Project: <percent-encode(项目路径)>`。这是官方支持的 header 通道：多行 header 用换行分隔，项目级 env 覆盖全局。**已本地抓包实证**。
- **全局**：`~/.claude/settings.json` 的 `ANTHROPIC_BASE_URL` 仍指向 cc-switch proxy（现有接管机制不变）。项目流量始终过 proxy，仅出口不同。
- **代理端**：handler 层读 header → percent-decode 得项目路径 → 查路由表。未携带 header（无绑定）的请求按现状处理。

**编码与匹配规范**（写入端 settings_local_writer 与读取端 handler 共同遵守，定死规则以杜绝两端实现漂移）：

- **percent-encode**：按 UTF-8 **字节级**编码；保留字符集 `[A-Za-z0-9/._~-]`，其余字节编码为 `%XX`（**大写**十六进制）。decode 遇非法序列（坏的 `%` 转义、非 UTF-8 字节）→ 该 header 值视为无效 → 按无项目标识处理（回默认策略，不报错）。
- **X-CC-Project 行规则**（`ANTHROPIC_CUSTOM_HEADERS` 值的解析与写入）：
  - 解析：值按换行拆分为行集合；逐行 trim；按**首个** `:` 分割名/值；名字段 ASCII **大小写不敏感**全等 `x-cc-project` 的行为目标行。
  - 写入：**替换全部目标行为单行**（杜绝重复行）；行间以 `\n` 分隔、**无尾随换行**（单行无尾 `\n`，多行末行无 `\n`）；目标行删除或目标行间/后的空行过滤后暴露在末尾时空行一并剥除（空行在该形态下不可表达），中间空行逐字节保留；其余非目标行逐字节保留。解析端对旧格式"每行尾随 `\n`"的既有文件仍兼容（拆行丢弃末尾空元素）。
- 上述规则纳入 §9 属性测试断言（编码往返、非法序列回退、多目标行合并为一、非目标行逐字节不变）。

### 3.3 项目探测（三源合并）

slug 反解方案不可行（slug 是有损编码：`/`、`-`、`_`、中文全部折叠为 `-`，无法还原真实路径），故采用三源合并：

| 源 | 内容 | 作用 |
|----|------|------|
| 主源 | `~/.claude.json` 的 `projects` 键 | 权威真实路径清单（实测 38 项，含中文路径，带 `lastSessionModified` 时间戳） |
| 补充 | `~/.claude/projects/*/` 各 jsonl 的 `cwd` 字段 + 文件 mtime | 实时活跃度；跳过无 jsonl 的陈旧残留目录 |
| 活跃标记 | `~/.claude/sessions/*.json` 的 `cwd` + `status` | 当前活跃会话标记 |

- **过滤**：目录已不存在、或活跃度过旧（N 天，默认 30）的项目不入列。
- **性能**：实测 3s 轮询全量扫描 64ms，全量即可满足。mtime 增量缓存（静态缓存各源文件 mtime，未变不重读）为**可选优化**，非必需。
- **前端**：react-query `refetchInterval: 3000` 条件轮询（条件轮询先例见 `src/lib/query/proxy.ts:28`，该先例为 `running ? 2000 : false` 模式），页面打开才轮询，离开页面即停。

## 4. 数据模型

### 4.1 新表 `project_routes`

在 `src-tauri/src/database/schema.rs` 的 `create_tables_on_conn`（`src-tauri/src/database/schema.rs:24`）中追加 `CREATE TABLE IF NOT EXISTS`——老库自动补建，免迁移：

```sql
CREATE TABLE IF NOT EXISTS project_routes (
  id TEXT PRIMARY KEY,
  project_path TEXT NOT NULL,
  app_type TEXT NOT NULL DEFAULT 'claude',
  provider_id TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_path, app_type)
);
```

- `app_type` 预留二期扩展，首期恒为 `'claude'`；`UNIQUE(project_path, app_type)` 保证一个项目一种 app 类型至多一条绑定。
- **级联清理**：供应商删除时级联清理其关联路由行——**实现在 DAO 层 `delete_provider` 内部**（`src-tauri/src/database/dao/providers.rs:379` 函数体内追加 `DELETE FROM project_routes WHERE provider_id = ?`，与供应商行删除同一事务提交）。理由：上层删除入口分散（`commands/provider.rs:78`、`services/provider/mod.rs` 5590 / 5611-5616 / 5631 多分支、7466-7563 六处静默清理，共 9 类调用入口），无单一"删除链路"可跟随；挂到 DAO 一处即覆盖全部入口，杜绝漏挂。

### 4.2 历史表加列（SCHEMA_VERSION 20 → 21）

当前 `SCHEMA_VERSION = 20`（定义于 `src-tauri/src/database/mod.rs:56`）。升级到 21，新增 `migrate_v20_to_v21`，完全仿 v19→v20 的 `vision_routed` 模式（`add_column_if_missing` 先例：`src-tauri/src/database/schema.rs:1637` 的 `migrate_v19_to_v20`，dispatch arm 见 `src-tauri/src/database/schema.rs:571-575`）：

- `proxy_request_logs.project_dir TEXT`——来源项目路径，NULL 表示未识别（无 header / 无绑定）
- `proxy_request_logs.project_routed INTEGER NOT NULL DEFAULT 0`——是否因绑定改变路由

新库路径（`create_tables_on_conn` 建表语句）与老库路径（迁移加列）最终收敛到同一结构，与 `vision_routed`（建表列见 `src-tauri/src/database/schema.rs:213`）同构。

### 4.3 备份表 `settings_local_backup`（settings.local.json 写前快照）

`settings_local_writer.rs`（§6.1 第 3 项）首次修改某项目的已存在文件前，将修改前原文入库。同在 `create_tables_on_conn` 追加建表：

```sql
CREATE TABLE IF NOT EXISTS settings_local_backup (
  project_path TEXT PRIMARY KEY,
  content      TEXT NOT NULL,   -- 修改前的 settings.local.json 原文
  updated_at   INTEGER NOT NULL
);
```

- 保留策略：每项目单份 `INSERT OR REPLACE`，仅保留最近一次修改前快照；主键即 `project_path`，天然按项目隔离。
- **明确禁用复用 `proxy_live_backup` 表**：其写入函数 `save_live_backup`（`src-tauri/src/database/dao/proxy.rs:779`）签名为 `(app_type, config_json)`，按 app_type 主键单行 `INSERT OR REPLACE`——无法区分多项目，且与接管配置的备份同 key 互踩。本设计仅借鉴其"写前备份入库"模式，表与写入路径均为新建。

## 5. 代理路由语义（核心矩阵）

`provider_router.rs` 新增 `select_providers_for_request(app_type, project_path)`；`handler_context.rs` 现有 `select_providers` 调用点（`src-tauri/src/proxy/handler_context.rs:139-141`）替换为它，**时序不变**（仍在 RequestContext 创建时调用一次，结果传给 forwarder）。

| 场景 | 候选链 | 失败行为 |
|------|--------|----------|
| failover 开 + 绑定 | `[绑定供应商, ...公共队列其余]`（绑定者熔断 open 则让位） | 绑定者失败 → 依序降级公共队列（软绑定） |
| failover 关 + 绑定 | `[绑定供应商]`（绑定者**跳过熔断检查**——跟随 `provider_router.rs:113` 现状：failover 关闭分支本就不查熔断） | 失败即失败（尊重全局开关） |
| failover 开 + 无绑定 | `[公共队列]` | 现状不变 |
| failover 关 + 无绑定 | `[当前供应商]` | 现状不变 |

语义细则：

1. **绑定供应商不要求在 failover 队列中**——显式指定优先于队列资格。救场语义：failover 开 + 绑定者健康 + 公共队列全熔断或为空 → 候选链 = [绑定者]（该分支在 `select_providers` 的全部熔断错误上抛之前介入，绑定请求不收到 AllProvidersCircuitOpen / NoProvidersConfigured）；绑定者也不可用时仍按现状返回该错误。
2. **路由查询每请求直查 SQLite**（跟随 `RequestContext::new` 现有读 DB 的既有模式，不做内存缓存）。
3. **forwarder 的供应商选择循环零改动**——只消费有序 `Vec<Provider>`，重排发生在路由层；唯一例外是细则 7 的全局切换判定：4 处 `should_switch` 判定需感知"经项目绑定路由"标记并排除绑定请求。
4. **软绑定（B）**：绑定者失败后依序降级公共队列，与全局 failover 语义对齐；failover 关闭时不降级。
5. **警戒线**：不得绕过 `select_providers` 在 handler 另拉列表。现有注释（`src-tauri/src/proxy/handler_context.rs:137-138`）明确约束"只在这里调用一次，结果传递给 forwarder，避免重复消耗 HalfOpen 名额"——新方法必须维持这一约束，这是熔断器 HalfOpen 名额单次消耗的硬性要求。
6. 补充核实：`src-tauri/src/proxy/handlers.rs:157` 存在另一处 `select_providers("claude-desktop")` 调用，属 claude-desktop app_type，**首期不在范围、不改**（首期仅 claude app_type）。
7. **项目绑定路由的请求不触发全局"当前供应商"切换**。forwarder 现有 4 处 `should_switch = current_provider_id_at_start != provider.id` 判定（`src-tauri/src/proxy/forwarder.rs` 561-574 / 664-677 / 810-825 / 974-986），成立则 tokio spawn `fm.try_switch` → `hot_switch_provider` **持久化改写全局 current**。绑定路由是 per-request 决策，不得改写全局状态——否则 failover 关闭时未绑定项目出口漂移、多项目绑定时全局 current 反复抢写。实现：RequestContext 携带"经项目绑定路由"标记（或绑定的 provider_id），forwarder 全部 4 处 `should_switch` 判定排除带该标记的请求（标记传递见 §6.2 第 6 项，forwarder 改动见 §6.2 第 8 项）。
8. **共享入口与分支回退**：`RequestContext::new` 是 claude / claude-desktop / codex / gemini 全部 messages 流量的共享入口，替换 `select_providers` 调用点即对全部 app_type 生效。非 claude app_type 或无 `X-CC-Project` header 时，**逐分支回退现状行为**（含 `provider_router.rs:72-81` 的 Codex Official 强制单路由分支原样保留）。
9. **`provider_supports_failover` 判定不适用于项目绑定者**（与细则 1"绑定优先于队列资格"同源）。首期 claude 场景该判定恒为 true，无实际影响；二期 codex 场景下"绑定优先于该判定"，标注为二期实现时需复核的决策点。

### 2026-10-05 决策修订（合并上游 v4.0.0）

合并上游 v4.0.0 后，用户拍板将项目绑定语义由"软绑定"改为**完全压制**：绑定命中即候选链唯一，绑定者故障不回退公共队列。实现已落地，本节为对齐实现的补记——**本节修订取代上文软绑定矩阵中"绑定者故障回退全局"象限**（救场 / 让位语义取消，原矩阵首行"绑定者失败 → 依序降级公共队列（软绑定）"及同源的细则 1、细则 4 不再描述现行实现）。

实现事实：

1. **`src-tauri/src/proxy/provider_router.rs`**：删除旧 `ProjectRoutePlan` / `apply_project_binding` / `select_providers_for_request`，新增 `resolve_project_binding(app_type, project_path) -> Result<Option<Provider>>`。返回 `None` 的情形：非 claude app_type / 无 project_path / 无绑定行 / `find_route` DB 错误（降级返回 None 并告警）/ 绑定悬空（绑定行指向的供应商已不存在）；`get_provider_by_id` 的 DB 错误**上抛**，不降级。
2. **`src-tauri/src/proxy/handler_context.rs`（`RequestContext::new`）**：绑定解析先于 `match stack`。命中 → `stack` 置 `None`（同时压制模型级 StackTarget 与模式级 stack 聚合）、候选链 = `[绑定供应商]`、`project_routed = true`；不改 `auto_failover_enabled`，不切换全局 current 供应商（细则 7 语义保留）。未命中 → 上游 v4.0.0 原逻辑：`stack_mode` → `[current]`，否则 `select_providers_with_current`。
3. **语义变更**：绑定命中后，绑定供应商故障**不再回退全局 failover 队列**——绑定即唯一出口，失败即失败；绑定悬空（供应商被删）视为未命中，回全局默认策略。
4. **边界**：请求同时携带 Stack 模型 id 与项目绑定时**绑定胜出**（stack 被压制），转发模型名为 `resolve_stack_target` 改写后的上游名。

## 6. 后端组件设计

### 6.1 新建（4 个文件）

**1. `src-tauri/src/database/dao/project_routes.rs`** —— 路由表 DAO

- CRUD：`insert_or_update_route` / `delete_route` / `find_route(project_path, app_type) -> Option<provider_id>`
- 注册进 `dao/mod.rs`
- 级联清理：提供 `delete_routes_by_provider(provider_id)`，由 DAO 层 `delete_provider`（`src-tauri/src/database/dao/providers.rs:379`）在同一事务内调用（挂载点决策见 §4.1，不依赖上层分散的删除入口逐个接入）

**2. `src-tauri/src/commands/project_routing.rs`** —— Tauri 命令层

- 命令：`list_projects`（调用 scanner + 合并 DB 绑定与同步状态）/ `set_project_route`（DB + 文件双写）/ `clear_project_route`（DB 删行 + 移除 header 行）
- `commands/mod.rs` 加 `pub mod` + `pub use`；`lib.rs` 的 `invoke_handler`（`src-tauri/src/lib.rs:1388`）注册三个命令

**3. `src-tauri/src/services/settings_local_writer.rs`** —— settings.local.json 编辑器

- 读取：容错文件不存在与损坏 JSON（解析失败按空对象处理，但见 §8 边界 2 的同步状态提示）
- 写入：`env.ANTHROPIC_CUSTOM_HEADERS` 多行值中**仅增删替换 `X-CC-Project` 行**，保留用户自有的其他 header 行
- 原子写：temp + rename（仿 `src-tauri/src/config.rs:383` 的 `atomic_write`）
- 备份：首次修改已存在文件前存 DB 备份（写入 §4.3 的 `settings_local_backup` 表，每项目单份；仅借鉴 `save_live_backup` 的"写前备份入库"模式，不复用 `proxy_live_backup` 表，理由见 §4.3）

**4. `src-tauri/src/services/project_scanner.rs`** —— 三源探测（mtime 增量缓存为可选优化）

- 按 §3.3 三源合并产出项目清单（路径、basename、活跃时间、活跃状态）
- mtime 增量缓存（静态缓存各源文件 mtime，未变不重读）为**可选优化**：全量扫描实测 64ms / 3s 间隔已满足，确有需要再启用

### 6.2 改动（5 个文件）

**5. `src-tauri/src/proxy/provider_router.rs`**

- 新增 `select_providers_for_request(app_type, project_path)`：候选链重排**纯函数化**（输入公共队列 + 绑定关系 + 熔断状态，输出有序列表），便于单测覆盖语义矩阵

**6. `src-tauri/src/proxy/handler_context.rs`**

- 提取 `X-CC-Project` header → percent-decode 得 `project_path`（编码规则见 §3.2 编码与匹配规范）
- 传参给路由方法（替换 `src-tauri/src/proxy/handler_context.rs:141` 的调用）
- 归因字段（`project_dir` / `project_routed`）随请求上下文传递到落库点
- 上下文传递字段另含**"经项目绑定路由"标记**（或绑定的 provider_id）：供 forwarder 全部 4 处 `should_switch` 判定（`src-tauri/src/proxy/forwarder.rs` 561-574 / 664-677 / 810-825 / 974-986）排除绑定请求，阻断其对全局"当前供应商"的改写（§5 细则 7）

**7. `src-tauri/src/proxy/usage/logger.rs`**（注意实际路径在 `proxy/usage/` 下，非 `services/`）与 `src-tauri/src/services/usage_stats.rs`

- `logger.rs`：`RequestLog` 结构（`src-tauri/src/proxy/usage/logger.rs:64`）加 `project_dir` / `project_routed` 字段；INSERT 列清单（`src-tauri/src/proxy/usage/logger.rs:173-178`）同步扩展
- `usage_stats.rs`：`RequestLogDetail` DTO 加字段；`LogFilters`（`src-tauri/src/services/usage_stats.rs:105`）加 `projectDir`；`get_request_logs`（`src-tauri/src/services/usage_stats.rs:1560`）SQL 增加 project_dir 筛选

**8. `src-tauri/src/proxy/forwarder.rs`**

- 全部 4 处 `should_switch = current_provider_id_at_start != provider.id` 判定（561-574 / 664-677 / 810-825 / 974-986）追加排除条件：请求带"经项目绑定路由"标记（§5 细则 7，标记由 handler_context 传入）则不触发 `hot_switch_provider`——绑定请求不改写全局"当前供应商"。供应商选择与重试循环逻辑不变。

## 7. 前端设计

### 7.1 新建

**1. 新 View `projectRouting`**

- `src/App.tsx` 四处小改：View 类型（`src/App.tsx:116`）/ `VALID_VIEWS`（`src/App.tsx:151`）/ `renderContent` case（`src/App.tsx:1030`）/ 头部按钮
- 组件：`src/components/projectRouting/ProjectRoutingPage.tsx`

**2. 项目绑定 UI**

- 项目列表：名称 = 路径 basename、完整路径 tooltip、活跃时间、"使用中"徽标（当前活跃会话所在项目）
- 每行 Radix Select：claude 供应商列表 + "默认"选项（即不绑定，走公共队列）。Radix Select 先例见 `src/components/usage/RequestLogTable.tsx:127-138`

**3. 同步状态徽标**

- 每轮对比 DB 绑定与 `settings.local.json` 实际 header，产出三态 `sync_status`（Rust 侧对比，随 `list_projects` 返回），真值表定死如下：

| DB 绑定 | 文件状态 | sync_status | 界面动作 |
|---------|----------|-------------|----------|
| 有 | 文件有目标行且值与绑定一致 | `synced` | 无 |
| 有 | 文件缺失 / 无目标行 / 值不一致 | `out_of_sync` | 警示 + 一键修复（重写文件使其与 DB 一致，保留用户其他内容） |
| 无 | 文件有目标行 | `orphan_header` | 提示"检测到未生效的 X-CC-Project（不参与路由）" + 可选清除（不自动删） |

- 补注：`orphan_header` 场景下 proxy 查表无绑定行 → 回默认策略，路由行为无影响。

**4. 数据层**

- `src/lib/api/projectRouting.ts`：invoke 封装（仿 `src/lib/api/proxy.ts` 的逐命令 invoke 函数模式）
- `src/lib/query/projectRouting.ts`：react-query hooks，`refetchInterval: 3000` 条件轮询（仿 `src/lib/query/proxy.ts:28` 先例模式）

### 7.2 改动

**5. 历史记录展示**

- `src/types/usage.ts` 的 `RequestLog`（`src/types/usage.ts:10`）加 `projectDir?: string`（camelCase 先例：`visionRouted`，`src/types/usage.ts:18`）
- `RequestLogTable.tsx` 加项目列 + 项目筛选
- i18n 四语言补文案

**6. 使用统计筛选**

- `LogFilters` 透传 `projectDir`（与 §6.2 第 7 项后端联动）

## 8. 边界与错误处理

实现阶段必须逐条覆盖以下边界（对应测试见 §9）：

1. **绑定供应商被用户删除** → 路由行级联清理（DAO 层 `delete_provider` 事务内删除 `project_routes.provider_id` 关联行，见 §4.1）→ 后续请求回默认策略，无残留悬空引用。测试需同时覆盖两条路径：**绑定行缺失**（表本无行 → 查表回默认策略）与**绑定行存在但供应商已删**（DAO 级联生效 → 删供应商后行已消失，查表同样回默认策略）。
2. **settings.local.json 被手动改 / 删** → `sync_status` 显示不同步 + 一键修复（按 DB 绑定重写 header 行；文件被删则重建仅含 `X-CC-Project` 的最小结构）。
3. **用户已有 `ANTHROPIC_CUSTOM_HEADERS` 其他行**（如自配 `X-Api-Key`）→ 合并保留，只动 `X-CC-Project` 行，其余行字节级不变。
4. **用户自配同名 `X-CC-Project`**（概率极低）→ cc-switch 接管该 header（在用户文档中声明此行为）。
5. **项目路径含中文 / 空格** → percent-encode 编码进 header，代理端 decode 还原；header 通道对非 ASCII 安全。
6. **`~/.claude.json` 缺失 / 损坏** → 探测降级到 slug+jsonl 源（jsonl 的 `cwd` 字段自身可用，不依赖 claude.json）。
7. **claude.json 中路径的目录已不存在** → 不入列（scanner 用目录存在性过滤）。
8. **多客户端同项目并发** → 无影响（header 每请求携带，无会话状态依赖）。
9. **供应商下拉显示**：下拉显示供应商名称；绑定的供应商已不在列表（如已删）→ 该行显示"已失效"状态（不静默清空绑定展示，用户可主动改选或清空）。

## 9. 测试策略

| 类别 | 内容 |
|------|------|
| 路由重排单测 | 纯函数单测：语义矩阵四象限 × 熔断状态（Closed / Open / HalfOpen）× 绑定供应商不在队列 × 绑定行缺失（回默认） |
| settings_local_writer | 属性测试：任意既有文件内容，增删 `X-CC-Project` 行后其余字节不变（保留用户自有 header）；断言 §3.2 编码与匹配规范——percent-encode 编码往返（含中文 / 空格路径）、decode 非法序列回退默认策略、多目标行合并为一、非目标行逐字节不变 |
| 全局切换回归 | 「绑定请求成功后全局 current 不变」+「未绑定请求 failover 后全局切换行为与现状一致」（§5 细则 7 的回归保障，覆盖 forwarder 全部 4 处 `should_switch` 判定） |
| DAO 级联清理 | 删供应商后其 `project_routes` 绑定行同事务消失（§4.1；覆盖 §8 边界 1 的两条路径） |
| project_scanner | fixture 三源数据（claude.json / jsonl / sessions）的合并、过滤（目录不存在、超 30 天）、降级（claude.json 损坏） |
| 端到端 | `claude -p` 实测 header 注入与路由归因：监听本地端口验证上游收到的流量与 `X-CC-Project` header（研究阶段已有验证方法） |

## 10. 风险与开放问题

### 风险清单

1. **语义哈希去重**：`logger.rs` 的 request_id 幂等依赖 `UsageSemantic`（`src-tauri/src/proxy/usage/logger.rs:15-25`）。**设计决策：`project_dir` / `project_routed` 不参与语义哈希，跟随 `vision_routed` 既有处理（归因字段不参与）**——核实证据：现有 `UsageSemantic` 字段为 app_type / provider_id / model / token 计数 / status_code，`vision_routed` 不在其中；project 字段同理不参与。实现计划阶段按此核对确认后落地。
2. **header 值长度**：项目路径极长时的 header 尺寸——HTTP 实践无碍（常见实现上限远大于路径长度），文档标注即可，不做截断。
3. **前后端类型同步**：前端 `RequestLog` 现无 `sessionId` 字段（已核实 `src/types/usage.ts:10-39`），加 `projectDir` 时后端 DTO（`RequestLogDetail`）与前端类型需同步更新，避免 camelCase 序列化遗漏。

### 开放问题

1. 前端轮询间隔：设计取 3000ms，先例 `src/lib/query/proxy.ts:28` 实际为 2000ms（条件模式相同，数值不同）。保守取 3000（探测全量实测 64ms，余量充足）；若实现中发现活跃徽标延迟明显可下调，属参数而非结构决策。
2. `list_projects` 单命令 vs 拆分（projects / routes / sync_status）多命令：本设计按单命令返回合并结构（一次轮询一次 IPC）；若返回体积成为问题再拆，属接口形态而非语义决策。
3. 30 天活跃过滤阈值是否需要暴露为用户可配置设置：首期硬编码默认值，观察反馈。

---

## 附录 A：研究结论引用（关键行号清单）

以下行号均经本次撰写时抽样核实（Read 原文件比对），标注核实结果：

| 引用 | 用途 | 核实结果 |
|------|------|----------|
| `src/lib/query/proxy.ts:28` | 条件轮询先例 | 确认：`refetchInterval: (query) => (query.state.data?.running ? 2000 : false)`；先例值为 2000，本设计取 3000（见开放问题 1） |
| `src/components/usage/RequestLogTable.tsx:127-138` | Radix Select 先例 | 确认：SelectTrigger 127-129 / SelectContent 130-137 / Select 闭合 138 |
| `src-tauri/src/config.rs:383` | 原子写 temp+rename 先例 | 确认：`pub fn atomic_write`（382 行为中文注释"原子写入：写入临时文件后 rename 替换，避免半写状态"） |
| `src-tauri/src/proxy/handler_context.rs:139-141` | select_providers 调用点 | 确认：141 行 `.select_providers(app_type_str)`；137-138 行注释即 HalfOpen 单次消耗约束（§5 警戒线依据） |
| `src-tauri/src/proxy/provider_router.rs:45` | select_providers 定义 | 确认：`pub async fn select_providers(&self, app_type: &str)`，40-44 行文档注释描述现有 failover 语义 |
| `src-tauri/src/database/schema.rs:24` | create_tables_on_conn | 确认：`pub(crate) fn create_tables_on_conn` |
| `src-tauri/src/database/schema.rs:1637` / `571-575` | v19→v20 迁移先例 | 确认：`migrate_v19_to_v20` 定义 1637，dispatch arm 571-575；`add_column_if_missing` 调用先例另见 561-566（v18→v19） |
| `src-tauri/src/database/schema.rs:213` | vision_routed 建表列 | 确认：`vision_routed INTEGER NOT NULL DEFAULT 0`（proxy_request_logs 建表语句内） |
| `src-tauri/src/database/mod.rs:56` | SCHEMA_VERSION 当前值 | 确认：`pub(crate) const SCHEMA_VERSION: i32 = 20;`（20→21 设计成立） |
| `src-tauri/src/proxy/usage/logger.rs:64` / `65` / `89` | RequestLog 结构 / request_id / vision_routed | 确认；**路径修正**：原研究引用 `services/logger.rs` 有误，实际为 `proxy/usage/logger.rs` |
| `src-tauri/src/proxy/usage/logger.rs:15-25` / `42-59` / `133-154` | 语义哈希构成 / 幂等与碰撞回退 | 确认：`UsageSemantic` 不含 `vision_routed`（风险 1 证据）；碰撞回退 `{}:collision:{}` 格式在 149 行 |
| `src-tauri/src/proxy/usage/logger.rs:173-178` | INSERT 列清单 | 确认：列含 `vision_routed`（178 行），加 project 两列处扩展 |
| `src-tauri/src/services/usage_stats.rs:105` / `1560` | LogFilters / get_request_logs | 确认：LogFilters 字段 106-111（camelCase serde）；`get_request_logs` 方法 1560 行 |
| `src/App.tsx:116` / `151` / `1030` | View 类型 / VALID_VIEWS / renderContent | 确认 |
| `src/types/usage.ts:10` | 前端 RequestLog 接口 | 确认：无 sessionId、无 projectDir 字段；`visionRouted` camelCase 先例在 18 行 |
| `src-tauri/src/database/dao/proxy.rs:779` | save_live_backup 备份模式 | 确认：`pub async fn save_live_backup` 定义处 |
| `src-tauri/src/lib.rs:1388` | invoke_handler 注册点 | 确认：`tauri::generate_handler![` |
| `src-tauri/src/proxy/handlers.rs:157` | 另一 select_providers 调用点（claude-desktop） | 核实发现：首期范围外、不改（§5 细则 6） |

> Revised-1 对抗性审阅阶段追加核实的引用（均经 Read 原文件比对）：`forwarder.rs` 561-574 / 664-677 / 810-825 / 974-986（4 处 `should_switch` 判定，§5 细则 7 / §6.2 第 8 项依据）、`provider_router.rs:72-81`（Codex Official 强制单路由分支）与 `:113`（failover 关闭分支不查熔断，§5 矩阵补注依据）、`dao/providers.rs:379`（`delete_provider`，§4.1 级联清理挂载点）、`commands/provider.rs:78` 与 `services/provider/mod.rs` 5590 / 5611-5616 / 5631 / 7466-7563（分散的上层删除入口，B2 挂载点决策依据）。
