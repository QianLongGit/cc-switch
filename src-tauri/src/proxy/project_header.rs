//! `X-CC-Project` header 的编码与行操作纯函数。
//!
//! 这是写入端（settings_local_writer）与读取端（handler 路由归因）共同遵守的
//! 编码契约（spec §3.2 编码与匹配规范），规则集中在本模块定死，杜绝两端实现漂移：
//!
//! - **percent-encode**：UTF-8 字节级编码；保留字符集 `[A-Za-z0-9/._~-]`，其余
//!   字节输出 `%XX`（大写十六进制）。换行、冒号、空格等 header 语法敏感字节
//!   必然被转义，编码结果恒为单行。
//! - **percent-decode**：遇坏 `%` 转义（裸 `%`、两位非十六进制）或解码字节拼不
//!   成合法 UTF-8 → 整体视为无效（`None`），调用方按「无项目标识」回退默认
//!   策略，绝不 panic。
//! - **X-CC-Project 行规则**（`ANTHROPIC_CUSTOM_HEADERS` 多行值的解析与写入）：
//!   值按 `\n` 拆行、逐行 trim、按首个 `:` 分割名/值，名字段 ASCII 大小写不
//!   敏感**全等** `x-cc-project` 的行为目标行；写入时替换全部目标行为单行、
//!   行间以 `\n` 分隔**无尾随换行**、非目标行逐字节保留（含 `\r` 与行内空白）。

// ===================================================================
// percent 编解码
// ===================================================================

/// 十六进制输出表（大写），编码转义专用。
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

/// 保留字符集 `[A-Za-z0-9/._~-]`：命中原样输出，其余字节转义。
#[inline]
fn is_unreserved_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-')
}

