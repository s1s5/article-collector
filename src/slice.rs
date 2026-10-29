use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: usize,
}

#[derive(Clone, Debug, Default)]
pub struct DateBlock {
    pub day: Option<u8>,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MonthSection {
    pub heading_path: Vec<String>,
    pub blocks: Vec<DateBlock>,
}

pub const MONTH_PARENTS: [&str; 2] = ["できごと", "予定・予測"];

/// 英語の年記事は `## Events` 配下に `### April` を持つ（2000 年も 2001 年以降も同じ）。
pub const EN_EVENTS_PARENT: &str = "Events";

/// `--lang en` で月見出しに使う英語名。
pub fn en_month_name(month: u8) -> &'static str {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES[(month.clamp(1, 12) - 1) as usize]
}

fn heading_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^(#{2,3})\s+(.+?)\s*$").expect("heading pattern"))
}

fn date_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\s*(?:\*\s*)?\[?(\d{1,2})月(\d{1,2})日").expect("date pattern"))
}

pub fn headings(markdown: &str) -> Vec<Heading> {
    markdown
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = heading_re().captures(line)?;
            Some(Heading {
                level: caps[1].len() as u8,
                text: caps[2].to_owned(),
                line: i,
            })
        })
        .collect()
}

fn parent_of(hs: &[Heading], index: usize) -> Option<&str> {
    hs[..index]
        .iter()
        .rev()
        .find(|h| h.level < hs[index].level)
        .map(|h| h.text.as_str())
}

pub fn month_heading_text(month: u8) -> String {
    format!("{month}月")
}

pub fn slice_month(markdown: &str, month: u8) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let hs = headings(markdown);
    let wanted = month_heading_text(month);
    let target = (0..hs.len()).find(|&i| {
        hs[i].level == 3
            && hs[i].text == wanted
            && parent_of(&hs, i).is_some_and(|p| MONTH_PARENTS.contains(&p))
    })?;
    let parent = parent_of(&hs, target)?;
    let start = hs[target].line + 1;
    let end = hs[target + 1..]
        .iter()
        .find(|h| h.level <= 3)
        .map_or(lines.len(), |h| h.line);
    Some(MonthSection {
        heading_path: vec![parent.to_owned(), wanted],
        blocks: blockify(&lines[start..end]),
    })
}

/// 特定の親見出し配下の月セクションだけを取る。
///
/// ja は `## できごと` と `## 誕生` に同じ `### N月` があり、
/// 親を指定しないと誕生記事を拾う（実測で 12 個ずつ存在）。
pub fn slice_month_under(markdown: &str, month: u8, parent_name: &str) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let hs = headings(markdown);
    let wanted = month_heading_text(month);
    let target = (0..hs.len()).find(|&i| {
        hs[i].level == 3 && hs[i].text == wanted && parent_of(&hs, i) == Some(parent_name)
    })?;
    let start = hs[target].line + 1;
    let end = hs[target + 1..]
        .iter()
        .find(|h| h.level <= 3)
        .map_or(lines.len(), |h| h.line);
    Some(MonthSection {
        heading_path: vec![parent_name.to_owned(), wanted],
        blocks: blockify(&lines[start..end]),
    })
}

/// 英語の日付行を検出する。`* [April 3](...) – ...` / `April 5, 2010 (2010-04-05)` など。
fn en_date_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"^\s*(?:\*\s*)?\[?(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{1,2})\b",
        )
        .expect("en date pattern")
    })
}

fn en_iso_date_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(\d{4})-(\d{2})-(\d{2})").expect("iso date pattern"))
}

/// Portal の日付見出しを拾う。
///
/// 実測 `Portal:Current events/April 2026` の形式:
/// `April5,2026(2026-04-05) (Sunday)`
///
/// **月名と日の間に空白が無い**ので `en_date_re` の `\s+` では一致しない。
/// sanitize でリンクを外した後もこの形が残る。
fn portal_date_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"^(?:January|February|March|April|May|June|July|August|September|October|November|December)\s*(\d{1,2})\s*,\s*\d{4}\s*\(\d{4}-\d{2}-\d{2}\)",
        )
        .expect("portal date pattern")
    })
}

