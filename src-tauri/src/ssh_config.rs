//! `~/.ssh/config` 只读解析器：把 Host 段提取为轻量主机清单，供远程主机选择 UI 使用。
//!
//! 设计取舍（KISS）：
//! - 只解析 `Host` / `HostName` / `User` / `Port` 四个关键字，其余全部忽略
//! - 关键字大小写不敏感；Tab / 空格缩进都合法（OpenSSH 允许任意空白分隔）
//! - `Host` 值含 `*` / `?` 通配的条目跳过（不产出、其段内属性也不归属任何主机）
//! - `Match` 块整体跳过，直到下个 `Host` 行
//! - 同 alias 多段合并、后者覆盖；`Host a b` 多别名一行各自独立成条目、共享段内属性
//!
//! [`parse_ssh_config_str`] 为纯函数；[`load_ssh_config_hosts`] 负责读文件与
//! `Include` 相对路径展开一层。读失败一律返回空清单——本能力是「有则展示」的
//! 增强功能，不向用户报错。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/* ===== 数据结构 ===== */

/// `~/.ssh/config` 中一个 Host 段的提取结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfigHost {
    /// Host 行的别名（即用户敲 `ssh <alias>` 用的名字）
    pub alias: String,
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/* ===== 纯函数解析 ===== */

/// 解析 ssh config 文本为主机清单，条目按 alias 首次出现顺序排列。
///
/// 解析器为单遍行级状态机，无需回溯：
/// - `entries` + `index` 实现「同 alias 多段合并、后者覆盖」：
///   已见过的 alias 复用既有条目，后续段的属性直接覆盖写入
/// - `current` 持有当前 Host 段激活的条目下标（通配 pattern 已过滤），
///   段内属性行广播到这些条目
/// - `in_match_block` 在遇到 `Match` 后置位、下个 `Host` 行复位，
///   置位期间的属性行全部忽略
pub fn parse_ssh_config_str(content: &str) -> Vec<SshConfigHost> {
    let mut entries: Vec<SshConfigHost> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut current: Vec<usize> = Vec::new();
    let mut in_match_block = false;

    for raw_line in content.lines() {
        // OpenSSH 注释为整行 `#`（允许缩进）；行内尾注释不是合法语法，不剥
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((keyword, value)) = split_keyword(line) else {
            continue;
        };
        let lower = keyword.to_ascii_lowercase();
        match lower.as_str() {
            "host" => {
                in_match_block = false;
                current.clear();
                for pattern in value.split_whitespace() {
                    // 通配条目（`*` / `?`）跳过：不产出条目，属性也随之无处归属
                    if pattern.contains('*') || pattern.contains('?') {
                        continue;
                    }
                    // 首见的 alias 建新条目；再见（同段或跨段）复用，实现合并覆盖
                    let pos = *index.entry(pattern.to_string()).or_insert_with(|| {
                        entries.push(SshConfigHost {
                            alias: pattern.to_string(),
                            host_name: None,
                            user: None,
                            port: None,
                        });
                        entries.len() - 1
                    });
                    if !current.contains(&pos) {
                        current.push(pos);
                    }
                }
            }
            // Match 块整体跳过，直到下个 Host 行
            "match" => in_match_block = true,
            "hostname" | "user" | "port" if !in_match_block => {
                apply_property(&mut entries, &current, &lower, value);
            }
            _ => {}
        }
    }
    entries
}

/// 把一行拆为 `(关键字, 值)`；注释 / 空行 / 只有关键字的行返回 `None`。
/// 关键字保持原始大小写，由调用方做大小写不敏感匹配。
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let (keyword, value) = line.split_once(char::is_whitespace)?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some((keyword, value))
}

/// 把段内属性行应用到当前段的所有 alias 上。
/// `port` 非数字或超出 u16 时容错跳过，不影响其余属性。
fn apply_property(entries: &mut [SshConfigHost], current: &[usize], keyword: &str, value: &str) {
    for &pos in current {
        let entry = &mut entries[pos];
        match keyword {
            "hostname" => entry.host_name = Some(value.to_string()),
            "user" => entry.user = Some(value.to_string()),
            "port" => {
                if let Ok(port) = value.parse::<u16>() {
                    entry.port = Some(port);
                }
            }
            _ => unreachable!("apply_property 只接受 hostname/user/port"),
        }
    }
}

/* ===== 文件加载与 Include 展开 ===== */

/// 读取当前用户 `~/.ssh/config` 的主机清单。
/// home 目录用 [`crate::config::get_home_dir`]（可被 `CC_SWITCH_TEST_HOME` 覆盖，
/// 与库内其余 home 路径逻辑一致）。
pub fn load_ssh_config_hosts() -> Vec<SshConfigHost> {
    load_ssh_config_hosts_from(&crate::config::get_home_dir())
}

