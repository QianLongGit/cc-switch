# SSH Remote Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 会话管理支持 SSH 远程主机：添加 Linux 服务器后，在会话页切换查看本机/远程的 claude/codex 历史会话（列表/阅读/导出/删除）。

**Architecture:** 系统 `ssh` 子进程（BatchMode + ControlMaster 复用）+ POSIX 远端原语（find/head/tail/cat/rm）+ 帧协议回传；`ssh:<hostId>/<path>` 前缀源在 `validate_source`/`load_messages`/缓存层分流；解析全部复用本地 provider（全文经临时文件喂 `&Path` 入口，摘要经 `parse_session_lines` 行数组入口）；扫描清单（进程内存）兼作缓存指纹来源与删除白名单。

**Tech Stack:** Tauri 2.8.2 / Rust（tokio、std::process、zstd 0.13、uuid v4——全部已在 Cargo.toml）/ React 18 + TanStack Query 5 / i18next（5 语言文件）。

**Spec:** `docs/superpowers/specs/2026-11-07-ssh-remote-sessions-design.md`

## Global Constraints

- 不新增任何 Rust / npm 依赖
- 13 个本地 scanner 解析与扫描逻辑零修改；仅允许：`parse_session_lines` 可见性 `pub(crate)`（claude/codex）、claude `last_relocated_cwd` 拆出纯函数、`FileParseCache` 加外部元数据入口
- `list_sessions` 参数缺省（`hostId: None`）时行为与现状完全一致
- 不存储任何 SSH 密码/口令；`identity_file` 仅路径引用
- 远端路径必须单引号安全转义（`'` → `'\''`）；删除仅限扫描清单内路径
- ssh 命令统一 `BatchMode=yes` + `ConnectTimeout=8`；ControlMaster 三参数仅非 Windows
- 注释用中文，ASCII 分块注释风格（与库内一致）
- 提交信息：conventional commit（英文，如 `feat(sessions): ...`），不 push；分支 `feat/ssh-remote-sessions` 直接在当前工作区创建（用户偏好不用 worktree）
- Rust 测试跟随邻近代码形态（内联 `#[cfg(test)]` 或 `tests.rs`）；前端 Vitest 跟随现有 `*.test.ts(x)` 形态

## Review Focus

1. **远端路径含单引号/空格/`$`/换行** → 转义后命令不被注入、帧解析不破碎 — Task 2（转义测试）、Task 4（帧解析含哨兵碰撞与特殊字符样例）
2. **主机已从设置删除但残留 `ssh:` 源被访问**（旧 query 缓存点开）→ 明确错误非 panic — Task 6（validate: hostId 不在 settings → Err）
3. **应用重启后清单为空，直接对 `ssh:` 源读/删** → 拒绝并提示先刷新列表 — Task 6（registry 无条目 → Err）、Task 10（删除白名单拒绝）
4. **帧输出被截断（网络中断/超时 kill）** → 解析失败报错而非半截数据当成功 — Task 4（截断样例测试）
5. **旧 `settings.json` 无 `sshHosts` 字段**（用户手编/旧版本） → serde 兼容为 None，保存不清空 — Task 1（serde 测试）、Task 13（透传 vitest）

---

### Task 1: 配置模型 SshHostConfig + AppSettings.ssh_hosts

**Files:**
- Modify: `src-tauri/src/settings.rs`（`WebDavSyncSettings` 约 135-154 为结构体参照；`AppSettings` 字段插入参照约 658-668 webdav_sync 块；`Default` 约 770；`normalize_paths` 约 881 为可选清理点）

**Interfaces:**
- Produces: `pub struct SshHostConfig { pub id: String, pub label: String, pub host: String, pub user: Option<String>, pub port: Option<u16>, pub identity_file: Option<String> }`（`#[derive(Debug, Clone, Serialize, Deserialize)]` + `#[serde(rename_all = "camelCase")]`，字段级 `#[serde(default, skip_serializing_if = "Option::is_none")]`，照抄 webdav_sync 模式）；`AppSettings` 新字段 `pub ssh_hosts: Option<Vec<SshHostConfig>>`

- [ ] **Step 1: 写失败测试**（settings.rs 内联 tests 或邻近 tests.rs，跟随现状）：旧 JSON（无 sshHosts）反序列化 → `ssh_hosts == None`；含 sshHosts 数组 round-trip 保持 camelCase 字段名（`identityFile`）；空数组序列化为 `"sshHosts": []`（`skip_serializing_if` 仅对 None）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test -p app --lib settings::`（包名以 `cargo metadata` 实际为准，下同）。Expected: 编译失败 `cannot find type SshHostConfig`
- [ ] **Step 3: 实现**：新增结构体 + `AppSettings` 字段（插在 webdav_backup 附近）+ `Default` 补 `ssh_hosts: None`。`get_settings_for_frontend` 不动（无敏感字段）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib settings`。Expected: PASS
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(settings): add SshHostConfig and ssh_hosts app setting"`

