# SSH 远程会话管理设计（SSH Remote Sessions）

- 日期：2026-11-07
- 状态：已对齐用户决策 + builder check 修订完毕，已自动批准
- 规模：project（跨 Rust / 前端多模块）

## 1. 意图与动机（Intent & Why）

CC Switch 的会话管理（查看智能 Agent 历史会话）目前只读本机。用户经常在 Linux 服务器上使用 Claude Code / Codex 等 Agent，希望把日常服务器加入 CC Switch，直接在会话管理里**切换查看本机或远程主机上的历史会话**。

核心体验目标：远程会话的列表、搜索、阅读、导出、删除体验与本地对齐，不牺牲现有阅读器性能。

## 2. 目标 / 非目标 / 反目标

**目标**
- 用户可添加 SSH 主机：从 `~/.ssh/config` 勾选导入，或手动填写自定义主机
- 会话页可切换「本机 / 主机A / 主机B」，切换后列表、搜索、阅读器、删除全部指向所选主机
- MVP 支持 claude 与 codex 两家 Agent 的远程会话；架构保留按目录配置扩展到其它 JSONL 类 Agent 的能力
- 远程会话支持删除（复用现有确认弹窗与批量逻辑）

**非目标（二期或明确不做）**
- SQLite / 二进制存储类 Agent 的远程支持（opencode / hermes / mcode / agy）
- 密码认证（仅免密：公钥 / ssh-agent）
- 远程会话的正文内容索引搜索（本地 session-index.db 只索引本机会话；远程下元数据搜索可用，正文搜索自然降级为空）
- 远程主机的自定义会话目录（一期用各家默认目录：`~/.claude/projects`、`~/.codex/sessions` + `~/.codex/archived_sessions`）
- 远程 codex 线程标题表（本地 `state.db` 的线程标题远程不可读，标题退化为既有 fallback 路径：首条用户消息截断——本地 state.db 缺失时同样走此 fallback，口径一致）
- 主机配置云同步（设备级，存 settings.json）
- 远程会话的「在终端中恢复」（launch_session_terminal）与「在文件管理器中显示」（reveal_session_path）——远程模式下隐藏
- 远程 `.zst` 会话的按偏移分页（压缩字节段无法随机定位，见 5.5，退化为全量拉取）

**反目标（做了即失败）**
- 在 CC Switch 里存储 SSH 密码或私钥口令
- 为远程功能引入重型依赖（SSH 协议库等）导致安装包与维护成本显著上升
- 远程拉取导致 UI 卡顿（阻塞主线程、无 loading/错误态）

## 3. 约束（Constraints）

- **认证前提**：所有目标主机均 ssh 免密（公钥 / agent）。连接命令统一 `BatchMode=yes`，失败快速返回，绝不挂起等密码
- **远端环境**：Linux 服务器，POSIX 标配工具（find / stat / head / tail / cat / rm）。不假设 python3 或任何非标配工具存在
- **技术栈**：Tauri 2.8.2 + Rust（tokio）+ React 18 + TanStack Query。不新增 Rust / npm 依赖
- **最小改动边界（精确表述）**：13 个本地 scanner 的**解析与扫描逻辑零修改**；允许的例外仅两处——`parse_session_lines` 可见性调整为 `pub(crate)`（claude/codex 各一行，供远程摘要复用）；`providers/utils.rs` 的 `FileParseCache` 增加「文件元数据由调用方供给」的入口函数（本地调用路径行为不变）。远程走平行链路，`list_sessions` 参数缺省 = 本地行为与现状完全一致
- **安全**：远端命令只允许操作会话目录下的文件；路径参数必须校验（单引号安全转义防注入；删除仅限该主机最近扫描清单中出现过的路径）

## 4. 方案选择（Approaches）

### 4.1 传输层：系统 `ssh` 子进程 vs 内嵌 russh/ssh2 库