/// 从指定 home 目录加载；读失败返回空清单。独立出来便于用临时目录做测试。
fn load_ssh_config_hosts_from(home: &Path) -> Vec<SshConfigHost> {
    let ssh_dir = home.join(".ssh");
    let Ok(content) = std::fs::read_to_string(ssh_dir.join("config")) else {
        return Vec::new();
    };
    parse_ssh_config_str(&expand_includes_once(&content, &ssh_dir, home))
}

/// 把 `Include` 行原位替换为被包含文件的内容（只展开一层：
/// 被包含文件里的 `Include` 不再递归，作为未知关键字被解析器忽略）。
/// 被包含文件读失败（不存在 / 无权限）静默跳过该条。
fn expand_includes_once(content: &str, ssh_dir: &Path, home: &Path) -> String {
    let mut out = String::with_capacity(content.len());
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if let Some((keyword, value)) = split_keyword(line) {
            if keyword.eq_ignore_ascii_case("include") {
                for path in value.split_whitespace() {
                    let resolved = resolve_include_path(path, ssh_dir, home);
                    if let Ok(text) = std::fs::read_to_string(&resolved) {
                        out.push_str(&text);
                        if !text.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
                continue;
            }
        }
        out.push_str(raw_line);
        out.push('\n');
    }
    out
}

/// 解析 Include 值为实际路径：绝对路径原样；`~/` 前缀展开到 home；其余相对 `~/.ssh/`。
fn resolve_include_path(raw: &str, ssh_dir: &Path, home: &Path) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        ssh_dir.join(raw)
    }
}