/// 单个 ASCII 字符的十六进制值；非十六进制返回 `None`。
#[inline]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 把项目路径编码为可安全放入 header 值的单行 ASCII 串。
///
/// UTF-8 字节级逐字节处理：保留字符原样，其余输出 `%XX`（大写十六进制）。
pub(crate) fn percent_encode_project_path(path: &str) -> String {
    // 最坏情况每字节膨胀 3 倍，预分配避免反复扩容
    let mut out = String::with_capacity(path.len() * 3);
    for &b in path.as_bytes() {
        if is_unreserved_byte(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(UPPER_HEX[(b >> 4) as usize] as char);
            out.push(UPPER_HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

/// 还原 [`percent_encode_project_path`] 的编码结果。
///
/// 非法输入（裸 `%`、`%XX` 两位非十六进制、解码字节拼不成合法 UTF-8）一律返回
/// `None`——该 header 值视为无效，按无项目标识处理，不报错不 panic。
pub(crate) fn percent_decode_project_path(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // % 后必须紧跟两位十六进制，缺位或非 hex 即整体无效
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // 字节序列必须整体是合法 UTF-8（中文多字节序列截断会在此失败）
    String::from_utf8(out).ok()
}

// ===================================================================
// X-CC-Project 行操作
// ===================================================================

/// 目标 header 名（ASCII 大小写不敏感比较的小写基准）。
const TARGET_NAME: &str = "x-cc-project";

/// 单行是否为 `X-CC-Project` 目标行。
///
/// 行级判定规则（解析与写入共用同一谓词，杜绝两套判定漂移）：
/// trim 后按首个 `:` 分割，名字段 ASCII 大小写不敏感**全等**（非前缀匹配）。
fn is_x_cc_project_line(line: &str) -> bool {
    line.trim()
        .split_once(':')
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(TARGET_NAME))
}

/// 从 `ANTHROPIC_CUSTOM_HEADERS` 多行值提取首个目标行的值（trim 后）。
///
/// 无目标行 → `None`。返回的是**编码态**值，调用方还需
/// [`percent_decode_project_path`] 还原真实路径。
pub(crate) fn extract_x_cc_project(headers_value: &str) -> Option<String> {
    // 与 replace 共用 is_x_cc_project_line 判定，杜绝两套行规则漂移（兑现上文注释）
    headers_value
        .split('\n')
        .find(|l| is_x_cc_project_line(l))
        .and_then(|l| l.trim().split_once(':').map(|(_, v)| v.trim().to_string()))
}

/// 设置 / 删除 / 合并 `X-CC-Project` 行。
///
/// - `Some(e)`：全部目标行替换为单行 `X-CC-Project: {e}`（追加在末尾）；
/// - `None`：删除全部目标行。
///
/// 非目标行逐字节保留（含 `\r` 与行内空白）。输出为规范形态：拆行时丢弃末尾
/// 单个空元素（`"a\nb\n"` 与 `"a\nb"` 视为同一行集合），各行以 `\n` join、
/// **无尾随换行**（单行无尾 `\n`，多行末行无 `\n`）。末尾空行在该形态下不可
/// 表达，序列化前一并剥除（中间空行不受影响）；行集合为空时返回空串。
/// 规范化保证写入操作幂等；解析端兼容旧格式带尾 `\n` 的既有文件。
pub(crate) fn replace_x_cc_project(headers_value: &str, encoded: Option<&str>) -> String {
    let mut lines: Vec<&str> = headers_value.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    // 收集非目标行（原样字节，不 trim），Some 时目标行合并为单行追加末尾
    let mut kept: Vec<String> = lines
        .into_iter()
        .filter(|l| !is_x_cc_project_line(l))
        .map(str::to_string)
        .collect();
    // 末尾空行序列化必产生尾随 \n，与「末行无 \n」规范矛盾，追加目标行前
    // 一并剥除（中间空行不受影响；set / remove 两分支对末尾空行处理一致）
    while kept.last().is_some_and(String::is_empty) {
        kept.pop();
    }
    if let Some(e) = encoded {
        kept.push(format!("X-CC-Project: {e}"));
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===================================================================
    // 固定用例
    // ===================================================================

    #[test]
    fn encode_keeps_unreserved_and_uppercases_hex() {
        // 保留集命中原样输出，空格与中文按 UTF-8 字节转义为 %XX（大写）
        assert_eq!(
            percent_encode_project_path("/a b中文"),
            "/a%20b%E4%B8%AD%E6%96%87"
        );
        assert_eq!(percent_encode_project_path("A9z/._~-"), "A9z/._~-");
    }

    #[test]
    fn decode_roundtrip_fixed() {
        let original = "/Users/dev/项目 A";
        assert_eq!(
            percent_decode_project_path(&percent_encode_project_path(original)),
            Some(original.to_string())
        );
    }

    #[test]
    fn decode_rejects_bad_sequences() {
        // 裸 % / 截断转义 / 非 hex / 多字节序列截断 / 非 UTF-8 字节 → 全部 None
        for bad in ["%", "A%2", "%zz", "%E4%B8", "%FF%FE"] {
            assert_eq!(percent_decode_project_path(bad), None, "case: {bad:?}");
        }
    }

    #[test]
    fn extract_matches_case_insensitive_first_colon() {
        // 名字段 ASCII 大小写不敏感；首个 ':' 分割，值中冒号原样保留
        assert_eq!(
            extract_x_cc_project("X-API-Key: k\nx-cc-project: %2Fp"),
            Some("%2Fp".to_string())
        );
        assert_eq!(
            extract_x_cc_project("X-CC-Project: a: b"),
            Some("a: b".to_string())
        );
        // 行首尾空白与名字段尾随空白均被 trim
        assert_eq!(
            extract_x_cc_project("  X-CC-PROJECT \t: v1 \n"),
            Some("v1".to_string())
        );
    }

    #[test]
    fn extract_none_when_absent() {
        assert_eq!(extract_x_cc_project(""), None);
        assert_eq!(extract_x_cc_project("X-Api-Key: k"), None);
        // 全等匹配：前缀相似名与无冒号行不得命中
        assert_eq!(extract_x_cc_project("x-cc-project2: v"), None);
        assert_eq!(extract_x_cc_project("x-cc-projec: v"), None);
        assert_eq!(extract_x_cc_project("x-cc-project"), None);
    }

    #[test]
    fn replace_merges_multiple_target_lines() {
        let input = "x-cc-project: a\nX-Api-Key: k\nX-CC-PROJECT: b";
        let out = replace_x_cc_project(input, Some("%2Fp"));
        // 两行目标行合并为单行（追加在非目标行之后），k 行逐字节保留；
        // 行间恰一个 \n、末行无尾随 \n
        assert_eq!(out, "X-Api-Key: k\nX-CC-Project: %2Fp");
        assert_eq!(
            out.split('\n').filter(|l| is_x_cc_project_line(l)).count(),
            1
        );
    }

    #[test]
    fn replace_none_removes_all_targets() {
        let input = "x-cc-project: a\nX-Api-Key: k\nX-CC-PROJECT: b";
        let out = replace_x_cc_project(input, None);
        assert_eq!(out, "X-Api-Key: k");
        assert_eq!(extract_x_cc_project(&out), None);
    }

    #[test]
    fn replace_appends_when_absent() {
        let out = replace_x_cc_project("X-Api-Key: k", Some("%2FUsers"));
        assert_eq!(out, "X-Api-Key: k\nX-CC-Project: %2FUsers");
    }

    #[test]
    fn replace_keeps_cr_bytes() {
        // CRLF 源：split('\n') 后行内容含 \r，非目标行逐字节保留（\r 不丢）
        let input = "X-Api-Key: k\r\nx-cc-project: old\r\n";
        let out = replace_x_cc_project(input, Some("e"));
        assert_eq!(out, "X-Api-Key: k\r\nX-CC-Project: e");
    }

    #[test]
    fn replace_empty_value_forms() {
        // 空串行集合为空：None → 空串；Some → 仅目标行（单行无尾随 \n）
        assert_eq!(replace_x_cc_project("", None), "");
        assert_eq!(replace_x_cc_project("", Some("e")), "X-CC-Project: e");
        // 孤立空行是合法行，须保留；"无尾随换行"规范下单空行序列化为空串
        // （拆行丢末尾空元素语义下两者行集合等价，幂等性不受影响）
        assert_eq!(replace_x_cc_project("\nx-cc-project: a", None), "");
        // 输入带尾 \n 的旧形态收敛为无尾 \n（规范化幂等）
        assert_eq!(replace_x_cc_project("a\n", None), "a");
    }

    #[test]
    fn replace_no_trailing_newline_fixed() {
        // 用户实测场景钉死：单行输出无尾随 \n
        let out = replace_x_cc_project(
            "X-CC-Project: %2FUsers%2Fdev%2FAi_Daily\n",
            Some("%2FUsers%2Fdev%2FAi_Daily"),
        );
        assert_eq!(out, "X-CC-Project: %2FUsers%2Fdev%2FAi_Daily");
        // 两行输出形如 "a\nX-CC-Project: v"：行间恰一个 \n、末行无 \n
        assert_eq!(
            replace_x_cc_project("a\nx-cc-project: old\n", Some("v")),
            "a\nX-CC-Project: v"
        );
        // 输入已是无尾 \n 的规范形态，再写同值输出不变（幂等）
        assert_eq!(
            replace_x_cc_project("a\nX-CC-Project: v", Some("v")),
            "a\nX-CC-Project: v"
        );
        // 目标行间的空行过滤后暴露在末尾，同样剥除（否则末行必为空行）
        assert_eq!(
            replace_x_cc_project("a\n\nx-cc-project: old", Some("v")),
            "a\nX-CC-Project: v"
        );
    }

    #[test]
    fn extract_still_parses_legacy_trailing_newline() {
        // 兼容性：旧写入端落盘的"每行尾随 \n"形态仍可正常解析（拆行丢弃
        // 末尾空元素保证），写入格式切换对既有文件零感知
        assert_eq!(
            extract_x_cc_project("X-CC-Project: %2Fp\n"),
            Some("%2Fp".to_string())
        );
        assert_eq!(
            extract_x_cc_project("X-Api-Key: k\nX-CC-Project: %2Fp\n"),
            Some("%2Fp".to_string())
        );
    }

    // ===================================================================
    // 属性测试（spec §9 settings_local_writer 类别的前半：
    // 编码往返 / 非法回退 / 多行合并）——手写确定性 LCG，零新依赖
    // ===================================================================

    /// xorshift64*：状态非零，输出乘黄金比率常数打散低位。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// 均匀取 [0, bound)。
        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// 宽表随机串：ASCII 字母数字 + `/._~-%:\n\r\t ` + 中文池。
    /// 表内含 `%`、`:`、`\n`，天然覆盖非法转义与目标行变体。
    fn rand_string(rng: &mut Lcg, len: usize) -> String {
        const CHARSET: &[&str] = &[
            "a", "B", "z", "0", "9", "/", ".", "_", "~", "-", "%", ":", "\n", "\r", "\t", " ",
            "2", "4", "8", "E", "F", "f", "项目", "开", "发", "文", "件",
        ];
        (0..len)
            .map(|_| CHARSET[rng.below(CHARSET.len() as u64) as usize].to_string())
            .collect()
    }

    /// 单行随机内容：不含 `\n`（行级结构可控），可含 `:`、`%`、`\r`、空白、中文。
    /// 拒绝采样：碰巧拼成目标行的重新生成，保证普通行与目标行职责分离。
    fn rand_line(rng: &mut Lcg) -> String {
        const LINE_CHARSET: &[&str] = &[
            "a", "B", "z", "0", "9", "/", ".", "_", "~", "-", ":", "\r", "\t", " ", "%", "2",
            "F", "x", "X", "c", "k", "项目",
        ];
        loop {
            let line: String = (0..rng.below(24) + 1)
                .map(|_| LINE_CHARSET[rng.below(LINE_CHARSET.len() as u64) as usize].to_string())
                .collect();
            if !is_x_cc_project_line(&line) {
                return line;
            }
        }
    }

    /// 目标行名字混排大小写变体（验证 ASCII 大小写不敏感匹配）。
    fn mix_case(rng: &mut Lcg, name: &str) -> String {
        name.chars()
            .map(|c| {
                if rng.below(2) == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }

    /// 随机构造 `ANTHROPIC_CUSTOM_HEADERS` 多行值：
    /// 随机普通行 + 随机数量（0..3）目标行，行序随机，随机决定尾 `\n`。
    fn rand_headers_value(rng: &mut Lcg) -> String {
        let mut lines: Vec<String> = Vec::new();
        for _ in 0..rng.below(3) {
            let name = mix_case(rng, "x-cc-project");
            let len = rng.below(8) as usize + 1;
            let value = rand_string(rng, len);
            lines.push(format!("{name}: {value}"));
        }
        for _ in 0..rng.below(4) {
            lines.push(rand_line(rng));
        }
        // Fisher–Yates 洗牌打散目标行位置
        for i in (1..lines.len()).rev() {
            lines.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let mut v = lines.join("\n");
        if rng.below(2) == 1 {
            v.push('\n');
        }
        v
    }

    /// 断言辅助：串是否仅由保留字符与 %XX（大写 hex）构成。
    fn is_canonical_percent(s: &str) -> bool {
        let b = s.as_bytes();
        let is_reserved =
            |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'.' | b'_' | b'~' | b'-');
        let is_upper_hex = |c: u8| matches!(c, b'0'..=b'9' | b'A'..=b'F');
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                if i + 3 > b.len() || !is_upper_hex(b[i + 1]) || !is_upper_hex(b[i + 2]) {
                    return false;
                }
                i += 3;
            } else if is_reserved(b[i]) {
                i += 1;
            } else {
                return false;
            }
        }
        true
    }

    #[test]
    fn prop_encode_decode_roundtrip() {
        let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
        for round in 0..500 {
            let len = rng.below(48) as usize + 1;
            let s = rand_string(&mut rng, len);
            let encoded = percent_encode_project_path(&s);
            // 编码输出仅含保留字符与 %XX（大写 hex）
            assert!(is_canonical_percent(&encoded), "round {round}: {encoded:?}");
            // 编码往返恒等
            assert_eq!(
                percent_decode_project_path(&encoded),
                Some(s.clone()),
                "round {round}: {s:?}"
            );
        }
    }

    #[test]
    fn prop_replace_preserves_other_lines() {
        let mut rng = Lcg(0x0BAD_C0DE_DEAD_10CC);
        for round in 0..500 {
            let v = rand_headers_value(&mut rng);
            let e1 = percent_encode_project_path(&rand_string(&mut rng, 12));
            let e2 = percent_encode_project_path(&rand_string(&mut rng, 8));

            let out = replace_x_cc_project(&v, Some(&e2));

            // (a) 目标行恰 1 行且值 == e2
            assert_eq!(
                extract_x_cc_project(&out),
                Some(e2.clone()),
                "round {round}: {v:?}"
            );
            assert_eq!(
                out.split('\n').filter(|l| is_x_cc_project_line(l)).count(),
                1,
                "round {round}: {out:?}"
            );

            // (b) 除目标行外其余行逐字节与输入一致（按行对比）。基线与实现
            //     序列化步骤严格对齐：解析拆行（丢末尾单个空元素）→ 过滤
            //     目标行 → 剥末尾空行（目标行间/后的空行过滤后暴露在末尾，
            //     在"无尾随换行"形态下不可表达）。输出端无尾随换行，直接
            //     split 即得行集合（不走 strip 通道，防假绿）
            let mut in_rows: Vec<&str> = v.split('\n').collect();
            if in_rows.last() == Some(&"") {
                in_rows.pop();
            }
            let mut in_others: Vec<&str> = in_rows
                .into_iter()
                .filter(|l| !is_x_cc_project_line(l))
                .collect();
            while in_others.last() == Some(&"") {
                in_others.pop();
            }
            let out_others: Vec<&str> = out
                .split('\n')
                .filter(|l| !is_x_cc_project_line(l))
                .collect();
            assert_eq!(out_others, in_others, "round {round}: {v:?}");

            // (b') 写入形态：整串不得带尾随换行（直接断言，钉死规范）
            assert!(!out.ends_with('\n'), "round {round}: {out:?}");

            // (c) 连续写入收敛：先写 e1 再写 e2 == 直接写 e2
            assert_eq!(
                replace_x_cc_project(&replace_x_cc_project(&v, Some(&e1)), Some(&e2)),
                out,
                "round {round}"
            );
        }
    }

    #[test]
    fn prop_replace_none_idempotent() {
        let mut rng = Lcg(0xF00B_A122_5A17_ED5B);
        for round in 0..500 {
            let v = rand_headers_value(&mut rng);
            let once = replace_x_cc_project(&v, None);
            // 删除幂等，删干净（无残留目标行），且输出恒无尾随换行
            assert!(!once.ends_with('\n'), "round {round}: {once:?}");
            assert_eq!(
                replace_x_cc_project(&once, None),
                once,
                "round {round}: {v:?}"
            );
            assert_eq!(extract_x_cc_project(&once), None, "round {round}: {v:?}");
        }
    }
}