### Task 2: ssh.rs 基础 — 命令构造 / 路径转义 / 进程执行 / 错误分类

**Files:**
- Create: `src-tauri/src/session_manager/ssh.rs`（新模块，mod.rs 加 `pub mod ssh;`，lib.rs 无需动——session_manager 已是模块树）
- Modify: `src-tauri/src/session_manager/mod.rs`（仅加 `pub mod ssh;` 一行）

**Interfaces:**
- Consumes: Task 1 的 `SshHostConfig`
- Produces:
  - `pub(crate) enum SshError { ConnectOrAuth(String), Remote(String), Timeout, SshUnavailable }`（`impl std::fmt::Display`，中文错误文案：连接/认证失败、远端命令失败、连接超时、系统未安装 ssh）
  - `pub(crate) fn ssh_binary_available() -> bool`（PATH 查找 `ssh`，Windows 用 `where`、unix 用 `which` 语义实现，不引依赖：遍历 `PATH` env 手查）
  - `pub(crate) fn build_ssh_command(host: &SshHostConfig, remote_cmd: &str) -> std::process::Command`（基础参数 + 非 Windows 加 `ControlMaster=auto`、`ControlPath=<get_app_config_dir()>/ssh/cm-%C`、`ControlPersist=10m`；目录不存在时惰性创建）
  - `pub(crate) fn escape_remote_path(p: &str) -> String`
  - `pub(crate) fn run_command(cmd: std::process::Command, timeout: Duration) -> Result<Vec<u8>, SshError>`（hermes_wsl.rs:88-152 模式：双线程读 stdout/stderr(截 4096) + `try_wait` 轮询 + 超时 kill；退出码映射：255→ConnectOrAuth、非 0→Remote）
  - `pub(crate) fn run_remote(host: &SshHostConfig, remote_cmd: &str, timeout: Duration) -> Result<Vec<u8>, SshError>`（组合上两者）

- [ ] **Step 1: 写失败测试**：`escape_remote_path` 样例（`/a b.jsonl`→`'/a b.jsonl'`；含单引号 `a'b`→`'a'\''b'`；空串）；`build_ssh_command` 断言参数序列含 BatchMode/ConnectTimeout/ControlPersist 与 `user@host`（用 `Command::get_args()` 断言；Windows 分支 `#[cfg]` 测试可只在非 Windows 断言 CM 三参数存在）；`run_command` 用 `/bin/echo ok`（输出断言）、`/bin/sleep 5` + 100ms 超时（断言 Timeout）、exit 1（断言 Remote）；`SshError` Display 中文
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager::ssh`。Expected: 编译失败模块不存在
- [ ] **Step 3: 实现 ssh.rs**（ASCII 分块注释：`/* ===== 命令构造 ===== */` 等分节）。转义算法：整个路径包单引号，内部 `'` 替换为 `'\''`
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib session_manager::ssh`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): add ssh transport layer (command builder, escaping, process runner)`

### Task 3: `ssh:` 源路径解析 + 扫描清单 registry

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`（追加分节）

**Interfaces:**
- Produces:
  - `pub(crate) fn parse_ssh_source(raw: &str) -> Option<(String /*host_id*/, String /*remote_path*/)>`（`ssh:<hostId>/<绝对路径>`；hostId 非空且不含 `/`；路径必须 `/` 开头且 normalize 后不含 `..` 段）
  - `#[derive(Clone, Copy)] pub(crate) struct RemoteFileMeta { pub len: u64, pub mtime_ms: i64 }`
  - `pub(crate) struct SshScanRegistry`（`global()` 单例，内部 `RwLock<HashMap<String, HashMap<String, RemoteFileMeta>>>`）：`replace_manifest(host_id, entries: HashMap<String, RemoteFileMeta>)`、`lookup(host_id, path) -> Option<RemoteFileMeta>`、`remove_entry(host_id, path)`（删除成功后调用）

- [ ] **Step 1: 写失败测试**：`parse_ssh_source` 合法（`ssh:h1/home/u/.claude/projects/x.jsonl`）；非法（无前缀、hostId 空/含 `/`、相对路径、`..` 段、空路径）；registry：replace 全量替换旧清单、lookup 命中/未命中、remove_entry 后 lookup miss
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager::ssh`。Expected: FAIL undefined
- [ ] **Step 3: 实现**（纯函数 + `OnceLock`/`LazyLock` 单例，库内 `cache.rs::global()` 为参照）
- [ ] **Step 4: 运行确认通过** — Run: 同上。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): add ssh source path parsing and scan manifest registry`

