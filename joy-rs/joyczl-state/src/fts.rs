//! 把人类的输入变成 FTS5 能安全吃下的查询。

/// 用户说什么都可能：引号、星号、冒号、半个括号。直接拼进 `MATCH` 会抛
/// SQLite 语法错误 —— 而检索门是**失败开放**的，一次报错就会退化成「每轮都检索」。
/// 所以这里把输入切成词、逐个加引号、用 OR 连起来：语法永远合法，语义也够用
/// （命中任意一个词即可，比整串精确匹配更符合「想起相关的」）。
///
/// 返回 `None` 表示没有任何可用词（全是标点 / 空串），调用方应当跳过检索。
///
/// 分词器是 trigram（见 migrations/0002），按连续三字符切片建索引，中文天然可用。
/// 短于 3 个字符的词切不出三元组——那部分交给 [`like_pattern`] 兜底。
pub fn to_match_expr(input: &str) -> Option<String> {
    // 按「非字母数字」切，而不是只削掉首尾：中间的冒号、斜杠一样会破坏
    // FTS5 语法（`a:b` 不是一个合法的裸词，虽然加引号后能过，但语义也不对）。
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// FTS 没命中时的兜底：普通 LIKE 子串扫描。
///
/// 覆盖两类情况：查询词短于 3 个字符（中文两字词很常见，切不出三元组），
/// 以及 trigram 对极短英文词同样无能为力。个人记忆库就几千行，
/// 一次全表 LIKE 是微秒级 —— 不值得为此引入外部分词器。
///
/// 通配符 `%` `_` 会被剔掉，否则用户能用它把 LIKE 变成全表扫描（虽然也
/// 只是慢，但没必要留这个口子）。
pub fn like_pattern(input: &str) -> Option<String> {
    // 只剔掉 LIKE 自己的两个通配符，其余原样保留 ——
    // 包括空格，因为「alex morning」作为连续子串去匹配才是用户想要的。
    let needle: String = input.chars().filter(|c| !matches!(c, '%' | '_')).collect();
    let needle = needle.trim();
    // 至少得有一个字母数字（中文也算），否则这次扫描注定是空转。
    if !needle.chars().any(char::is_alphanumeric) {
        None
    } else {
        Some(format!("%{needle}%"))
    }
}

#[cfg(test)]
mod fts_tests {
    use super::{like_pattern, to_match_expr};

    #[test]
    fn quotes_each_token() {
        assert_eq!(
            to_match_expr("alex morning meetings").as_deref(),
            Some("\"alex\" OR \"morning\" OR \"meetings\"")
        );
    }

    #[test]
    fn strips_punctuation_that_would_break_the_grammar() {
        // 每个词都被加引号，所以危险字符必须一个都不剩地先被切掉。
        for bad in [
            "what's on?",
            "a:b",
            "\"quoted\"",
            "star*",
            "(unclosed",
            "--",
            "a/b",
        ] {
            if let Some(expr) = to_match_expr(bad) {
                // 逐个检查被加引号的词，而不是整串（整串里本来就有引号和 OR）。
                for token in expr.split(" OR ") {
                    let inside = token.trim_matches('"');
                    for dangerous in ['(', ')', ':', '*', '"', '/', '-'] {
                        assert!(
                            !inside.contains(dangerous),
                            "{bad:?} 生成的 {expr} 里 {token} 仍含 {dangerous}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn splits_on_inner_punctuation_too() {
        // 只削首尾的话 "a:b" 会整块留下来。
        assert_eq!(to_match_expr("a:b").as_deref(), Some("\"a\" OR \"b\""));
    }

    #[test]
    fn punctuation_only_is_none() {
        assert_eq!(to_match_expr("???  ...  "), None);
        assert_eq!(to_match_expr(""), None);
        assert_eq!(like_pattern("???"), None);
    }

    #[test]
    fn like_pattern_keeps_chinese_and_drops_wildcards() {
        assert_eq!(like_pattern("早上").as_deref(), Some("%早上%"));
        // 用户塞进来的 % 不能变成通配符。
        assert_eq!(like_pattern("a%b").as_deref(), Some("%ab%"));
    }
}
