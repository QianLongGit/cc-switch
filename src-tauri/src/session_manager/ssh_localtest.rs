//! localhost SSH 全链路集成冒烟测试（环境探测自动跳过）。
//!
//! 前提是本机 sshd + 免密登录可用（`ssh -o BatchMode=yes localhost true`
//! 可通）；无本地 sshd / 未授权的机器与 CI 上自动跳过（早退 `return` +
//! `eprintln` 说明，而非 `#[ignore]`——让有环境的机器零配置自动执行）。
//!
//! 通过探测后覆盖完整远程链路：
//!
//! ```text
//! 远端 $HOME 下唯一前缀夹具目录 + 最小合法 JSONL
//!   → scan_remote_sessions（清单 + 头尾帧协议 + 解析缓存）
//!   → load_messages（全文拉取落临时文件 + provider 解析）
//!   → ssh::read_block（远端字节区间窗口）
//!   → delete_session（stem 校验 + 清单白名单 + rm + registry 摘除）
//!   → Drop guard 清理远端夹具
//! ```
//!
//! 全局态隔离（`#[serial_test::serial]` 串行 + [`FixtureGuard`] 兜底恢复）：
//!
//! - settings 单例：注入 `id=localtest` 主机供 `load_messages` /
//!   `delete_session` 的 `ssh_host_by_id` 查询，结束恢复原值
//! - `CC_SWITCH_TEST_HOME`：落盘重定向到 `/tmp` 下短前缀 tempdir（长
//!   tempdir 前缀会让 ControlMaster socket 路径超过 unix 域 104 字节上限），
//!   真实 settings.json 零接触
//! - ControlMaster：测试窗口内全局禁用（macOS 多线程进程下 ssh mux 传 fd
//!   偶发 `EMSGSIZE`，libtest 并发语境曾整批失败），直连 localhost 免疫；
//!   Drop 时仍防御性点杀 socket 目录下的残留 master
//! - 远端夹具目录：带 `cc-switch-localtest-<uuid>` 唯一前缀，断言失败也清理

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use serial_test::serial;

use crate::settings::SshHostConfig;

use super::ssh::{
    escape_remote_path, read_block, run_remote, SshScanRegistry, DISABLE_CONTROL_MASTER_FOR_TEST,
};

/* ===== 远端命令超时（与 mod.rs 链路口径对齐）===== */

/// 远端目录解析 / 存在性检查（亚秒级完成，留足连接建立余量）
const QUICK_CMD_TIMEOUT: Duration = Duration::from_secs(15);
/// 远端夹具构造 / 清理（mkdir + printf 多目录，同样亚秒级）
const SETUP_CMD_TIMEOUT: Duration = Duration::from_secs(60);

/* ===== 夹具内容（最小合法 JSONL，行格式对齐 providers 既有测试 fixture）===== */

/// claude 夹具正文：首行 user 消息给标题 / sessionId / cwd，次行 assistant
/// 消息给 last_active_at（head 10 / tail 30 行窗口内两行都可见）
const CLAUDE_BODY: &str = concat!(
    "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello localtest\"},",
    "\"sessionId\":\"sess-localtest\",\"cwd\":\"/tmp/localtest-project\",",
    "\"timestamp\":\"2026-11-07T10:00:00Z\"}\n",
    "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"hi from localtest\"},",
    "\"sessionId\":\"sess-localtest\",\"cwd\":\"/tmp/localtest-project\",",
    "\"timestamp\":\"2026-11-07T10:00:05Z\"}\n",
);

/// codex 夹具正文：session_meta 给 cwd，response_item user 消息给标题与正文
const CODEX_BODY: &str = concat!(
    "{\"timestamp\":\"2026-11-07T21:50:12Z\",\"type\":\"session_meta\",",
    "\"payload\":{\"id\":\"rollout-localtest\",\"cwd\":\"/tmp/localtest-project\"}}\n",
    "{\"timestamp\":\"2026-11-07T21:50:13Z\",\"type\":\"response_item\",",
    "\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello codex localtest\"}}\n",
);

/* ===== 环境探测 ===== */