### Task 4: 远端原语命令文本 + 输出/帧解析 + zst 前缀解压

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`（追加分节）

**Interfaces:**
- Produces:
  - `pub(crate) struct RemoteFileEntry { pub path: String, pub size: u64, pub mtime_ms: i64 }`
  - `pub(crate) fn list_files_command(dirs: &[String]) -> String`（`for` 循环内 `find '<dir>' -type f \( -name '*.jsonl' -o -name '*.jsonl.zst' \) -printf '%s\t%T@\t%p\n' 2>/dev/null`）
  - `pub(crate) fn parse_list_output(out: &[u8]) -> Vec<RemoteFileEntry>`（TSV；`%T@` 为 `秒.纳秒` 浮点 → `mtime_ms`，解析失败行跳过）
  - `pub(crate) struct HeadTailRequest { pub path: String, pub compressed: bool, pub head_lines: usize, pub tail_lines: usize, pub tail_bytes: u64 }`
  - `pub(crate) fn head_tail_batch_command(batch: &[HeadTailRequest], sentinel: &str) -> String`（每文件按需打印 `<<<sentinel:H:path` / `<<<sentinel:T:path` / `<<<sentinel:C:path` 标记 + `head -n`/`tail -n`/`tail -c`；压缩文件仅 `head -c 262144` 于 C 标记）
  - `pub(crate) struct HeadTailData { pub head: Vec<String>, pub tail: Vec<String>, pub tail_bytes: Option<Vec<u8>> }`
  - `pub(crate) fn parse_head_tail_output(out: &[u8], sentinel: &str, batch: &[HeadTailRequest]) -> Result<HashMap<String, HeadTailData>, String>`（按标记切段；输出截断（无结束标记）→ Err）
  - `pub(crate) fn decode_zst_prefix(bytes: &[u8]) -> Result<String, String>`（zstd 流式解压，容忍尾部不完整 frame：已解压字节返回，遇 frame 截断即停；完全无法解压 → Err）
  - `pub(crate) fn read_full_command(path: &str) -> String`（`cat '<path>'`）/ `read_range_command(path, offset, len)`（`tail -c +<offset+1> '<path>' | head -c <len>`）/ `delete_file_command(path)`（`rm -f -- '<path>'`）

- [ ] **Step 1: 写失败测试**：TSV 解析（正常行、`%T@` 浮点、乱序/空行跳过、非 UTF-8 行跳过）；帧解析（多文件多段、哨兵在文件内容中出现的碰撞样例——要求 sentinel 每次随机生成、解析器只在行首匹配标记、标记含 path 精确归属；截断输出 → Err）；`decode_zst_prefix`（本地 zstd 压缩 "line1\nline2\n..." 后截半 → 解出前缀行；随机字节 → Err）；命令文本生成断言（含转义路径）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager::ssh`。Expected: FAIL
- [ ] **Step 3: 实现**（zstd 用 `zstd::stream::read::Decoder` 循环读，`io::Error` 时已收字节 >0 则成功；行按 `\n` 切分保留原字节转 String（有损转换 `String::from_utf8_lossy`——解析器本就跳过坏行））
- [ ] **Step 4: 运行确认通过** — Run: 同上。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): add remote primitives (list/head-tail framing, zst prefix decode)`

### Task 5: ~/.ssh/config 解析器 + ssh_hosts 命令

**Files:**
- Create: `src-tauri/src/ssh_config.rs`
- Create: `src-tauri/src/commands/ssh_hosts.rs`
- Modify: `src-tauri/src/lib.rs`（`mod ssh_config;` + commands/mod.rs 声明 + invoke_handler 注册三命令）
- Modify: `src-tauri/src/commands/mod.rs`（`pub mod ssh_hosts;`）

**Interfaces:**
- Consumes: Task 2 `ssh_binary_available`/`run_remote`、Task 1 `SshHostConfig`
- Produces:
  - `pub struct SshConfigHost { pub alias: String, pub host_name: Option<String>, pub user: Option<String>, pub port: Option<u16> }`
  - `pub fn parse_ssh_config_str(content: &str) -> Vec<SshConfigHost>`（纯函数：`Host`/`HostName`/`User`/`Port`；`Host` 含 `*`/`?` 跳过；`Match` 块跳过到下个 `Host`；同 alias 多段合并后者覆盖）；`pub fn load_ssh_config_hosts() -> Vec<SshConfigHost>`（读 `~/.ssh/config`，`Include` 相对路径展开一层，读失败返回空）
  - 命令（`#![allow(non_snake_case)]` 惯例，camelCase 参数）：`list_ssh_config_hosts() -> Result<Vec<SshConfigHost>, String>`；`test_ssh_host(host: SshHostConfig) -> Result<bool, String>`（`run_remote(host, "echo ok", 15s)` 映射错误文案）；`check_ssh_availability() -> bool`

