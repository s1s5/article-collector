use crate::dedup::Cluster;

/// 重要度の順序キー。**小さいほど重要。**
///
/// 1. `best_priority`（10=年記事 > 20=Portal > 90=検索）
/// 2. 日付が無いものは後ろ（`true` を後ろに）
/// 3. 記述が長いものを優先（`usize::MAX - len` で降順を昇順に畳む）
///
/// **`HashMap` の反復順を持ち込まない。** 同点は呼び出し側の安定ソートで
/// 入力順が保たれる。
pub fn rank_key(c: &Cluster) -> (u8, bool, usize) {
    let len = c.summary.chars().count();
    (
        c.best_priority,
        c.date.is_none(),
        usize::MAX - len.min(usize::MAX - 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(priority: u8, date: Option<&str>, summary_len: usize) -> Cluster {
        Cluster {
            date: date.map(|d| d.to_owned()),
            title: "t".to_owned(),
            summary: "x".repeat(summary_len),
            sources: vec![],
            members: 1,
            best_priority: priority,
        }
    }

    #[test]
    fn lower_priority_wins() {
        let a = cluster(10, Some("2007-04-01"), 50);
        let b = cluster(90, Some("2007-04-01"), 50);
        assert!(rank_key(&a) < rank_key(&b));
    }

    #[test]
    fn dated_beats_undated_at_same_priority() {
        let a = cluster(90, Some("2007-04-01"), 50);
        let b = cluster(90, None, 50);
        assert!(rank_key(&a) < rank_key(&b));
    }

    #[test]
    fn longer_summary_wins_at_same_priority_and_date() {
        let a = cluster(90, Some("2007-04-01"), 80);
        let b = cluster(90, Some("2007-04-01"), 20);
        assert!(rank_key(&a) < rank_key(&b));
    }

    #[test]
    fn key_is_total_order_for_sorting() {
        let mut v = vec![
            cluster(90, None, 10),
            cluster(10, Some("2007-04-02"), 30),
            cluster(90, Some("2007-04-01"), 50),
        ];
        v.sort_by_key(rank_key);
        assert_eq!(v[0].best_priority, 10);
        assert_eq!(v[1].best_priority, 90);
        assert!(v[1].date.is_some());
        assert!(v[2].date.is_none());
    }
}
