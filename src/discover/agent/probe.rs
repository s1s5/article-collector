use std::sync::OnceLock;

use crate::unit::{Lang, UnitKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Ok,
    MissingPage,
    EmptyContent,
    TooShort,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PageProbe {
    pub status: ProbeStatus,
    pub chars: usize,
    /// 対象年月の日付トークン数。**Rust 側で数える。モデルに数えさせない。**
    pub month_hits: u32,
    /// 先頭 800 文字（モデルが文脈を判断するため）。
    pub head: String,
}

const HEAD_CHARS: usize = 800;

fn head_of(markdown: &str) -> String {
    markdown.chars().take(HEAD_CHARS).collect()
}

/// 1 ページを調べて、その月のニュースを含むか判定する。
///
/// **`month_hits` の数え方がこの関数の要。** 素朴に `{M}月{N}日` を数えると
/// 年号換算ページ（yearwiz）が通ってしまう。実測: yearwiz の本文に
/// `1989年1月8日` が 1 件あり、`1月8日` として一致してしまう。
pub fn probe_page(unit: &UnitKey, markdown: &str) -> PageProbe {
    let chars = markdown.chars().count();
    let status = match crate::fetch::classify(markdown) {
        crate::fetch::Classified::Alive(_) => ProbeStatus::Ok,
        crate::fetch::Classified::Missing => ProbeStatus::MissingPage,
        crate::fetch::Classified::Empty => ProbeStatus::EmptyContent,
    };
    // 200 字未満は classify が Missing にするが、区別して返す。
    let status = if chars < crate::fetch::MIN_BODY_CHARS {
        ProbeStatus::TooShort
    } else {
        status
    };

    let month_hits = match unit.lang {
        Lang::Ja => count_ja(unit, markdown),
        Lang::En => count_en(unit, markdown),
    };

    PageProbe {
        status,
        chars,
        month_hits,
        head: head_of(markdown),
    }
}

// ---------- 日本語 ----------

fn ja_full_date() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(\d{4})年(\d{1,2})月(\d{1,2})日").expect("ja full date")
    })
}

fn ja_month_day() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(\d{1,2})月(\d{1,2})日").expect("ja month day"))
}

fn num_date() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(\d{4})[./-](\d{1,2})[./-](\d{1,2})").expect("numeric date")
    })
}

/// `1989年1月8日` の `1月8日` を二重に数えないため、直前に `\d{4}年` があるか見る。
fn preceded_by_year(text: &str, at: usize, chars: &[char]) -> bool {
    let start = at.saturating_sub(8);
    let prefix: String = chars[start..at].iter().collect();
    let _ = text;
    prefix.ends_with('年')
        && prefix
            .trim_end_matches('年')
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .count()
            == 4
}

fn count_ja(unit: &UnitKey, text: &str) -> u32 {
    let chars: Vec<char> = text.chars().collect();
    let mut hits: std::collections::HashSet<(usize, u8, u8)> = std::collections::HashSet::new();

    // 1) 年つき。年が unit と一致するときだけ。
    for caps in ja_full_date().captures_iter(text) {
        let y: u16 = caps[1].parse().unwrap_or(0);
        let m: u8 = caps[2].parse().unwrap_or(0);
        let d: u8 = caps[3].parse().unwrap_or(0);
        if y == unit.year && m == unit.month {
            hits.insert((y as usize, m, d));
        }
    }

    // 2) 年なし。直前に 4 桁年があれば飛ばす。
    for caps in ja_month_day().captures_iter(text) {
        let m: u8 = caps[1].parse().unwrap_or(0);
        let d: u8 = caps[2].parse().unwrap_or(0);
        if m != unit.month {
            continue;
        }
        let at = caps.get(0).map(|m| m.start()).unwrap_or(0);
        // `text` は char 単位でなくバイト単位なので、char 境界に直す。
        let byte_at = at.min(text.len());
        let char_at = text[..byte_at].chars().count();
        if preceded_by_year(text, char_at, &chars) {
            continue;
        }
        hits.insert((0, m, d));
    }

    // 3) 数値形式（response.jp の 2007.1.1 など）。
    for caps in num_date().captures_iter(text) {
        let y: u16 = caps[1].parse().unwrap_or(0);
        let m: u8 = caps[2].parse().unwrap_or(0);
        let d: u8 = caps[3].parse().unwrap_or(0);
        if y == unit.year && m == unit.month {
            hits.insert((y as usize, m, d));
        }
    }

    hits.len() as u32
}

// ---------- 英語 ----------

fn en_month_names() -> &'static [&'static str] {
    &[
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
    ]
}