- [ ] **Step 1: 写失败测试**（ssh_config.rs 内联 tests）：标准段、多 Host 一行（`Host a b`）、通配跳过、Match 跳过、Tab 缩进、大小写关键字、Port 非数字容错、alias 合并覆盖；Include 展开只测 `load_`（临时目录 fixture）或跳过文件系统部分只测纯函数
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib ssh_config`。Expected: FAIL
- [ ] **Step 3: 实现解析器与命令**；lib.rs invoke_handler 在 Session manager 块后追加三命令注册
- [ ] **Step 4: 运行确认通过 + 编译检查** — Run: `cargo test --lib ssh_config && cargo check`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(ssh): add ssh_config parser and host management commands`

### Task 6: SourceLocation::Ssh + validate_source / fingerprint / FileParseCache 入口

**Files:**
- Modify: `src-tauri/src/session_manager/content.rs`（`SourceLocation` 51-57、`validate_source` 79 起、`load_path` 61）
- Modify: `src-tauri/src/session_manager/cache.rs`（`source_fingerprint` 134-150）
- Modify: `src-tauri/src/session_manager/providers/utils.rs`（`FileParseCache` 67 起）

**Interfaces:**
- Consumes: Task 3 `parse_ssh_source`/`SshScanRegistry`/`RemoteFileMeta`、Task 1 settings（`crate::settings::get_settings().ssh_hosts` 查 hostId）
- Produces:
  - `SourceLocation::Ssh { host_id: String, path: String }`（枚举第三态；`load_path()` 对 Ssh 返回 `self.raw.clone()`）
  - `validate_source` 新前置分支：`ssh:` 前缀 → hostId 存在于 settings → registry `lookup` 有条目（无 → `Err("请先刷新该主机的会话列表")`）→ `Ok(ValidatedSource { raw, location: Ssh, .. })`。hostId 不在 settings → `Err("SSH 主机不存在或已移除")`
  - `cache::source_fingerprint`：`SourceLocation::Ssh` 分支从 registry 取 `RemoteFileMeta` 构造 `Fingerprint { modified: Some(UNIX_EPOCH + mtime_ms), len }`（registry 无条目已在 validate 拦截，防御性再查 miss → Err）
  - `FileParseCache::scan_remote<F>(&self, files: Vec<(PathBuf, Option<SystemTime>, u64)>, parse: F) -> Vec<SessionMeta> where F: Fn(&Path) -> io::Result<Option<SessionMeta>>`（复用 `CachedParse` 新鲜度对比逻辑，metadata 由参数供给；本地 `scan_with_status` 不动）

- [ ] **Step 1: 写失败测试**：validate ssh 源（合法 → Ssh location 且 raw 保序；hostId 缺 → "SSH 主机不存在"；registry 空 → "请先刷新"；`..`/相对路径 → parse_ssh_source 返回 None → Err）；fingerprint Ssh 分支（registry meta → Fingerprint 字段正确）；`FileParseCache::scan_remote`（同 meta 二次命中不重 parse——用计数闭包断言、meta 变化重 parse）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager`。Expected: FAIL
- [ ] **Step 3: 实现三分支**（注意 `fingerprint()` 主函数按 location match 的模式，Ssh 提前返回不落本地 fs）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib session_manager`。Expected: PASS（含既有回归全绿）
- [ ] **Step 5: Commit** — `feat(sessions): route ssh: sources through validation and cache fingerprints`

### Task 7: 远程扫描链路 + provider 可见性调整 + list_sessions 参数

**Files:**
- Modify: `src-tauri/src/session_manager/providers/claude.rs`（`parse_session_lines` 141 → `pub(crate)`；`last_relocated_cwd` 101 拆出 `pub(crate) fn parse_relocated_from_tail_bytes(tail: &[u8], file_len_hint: u64) -> Option<String>`，原函数读文件后调它——行为零变化）
- Modify: `src-tauri/src/session_manager/providers/codex.rs`（`parse_session_lines` 1976 → `pub(crate)`；`SessionLines`/`HEAD_LINES`/`TAIL_LINES`/`read_session_lines` 相关项 → `pub(crate)`）
- Modify: `src-tauri/src/session_manager/mod.rs`（新 `pub fn scan_remote_sessions(host: &crate::settings::SshHostConfig) -> Result<SessionScan, String>`；`pub fn scan_sessions` 不动）
- Modify: `src-tauri/src/commands/session_manager.rs`（`list_sessions(hostId: Option<String>)`）