| 方案 | 优势 | 劣势 |
|---|---|---|
| **系统 ssh（选定）** | 零依赖；`~/.ssh/config` 的 ProxyJump / 端口 / IdentityFile 全部免费复用 | 不支持交互式密码（用户已确认全部免密）；依赖系统 ssh 二进制（macOS/Linux 内置；Windows OpenSSH 可选组件需检测降级） |
| russh 纯 Rust 库 | 内嵌协议栈，可支持密码 | 新增依赖与维护面；需自行实现 ssh config 解析 / agent / 跳板；与「不存密码」约束冲突 |

**决策依据**：用户确认全部免密 → 密码能力无价值；KISS / 零依赖。进程实现参照库内 `hermes_wsl.rs` 先例（其为 `std::process` + 线程轮询、`#[cfg(windows)]`；本功能跨平台，用 `tokio::process` 或 `std::process` + `spawn_blocking` 均可，库内依赖已备，留给实施者）。

### 4.2 远端执行：POSIX 复合命令 + 帧协议 vs 内嵌 python3 脚本

- **POSIX 复合命令（选定）**：列表 `find -printf`（降级 `stat -c`）、摘要 `head -n / tail -n / tail -c`、全文 `cat`、删除 `rm -f`。零远端依赖，输出用帧协议（哨兵分隔）回传。
- 内嵌 python3 脚本：更可控但要求 python3 存在，普通 Linux 服务器无保证。

**决策依据**：远端零依赖（YAGNI）。

### 4.3 解析器复用路径：临时文件 vs provider 加字节入口

- **临时文件（选定）**：ssh 层拉取原始字节（压缩文件即为压缩原文）落临时文件（保留正确扩展名，`.jsonl` / `.jsonl.zst`）→ 喂现有 `&Path` 解析入口。codex 入口自带 zst 检测解压，逻辑免费复用；provider 文件零改动。临时文件路径 `<get_app_config_dir()>/cache/ssh-tmp/<hostId>/<uuid>.<ext>`，解析完成即删；TranscriptCache 命中时二次打开不落盘。
- provider 加 reader 入口：codex 管线已吃 `impl Read` 改造低，claude 需新入口，且「动了 scanner 文件」边界模糊。

**决策依据**：最小改动边界（第 3 节）；落盘 32MB 上限内成本可忽略（远低于网络传输）。摘要侧不走临时文件：`parse_session_lines` 收行数组，仅需可见性调整后直接喂远端拉回的行。

### 4.4 摘要拉取：批量头尾拉取 vs 懒加载补全

- **批量头尾拉取（选定）**：按各家 scanner 所需原语（claude：`head -n 10` + `tail -n 30` + `tail -c 65536` 尾窗找 relocated；codex 以 `providers/codex.rs` 实际所需为准）一条复合命令批量拉取（每批 ≤50 文件防 ARG_MAX），帧协议回传喂本地解析。
- 压缩文件分叉：拉**前 256KB 压缩字节**本地流式解压（zstd 前缀可解，容忍截断 frame），帧协议带压缩标记位。
- 懒加载：省流量但首屏体验差、列表闪烁。

**决策依据**：ControlMaster 复用下批量拉取延迟可控，首屏信息完整。

### 4.5 全文读取：一次 `cat` 全量 + 缓存 vs 流式增量解析

- **一次全量 + 缓存（选定）**：`ssh cat <file>` 拉全部字节（压缩原文）→ 落临时文件喂 `&Path` 入口 → TranscriptCache 缓存解析结果（二次打开零延迟零落盘）。
- 流式增量解析：体验更优但需改造解析器为行缓冲增量模式，侵入大，列二期。

### 4.6 连接复用：SSH ControlMaster

**选定**：`-o ControlMaster=auto -o ControlPath=<get_app_config_dir()>/ssh/cm-%C -o ControlPersist=10m`。首条命令建主连接，后续操作复用同一条 TCP 连接（延迟从完整握手 0.5–2s 降到 ~10–50ms）。`%C` 为 OpenSSH 内置 hash，多主机天然隔离。**Windows 分支**：Windows OpenSSH 客户端不支持连接复用，Windows 上无条件省略 CM 三参数（每批一次完整握手，性能可接受），应用退出与 socket 陈旧清理由 ControlPersist 自动处理 + 建连失败时清理 socket 重试一次。

## 5. 设计细节（How）

### 5.1 配置模型

`src-tauri/src/settings.rs` 的 `AppSettings` 新增：