/* ===== 测试 ===== */

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言辅助：按 alias 找到条目（不存在则 panic），再校验三属性
    fn find<'a>(hosts: &'a [SshConfigHost], alias: &str) -> &'a SshConfigHost {
        hosts
            .iter()
            .find(|h| h.alias == alias)
            .unwrap_or_else(|| panic!("alias {alias} 不存在: {hosts:?}"))
    }

    // ---- 场景 1：标准段 ----
    #[test]
    fn parses_standard_block() {
        let hosts =
            parse_ssh_config_str("Host web\n  HostName example.com\n  User deploy\n  Port 2222\n");
        assert_eq!(hosts.len(), 1);
        let h = find(&hosts, "web");
        assert_eq!(h.host_name.as_deref(), Some("example.com"));
        assert_eq!(h.user.as_deref(), Some("deploy"));
        assert_eq!(h.port, Some(2222));
    }

    // ---- 场景 2：多 Host 一行（Host a b）各自成条目、共享段内属性 ----
    #[test]
    fn expands_multi_alias_line() {
        let hosts = parse_ssh_config_str("Host a b\n  HostName example.com\n  Port 2200\n");
        assert_eq!(hosts.len(), 2);
        for alias in ["a", "b"] {
            let h = find(&hosts, alias);
            assert_eq!(h.host_name.as_deref(), Some("example.com"));
            assert_eq!(h.port, Some(2200));
        }
    }

    // ---- 场景 3：通配条目跳过，其段内属性不归属任何主机 ----
    #[test]
    fn skips_wildcard_entries() {
        let hosts =
            parse_ssh_config_str("Host *\n  User global-user\nHost web\n  HostName example.com\n");
        assert_eq!(hosts.len(), 1);
        let h = find(&hosts, "web");
        assert_eq!(h.host_name.as_deref(), Some("example.com"));
        assert_eq!(h.user, None, "Host * 段的属性不应泄漏到 web");
    }

    #[test]
    fn skips_question_mark_wildcard() {
        let hosts = parse_ssh_config_str("Host web?\n  HostName example.com\n");
        assert!(hosts.is_empty());
    }

    // ---- 场景 4：Match 块跳过到下个 Host ----
    #[test]
    fn skips_match_block() {
        let hosts = parse_ssh_config_str(
            "Host web\n  HostName example.com\nMatch originalhost example.com\n  User matched-user\n  Port 2222\nHost ci\n  HostName ci.internal\n",
        );
        assert_eq!(hosts.len(), 2);
        let web = find(&hosts, "web");
        assert_eq!(web.user, None, "Match 块内的属性不应归属 web");
        assert_eq!(web.port, None);
        let ci = find(&hosts, "ci");
        assert_eq!(ci.host_name.as_deref(), Some("ci.internal"));
        assert_eq!(ci.user, None);
    }

    // ---- 场景 5：Tab 缩进合法 ----
    #[test]
    fn accepts_tab_indentation() {
        let hosts = parse_ssh_config_str("Host web\n\tHostName example.com\n\tPort 2222\n");
        let h = find(&hosts, "web");
        assert_eq!(h.host_name.as_deref(), Some("example.com"));
        assert_eq!(h.port, Some(2222));
    }

    // ---- 场景 6：关键字大小写不敏感（含混合大小写） ----
    #[test]
    fn keywords_case_insensitive() {
        let hosts = parse_ssh_config_str(
            "host web\nhostname example.com\nuser deploy\nport 2222\nHOST extra\nHoStNaMe e2.com\n",
        );
        assert_eq!(hosts.len(), 2);
        let web = find(&hosts, "web");
        assert_eq!(web.host_name.as_deref(), Some("example.com"));
        assert_eq!(web.user.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(find(&hosts, "extra").host_name.as_deref(), Some("e2.com"));
    }

    // ---- 场景 7：Port 非数字容错跳过 ----
    #[test]
    fn non_numeric_port_is_tolerated() {
        let hosts = parse_ssh_config_str(
            "Host web\n  Port abc\n  HostName example.com\nHost big\n  Port 99999\n",
        );
        assert_eq!(find(&hosts, "web").port, None);
        assert_eq!(
            find(&hosts, "web").host_name.as_deref(),
            Some("example.com")
        );
        assert_eq!(find(&hosts, "big").port, None, "u16 溢出同样容错");
    }

    // ---- 场景 8：同 alias 多段合并、后者覆盖、未提及字段保留 ----
    #[test]
    fn merges_same_alias_later_wins() {
        let hosts = parse_ssh_config_str(
            "Host web\n  HostName old.example.com\n  Port 22\nHost web\n  HostName new.example.com\n  User deploy\n",
        );
        assert_eq!(hosts.len(), 1);
        let h = find(&hosts, "web");
        assert_eq!(h.host_name.as_deref(), Some("new.example.com"), "后者覆盖");
        assert_eq!(h.port, Some(22), "后段未提及则保留前段值");
        assert_eq!(h.user.as_deref(), Some("deploy"));
    }

    // ---- 边界：注释 / 空行 / 只有关键字的行 ----
    #[test]
    fn ignores_comments_and_blank_lines() {
        assert!(parse_ssh_config_str("").is_empty());
        assert!(parse_ssh_config_str("# 全局注释\n\n   \n  # 缩进注释\n").is_empty());
        let hosts = parse_ssh_config_str("Host\nHostName example.com\n");
        assert!(hosts.is_empty(), "无值的 Host 行整段不归属");
    }

    // ---- Include 展开（临时目录 fixture） ----
    #[test]
    fn load_expands_relative_include_once() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let ssh_dir = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("创建 .ssh 失败");
        std::fs::write(
            ssh_dir.join("extra.conf"),
            "Host ci\n  HostName ci.internal\n  Port 2200\n",
        )
        .expect("写 extra.conf 失败");
        std::fs::write(
            ssh_dir.join("config"),
            "Include extra.conf\nHost web\n  HostName example.com\nInclude missing.conf\n",
        )
        .expect("写 config 失败");

        let hosts = load_ssh_config_hosts_from(dir.path());
        assert_eq!(hosts.len(), 2, "include 的 ci 与主文件 web 都要出现");
        let ci = find(&hosts, "ci");
        assert_eq!(ci.host_name.as_deref(), Some("ci.internal"));
        assert_eq!(ci.port, Some(2200));
        assert_eq!(
            find(&hosts, "web").host_name.as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn load_expands_absolute_and_home_prefix_include() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let ssh_dir = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("创建 .ssh 失败");
        // 绝对路径 include 与 ~/ 前缀 include 各自指向临时目录内文件
        let abs = dir.path().join("absolute.conf");
        std::fs::write(&abs, "Host abs-host\n  HostName abs.com\n").expect("写绝对 include 失败");
        // `~/home.conf` 展开到 home 根目录而非 ~/.ssh/
        std::fs::write(
            dir.path().join("home.conf"),
            "Host home-host\n  HostName home.com\n",
        )
        .expect("写 ~/ include 失败");
        std::fs::write(
            ssh_dir.join("config"),
            format!("Include {}\nInclude ~/home.conf\n", abs.display()),
        )
        .expect("写 config 失败");

        let hosts = load_ssh_config_hosts_from(dir.path());
        assert_eq!(hosts.len(), 2);
        assert_eq!(
            find(&hosts, "abs-host").host_name.as_deref(),
            Some("abs.com")
        );
        assert_eq!(
            find(&hosts, "home-host").host_name.as_deref(),
            Some("home.com")
        );
    }

    #[test]
    fn load_include_does_not_recurse() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let ssh_dir = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("创建 .ssh 失败");
        // 二层 include：extra.conf 里再 include deep.conf，只展开一层则 deep 不出现
        std::fs::write(
            ssh_dir.join("extra.conf"),
            "Host ci\n  HostName ci.internal\nInclude deep.conf\n",
        )
        .expect("写 extra.conf 失败");
        std::fs::write(
            ssh_dir.join("deep.conf"),
            "Host deep\n  HostName deep.com\n",
        )
        .expect("写 deep.conf 失败");
        std::fs::write(ssh_dir.join("config"), "Include extra.conf\n").expect("写 config 失败");

        let hosts = load_ssh_config_hosts_from(dir.path());
        assert_eq!(hosts.len(), 1, "只展开一层：deep 不应出现");
        assert!(hosts[0].alias == "ci");
    }

    #[test]
    fn load_missing_config_returns_empty() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        assert!(load_ssh_config_hosts_from(dir.path()).is_empty());
    }
}