**Interfaces:**
- Consumes: Task 4 全部原语、Task 3 registry、Task 6、Task 2 `run_remote`
- Produces:
  - `scan_remote_sessions`：目录 = claude `$HOME/.claude/projects`、codex `$HOME/.codex/sessions` + `$HOME/.codex/archived_sessions`（远端 `$HOME` 由单条 `echo $HOME` 或直接在 find 里用 `~` 展开——用 `~` 路径拼进命令即可，`find ~/'.'...` 注意引用；实现取 `printf '%s\n' "$HOME/.claude/projects" ...` 先解析）→ `list_files_command` → `parse_list_output` → 按 provider 归类（路径前缀判断 claude/codex）→ `.jsonl`/`.jsonl.zst` 相邻共存判定（等价 `codex_source_or_sibling`：优先未压缩，清单里两者都有时取 zst 存在则用未压缩——以 `content.rs:120` 现逻辑为准：优先存在的未压缩文件）→ 分批（≤50）`head_tail_batch_command`（claude: head 10/tail 30/tail-c 65536；codex 未压缩: head 10/tail 30；codex 压缩: head-c 262144）→ 行/字节喂 `parse_session_lines`（claude 附 `parse_relocated_from_tail_bytes`；codex 压缩 `SessionLines { head, tail: vec![], fallback_last_active: Some(mtime_ms) }`，`thread_titles` 传空 map）→ `SessionMeta { provider_id, session_id: 文件 stem, source_path: Some(format!("ssh:{host_id}/{path}")), resume_command: None, ..解析结果 }` → 排序 mtime desc → `SshScanRegistry::replace_manifest` 写入 → 返回 `SessionScan { sessions, complete_providers: ["claude","codex"] }`
  - 命令：`list_sessions(hostId)`：`None` → 现有 `scan_sessions`；`Some(id)` → settings 查主机（缺 → Err）→ `spawn_blocking(scan_remote_sessions)`

- [ ] **Step 1: 写失败测试**（可离线部分）：claude `parse_relocated_from_tail_bytes`（构造含 relocated 尾窗字节 → Some(cwd)；无 relocated → None；半截首行被跳过——与原 `last_relocated_cwd` 行为对齐，原函数测试保持全绿即回归证据）；分批函数 `chunks_of_50`（51 → [50,1]）若实现为独立函数则测；`SessionMeta` 组装纯函数（若抽取 `build_remote_meta(provider, path, parse_result, host_id)` 则测 source_path 前缀与 stem）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager`。Expected: FAIL（新测试）/ 原有全绿
- [ ] **Step 3: 实现**（scan_remote_sessions 主体 ssh 依赖部分不做单测，逻辑下沉到已测纯函数）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib`。Expected: 全 PASS
- [ ] **Step 5: Commit** — `feat(sessions): remote scan pipeline for claude/codex via ssh`