/// 探测 localhost SSH 免密可达。失败只说明本机无 sshd / 未授权 / 无 ssh
/// 二进制——属环境缺失而非代码缺陷，调用方打印说明后按 skipped 语义早退。
fn localhost_ssh_available() -> bool {
    let probe = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "localhost",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match probe {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!(
                "skip: ssh localhost 不可用（exit={}）：{}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            eprintln!("skip: 无法启动 ssh 进程（未安装？）：{e}");
            false
        }
    }
}

/* ===== 全局态与远端夹具的清理 guard ===== */

/// 无论断言成败，Drop 按序恢复全部副作用：
///
/// 1. 恢复 ControlMaster 测试开关（最先——后续清理自身的 ssh 调用回默认行为）
/// 2. `rm -rf` 远端夹具目录（失败仅告警——目录带唯一前缀，残留可人工识别）
/// 3. 点杀 socket 目录下残留的 ControlMaster（防泄漏；须在恢复
///    `CC_SWITCH_TEST_HOME` 之前——socket 路径随它变化）
/// 4. 恢复 settings 原值（此时 `CC_SWITCH_TEST_HOME` 仍指 tempdir，
///    `update_settings` 的落盘只写 tempdir，真实 settings.json 零接触）
/// 5. 恢复 `CC_SWITCH_TEST_HOME` 原值、删除 tempdir
struct FixtureGuard {
    host: SshHostConfig,
    remote_dirs: Vec<String>,
    original_settings: crate::settings::AppSettings,
    original_env: Option<std::ffi::OsString>,
    /// 本地 tempdir 路径（Drop 时手动删除——`tempfile::TempDir` 的句柄
    /// 放这里会与远端清理的执行顺序耦合，统一在 Drop 末尾显式清理更直白）
    local_temp: std::path::PathBuf,
}

impl FixtureGuard {
    /// 遍历本测试 socket 目录（`<tempdir>/.cc-switch/ssh`）下的 `cm-*`
    /// socket，逐个 `ssh -O exit` 请求 master 退出。
    ///
    /// `%C` hash 无法本地反推，按文件名前缀枚举；socket 失效 / 无 master
    /// 返回非零属正常态（静默），仅成功退出时打印一行确认。
    fn shutdown_control_masters(&self) {
        let dir = crate::config::get_app_config_dir().join("ssh");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("cm-") {
                continue;
            }
            let socket = entry.path();
            let Ok(out) = Command::new("ssh")
                .arg("-o")
                .arg(format!("ControlPath={}", socket.display()))
                .arg("-O")
                .arg("exit")
                .arg(&self.host.host)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()
            else {
                continue;
            };
            if out.status.success() {
                eprintln!("localtest: 已退出残留 ControlMaster ({})", socket.display());
            }
        }
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        DISABLE_CONTROL_MASTER_FOR_TEST.store(false, Ordering::Relaxed);
        if !self.remote_dirs.is_empty() {
            let mut cmd = String::from("rm -rf --");
            for dir in &self.remote_dirs {
                cmd.push(' ');
                cmd.push_str(&escape_remote_path(dir));
            }
            if let Err(e) = run_remote(&self.host, &cmd, QUICK_CMD_TIMEOUT) {
                eprintln!("warn: 清理远端夹具目录失败（残留可人工删除）: {e}");
            }
        }
        // ControlPersist=10m 下 master 会存活到超时（失败运行曾累积 8 个残留
        // 进程）；此刻若走 mux，上面的 rm 已用完连接，此处点杀不再产生新 master
        self.shutdown_control_masters();
        if let Err(e) = crate::settings::update_settings(self.original_settings.clone()) {
            eprintln!("warn: 恢复 settings 失败: {e}");
        }
        match self.original_env.take() {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.local_temp);
    }
}

/* ===== 全链路冒烟 ===== */