fn count_en(unit: &UnitKey, text: &str) -> u32 {
    let name = en_month_names()[(unit.month.clamp(1, 12) - 1) as usize];

    // **年を任意にして 1 本で判定する。**
    // 年なし用の正規表現を別に持つと `April 19, 1999` の `April 19` だけが
    // 一致して他年が通ってしまう（実測でテストが落ちた）。
    let re = regex::Regex::new(&format!(
        r"{name}\s*(\d{{1,2}})(?:\s*,\s*(\d{{4}}))?"
    ))
    .expect("en date with optional year");

    let mut days: std::collections::HashSet<u8> = std::collections::HashSet::new();

    for caps in re.captures_iter(text) {
        let d: u8 = caps[1].parse().unwrap_or(0);
        match caps.get(2) {
            // 年が書かれているなら unit の年と一致するときだけ数える。
            Some(y) => {
                if y.as_str().parse::<u16>().ok() == Some(unit.year) {
                    days.insert(d);
                }
            }
            // 年が無い場合は、そのページが対象月のものとして数える。
            None => {
                days.insert(d);
            }
        }
    }

    // ISO 形式（Portal の `2026-04-05`）。
    for caps in num_date().captures_iter(text) {
        let y: u16 = caps[1].parse().unwrap_or(0);
        let m: u8 = caps[2].parse().unwrap_or(0);
        let d: u8 = caps[3].parse().unwrap_or(0);
        if y == unit.year && m == unit.month {
            days.insert(d);
        }
    }

    days.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ja(year: u16, month: u8) -> UnitKey {
        UnitKey {
            year,
            month,
            lang: Lang::Ja,
        }
    }

    /// 実測: yearwiz の本文。`1989年1月8日` が 1 件ある。
    const YEARWIZ: &str = "\
## 今年は令和何年? 2007年は平成何年?
# 2007年は平成19年
平成は日本の年号で、昭和の後、令和の前である。
昭和の後、1989年1月8日から現在に至る。
";

    #[test]
    fn rejects_era_converter_page() {
        let p = probe_page(&ja(2007, 1), YEARWIZ);
        assert_eq!(p.month_hits, 0, "1989年1月8日 を数えてはいけない");
    }

    #[test]
    fn counts_jiken_timeline() {
        let text = "1月1日 能登半島地震が発生\n1月2日 何かの出来事\n1月3日 別の出来事\n";
        assert!(probe_page(&ja(2007, 1), text).month_hits >= 3);
    }

    #[test]
    fn counts_response_dot_format() {
        let text = "2007.1.1(Mon) 23:10 記事A\n2007.1.2(Tue) 10:00 記事B\n";
        assert!(probe_page(&ja(2007, 1), text).month_hits >= 2);
    }

    #[test]
    fn rejects_other_month_dates() {
        let text = "2月1日 あ\n2月2日 い\n2月3日 う\n";
        assert_eq!(probe_page(&ja(2007, 1), text).month_hits, 0);
    }

    #[test]
    fn rejects_other_year_in_full_date() {
        let text = "2006年1月1日 あ\n2006年1月2日 い\n";
        assert_eq!(probe_page(&ja(2007, 1), text).month_hits, 0);
    }

    #[test]
    fn short_page_is_too_short() {
        let p = probe_page(&ja(2007, 1), "短い");
        assert_eq!(p.status, ProbeStatus::TooShort);
    }

    #[test]
    fn missing_marker_is_missing_page() {
        let body = format!(
            "# 指定されたページが見つかりません\n3秒後にトップページに移動します。\n{}",
            "x".repeat(300)
        );
        assert_eq!(probe_page(&ja(2007, 1), &body).status, ProbeStatus::MissingPage);
    }

    #[test]
    fn long_page_with_dates_is_ok() {
        let body = format!(
            "1月1日 あ\n1月2日 い\n1月3日 う\n{}",
            "y".repeat(300)
        );
        let p = probe_page(&ja(2007, 1), &body);
        assert_eq!(p.status, ProbeStatus::Ok);
        assert!(p.month_hits >= 3);
    }

    #[test]
    fn counts_english_month_with_year() {
        let u = UnitKey {
            year: 2007,
            month: 4,
            lang: Lang::En,
        };
        let text = "April 19, 2007 – something\nApril 30, 2007 – another\n";
        assert!(probe_page(&u, text).month_hits >= 2);
    }

    #[test]
    fn rejects_english_other_year() {
        let u = UnitKey {
            year: 2007,
            month: 4,
            lang: Lang::En,
        };
        let text = "April 19, 1999 – something\nApril 30, 1999 – another\n";
        assert_eq!(probe_page(&u, text).month_hits, 0);
    }

    #[test]
    fn head_is_bounded() {
        let body = "x".repeat(5000);
        assert!(probe_page(&ja(2007, 1), &body).head.chars().count() <= HEAD_CHARS);
    }
}