### Task 8: 内容读取分流 — load_messages 临时文件 + load_transcript

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`（追加 `pub(crate) fn fetch_to_temp_file(host: &SshHostConfig, remote_path: &str, max: u64) -> Result<PathBuf, SshError>`：`read_full_command` + 32MB 上限 + 写入 `<get_app_config_dir()>/cache/ssh-tmp/<host_id>/<uuid>.<原扩展名>`；调用方用后 `let _ = std::fs::remove_file`）
- Modify: `src-tauri/src/session_manager/mod.rs`（`load_messages` 151：前置 `ssh:` 分支 → parse → registry 查 host（settings）→ `fetch_to_temp_file` → 按 provider_id 调 `claude::load_messages(&temp)` / `codex::load_messages(&temp)` → 删临时文件；`load_transcript` 无需改——validate/fingerprint 已分流，`get_or_load` 的 key=(provider_id, raw) 天然隔离）

**Interfaces:**
- Consumes: Task 6 validate/fingerprint、Task 4 `read_full_command`、Task 2
- Produces: `ssh::fetch_to_temp_file`（Task 9/10 复用）；`load_messages` 支持 `ssh:` 源

- [ ] **Step 1: 写失败测试**：临时文件路径生成纯函数 `temp_file_path(host_id, remote_path) -> PathBuf`（扩展名保留 `.jsonl.zst` 双段、uuid 注入点、目录创建）；32MB 上限检查函数（`enforce_limit(bytes_len, MAX)` → Err 文案与本地 32MB 一致）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager::ssh`。Expected: FAIL
- [ ] **Step 3: 实现**（`load_messages` 的 ssh 分支放最前，`content::validate_source` 已保证合法性——注意 `load_messages` 也被 `load_transcript` 的 load 闭包调用，`raw` 即完整 `ssh:` 源，避免二次拼接）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): stream remote transcripts via temp-file handoff to local parsers`

### Task 9: block content 远程读取 + zst 内存取窗 + 图片适配

**Files:**
- Modify: `src-tauri/src/session_manager/ssh.rs`（追加 `pub(crate) fn read_block(host, remote_path, offset, len, compressed) -> Result<Vec<u8>, SshError>`：未压缩 `read_range_command`；压缩 `read_full_command` 拉全量 → zstd 全解压 → 内存 `[offset..offset+len]` 取窗；`pub(crate) fn fetch_image(host, remote_path) -> Result<Vec<u8>, SshError>`（≤20MB））
- Modify: `src-tauri/src/session_manager/content.rs`（`rg "get_session_block_content|read_compressed_window"` 定位块读取分发处——线索 `content.rs:386` 与 commands/session_manager.rs:130；为 `SourceLocation::Ssh` 加分支调 `ssh::read_block`。图片：`load_image`（553）的 `ImageSource::LocalFile` 路径在 Ssh 源下改走 `ssh_image_ok(source, image_path) -> bool`（字符串规范化 + 白名单前缀：远端路径须以该 host 清单根目录（`/home/*/.claude/projects`、`/home/*/.codex/sessions` 等扫描目录）为前缀）+ `ssh::fetch_image`；扩展名/SVG/大小校验复用现有常量）

**Interfaces:**
- Consumes: Task 4/8
- Produces: `ssh::read_block` / `ssh::fetch_image` / `content.rs` 内 Ssh 图片校验辅助 `fn remote_image_allowed(host_id, path) -> bool`

- [ ] **Step 1: 写失败测试**：zst 内存取窗（本地构造 zst 字节 → offset/len 窗口 == 原文对应切片；offset 超长 → 空/Err）；`remote_image_allowed`（清单根目录前缀内 → true；`..`/前缀外/SVG → false；大小写与多根目录样例）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager`。Expected: FAIL
- [ ] **Step 3: 实现**（块读取分支保持现有 STALE 语义不引入——远程每次实时读，无 stale）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): remote block content paging, zst in-memory window, image fetch`

### Task 10: 远程删除

**Files:**
- Modify: `src-tauri/src/session_manager/mod.rs`（`delete_session` 207 与 `delete_sessions`：前置 `ssh:` 分支 → registry `contains` 校验（无 → Err "请先刷新…"）→ `delete_file_command` + `run_remote` → 成功后 `SshScanRegistry::remove_entry` + `cache::global().remove` + `search::forget_source`（远端源不在本地索引，forget 幂等无害））

**Interfaces:**
- Consumes: Task 3 registry、Task 4 `delete_file_command`、Task 2 `run_remote`

- [ ] **Step 1: 写失败测试**：删除白名单判定纯函数 `can_delete_remote(registry_lookup_result)`（Some → true / None → false + 文案）；`ssh:` 源路径下 provider/session_id 校验（sessionId == 文件 stem，防路径与 id 不符）
- [ ] **Step 2: 运行确认失败** — Run: `cargo test --lib session_manager`。Expected: FAIL
- [ ] **Step 3: 实现**（批量 `delete_sessions` 循环复用单条逻辑，`DeleteSessionOutcome` 语义对齐本地）
- [ ] **Step 4: 运行确认通过** — Run: `cargo test --lib`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): remote session deletion with manifest whitelist`

### Task 11: 前端 API 层 + query key 扩维

**Files:**
- Modify: `src/lib/api/sessions.ts`（`list(hostId?: string)` → `invoke("list_sessions", { hostId: hostId ?? null })`；新增 `listSshConfigHosts` / `testSshHost` / `checkSshAvailability` 封装，形态照抄现有解构展开）
- Modify: `src/lib/query/queries.ts:308`（`useSessionsQuery(hostId?: string)`，`queryKey: ["sessions", hostId ?? "local"]`）
- Modify: `src/components/sessions/SessionManagerPage.tsx:617-624`（`setQueryData(["sessions"], …)` → `setQueryData(["sessions", currentHostKey], …)`——currentHost 由组件 state 传入 mutation 上下文或 key 工厂）
- Modify: `src/lib/query/mutations.ts:424-460`（`getQueryData/setQueryData/invalidateQueries` 三处 `["sessions"]` → 经 `sessionKeys.list(hostId?)` key 工厂；`src/lib/query/sessions.ts` 若已有 `sessionKeys` 就扩展，无则在 queries.ts 定义 `export const sessionKeys = { list: (hostId?: string) => ["sessions", hostId ?? "local"] as const, ... }`）
- Modify: `src/components/sessions/utils.ts`（`SESSION_SOURCE_PATHS` 补 ssh 提示文案，如需要）

