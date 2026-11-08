# SSH 远程会话二期迭代设计（SSH Remote Sessions Iteration 2）

- 日期：2026-11-08
- 状态：**已批准**（builder check 8 问全部裁决并入；self-review 一轮 FAIL → 修复 → 复核 PASS；用户中途指令已吸收：rg/grep 双方案、测试服务器 0.165）
- 规模：project（跨 Rust / 前端多模块；含一个小的纯样式任务）
- 前置：`2026-11-07-ssh-remote-sessions-design.md`（一期，v4.1.0 已发布）——本文件沿用其全部既有机制（ssh.rs 原语、`ssh:` 源路径、SshScanRegistry、批量帧协议），不重复表述

## 1. 意图与动机

一期交付了远程会话的「列表 / 阅读 / 导出 / 删除」。实际使用暴露四个缺口：

1. **设置表单样式不协调**：SSH 主机添加/编辑对话框边距失衡、输入框高宽比例差（用户截图反馈）
2. **远程无正文搜索**：一期非目标，远程下正文搜索降级为空，只有元数据命中
3. **远程无法恢复会话**：连复制恢复命令都不行（恢复入口被整体隐藏）
4. **用量统计看不到远程主机**：统计页只聚合本机数据

核心体验目标：远程主机的搜索、恢复（复制级）、用量统计与本机**口径一致**；样式与项目成熟表单对齐。

## 2. 目标 / 非目标 / 反目标

**目标**
- SSH 主机添加/编辑/导入对话框样式对齐项目成熟表单模式（边距、列比例、按钮区统一）
- 远程主机下正文搜索可用：`grep -inF` 远端批量扫描，结果与本地索引搜索同构（摘录 + 高亮 + 跳转），前端搜索合并链路零改动
- 远程会话可复制恢复命令：`ssh [-p N] [-i key] -t [user@]host 'cd <dir> && claude --resume <id>'`
- 用量统计页支持主机切换：远程用量走「轻量清单 diff 自动 + 增量字节按需拉取 + SQLite 聚合」；首扫限定近 90 天（用户已裁定）

**非目标（二期或明确不做）**
- 远程会话「一键恢复」（本机终端直接执行 ssh -t）——用户裁定只开复制命令
- 远程 `.jsonl.zst` 的正文搜索（zst 是归档会话，grep 需远端 zgrep 无保证；元数据搜索天然覆盖）
- 远程搜索结果的精确 messageIndex 跳转——降级为会话级跳转 + 阅读器内 initialFind 定位第一个命中（后续迭代可加行号映射）
- 90 天窗口之外的历史用量同步（窗口固定 90 天，按远端文件 mtime）
- 远程用量统计的 claude/codex 以外 provider（对齐一期 MVP）
- 远程定时自动深同步（远程自动刷新仅到「清单 diff + 拉增量」；深拉只在页面打开/手动触发）
- 前端「复制进入目录并恢复」按钮对远程的特殊适配以外的阅读器改动

**反目标（做了即失败）**
- 在远端复刻 usage 解析口径（message.id 去重、stop_reason、timing 链）——双份口径必然漂移
- 远程搜索/统计导致 UI 卡顿或无反馈的长静默 ssh 往返
- 存储任何 SSH 密码/口令（沿用一期约束）
- 远端命令注入（搜索词/路径必须经既有单引号转义）

## 3. 约束

- 沿用一期全部技术约束：系统 ssh 子进程、BatchMode、ControlMaster 复用、POSIX 远端原语、不新增 Rust/npm 依赖、13 个本地 scanner 解析逻辑零修改（既有两处例外继续）
- 本机行为零回归：所有新命令参数 `hostId: Option<String>` 缺省 = 本机 = 现状完全一致；`list_sessions` / `search_session_content` / usage 五命令均如此
- 远端 grep 语义对齐本机索引搜索：**大小写不敏感、固定子串、非正则**（本机为 FTS5 trigram 短语 / instr 子串）
- 用量数据落库必须复用现有解析函数（claude/codex 逐行 usage 提取），本地一份口径

## 4. 方案选择（Approaches）

### 4.1 需求 1：表单样式（无分叉，对齐既有模式）

