use super::{Section, SourceKind, UrlEntry};
use crate::unit::UnitKey;

/// 日本語の年記事 1 枚から、`できごと` と `予定・予測` の 2 セクションを候補にする。
///
/// 実測 2026: `できごと` に 1〜10月、`予定・予測` に 11〜12月しか無い。
/// 片方だけ見ると未来月が 0 件になる。
///
/// `## 誕生` は**候補に入れない**。同じ `### N月` が 12 個あり、
/// 親を指定しないと誕生記事を拾う。
pub fn ja_sources(u: &UnitKey) -> Vec<UrlEntry> {
    let url = format!(
        "https://ja.wikipedia.org/wiki/{}",
        urlencode_path(&format!("{}年の日本", u.year))
    );
    vec![
        UrlEntry {
            url: url.clone(),
            kind: SourceKind::JaYearArticle,
            priority: 10,
            section: Some(Section::JaKigoto),
            query: None,
        },
        UrlEntry {
            url,
            kind: SourceKind::JaYearArticle,
            priority: 20,
            section: Some(Section::JaYotei),
            query: None,
        },
    ]
}

/// パス要素を percent-encode する。非 ASCII と予約文字だけを対象にする。
pub fn urlencode_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Lang;

    #[test]
    fn produces_kigoto_then_yotei() {
        let u = UnitKey {
            year: 2007,
            month: 1,
            lang: Lang::Ja,
        };
        let s = ja_sources(&u);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].section, Some(Section::JaKigoto));
        assert_eq!(s[0].priority, 10);
        assert_eq!(s[1].section, Some(Section::JaYotei));
        assert_eq!(s[1].priority, 20);
        assert!(s.iter().all(|e| e.kind == SourceKind::JaYearArticle));
    }

    #[test]
    fn url_is_percent_encoded() {
        let u = UnitKey {
            year: 2007,
            month: 1,
            lang: Lang::Ja,
        };
        let s = ja_sources(&u);
        assert_eq!(
            s[0].url,
            "https://ja.wikipedia.org/wiki/2007%E5%B9%B4%E3%81%AE%E6%97%A5%E6%9C%AC"
        );
    }
}
