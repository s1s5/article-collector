use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// タイトルの比較キー。
///
/// NFKC → 小文字化 → 英数字以外を除去。
/// 実測: `"ＡＢＣ テスト！"` → `"abcテスト"`。
pub fn normalize_key(title: &str) -> String {
    let nfkc: String = title.nfkc().collect();
    let lower = nfkc.to_lowercase();
    lower
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
}

/// 文字バイグラムの Jaccard 係数。
///
/// 実測: `"安倍首相が訪米"` vs `"安倍首相が米国訪問"` → **0.4**
/// （閾値 0.6 では統合されず、段 3 の LLM 裁定に回る）。
pub fn jaccard_bigrams(a: &str, b: &str) -> f32 {
    let sa = bigrams(a);
    let sb = bigrams(b);
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

fn bigrams(text: &str) -> HashSet<(char, char)> {
    let chars: Vec<char> = normalize_key(text).chars().collect();
    if chars.len() < 2 {
        // 1 文字なら 1 要素の集合として扱う（空同士は 1.0 で別扱い）。
        return chars.iter().map(|c| (*c, *c)).collect();
    }
    chars.windows(2).map(|w| (w[0], w[1])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_nfkc_and_symbols() {
        assert_eq!(normalize_key("ＡＢＣ テスト！"), "abcテスト");
        assert_eq!(normalize_key("Hello, World!"), "helloworld");
        assert_eq!(normalize_key(""), "");
    }

    #[test]
    fn jaccard_of_identical_is_one() {
        assert_eq!(jaccard_bigrams("安倍首相が訪米", "安倍首相が訪米"), 1.0);
    }

    #[test]
    fn jaccard_of_paraphrase_is_below_threshold() {
        // 実測値。0.6 未満なので段 2 では統合されない。
        let j = jaccard_bigrams("安倍首相が訪米", "安倍首相が米国訪問");
        assert!((j - 0.4).abs() < 0.05, "expected ~0.4, got {j}");
        assert!(j < 0.6);
    }

    #[test]
    fn jaccard_of_disjoint_is_zero() {
        assert_eq!(jaccard_bigrams("あいうえお", "かきくけこ"), 0.0);
    }

    #[test]
    fn empty_against_nonempty_is_zero() {
        assert_eq!(jaccard_bigrams("", "なにか"), 0.0);
    }

    #[test]
    fn both_empty_is_one() {
        assert_eq!(jaccard_bigrams("", ""), 1.0);
    }

    #[test]
    fn near_duplicate_is_above_threshold() {
        let j = jaccard_bigrams(
            "熊本城築城400年祭が開幕",
            "熊本城築城400年祭が開幕した",
        );
        assert!(j >= 0.6, "expected >= 0.6, got {j}");
    }
}