**Interfaces:**
- Produces: `sessionsApi.list(hostId?)`、`sessionKeys.list(hostId?)`（全库统一 key 工厂，后续任务强制使用）、`useSessionsQuery(hostId?)`

- [ ] **Step 1: 写失败测试**（vitest，跟随现有测试文件形态，`rg "\\.test\\.(ts|tsx)" src/lib` 找参照）：`sessionKeys.list()` === `["sessions","local"]`、`sessionKeys.list("h1")` === `["sessions","h1"]`；`sessionsApi.list` invoke 参数含 `hostId: null`（mock invoke）
- [ ] **Step 2: 运行确认失败** — Run: `npx vitest run src/lib`（范围以实际文件位置调整）。Expected: FAIL
- [ ] **Step 3: 实现**（全库 `rg '\["sessions"\]'` 清点所有精确 key 用法逐一迁移到 key 工厂，含本任务未列出的命中处）
- [ ] **Step 4: 运行确认通过** — Run: `npx vitest run`。Expected: 全 PASS
- [ ] **Step 5: Commit** — `refactor(sessions): host-scoped session query keys and ssh api bindings`

### Task 12: 会话页主机切换 UI

**Files:**
- Modify: `src/components/sessions/SessionManagerPage.tsx`（`appFilter` state 166 旁加 `hostId: string | null` state；应用筛选 DropdownMenu 1230 旁加主机 DropdownMenu（「本机」+ settings.sshHosts 列表，label+host 副文案，lucide `Server`/`Monitor` 图标）；`useSessionsQuery(hostId)`；错误态：query.error 含分类文案 + 重试按钮（复用现有错误 UI 形态，`rg "retry" src/components/sessions`）；远程模式隐藏「在终端中恢复」（SessionItem/ReaderHeader 的 launch 入口按 `hostId === null` 条件）与「在文件管理器中显示」（PathChip onReveal 注入处））
- Modify: `src/components/sessions/reader/SessionReaderHeader.tsx`（`launchTerminal` prop 由父级在远程时传 null——现有 `command && launchLabel` 条件即自动隐藏，确认无需改内部逻辑则只改父级传参）
- Modify: `src/components/sessions/reader/PathChip.tsx` 或其调用处（远程时 `onReveal` 不传/传 undefined，走无 reveal 形态）
- Modify: `src/i18n/locales/{zh,zh-TW,en,ja,tr}.json`（`sessionManager.hostSelector.*`：本机/远程主机/连接失败/重试/主机不可达等 key，五语言补齐，en 至少准确、zh 为源）

**Interfaces:**
- Consumes: Task 11 `useSessionsQuery(hostId)` / `sessionsApi`、settings `sshHosts`
- Produces: `SessionManagerPage` 接受/持有当前 `hostId`（从 `usePageParams` 的 fromApp 语义不变：默认 null=本机）