```rust
/// SSH 远程主机配置（设备级，不随云同步）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostConfig {
    pub id: String,                    // UUID
    pub label: String,                 // 显示名（默认取 host）
    pub host: String,                  // HostName 或手填地址
    pub user: Option<String>,
    pub port: Option<u16>,             // 默认 22
    pub identity_file: Option<String>, // 可选，缺省走 agent / ssh_config
}
// AppSettings:
pub ssh_hosts: Option<Vec<SshHostConfig>>,
```

存储于 `~/.cc-switch/settings.json`（设备级）。**不存任何密码/口令**，identity_file 仅存路径引用。

**保存链路（防清空）**：`save_settings` 为全量替换 + `merge_settings_for_save` 合并。策略 = **前端透传**：前端 Settings TS 类型与 `SettingsFormState`（`...data` 全展开）补 `sshHosts` 字段，任何设置保存都携带现有值——与 `webdavSync` 同构先例。主机增删改经设置页表单走现有 `save_settings`，无需专用命令。

### 5.2 Rust SSH 层（新模块 `src-tauri/src/session_manager/ssh.rs`）

职责单一：把「远端文件操作」封装为原语，调用方不感知 ssh 细节。

**命令构造**（统一基础参数）：

```
ssh -o BatchMode=yes -o ConnectTimeout=8 \
    [-o ControlMaster=auto -o ControlPath=<config_dir>/ssh/cm-%C -o ControlPersist=10m]  ← 非 Windows
    [-p <port>] [-i <identity_file>] [<user>@]<host> <remote_cmd>
```

**远端原语**：

| 原语 | 远端命令（示意） | 说明 |
|---|---|---|
| `test_connection` | `echo ok` | BatchMode 下认证失败立即返回 |
| `list_files(dirs)` | `find <dir> -type f \( -name '*.jsonl' -o -name '*.jsonl.zst' \) -printf '%s\t%T@\t%p\n'` | `-printf` 不支持（BusyBox）时探测降级 `stat -c`；目录不存在输出空 |
| `read_head_tail(batch)` | 循环 `head -n N` / `tail -n N` / `tail -c N` + 随机哨兵帧 | 每批 ≤50 文件；压缩文件拉前 256KB 压缩字节并置标记位，本地流式截断解压 |
| `read_full(path)` | `cat <path>` | ≤32MB 上限；压缩文件拉压缩原文落临时文件（保留扩展名） |
| `read_range(path, offset, len)` | `tail -c +<offset+1> <path> \| head -c <len>` | **仅未压缩文件**；压缩文件调用方走 `read_full` |
| `delete_file(path)` | `rm -f -- <path>` | 仅允许删除该主机扫描清单（见 5.4）中的路径 |

**注入防护**：远端路径以 POSIX 单引号安全转义（`'` → `'\''`）后拼入；`list_files` 目录由代码常量给出。

**错误分类**（前端据此展示）：exit 255 → 连接/认证失败；exit 1 → 远端命令失败；超时 → 网络不通；stderr 原文透传给 UI 错误态。

**临时文件**：`<get_app_config_dir()>/cache/ssh-tmp/<hostId>/`，解析完成即删；目录惰性创建。

### 5.3 源路径 scheme：`ssh:` 前缀

沿用 `sqlite:` / `mcode:` 前缀分流模式：

- 源路径形态：`ssh:<hostId>/<远端绝对路径>`
- `content.rs` 的 `SourceLocation` 加第三态 `Ssh { host_id: String, path: String }`；`validate_source` 对 `ssh:` 源校验 hostId 存在 + 远端路径为绝对路径且不含 `..`，**不做本地 canonicalize**
- `mod.rs` `load_messages` / `load_transcript` 按 `ssh:` 前缀分流：`read_full` → 临时文件 → 现有 `&Path` 解析入口（claude `claude.rs` / codex `codex.rs` 的 `load_messages`）
- 摘要链路：远端行数组 → `parse_session_lines`（可见性 `pub(crate)`）→ `SessionSummary` 字段口径与本地一致
- 缓存 key（`FileParseCache` / `TranscriptCache`）已含原始 sourcePath → 远程/本地天然隔离