/// Portal 形式の本文を日付ブロックに分ける。
///
/// 日付見出しが本文中に埋まっている（見出し記号が無い）のが特徴。
/// 1 つ目の日付より前の行は捨てる（導入文・目次）。
pub fn blockify_portal(lines: &[&str]) -> Vec<DateBlock> {
    let mut blocks: Vec<DateBlock> = Vec::new();
    let mut current: Option<DateBlock> = None;

    for line in lines {
        let day = portal_date_re()
            .captures(line.trim())
            .and_then(|c| c[1].parse::<u8>().ok());
        match day {
            Some(day) => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
                current = Some(DateBlock {
                    day: Some(day),
                    lines: vec![(*line).to_owned()],
                });
            }
            None => {
                if let Some(block) = current.as_mut() {
                    block.lines.push((*line).to_owned());
                }
            }
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }
    blocks.retain(|b| b.lines.iter().any(|l| !l.trim().is_empty()));
    blocks
}

/// Portal の月ページを切り出す。
///
/// `Portal:Current events/April 2026` は全体がその月なので、見出しで切らずに
/// 日付行でブロック化する。
pub fn slice_portal(markdown: &str, year: u16, month: u8) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let blocks = blockify_portal(&lines);
    let days: std::collections::HashSet<u8> = blocks.iter().filter_map(|b| b.day).collect();
    // 日付が 2 日未満なら Portal として使えない（別のページ・空ページ）。
    if days.len() < 2 {
        return None;
    }
    Some(MonthSection {
        heading_path: vec![
            format!("Portal:Current events/{}", en_month_name(month)),
            format!("{year:04}-{month:02}"),
        ],
        blocks,
    })
}

pub fn blockify_en(lines: &[&str]) -> Vec<DateBlock> {
    let mut blocks: Vec<DateBlock> = Vec::new();
    let mut current = DateBlock::default();
    let mut seen_date = false;
    for line in lines {
        let day = en_date_re()
            .captures(line)
            .and_then(|c| c[1].parse::<u8>().ok())
            .or_else(|| {
                // `April 5, 2010 (2010-04-05) (Monday)` のように ISO が埋まっている場合。
                en_iso_date_re()
                    .captures(line)
                    .and_then(|c| c[3].parse::<u8>().ok())
            });
        match day {
            Some(day) => {
                seen_date = true;
                if current.day.is_some() || !current.lines.is_empty() {
                    blocks.push(current);
                }
                current = DateBlock {
                    day: Some(day),
                    lines: vec![(*line).to_owned()],
                };
            }
            None => {
                // **最初の日付行より前の行は捨てる。**
                // 実測: en 2001 の `### April` は画像キャプション
                // （`Two men marrying in Amsterdam on April 1, ...`）で始まり、
                // これを日付なしブロックにすると `date: null` の項目が 1 件混ざった。
                if seen_date {
                    current.lines.push((*line).to_owned());
                }
            }
        }
    }
    if current.day.is_some() || !current.lines.is_empty() {
        blocks.push(current);
    }
    blocks.retain(|b| b.lines.iter().any(|l| !l.trim().is_empty()));
    blocks
}

/// 汎用の日付行スキャン。**検索由来のページ向け**（jiken.jp / response.jp / 報道サイト）。
///
/// Wikipedia 以外は月見出しの構造がバラバラなので、日付行を直接拾って
/// ブロック化する。認識する形式:
/// - `2007年4月1日` / `4月1日`
/// - `2007.4.1` / `2007/4/1`
/// - `2007-04-01`
/// - `April 1, 2007` / `April 1`
///
/// 対象年月の日付が **2 日以上** 見つからなければ `None`。
/// 1 日しか無いページ（年次まとめ・白書の索引）は月次ソースとして使えない。
pub fn slice_generic(markdown: &str, year: u16, month: u8) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks: Vec<DateBlock> = Vec::new();
    let mut current: Option<DateBlock> = None;
    let mut days: std::collections::HashSet<u8> = std::collections::HashSet::new();

    for line in &lines {
        match generic_day(line, year, month) {
            Some(day) if (1..=31).contains(&day) => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
                days.insert(day);
                current = Some(DateBlock {
                    day: Some(day),
                    lines: vec![(*line).to_owned()],
                });
            }
            Some(_) => {}
            None => {
                if let Some(block) = current.as_mut() {
                    block.lines.push((*line).to_owned());
                }
            }
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }

    if days.len() < 2 || blocks.is_empty() {
        return None;
    }
    blocks.retain(|b| b.lines.iter().any(|l| !l.trim().is_empty()));
    Some(MonthSection {
        heading_path: vec![format!("{year:04}-{month:02}"), "(検索由来)".to_owned()],
        blocks,
    })
}

