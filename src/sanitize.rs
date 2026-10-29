use std::sync::OnceLock;

use crate::slice::{DateBlock, MonthSection};

/// `[表示テキスト](URL "title")` → `表示テキスト`
///
/// URL 部分は文字数の 83% を占める（実測: ja 2007-01 で 62,251 字 → 10,077 字）。
/// 入れ子括弧 `\(1st_generation\)` と括弧を含む title も 1 パスで処理する。
fn link_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\[([^\]]*)\]\(((?:[^()\\]|\\.|\([^()]*\))*)\)")
            .expect("link pattern compiles")
    })
}

/// 行 1 本をサニタイズする。
///
/// - Markdown リンクを表示テキストに置換する
/// - 行頭の箇条書き記号（`* ` / `- `）を落とす
/// - 行末の空白を落とす
///
/// 日付行（例 `[1月1日](...)`）は `1月1日` という裸のテキストとして残る。
/// これは意図的で、抽出時に LLM が「この行は日付見出し」と判別できるようにする。
pub fn sanitize_line(line: &str) -> String {
    let replaced = link_re().replace_all(line, "$1");
    let trimmed_start = replaced.trim_start();
    let no_bullet = trimmed_start
        .strip_prefix("* ")
        .or_else(|| trimmed_start.strip_prefix("- "))
        .unwrap_or(trimmed_start);
    no_bullet.trim_end().to_owned()
}

/// 月セクション全体をサニタイズする。空行になった行は捨てる。
pub fn sanitize_section(section: &MonthSection) -> MonthSection {
    let mut blocks = Vec::with_capacity(section.blocks.len());
    for block in &section.blocks {
        let mut lines = Vec::with_capacity(block.lines.len());
        for line in &block.lines {
            let text = sanitize_line(line);
            if !text.is_empty() {
                lines.push(text);
            }
        }
        if !lines.is_empty() {
            blocks.push(DateBlock {
                day: block.day,
                lines,
            });
        }
    }
    MonthSection {
        heading_path: section.heading_path.clone(),
        blocks,
    }
}

/// サニタイズ済みセクションをプロンプト用のプレーンテキストにする。
///
/// 日付ブロックの区切りが失われないよう、ブロック間に空行を 1 つ置く。
/// 連続する空行は 1 つに圧縮する（トークンの無駄を避けるため）。
pub fn section_text(section: &MonthSection) -> String {
    let mut out = String::new();
    for (i, block) in section.blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for line in &block.lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    collapse_blank_lines(&out)
}