**缓存新鲜度（重要，非零改动）**：
- `load_transcript` 的 `source_fingerprint` 现走本地 `fs::metadata`（`cache.rs`）——Ssh 源需加分支：fingerprint **从扫描清单供给**（见 5.4），`Fingerprint` 字段构造保持同构
- `FileParseCache`（`providers/utils.rs`）内部自取 `std::fs::metadata`——增加「文件元数据由调用方供给」入口（如 `lookup_with_meta(path, size, mtime)`），本地调用路径不变
- 限额（32MB 全文 / 20MB 图片）远程同口径执行

### 5.4 扫描链路与清单（平行，不动 13 个本地 scanner）

- `list_sessions` 增加可选参数 `host_id: Option<String>`（缺省 = 本机，现状零变化）
- 远程链路：取主机配置 → `list_files`（claude + codex 目录）→ 批量 `read_head_tail` → 本地解析生成 `SessionSummary` → 按修改时间排序返回
- **扫描清单（双职责：缓存新鲜度 + 删除白名单）**：每次远程扫描结果 `(远端绝对路径 → (size, mtime))` 存**进程内存**（per-host `RwLock<HashMap>`，随每次 `list_sessions` 整体替换）。生命周期 = 应用进程：重启后首次操作即重建（重启后未扫描前执行删除 → 拒绝并提示先刷新列表，语义更严）。codex `.jsonl` / `.jsonl.zst` 共存判定（`codex_source_or_sibling` 等价逻辑）在清单阶段对相邻路径处理
- codex 远程标题：`state.db` 线程表不可远程读（SQLite 非目标）→ 走既有 fallback（首条用户消息截断）

### 5.5 内容读取 / 图片 / 导出 / 删除

- `stream_session_messages` / `get_session_messages`：`ssh:` 源 → `read_full` 落临时文件 → `&Path` 入口 → 解析结果进 TranscriptCache；Channel 流式协议不变（前端零改动）
- `get_session_block_content`（512K 分页）：未压缩 → `read_range`；**压缩 → `read_full` 拉压缩原文、本地解压后内存取窗**（压缩字节偏移不可随机定位）
- `get_session_image`：**Ssh 源图片校验适配**——codex 的 `ImageSource::LocalFile` 校验链（`canonical_local_image` / `local_image_dirs` 白名单 / `read_local_image`，`content.rs`）现为本地 fs 调用，Ssh 源下替换为：字符串规范化 + 远端白名单前缀匹配（白名单 = 该主机会话根目录）+ `read_full`（≤20MB）；校验失败显示占位（与本地失败行为一致）
- `export_session_markdown`：解析走上链路，保存对话框仍在本机
- `delete_session` / `delete_sessions`：`ssh:` 源 → 清单白名单校验 → `delete_file`；确认弹窗、批量、缓存清理（TranscriptCache/FileParseCache 按源路径）逻辑复用
- `reveal_session_path` / `launch_session_terminal`：`ssh:` 源返回错误，前端隐藏入口

### 5.6 Tauri 命令（新增 / 修改）

| 命令 | 作用 |
|---|---|
| `list_ssh_config_hosts` | 解析 `~/.ssh/config`（含 `Include` 一层展开），返回 Host 条目（别名 / HostName / User / Port）；跳过通配符 Host 与 `Match` 块；前端标记已导入条目 |
| `test_ssh_host(config)` | `echo ok` 验证，返回分类错误 |
| `list_sessions`（改） | 加 `host_id: Option<String>` 参数 |

均注册进 `lib.rs` invoke_handler。主机增删改走现有 `get_settings` / `save_settings`（见 5.1）。

### 5.7 前端

**设置页 —「SSH 主机」区块**（沿用 SettingsPage section 机制）：
- 主机列表 + 添加对话框（两 tab：「从 SSH Config 导入」勾选列表 / 「手动添加」表单：label / host / user / port / identity_file）
- 「测试连接」（添加前 + 列表行）；编辑 / 删除主机（删除提示远程会话缓存保留，清理入口二期）
- `SettingsFormState` / Settings TS 类型补 `sshHosts` 透传（防清空，见 5.1）