根因（代码证据）：
- `DialogContent` 本体无 padding（`dialog.tsx:64`），Header 自带 `px-6`——SSH 对话框的 Tabs/列表/表单直接裸放，左右贴边（`SshHostsSection.tsx:395-508`、`542-552`），这是截图「边距丑」的主因
- user/port 双列 `grid-cols-2` 50/50（L243）→ 端口框过宽；项目惯例短字段窄列 `grid-cols-[1fr_140px]`（`CodexFormFields.tsx:1735` 端口列先例）
- 手动 tab 按钮区手写 `<div className="flex justify-end gap-2 pt-1">`（L306），与导入 tab 的 DialogFooter（L479）不统一，且无底部边距
- 导入列表行 `py-2`（L441）密度偏高（截图 1 的松散感）
- 对话框 `max-w-lg` 宽于设置页惯例 `max-w-md`（`BackupListSection.tsx:421` 等）

**选定**：全部照项目成熟模式改——内容区包 `px-6`（参照 `NewLayoutDialog.tsx:122`）、端口窄列、统一 DialogFooter、列表行收紧、对话框收窄至 `max-w-md`。输入框控件本身规格统一（h-control），不动控件。

### 4.2 需求 2：远程正文搜索（用户已指定远端 grep 路线）

| 方案 | 取舍 |
|---|---|
| **远端 rg/grep（选定）** | 增量、零索引维护、与 ssh 原语链路同构；rg 可用时大幅提速；每次搜索 1-N 次 ssh 往返（ControlMaster 复用下可控） |
| 全量拉回本地索引 | 首扫 GB 级流量、新鲜度维护复杂（一期已论证否决） |
| 远端 grep + 本地行号映射精确跳转 | 需复刻 provider 行过滤谓词，脆；降级方案体验损失极小，列二期 |

**命令形态（用户指定双方案：rg 优先，grep 降级）**：

- 探测：per-host 首次搜索前 `command -v rg` 单次往返判定，结果进程内缓存（`RwLock<HashMap<hostId, bool>>`）；rg 缺失或探测失败 → 降级 grep，不再重复探测
- rg 形态：`rg -i -n -F --no-heading --with-filename --no-ignore -m 5 -- <pattern> <f1> <f2> …`
- grep 形态：`grep -inFH -m 5 -- <pattern> <f1> <f2> …`
- 两者输出统一为 `path:lineno:line` 行格式，**解析器单一**；`-i` 大小写不敏感、`-n` 行号、`-F` 固定串防正则元字符、`-m 5` 每文件截断防超长输出；rg 额外 `--with-filename` 强制文件名前缀（单文件时两者默认都省略）、`--no-heading` 保持行式输出、`--no-ignore` 防远端 .gitignore 干扰（显式传参本不受 ignore 影响，防御性保留）
- 每批 ≤50 文件（对齐 `REMOTE_HEAD_TAIL_BATCH`）；pattern 与文件路径均经 `escape_remote_path` 单引号转义（该函数与"路径"无关，可转义任意串，测试已覆盖内嵌单引号）
- **退出码语义（builder check Q4 裁决，必现缺陷修复）**：grep/rg 无匹配时 exit 1，而 `run_command` 现把非 0/255 一律当 `Err(SshError::Remote)`——搜索无命中会必现报错。搜索路径需专用退出码处理：**exit 0 与 1 均为成功（1 = 无命中，返回空结果）；exit ≥2 = 远端错误透传（权限/文件消失等）；255 = 连接/认证错误**。禁止用 `; :` 归零（会把 exit 2 真错误静默成空结果）。实现形态：ssh.rs 增加接受退出码集合的 run 变体（如 `run_remote_with_codes`），仅搜索路径使用，既有原语不动
- 真机参考：测试服务器 0.165 = GNU grep 3.3 + ripgrep 0.10.0，双路径均可实测；rg 0.10 为老版本，上述参数均在其支持范围内

**数据流**：
1. `search_session_content(query, providerIds, limit, hostId?)` 加可选参数；`hostId` 有值 → 走新函数 `search_remote_content`
2. 从 `SshScanRegistry` 取该主机清单中全部**未压缩** `.jsonl`（zst 跳过；无清单时返回空结果 + 提示状态，对齐「先刷新列表」语义）
3. 批量 grep（批 50）→ 解析 `path:lineno:line` 输出 → 按文件聚合
4. **仅命中文件**按需 `fetch_to_temp_file` 全文拉回（≤32MB 既有上限）→ 复用 `load_messages` 解析 → 在解析出的消息文本上定位命中行所属消息，产出 snippet（每会话 ≤3 段、matchCount 总数，对齐本地 ContentHit 形态）；**命中文件超 32MB 或拉取失败：跳过该文件并标记**（该会话不出 snippet，仅计 matchCount 占位由 rg/grep 行数得出；单文件失败不毁整搜，结果状态带 skipped 计数供 UI 提示）
5. 高亮区间在本地对 snippet 文本做大小写不敏感子串扫描（与本地 search.rs 同口径），避免远端 locale 折叠差异
6. 返回 `Vec<ContentHit>` 同构结构（`sourcePath` 为 `ssh:` 源路径）——**前端类型零改动**