/// localhost SSH 扫描 → 正文 → 区间读取 → 删除的完整链路冒烟。
///
/// 无环境自动跳过；有环境时任何一步失败都会让 guard 兜底清理并暴露断言。
#[test]
#[serial]
fn localhost_ssh_full_chain_smoke() {
    if !localhost_ssh_available() {
        return;
    }
    eprintln!("localtest: ssh localhost 可用，执行全链路冒烟");

    // ── 全局态准备：落盘重定向短 tempdir + settings 注入 localtest 主机 ──
    // ControlMaster socket 已独立落在 `/tmp/cc-switch-ssh-<user>` 短目录
    // （规避 unix 域 104 字节上限），tempdir 仅承载配置/settings 落盘，
    // 仍取 `/tmp` 短前缀以保持路径简短
    let local_temp = tempfile::Builder::new()
        .prefix("ccsw-lt-")
        .tempdir_in("/tmp")
        .expect("创建 /tmp 短前缀 tempdir")
        .keep();
    let original_env = std::env::var_os("CC_SWITCH_TEST_HOME");
    std::env::set_var("CC_SWITCH_TEST_HOME", &local_temp);

    let host = SshHostConfig {
        id: "localtest".to_string(),
        label: "localtest".to_string(),
        host: "localhost".to_string(),
        user: None,
        port: None,
        identity_file: None,
    };
    let original_settings = crate::settings::get_settings();
    let mut with_host = original_settings.clone();
    // 测试窗口内只有 localtest 一台主机：load / delete 链路只查它，
    // 窗口结束 guard 整体恢复，不叠加既有主机可最小化干扰面
    with_host.ssh_hosts = Some(vec![host.clone()]);
    crate::settings::update_settings(with_host).expect("注入 localtest 主机到 settings");

    let mut guard = FixtureGuard {
        host: host.clone(),
        remote_dirs: Vec::new(),
        original_settings,
        original_env,
        local_temp,
    };

    // ── 禁用 ControlMaster：macOS 上多线程进程 spawn 的 ssh 走 mux 传 fd ──
    // 时 sendmsg 偶发 EMSGSIZE（mm_send_fd: sendmsg(1): Message too long），
    // libtest 默认并发的多线程语境曾整批触发；直连 localhost 免疫该偶发
    // 类别。开关在 guard 创建之后打开、Drop 最先关闭——任何后续 panic 都
    // 由 guard 恢复，不影响同进程其他测试的 CM 断言
    DISABLE_CONTROL_MASTER_FOR_TEST.store(true, Ordering::Relaxed);

    // ── 远端夹具：$HOME 下唯一前缀目录 + 最小合法 JSONL ──
    // 扫描根固定为远端 $HOME 下的 .claude/projects 与 .codex/sessions，
    // 夹具必须落在其内（mktemp 独立目录扫不到）；唯一前缀保证断言可过滤、
    // 清理不误伤真实会话
    let home_raw =
        run_remote(&host, "printf '%s' \"$HOME\"", QUICK_CMD_TIMEOUT).expect("解析远端 HOME");
    let home = String::from_utf8_lossy(&home_raw).trim().to_string();
    assert!(home.starts_with('/'), "远端 HOME 应为绝对路径: {home}");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let claude_dir = format!("{home}/.claude/projects/cc-switch-localtest-{tag}");
    let codex_dir = format!("{home}/.codex/sessions/cc-switch-localtest-{tag}");
    let claude_file = format!("{claude_dir}/sess-localtest.jsonl");
    let codex_file = format!("{codex_dir}/rollout-localtest.jsonl");

    // 先登记待清理目录再执行 mkdir：setup 半途失败（首个目录已建、第二个
    // 未建）时 panic 前已入册，guard 仍能 rm 掉已建目录，不残留
    guard.remote_dirs.push(claude_dir.clone());
    guard.remote_dirs.push(codex_dir.clone());
    let setup = format!(
        "mkdir -p {} {} && printf '%s' {} > {} && printf '%s' {} > {}",
        escape_remote_path(&claude_dir),
        escape_remote_path(&codex_dir),
        escape_remote_path(CLAUDE_BODY),
        escape_remote_path(&claude_file),
        escape_remote_path(CODEX_BODY),
        escape_remote_path(&codex_file),
    );
    run_remote(&host, &setup, SETUP_CMD_TIMEOUT).expect("构造远端夹具");

    // ── 1. 扫描：夹具会话 ≥2 条 + source_path 前缀 + manifest 写入 ──
    let scan = super::scan_remote_sessions(&host).expect("远程扫描应成功");
    let mine: Vec<_> = scan
        .sessions
        .iter()
        .filter(|meta| {
            meta.source_path
                .as_deref()
                .is_some_and(|src| src.starts_with("ssh:localtest/") && src.contains(&tag))
        })
        .collect();
    assert!(
        mine.len() >= 2,
        "应扫出 ≥2 条夹具会话（claude + codex），实际 {}: {mine:?}",
        mine.len()
    );
    for meta in &mine {
        let src = meta
            .source_path
            .as_deref()
            .expect("远程会话必有 source_path");
        assert!(
            src.starts_with("ssh:localtest/"),
            "source_path 应带 ssh:localtest/ 前缀: {src}"
        );
    }
    let claude_meta = mine
        .iter()
        .find(|meta| meta.provider_id == "claude")
        .expect("夹具 claude 会话应存在");
    assert_eq!(
        claude_meta.session_id, "sess-localtest",
        "session_id 应为文件 stem"
    );
    let codex_meta = mine
        .iter()
        .find(|meta| meta.provider_id == "codex")
        .expect("夹具 codex 会话应存在");
    assert_eq!(
        codex_meta.session_id, "rollout-localtest",
        "session_id 应为文件 stem"
    );

    let registry = SshScanRegistry::global();
    assert!(
        registry.lookup("localtest", &claude_file).is_some(),
        "manifest 应写入 claude 夹具: {claude_file}"
    );
    assert!(
        registry.lookup("localtest", &codex_file).is_some(),
        "manifest 应写入 codex 夹具: {codex_file}"
    );

    // ── 2. 正文读取：全文拉取落临时文件 → provider 解析出消息 ──
    let claude_src = claude_meta.source_path.clone().expect("claude source_path");
    let codex_src = codex_meta.source_path.clone().expect("codex source_path");

    let claude_msgs = super::load_messages("claude", &claude_src).expect("claude 远程正文解析");
    assert!(
        !claude_msgs.is_empty(),
        "claude 夹具应解析出消息（{CLAUDE_BODY:?}）"
    );
    assert!(
        claude_msgs
            .iter()
            .any(|msg| msg.content.contains("hello localtest")),
        "消息文本应含夹具首条 user 消息: {:?}",
        claude_msgs
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
    );

    let codex_msgs = super::load_messages("codex", &codex_src).expect("codex 远程正文解析");
    assert!(
        !codex_msgs.is_empty(),
        "codex 夹具应解析出消息（{CODEX_BODY:?}）"
    );
    assert!(
        codex_msgs
            .iter()
            .any(|msg| msg.content.contains("hello codex localtest")),
        "消息文本应含夹具 user 消息: {:?}",
        codex_msgs
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
    );

    // ── 3. 区间读取：远端字节窗口与本地已知内容逐字节一致 ──
    let claude_bytes = CLAUDE_BODY.as_bytes();
    let window = read_block(&host, &claude_file, 3, 9, false).expect("read_block 区间读取");
    assert_eq!(window, &claude_bytes[3..12], "远端窗口应等于夹具对应切片");
    // 起点贴文件尾：空窗不报错
    let tail = read_block(&host, &claude_file, claude_bytes.len() as u64, 8, false)
        .expect("read_block 文件尾窗口");
    assert!(tail.is_empty(), "文件尾起读应为空窗");

    // ── 4. 删除：stem 校验 + 白名单放行 + 远端 rm + registry 摘除 ──
    let deleted =
        super::delete_session("claude", "sess-localtest", &claude_src).expect("远程删除应成功");
    assert!(deleted, "删除应返回 true");
    assert!(
        registry.lookup("localtest", &claude_file).is_none(),
        "删除后 manifest 应摘除该条目"
    );
    let still_there = run_remote(
        &host,
        &format!("test -e {}", escape_remote_path(&claude_file)),
        QUICK_CMD_TIMEOUT,
    );
    assert!(
        still_there.is_err(),
        "远端夹具文件应已被 rm 删除: {claude_file}"
    );

    // guard Drop：清理剩余夹具目录 + 恢复 settings 与环境变量
    let _ = std::io::stdout().flush();
    eprintln!("localtest: 全链路冒烟通过");
}
