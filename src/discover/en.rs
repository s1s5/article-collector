use super::{Section, SourceKind, UrlEntry};
use crate::unit::UnitKey;

/// 英語の年記事 1 枚から `## Events` 配下の月見出しを取る。
///
/// 実測: 2000 年も 2001 年以降も `## Events > ### Month` の H3。
/// `Portal:Current events` は 2002 年以降しか無く、2000〜2001 は存在しないので、
/// 年記事を主ソースにする（S6 で Portal を補完に足す）。
pub fn en_sources(u: &UnitKey) -> Vec<UrlEntry> {
    vec![UrlEntry {
        url: format!("https://en.wikipedia.org/wiki/{}", u.year),
        kind: SourceKind::EnYearArticle,
        priority: 10,
        section: Some(Section::EnEvents),
        query: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Lang;

    #[test]
    fn produces_single_year_article() {
        let u = UnitKey {
            year: 2000,
            month: 4,
            lang: Lang::En,
        };
        let s = en_sources(&u);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, SourceKind::EnYearArticle);
        assert_eq!(s[0].url, "https://en.wikipedia.org/wiki/2000");
        assert_eq!(s[0].section, Some(Section::EnEvents));
    }
}