**跳转降级**：远程 snippet 的 `messageIndex` 用哨兵 `-1`（TS 类型必填不动）；前端两处消费点（`SessionItem` 的 `onOpenAt`、`SessionManagerPage` 的 openReader 调用）加降级分支：`messageIndex >= 0` 精确跳转，否则 `openReader(session)` 无 index + `initialFind={query}` 定位第一个匹配（既有链路支持）。本机行为不变。

**前端显式改动清单**（修正「零改动」声明——合并逻辑与结果类型零改动，hook 层三处必动）：
1. `sessionContentKeys.search` 加 hostId 维度（对齐一期 `sessions` key 扩维先例，防切主机串缓存）
2. `searchContent` invoke 传 hostId
3. 远程下（hostId 非 null）`listUpdatedAt` 触发的 `syncContentIndex()` 跳过（同步的是本机索引，远程下白跑）

**边界**：grep 输出行超长（单行 JSON 数 MB）→ `-m` 截断 + 输出总量上限（复用 `MAX_STDOUT_BYTES` 40MB 保护，超限报错非截断）；远端 grep 缺 `-F/-H/-m`（极老 BusyBox）→ 命令失败按现有错误分类透传（不做参数探测降级，KISS）。

### 4.3 需求 3：复制恢复命令（唯一路径 + 用户裁定边界）

现状：前端「复制恢复命令」按钮已存在（`SessionItem.tsx:328`、阅读器 `SessionReaderHeader`），由 `resumeCommand` 驱动；远程下 `rebind_remote_meta`（`ssh.rs:904-916`）把 `resume_command` 置 `None` 才禁用。`session_id` 已被覆盖为远端文件 stem（对 claude/codex 等价无损），`project_dir` 保留为远端路径。

**选定**：`rebind_remote_meta` 不再置 `None`，改为生成完整可复制命令：

```
ssh [-p <port>] [-i <identity_file>] -t [<user>@]<host> 'cd <远端project_dir> && <内层resume命令>'
```

- 内层命令不沿用 rebind 输入 meta 中的 resume_command（那是本机路径语义）：按 provider 类型重新生成，claude `claude --resume <stem>`、codex `codex resume <stem>`，逻辑与 claude.rs:294 / codex.rs:2102 同式两行
- `cd` 前缀：`project_dir` 存在且为远端绝对路径时拼入；缺失时仅内层命令
- 转义：`project_dir` 与内层命令整体经 `escape_remote_path` 单引号包裹
- **不含** BatchMode / ConnectTimeout / ControlMaster 参数（用户手动执行场景，无需免密探测与连接复用）
- 前端「一键恢复」按钮远程下继续隐藏（`launchTerminal` 传 null 维持现状——用户裁定）
- 前端「复制进入目录并恢复」（`buildCdResumeCommand` 前端拼 `cd`）：远程下**隐藏该按钮**——cd 语义已在命令内，前端再拼会错（前端拼的是本机 cd）。列表行的「复制恢复命令」按钮零改动自动点亮

### 4.4 需求 4：远程用量统计（架构镜像本机增量链路）

| 方案 | 取舍 |
|---|---|
| **镜像本机增量架构（选定）** | 远端只做「清单 + 字节区间」两个原语；解析/去重/入库全在本地复用现有函数，单一口径 |
| 远端 grep/awk 抽取 usage 字段 | 省带宽但需在远端复刻解析口径（message.id 去重、stop_reason、timing 链），双份口径漂移，否决 |
| 全量拉回 | GB 级流量 + 40MB stdout 上限拦截，否决 |

**数据流（三层：自动轻 → 按需增量 → 手动首扫）**：

1. **清单 diff（自动）**：usage 页打开 + 选中远程主机 → 单次 `list_files` TSV 往返（既有原语）与上次清单比对：
   - 无变化 → 直接返回 SQLite 聚合（零额外流量）
   - 有变化（新文件 / size 增长）→ 自动对变化文件执行增量拉取（活跃会话通常 1-2 个，增量字节 KB 级）