**会话页（SessionManagerPage）**：
- 应用筛选层旁加主机切换（本机 + 已添加主机；控件形态 SegmentedControl / Dropdown 依现有组件选）
- 远程选中：loading → 错误态（分类信息 + 重试）→ 列表
- query key 扩维：`["sessions", hostId ?? "local"]`；**三处精确 key 操作同步扩维**（`SessionManagerPage` 的 `setQueryData/getQueryData(["sessions"])`、`mutations.ts` 两处——否则远程删除乐观更新写错 key）
- `useSessionTranscript` / `useBlockContent` 等 key 已含 sourcePath（`ssh:` 前缀）天然隔离，核对确认
- 远程模式隐藏「在文件管理器中显示」「在终端中恢复」；正文搜索降级为仅元数据
- `fromApp` 跳转语义不变（默认本机 + appId 筛选）
- i18n 中英文案补全

### 5.8 Windows

- 检测 ssh 二进制：不存在 → 设置页区块与主机切换入口禁用 + 提示安装 OpenSSH 客户端
- 存在 → 省略 ControlMaster 三参数（Windows OpenSSH 不支持连接复用），见 4.6

## 6. 测试策略

- **Rust 单测**（不依赖真实 ssh）：
  - `~/.ssh/config` 解析：标准样例 / Include 展开 / 通配 Host 跳过 / 畸形行容错
  - ssh 命令行构造（参数拼接、可选项、Windows 省略 CM）
  - `ssh:` 源路径解析与 `validate_source`（hostId 存在性、绝对路径、`..` 拒绝、注入样例）
  - 帧协议解析（伪 stdout 字节流 → 文件摘要映射，含压缩标记位与随机哨兵碰撞样例）
  - zst 前 256KB 截断流式解压（不完整 frame 容忍）
  - `FileParseCache` 外部元数据入口（本地路径行为回归）
  - 扫描清单替换 / 删除白名单校验（清单外路径拒绝、重启后未扫描拒绝）
  - 远端路径单引号转义
- **前端 vitest**：主机切换状态与 query key 变化、远程模式按钮隐藏、三处精确 key 扩维、设置表单 sshHosts 透传（保存不清空）
- **集成（本地手跑，条件跳过）**：`ssh localhost` 全链路（列表 / 摘要 / 全文 / 分页 / 删除 / zst）——无 sshd 环境跳过
- **手动 QA**：用户真实服务器验收（ControlMaster 复用生效、断网错误态、BusyBox 降级路径）

## 7. 风险与缓解

| 风险 | 缓解 |
|---|---|
| GNU / BusyBox 工具语法差异 | `find -printf` 探测失败降级 `stat -c`；再失败明确报「远端工具不受支持」 |
| 批量摘要命令超 ARG_MAX | 每批 ≤50 文件分批 |
| ControlMaster socket 陈旧 | ControlPersist 自动回收 + 建连失败清理 socket 重试一次 |
| 大会话首次打开慢（公网） | loading 态 + TranscriptCache 二次打开零延迟；流式增量解析二期 |
| 用户误删服务器文件 | 删除仅限扫描清单路径 + 现有确认弹窗 + 重启后须先扫描 |
| 前端设置保存清空 sshHosts | SettingsFormState 全展开透传（webdavSync 同构先例）+ vitest 回归 |
| zst 截断解压兼容性 | 256KB 前缀 + 容忍不完整 frame；解压失败降级为文件名标题 |
| Windows 无 ssh / 不支持 CM | 入口禁用引导安装（5.8）；Windows 省略 CM 参数（4.6） |

## 8. 留给实施者的决定（Builder's Call）

- 帧协议哨兵具体格式（建议每次命令生成随机 sentinel，防内容碰撞）
- codex 摘要所需远端原语集合，以 `providers/codex.rs` 现有逻辑为准对齐
- 主机切换控件形态依现有组件库选用
- i18n key 命名遵循现有 locales 结构
- ssh.rs 错误类型形态（建议 `thiserror`，库内已有惯例则跟随）
- 进程实现（`tokio::process` vs `std::process` + `spawn_blocking`）依库内并发风格定