/// 1 行から対象年月の「日」を取る。別月・別年なら `Some(0)` を返して無視させる。
fn generic_day(line: &str, year: u16, month: u8) -> Option<u8> {
    static JA: OnceLock<regex::Regex> = OnceLock::new();
    static NUM: OnceLock<regex::Regex> = OnceLock::new();
    static ISO: OnceLock<regex::Regex> = OnceLock::new();

    // 2007年4月1日 / 4月1日
    let ja = JA.get_or_init(|| {
        regex::Regex::new(r"(?:(\d{4})年)?(\d{1,2})月(\d{1,2})日").expect("ja date")
    });
    if let Some(c) = ja.captures(line) {
        let y = c.get(1).and_then(|m| m.as_str().parse::<u16>().ok());
        let mo: u8 = c[2].parse().unwrap_or(0);
        let d: u8 = c[3].parse().unwrap_or(0);
        if y.is_some_and(|y| y != year) || mo != month {
            return Some(0);
        }
        return Some(d);
    }

    // 2007-04-01
    let iso = ISO.get_or_init(|| {
        regex::Regex::new(r"(\d{4})-(\d{2})-(\d{2})").expect("iso date")
    });
    if let Some(c) = iso.captures(line) {
        let y: u16 = c[1].parse().unwrap_or(0);
        let mo: u8 = c[2].parse().unwrap_or(0);
        let d: u8 = c[3].parse().unwrap_or(0);
        return Some(if y == year && mo == month { d } else { 0 });
    }

    // 2007.4.1 / 2007/4/1
    let num = NUM.get_or_init(|| {
        regex::Regex::new(r"\b(\d{4})[./](\d{1,2})[./](\d{1,2})\b").expect("num date")
    });
    if let Some(c) = num.captures(line) {
        let y: u16 = c[1].parse().unwrap_or(0);
        let mo: u8 = c[2].parse().unwrap_or(0);
        let d: u8 = c[3].parse().unwrap_or(0);
        return Some(if y == year && mo == month { d } else { 0 });
    }

    // April 1, 2007 / April 1
    let name = en_month_name(month);
    if let Some(pos) = find_month_name(line, name) {
        let rest = &line[pos + name.len()..];
        let digits: String = rest
            .trim_start_matches(|c: char| c == ',' || c.is_whitespace())
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(d) = digits.parse::<u8>() {
            return Some(d);
        }
    }

    None
}

 fn find_month_name(line: &str, name: &str) -> Option<usize> {
    line.to_lowercase().find(&name.to_lowercase())
}

#[cfg(test)]
mod generic_tests {
    use super::*;

    #[test]
    fn picks_ja_dates_with_year() {
        let doc = "2007年4月1日 何かあった。\n2007年4月2日 別のことがあった。\n";
        let s = slice_generic(doc, 2007, 4).expect("slice");
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.blocks[0].day, Some(1));
        assert_eq!(s.blocks[1].day, Some(2));
    }

    #[test]
    fn picks_dotted_dates() {
        let doc = "2010.4.1(Thu) 23:10 記事A\n2010.4.2(Fri) 12:00 記事B\n";
        let s = slice_generic(doc, 2010, 4).expect("slice");
        assert_eq!(s.blocks.len(), 2);
    }

    #[test]
    fn rejects_other_month() {
        let doc = "2007年3月1日 あ\n2007年3月2日 い\n";
        assert!(slice_generic(doc, 2007, 4).is_none());
    }

    #[test]
    fn rejects_single_day_page() {
        let doc = "2007年4月1日 あ\n本文だけが続く。\n";
        assert!(slice_generic(doc, 2007, 4).is_none());
    }

    #[test]
    fn picks_english_dates() {
        let doc = "April 1, 2007 — something.\nApril 2, 2007 — another.\n";
        let s = slice_generic(doc, 2007, 4).expect("slice");
        assert_eq!(s.blocks.len(), 2);
    }
}
///
/// 2000 年も 2001 年以降も `## Events > ### Month`（H3）。
/// 将来 H2 で月名が来る版に備え、親が `Events` でない場合は
/// `## Month` 直接も探す。
pub fn slice_en_month(markdown: &str, month: u8) -> Option<MonthSection> {
    let month_name = en_month_name(month);
    if let Some(section) = slice_month_under_en(markdown, month_name) {
        return Some(section);
    }
    slice_h2_month(markdown, month_name)
}