2. **增量拉取**：变化文件 `read_range(offset=游标, len=size-offset)` 拉新增字节 → 喂现有 claude/codex 逐行 usage 解析（需将现有"整文件解析"入口加"字节段续读"能力，沿用 `session_log_sync` 游标模型：last_byte_offset + tail_fingerprint + parser_state，按 `ssh:` 源路径天然隔离）→ 入库 + 游标推进
3. **首扫（手动）**：某文件无游标且 mtime ≥ now-90d → 列入待首扫集合；**不自动拉**，UI 显示「首次同步需拉取 N 个文件 / 约 X MB」+ 手动「开始同步」按钮（用户裁定 90 天窗口；mtime 早于 90d 的文件永不建游标）
4. **zst 会话**：无法区间读 → 全量拉（`fetch_to_temp_file` ≤32MB 上限内，单文件 MB 级；归档文件不变，拉一次即终态）
5. **文件消失**（清单 diff 中移除）：清理游标，已入库数据保留（对齐本机语义）

**数据库改动（builder check 修正版，Q1/Q2/Q7 裁决）**：
- 游标表 `session_log_sync`：键含源路径，`ssh:` 前缀天然隔离，**零迁移**
- 明细表 `proxy_request_logs`：`data_source` 列写 `ssh:<hostId>`（hostId 为 UUID，无歧义字符）；provider 维度走既有 `app_type` 列不丢信息。**既有谓词适配裁定**：
  - 速度/首字口径 `= 'proxy'` / `<> 'proxy'`：ssh 行自然落 session 侧，语义正确，零改动
  - 跨来源去重 `effective_usage_log_filter`（`data_source IN ('session_log','codex_session',…)`）：ssh 行**不入集合**——远程流量不经本机代理，与 proxy 行无双记，不参与去重是正确语义（设计明确表态）
  - `reset_codex_usage_on_conn` 只清 `data_source='codex_session'`：远程 codex 行 data_source 为 `ssh:<hostId>`，天然不在清理范围，**零改动**
  - host 过滤谓词：本机视图 `data_source NOT LIKE 'ssh:%'`、远程视图 `data_source LIKE 'ssh:<hostId>%'`（COALESCE 表达式索引对前缀 LIKE 不保证命中，明细查询有日期范围+limit 兜底，性能不足时后续迭代加表达式索引）
- 日汇总表 `usage_daily_rollups`：**加 `host_id TEXT NOT NULL DEFAULT ''` 列并纳入主键（本机=空串），迁移重建**（新建表+拷贝，库内 v10→v11 式 rollup 迁移先例（schema.rs:560）；当前 SCHEMA_VERSION=26，本迁移为 26→27。行数为 30 天×组合数，量轻）。不做「零迁移」——主键 (date,app_type,provider_id,model,request_model,pricing_model) 无 host 维度，30 天自动剪枝（`rollup_and_prune`）会把远程明细并入本机同键 rollup，**不可逆污染本机统计**。加列后：聚合/剪枝/查询按 host_id 归位；`reset_codex_usage_on_conn` 的 rollup 清理谓词加 `host_id=''`（只清本机）；远程 codex 行 rollup 的 provider_id 沿用 `'_codex_session'`（口径一致，host_id 区分归属）。历史已合并行不存在（远程数据从未写入）
- 文件 size > 32MB（或拉取失败）的首扫文件：**跳过并标记**（skipped 计数进进度事件，完成时 UI 提示「N 个文件过大已跳过」），单文件失败不毁整个首扫；不做 read_range 分段拼接（解析需完整字节流，复杂度不值）

**命令层**：
- usage 五命令（`get_usage_summary` / `get_usage_summary_by_app` / `get_provider_stats` / `get_model_stats` / `get_request_logs`）**全部**加 `hostId: Option<String>`（缺省本机=现状零变化）；有值时先执行「清单 diff →（轻量）增量」再聚合返回
- 同步收敛到 `sync_remote_usage(hostId, force: bool)`（force=true 走首扫）：单命令**内部分批**处理，每批完成 emit 进度事件（仿既有 `session-index-status` 事件先例，载荷字段：`doneFiles`（累计已处理文件数）/ `totalFiles`（本轮流扫文件总数）/ `doneBytes`（累计已拉字节）/ `skipped`（超限/失败跳过计数））；新增 `cancel_remote_usage_sync()` 命令（模块级 AtomicBool 取消标志，前端关闭页面/点停止时调用）；页面打开的自动路径与手动按钮都调 sync（force 仅手动）