/// プロンプト用に、各日付ブロックの先頭へ ISO 日付を**機械的に**付ける。
///
/// 本文の日付行は `1月1日` 形式で年を持たない。これをモデルに補わせると
/// 実測で 115 件すべてが `date: null` になった。年は Rust 側が知っているので、
/// ここで確定させて渡し、モデルには「先頭の日付をそのまま写せ」と指示する。
pub fn render_for_prompt(
    section: &MonthSection,
    year: u16,
    month: u8,
    lang: crate::unit::Lang,
) -> String {
    let sanitized = sanitize_section(section);
    let unknown = match lang {
        crate::unit::Lang::Ja => "(日付不明)",
        crate::unit::Lang::En => "(unknown date)",
    };
    let mut out = String::new();
    for (i, block) in sanitized.blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match block.day {
            Some(day) => out.push_str(&format!("{year:04}-{month:02}-{day:02}\n")),
            None => {
                out.push_str(unknown);
                out.push('\n');
            }
        }
        for line in &block.lines {
            // 行頭の日付を剥がす。
            // ja は `1月1日` だけで残りが空 → 行ごと落ちる。
            // en は `April 19 – Air Philippines Flight 541...` のように
            // **同じ行に本文が続く**ので、日付だけ剥がして本文を残す。
            match strip_date_prefix(line) {
                Some(rest) => {
                    if !rest.is_empty() {
                        out.push_str(&rest);
                        out.push('\n');
                    }
                }
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    collapse_blank_lines(&out)
}

/// 行頭の日付を剥がし、残りの本文を返す。日付が無ければ `None`。
///
/// **英語の日付行は本文を含む。**
/// `* [April 19](...) – [Air Philippines Flight 541](...), a Boeing 737-200, crashes`
/// のような形なので、行ごと落とすと本文まで消える。
/// 実測: この取り違えで en 2000-04 が 352 字（本来 2,000 字以上）になり、抽出が空になった。
fn strip_date_prefix(line: &str) -> Option<String> {
    static JA: OnceLock<regex::Regex> = OnceLock::new();
    static EN: OnceLock<regex::Regex> = OnceLock::new();

    let trimmed = line.trim_start();
    let body = trimmed
        .strip_prefix("* ")
        .or_else(|| trimmed.strip_prefix("- "))
        .unwrap_or(trimmed)
        .trim();

    // ja: `1月2日` / `1月2日（補足）`
    let ja = JA.get_or_init(|| {
        regex::Regex::new(r"^\d{1,2}月\d{1,2}日\s*([（(].*)?$").expect("ja date prefix")
    });
    if let Some(c) = ja.captures(body) {
        let rest = c.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        return Some(rest.to_owned());
    }

    // en: `April 19, 2007 – 本文` / `April 19 – 本文` / `April 19`
    let en = EN.get_or_init(|| {
        regex::Regex::new(
            r"^(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2}(?:,\s*\d{4})?\s*(?:[\u{2013}\u{2014}-]\s*)?(.*)$",
        )
        .expect("en date prefix")
    });
    if let Some(c) = en.captures(body) {
        let rest = c.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        return Some(rest.to_owned());
    }

    None
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// サニタイズ後の文字数。実測の窓（6,000〜20,000）を判定するのに使う。
///
/// **内部でサニタイズする。** 呼び出し側がサニタイズを忘れると
/// raw の 62,802 字（実測）が返り、窓の判定が必ず落ちる。
pub fn section_chars(raw: &MonthSection) -> usize {
    section_text(&sanitize_section(raw)).chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markdown_link() {
        let line = "  * [熊本城](https://ja.wikipedia.org/wiki/%E7%86%8A%E6%9C%AC%E5%9F%8E \"熊本城\")築城400年祭開幕。";
        assert_eq!(sanitize_line(line), "熊本城築城400年祭開幕。");
    }

    #[test]
    fn strips_nested_parens_in_title() {
        let line = r#"* [iPad](https://en.wikipedia.org/wiki/IPad_\(1st_generation\) "iPad (1st generation)") was announced."#;
        assert_eq!(sanitize_line(line), "iPad was announced.");
    }

    #[test]
    fn keeps_date_line_as_bare_text() {
        let line = "  * [1月1日](https://ja.wikipedia.org/wiki/1%E6%9C%881%E6%97%A5 \"1月1日\")";
        assert_eq!(sanitize_line(line), "1月1日");
    }

    #[test]
    fn keeps_bullet_without_space() {
        assert_eq!(sanitize_line("*印"), "*印");
    }

    #[test]
    fn section_text_keeps_date_as_own_line() {
        let section = MonthSection {
            heading_path: vec!["できごと".into(), "1月".into()],
            blocks: vec![
                DateBlock {
                    day: Some(1),
                    lines: vec![
                        "1月1日".into(),
                        "熊本城築城400年祭開幕。".into(),
                    ],
                },
                DateBlock {
                    day: Some(3),
                    lines: vec!["1月3日".into(), "盗難事件。".into()],
                },
            ],
        };
        let text = section_text(&section);
        assert_eq!(text, "1月1日\n熊本城築城400年祭開幕。\n\n1月3日\n盗難事件。");
    }

    #[test]
    fn collapse_blank_runs() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb\n"), "a\n\nb");
    }
}