fn slice_month_under_en(markdown: &str, month_name: &str) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let hs = headings(markdown);
    let target = (0..hs.len()).find(|&i| {
        hs[i].level == 3
            && hs[i].text.eq_ignore_ascii_case(month_name)
            && parent_of(&hs, i) == Some(EN_EVENTS_PARENT)
    })?;
    let start = hs[target].line + 1;
    let end = hs[target + 1..]
        .iter()
        .find(|h| h.level <= 3)
        .map_or(lines.len(), |h| h.line);
    Some(MonthSection {
        heading_path: vec![EN_EVENTS_PARENT.to_owned(), month_name.to_owned()],
        blocks: blockify_en(&lines[start..end]),
    })
}

fn slice_h2_month(markdown: &str, month_name: &str) -> Option<MonthSection> {
    let lines: Vec<&str> = markdown.lines().collect();
    let hs = headings(markdown);
    let target = (0..hs.len())
        .find(|&i| hs[i].level == 2 && hs[i].text.eq_ignore_ascii_case(month_name))?;
    let start = hs[target].line + 1;
    let end = hs[target + 1..]
        .iter()
        .find(|h| h.level <= 2)
        .map_or(lines.len(), |h| h.line);
    Some(MonthSection {
        heading_path: vec![month_name.to_owned()],
        blocks: blockify_en(&lines[start..end]),
    })
}

pub fn blockify(lines: &[&str]) -> Vec<DateBlock> {
    let mut blocks: Vec<DateBlock> = Vec::new();
    let mut current = DateBlock::default();
    for line in lines {
        match date_re().captures(line) {
            Some(caps) => {
                let day = caps[2].parse::<u8>().ok().filter(|d| (1..=31).contains(d));
                if current.day.is_some() || !current.lines.is_empty() {
                    blocks.push(current);
                }
                current = DateBlock {
                    day,
                    lines: vec![(*line).to_owned()],
                };
            }
            None => current.lines.push((*line).to_owned()),
        }
    }
    if current.day.is_some() || !current.lines.is_empty() {
        blocks.push(current);
    }
    blocks.retain(|b| b.lines.iter().any(|l| !l.trim().is_empty()));
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
## できごと
### 1月
  * [1月1日](https://example/1 \"1月1日\")
    * 最初の出来事。
    * 二つ目の出来事。
  * [1月3日](https://example/3 \"1月3日\")
    * 三日の出来事。
### 2月
  * [2月1日](https://example/x \"2月1日\")
    * 二月の出来事。
## 誕生
### 1月
  * [1月9日](https://example/9 \"1月9日\")
    * 生まれた人。
";

    #[test]
    fn slices_month_under_kigoto_only() {
        let s = slice_month(DOC, 1).expect("january is present");
        assert_eq!(s.heading_path, vec!["できごと".to_owned(), "1月".to_owned()]);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.blocks[0].day, Some(1));
        assert_eq!(s.blocks[1].day, Some(3));
    }

    #[test]
    fn date_is_sticky_across_body_lines() {
        let s = slice_month(DOC, 1).expect("january is present");
        assert_eq!(s.blocks[0].lines.len(), 3);
        assert!(s.blocks[0].lines[2].contains("二つ目の出来事"));
    }

    #[test]
    fn ignores_tanjou_parent() {
        let s = slice_month(DOC, 1).expect("january is present");
        assert!(!s.blocks.iter().flat_map(|b| &b.lines).any(|l| l.contains("生まれた人")));
    }

    #[test]
    fn missing_month_is_none() {
        assert!(slice_month(DOC, 5).is_none());
    }

    #[test]
    fn yotei_parent_is_accepted() {
        let doc = "## 予定・予測\n### 11月\n  * [11月1日](https://example/y \"11月1日\")\n    * 予定。\n";
        let s = slice_month(doc, 11).expect("november is present");
        assert_eq!(s.heading_path, vec!["予定・予測".to_owned(), "11月".to_owned()]);
        assert_eq!(s.blocks[0].day, Some(1));
    }

    #[test]
    fn heading_stops_at_next_same_level() {
        let s = slice_month(DOC, 2).expect("february is present");
        assert_eq!(s.blocks.len(), 1);
        assert!(!s.blocks[0].lines.iter().any(|l| l.contains("生まれた人")));
    }
}