**前端**：
- `UsageDashboard` 页头挂主机切换（复用 `SessionHostMenu` 组件，含 ssh 可用性检测）；`usageKeys` 全系加 hostId 维度
- 远程选中：loading / 同步进度（首扫文件数与字节进度）/ 错误态（分类信息 + 重试）；自动刷新定时器远程下仍生效但只走轻路径（清单 diff）
- 时间范围 / provider / model 筛选对远程数据照常工作（SQL 聚合同构）

## 5. 测试策略

- **Rust 单测**：
  - 恢复命令构造：port/identity_file/user 各组合、project_dir 缺失、内嵌单引号路径转义、不含 BatchMode/CM 参数断言
  - rg/grep 命令构造与输出解析：探测降级逻辑（rg 有/无两路径）、多文件强制文件名前缀（rg `--with-filename` / grep `-H`）、`-m` 截断、转义 pattern（空格/引号/`--` 前缀串）、超长行保护、批 50 分批；两路径输出共用单一解析器（`path:lineno:line` 行格式）
  - 退出码分类单测：`run_remote_with_codes` 对 exit 0（正常）/ 1（无命中，空结果）/ 2（远端错误透传）/ 255（连接/认证错误）四分支的映射断言
  - 远程 ContentHit 聚合：每会话 ≤3 snippet、matchCount、高亮区间本地口径
  - usage：`ssh:` 游标隔离、`data_source` 前缀过滤 SQL、清单 diff 三态（无变化/增量/新文件）、90 天窗口过滤、zst 全量路径、文件消失清理
- **前端 vitest**：远程搜索命中渲染与跳转降级（无 messageIndex）、复制按钮远程点亮 / 一键恢复仍隐藏、「复制进入目录并恢复」远程隐藏、usage 主机切换 query key、首扫确认交互、表单样式类名回归（px-6 / 窄列 / DialogFooter）
- **集成（localhost 冒烟，既有条件跳过机制）**：远程搜索全链路（造含关键词的 jsonl → grep → 命中 → 拉回解析）；usage 增量（写入新字节 → diff → 增量解析入库）
- **手动 QA**：真实服务器验证 grep 语义（locale 大小写）、首扫流量与耗时、zst 会话统计

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 远端搜索输出超 40MB（超长行 × 多命中） | `-m 5` 每文件截断 + 既有 MAX_STDOUT_BYTES 显式报错 |
| 远端 locale 的 grep `-i` 折叠与本机口径细微差异 | rg 路径为 Unicode 折叠（口径更近）；grep 路径高亮与 snippet 定位全在本地重算，命中判定差异可容忍（展示层一致） |
| 远端 rg 版本过老缺参数（<0.10） | 探测失败按连接错误分类透传；不做参数级降级探测（KISS） |
| usage 增量解析的"字节段续读"改造引入本机回归 | 本机调用路径行为不变（新增入口而非改签名）；vitest + 单测回归 |
| 首扫仍可能数百 MB（90 天 × 大仓库） | 手动触发 + UI 预告文件数/字节数 + 可中断；窗口固定 90 天 |
| rollup 30 天剪枝把远程数字并入本机（不可逆污染） | rollup 表加 host_id 列入主键 + 迁移重建（见 4.4，builder check Q1 裁决） |
| 复制命令在不同 shell 的引号兼容 | 全命令为单层单引号 POSIX 形态，bash/zsh/sh 通用 |
| 前端合并逻辑误吞远程结果 | ContentHit 同构 + 现有 `providerId:sourcePath` 键天然区分，vitest 断言 |

## 7. 留给实施者的决定（Builder's Call）

- `search_remote_content` 内命中行→所属消息的定位算法（建议：拉回后逐行扫描，行文本前缀匹配 rg/grep 行内容）
- usage 增量续读入口的具体形态（独立函数 vs 现有函数加参数，遵循"本地路径行为不变"）
- 首扫进度 UI 形态（进度条 vs 计数文本；进度事件载荷已定）
- usage 同步的并发粒度（建议 per-host 串行，防同主机 ssh 通道争用）
- 表单样式的具体像素级调整（py 值、gap 值在既有惯例范围内微调）

## 8. 发布

- 版本：4.2.0（新功能 minor）
- 流程沿用一期：全部任务完成 + 终审通过后 `npm run tauri build` → GitHub Release + DMG 上传
