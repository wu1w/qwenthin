//! Strip simple-greeting restatement from the visible reply.
//!
//! Qwen3.8 often echoes "你好" as its own first line. Do not lecture the
//! model; drop that line in the harness when the user turn is a short greeting.

pub fn is_simple_greeting(user: &str) -> bool {
    let t = user.trim();
    if t.is_empty() || t.chars().count() > 24 {
        return false;
    }
    let folded: String = t
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let stripped: String = folded
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '!' | '?' | '.' | '！' | '？' | '。' | '，' | ',' | '~' | '～'
            )
        })
        .collect();
    matches!(
        stripped.as_str(),
        "hi" | "hii"
            | "hiii"
            | "hello"
            | "hey"
            | "yo"
            | "sup"
            | "你好"
            | "您好"
            | "嗨"
            | "哈喽"
            | "哈罗"
            | "在吗"
            | "在么"
            | "在嘛"
            | "早上好"
            | "晚上好"
            | "中午好"
            | "下午好"
            | "谢谢"
            | "thanks"
            | "thankyou"
            | "ok"
            | "okay"
            | "好的"
            | "嗯"
            | "嗯嗯"
    )
}

/// Drop a leading restatement of `user` from `reply`. If nothing looks like an
/// echo, return `reply` unchanged (including a friendly "你好！" answer).
pub fn strip_greeting_echo(user: &str, reply: &str) -> String {
    if !is_simple_greeting(user) {
        return reply.to_string();
    }
    let u = user.trim();
    let mut lines: Vec<&str> = reply.lines().collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    let Some(first) = lines.first() else {
        return reply.to_string();
    };
    let f = first.trim();
    if !is_echo_line(u, f) {
        return reply.to_string();
    }
    lines.remove(0);
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    let out = lines.join("\n").trim().to_string();
    if out.is_empty() {
        reply.to_string()
    } else {
        out
    }
}

fn is_echo_line(user: &str, line: &str) -> bool {
    let u = user.trim();
    let f = line.trim();
    if f.eq_ignore_ascii_case(u) {
        return true;
    }
    let f_stripped: String = f
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '!' | '?' | '.' | '！' | '？' | '。' | '"' | '“' | '”' | '\'' | '「' | '」'
            )
        })
        .collect();
    if f_stripped.eq_ignore_ascii_case(u) {
        return true;
    }
    let lower = f.to_lowercase();
    lower.starts_with("你问")
        || lower.starts_with("你刚才")
        || lower.starts_with("你说的是")
        || lower.starts_with("你的问题")
        || lower.starts_with("your question")
        || lower.starts_with("you said")
        || lower.starts_with("you asked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_detects_short_hi() {
        assert!(is_simple_greeting("你好"));
        assert!(is_simple_greeting("你好！"));
        assert!(is_simple_greeting("hi"));
        assert!(is_simple_greeting("Hello"));
        assert!(!is_simple_greeting("你好，帮我看一下这段 Rust"));
        assert!(!is_simple_greeting("read Cargo.toml"));
    }

    #[test]
    fn strips_standalone_echo_line() {
        let out = strip_greeting_echo("你好", "你好\n\n有什么我可以帮你的吗？");
        assert_eq!(out, "有什么我可以帮你的吗？");
        let out = strip_greeting_echo("你好", "你问的是「你好」吗？\n嗨，我在。");
        assert_eq!(out, "嗨，我在。");
    }

    #[test]
    fn keeps_inline_hello_answer() {
        let src = "你好！今天想做点什么？";
        assert_eq!(strip_greeting_echo("你好", src), src);
    }

    #[test]
    fn ignores_non_greetings() {
        let src = "你问的是怎么修这个 underflow。\n改返回类型。";
        assert_eq!(strip_greeting_echo("修一下 idx", src), src);
    }
}