- [ ] **Step 1: 写失败测试**（组件测试跟随 `rg "\\.test\\.tsx" src/components` 现有形态；若库内组件测试稀少，以逻辑 hook 测试为主）：主机切换后 `sessionKeys.list(hostId)` 生效（mock useSessionsQuery 断言参数）；远程模式 reveal/launch 不渲染（render SessionItem/PathChip 断言无 reveal 回调暴露）；sshHosts 为空时仅显示「本机」且无下拉
- [ ] **Step 2: 运行确认失败** — Run: `npx vitest run src/components/sessions`。Expected: FAIL
- [ ] **Step 3: 实现**（KISS：主机切换控件直接复用应用筛选的 DropdownMenu 结构；`check_ssh_availability` 为 false 时主机菜单禁用 + tooltip 提示安装 OpenSSH）
- [ ] **Step 4: 运行确认通过** — Run: `npx vitest run && npx tsc --noEmit`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(sessions): host switcher UI with remote error states and entry hiding`

### Task 13: 设置页 SSH 主机管理 + sshHosts 透传

**Files:**
- Modify: `src/types.ts`（`Settings` 加 `sshHosts?: SshHostConfig[]`；新增 `export interface SshHostConfig { id: string; label: string; host: string; user?: string; port?: number; identityFile?: string }`，插在 `WebDavSyncSettings` 340 附近）
- Modify: `src/hooks/useSettingsForm.ts`（`SettingsFormState` 经 `Omit<Settings,…>` 自动含 sshHosts——验证 `{...data}` 初始化 113-134 与提交路径全量携带；若提交有字段白名单则补）
- Create: `src/components/settings/SshHostsSection.tsx`（新 section 组件：`SettingsBlock`/`SettingsCard`/`SettingsRow` 版式；主机列表行（label/host/状态点）+ 编辑/删除（删除确认对话框）+「测试连接」按钮调 `sessionsApi.testSshHost`；添加对话框双 tab：「从 SSH Config 导入」（`listSshConfigHosts` 勾选列表，已导入条目——id 按 `host:port:user` 归一化匹配——标记禁用）与「手动添加」（react-hook-form + zod：label 必填、host 必填、port 1-65535 可选、identityFile 可选）；保存 = 表单 state 更新 sshHosts → 现有 saveSettings 流）
- Modify: `src/lib/navigation.ts`（`SettingsSection` 加 `"sshHosts"`，`SETTINGS_SECTIONS` 数组插入 `network` 后）
- Modify: `src/components/settings/SettingsPage.tsx`（`renderSection()` 加 case + `SECTION_ICON` 加图标（lucide `Server`））
- Modify: `src/i18n/locales/*.json`（`settings.sections.sshHosts` + `settings.sshHosts.*`：标题/帮助/添加/导入/手动/测试连接/连接成功/失败文案/删除确认等，五语言）

**Interfaces:**
- Consumes: Task 5 三命令、Task 11 api 封装、Task 1 serde
- Produces: `SshHostConfig` TS 类型；设置保存链路携带 sshHosts（防清空——Review Focus #5 的前端半场）

- [ ] **Step 1: 写失败测试**：`useSettingsForm` 提交 payload 含 `sshHosts` 原值（mock 后端，预置 1 主机 → 改其它字段保存 → payload.sshHosts 深等）；zod schema（port 0/70000 拒绝、label 空拒绝）
- [ ] **Step 2: 运行确认失败** — Run: `npx vitest run src`。Expected: FAIL
- [ ] **Step 3: 实现**（导入条目 → `SshHostConfig { id: uuid（前端 crypto.randomUUID 或后端生成——用 crypto.randomUUID）, label: alias, host: hostName ?? alias, user, port }`）
- [ ] **Step 4: 运行确认通过** — Run: `npx vitest run && npx tsc --noEmit`。Expected: PASS
- [ ] **Step 5: Commit** — `feat(settings): ssh host management section with config import and connection test`

### Task 14: 集成冒烟与全量验证

**Files:**
- Create: `src-tauri/src/session_manager/ssh_localtest.rs`（`#[cfg(all(test, unix))]` + 环境探测跳过：`ssh -o BatchMode=yes localhost true` 失败 → `eprintln!` + return；成功 → 在远端 `mktemp -d` 造 claude 假目录 + 假 `.jsonl`（最小合法行）→ 走 `scan_remote_sessions` / `load_messages` / `read_block` / 删除全链路断言 → 清理。主机配置 `SshHostConfig { id: "localtest", host: "localhost", .. }`；依赖本机 `~/.ssh/authorized_keys` 免密——CI 无则自动跳过，报告 skipped）
- 无生产代码改动的收尾验证任务

- [ ] **Step 1: 写集成测试**（上述；测试自身即交付物）
- [ ] **Step 2: 本机运行** — Run: `cargo test --lib ssh_localtest -- --nocapture`。Expected: PASS 或 skipped（无本地 sshd）
- [ ] **Step 3: 全量验证** — Run: `cargo test --lib && cargo clippy --all-targets -- -D warnings && cargo fmt --check && npx vitest run && npx tsc --noEmit && npm run build`。Expected: 全 PASS（clippy/fmt 若库内存量告警非本次引入，允许 `--cap-lints` 或按库内 CI 口径）
- [ ] **Step 4: 手动 QA 清单输出**（不阻塞）：真机连一台服务器，走查：添加主机（导入+手填）/测试连接/切换主机/列表/阅读大会话/zst 会话/图片/导出/删除/断网错误态。清单写入交付报告
- [ ] **Step 5: Commit** — `test(sessions): localhost ssh integration smoke test`

---

## 执行说明

- 执行方式：subagent-driven（每任务 fresh implementer + fresh reviewer，用户全局规范钦定子代理执行；并发 ≤ 4）
- 任务依赖链：1 → 2 → 3 → 4 → 5（1-4 后 5 可并行）；6 依赖 3；7 依赖 4+6；8 依赖 6；9 依赖 8；10 依赖 3；11 独立（可早于 7 并行）；12 依赖 11+5；13 依赖 5+11；14 最后
- 每任务完成即 commit（superpowers 流程内惯例，conventional 英文）；全程不 push
