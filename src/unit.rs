use std::fmt;
use std::str::FromStr;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize)]
pub enum Lang {
    Ja,
    En,
}

impl Lang {
    pub fn as_dir(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ja" => Some(Self::Ja),
            "en" => Some(Self::En),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UnitKey {
    pub year: u16,
    pub month: u8,
    pub lang: Lang,
}

impl fmt::Display for UnitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}/{}", self.year, self.month, self.lang.as_dir())
    }
}

impl FromStr for UnitKey {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (ym, lang) = raw
            .split_once('/')
            .ok_or_else(|| format!("invalid unit `{raw}`: expected YYYY-MM/lang"))?;
        let (y, m) = ym
            .split_once('-')
            .ok_or_else(|| format!("invalid unit `{raw}`: expected YYYY-MM/lang"))?;
        let year: u16 = y.parse().map_err(|_| format!("invalid year `{y}`"))?;
        let month: u8 = m.parse().map_err(|_| format!("invalid month `{m}`"))?;
        if !(1..=12).contains(&month) {
            return Err(format!("month out of range: {month}"));
        }
        let lang = Lang::parse(lang)
            .ok_or_else(|| format!("invalid lang `{lang}`: expected ja or en"))?;
        Ok(Self { year, month, lang })
    }
}

/// `2007-01` 形式を (year, month) に分解する。範囲の `--from` / `--to` 用。
pub fn parse_year_month(raw: &str) -> Result<(u16, u8), String> {
    let (y, m) = raw
        .split_once('-')
        .ok_or_else(|| format!("invalid month `{raw}`: expected YYYY-MM"))?;
    let year: u16 = y.parse().map_err(|_| format!("invalid year `{y}`"))?;
    let month: u8 = m.parse().map_err(|_| format!("invalid month `{m}`"))?;
    if !(1..=12).contains(&month) {
        return Err(format!("month out of range: {month}"));
    }
    Ok((year, month))
}

fn month_index(year: u16, month: u8) -> u32 {
    u32::from(year) * 12 + u32::from(month) - 1
}

fn from_index(index: u32) -> (u16, u8) {
    let year = (index / 12) as u16;
    let month = (index % 12 + 1) as u8;
    (year, month)
}

/// `from` から `to` まで（両端を含む）を `(year, month, lang)` 昇順で返す。
///
/// 順序は `(year, month, lang)` で `Ja` → `En`。`from > to` なら空。
pub fn iter_range(from: UnitKey, to: UnitKey) -> impl Iterator<Item = UnitKey> {
    let start = month_index(from.year, from.month);
    let end = month_index(to.year, to.month);
    (start..=end).flat_map(move |index| {
        let (year, month) = from_index(index);
        [Lang::Ja, Lang::En].into_iter().filter_map(move |lang| {
            let unit = UnitKey { year, month, lang };
            (unit >= from && unit <= to).then_some(unit)
        })
    })
}

/// `--lang` の値を解釈する。`both` は 2 言語ぶんの `Lang` を返す。
pub fn langs_of(raw: &str) -> Result<&'static [Lang], String> {
    match raw {
        "ja" => Ok(&[Lang::Ja]),
        "en" => Ok(&[Lang::En]),
        "both" => Ok(&[Lang::Ja, Lang::En]),
        other => Err(format!("invalid --lang `{other}`: expected ja, en, or both")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> UnitKey {
        s.parse().expect("valid unit")
    }

    #[test]
    fn display_roundtrip() {
        assert_eq!(u("2007-01/ja").to_string(), "2007-01/ja");
        assert_eq!(u("2007-12/en").to_string(), "2007-12/en");
    }

    #[test]
    fn parses_unit() {
        let k = u("2007-01/ja");
        assert_eq!((k.year, k.month, k.lang), (2007, 1, Lang::Ja));
    }

    #[test]
    fn rejects_bad_units() {
        assert!("2007-01".parse::<UnitKey>().is_err());
        assert!("2007-13/ja".parse::<UnitKey>().is_err());
        assert!("2007-00/ja".parse::<UnitKey>().is_err());
        assert!("2007-01/fr".parse::<UnitKey>().is_err());
        assert!("xx-01/ja".parse::<UnitKey>().is_err());
    }

    #[test]
    fn range_spans_year_boundary() {
        let got: Vec<String> = iter_range(u("2007-11/ja"), u("2008-01/ja"))
            .map(|k| k.to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "2007-11/ja",
                "2007-11/en",
                "2007-12/ja",
                "2007-12/en",
                "2008-01/ja",
            ]
        );
    }

    #[test]
    fn range_is_inclusive_both_ends() {
        // from=(2007,01,Ja) / to=(2007,02,Ja) なので 2007-02/en は範囲外。
        let got: Vec<String> = iter_range(u("2007-01/ja"), u("2007-02/ja"))
            .map(|k| k.to_string())
            .collect();
        assert_eq!(got, vec!["2007-01/ja", "2007-01/en", "2007-02/ja"]);
    }

    #[test]
    fn full_month_range_covers_both_langs() {
        let got: Vec<String> = iter_range(u("2007-01/ja"), u("2007-02/en"))
            .map(|k| k.to_string())
            .collect();
        assert_eq!(
            got,
            vec!["2007-01/ja", "2007-01/en", "2007-02/ja", "2007-02/en"]
        );
    }

    #[test]
    fn range_is_empty_when_reversed() {
        assert_eq!(iter_range(u("2007-02/ja"), u("2007-01/ja")).count(), 0);
    }

    #[test]
    fn range_single_en_unit() {
        let got: Vec<String> = iter_range(u("2007-01/en"), u("2007-01/en"))
            .map(|k| k.to_string())
            .collect();
        assert_eq!(got, vec!["2007-01/en"]);
    }

    #[test]
    fn langs_of_maps_both() {
        assert_eq!(langs_of("ja").unwrap(), &[Lang::Ja]);
        assert_eq!(langs_of("both").unwrap(), &[Lang::Ja, Lang::En]);
        assert!(langs_of("fr").is_err());
    }
}
