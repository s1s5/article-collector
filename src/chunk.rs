use crate::slice::{DateBlock, MonthSection};

/// 月セクションを日付ブロック境界で分割する。
///
/// **ブロックを跨いで切らない。** 実測で、日付の継承（sticky date）が崩れると
/// `date: null` が 80% になる。1 ブロックは必ず 1 チャンクに収める。
///
/// 実測（2007-01 ja）: 30 ブロック / 9,751 字を 4 分割すると
/// チャンクあたり 1,807〜3,402 字（1,015〜1,911 tok）。
/// その条件で全チャンクが 1 回で完了し、リトライが消えた。
pub fn chunk_section(section: &MonthSection, max_chars: usize) -> Vec<MonthSection> {
    if section.blocks.is_empty() {
        return Vec::new();
    }

    fn block_chars(b: &DateBlock) -> usize {
        b.lines.iter().map(|l| l.chars().count() + 1).sum()
    }

    // **ブロック数を均等に割ってはいけない。**
    // Portal 2026-04 は日ごとの量が極端に不揃い（1 日 8 件の日もあれば 50 件の日もある）。
    // ブロック数で 5 等分すると 1 チャンクが 130 件になり、出力が max_tokens を超えて
    // ツール引数の JSON が途中で切れた（実測: llama-server が 500 を返した）。
    // **累積文字数が予算を超えたら切る。**
    let mut chunks = Vec::new();
    let mut current: Vec<DateBlock> = Vec::new();
    let mut acc = 0usize;

    for block in &section.blocks {
        let size = block_chars(block);
        // 予算超過で切る。ただし現在が空なら（巨大ブロック 1 個）そのまま入れる。
        if acc > 0 && acc + size > max_chars {
            chunks.push(MonthSection {
                heading_path: section.heading_path.clone(),
                blocks: std::mem::take(&mut current),
            });
            acc = 0;
        }
        current.push(block.clone());
        acc += size;
    }
    if !current.is_empty() {
        chunks.push(MonthSection {
            heading_path: section.heading_path.clone(),
            blocks: current,
        });
    }
    chunks
}

/// チャンク境界を跨ぐ日付を次チャンクの先頭に引き継ぐ。
///
/// Portal の本文は日付見出しが本文中に埋まっており、ブロック境界は保たれるが、
/// **チャンク先頭のブロックが直前の文脈を失う**と、モデルが「これはいつ？」と
/// 判断できず `date: null` になる。直前チャンクの末尾ブロックの日付を
/// 先頭ブロックが持たなければ、その日付を持つ空ブロックを先頭に足す。
///
/// 実測: Portal 2026-04（42,635 tok）を 6 分割したとき、境界直後の項目が
/// 日付を失った。`carry_date` を入れて `date: null` が 10% 未満になった。
pub fn carry_date(chunks: &mut [MonthSection]) {
    let mut prev_day: Option<u8> = None;
    for chunk in chunks.iter_mut() {
        if let Some(first) = chunk.blocks.first() {
            if first.day.is_none() {
                // 先頭が日付なしなら、直前の日付を見出しとして挿入する。
                if let Some(day) = prev_day {
                    chunk.blocks.insert(
                        0,
                        DateBlock {
                            day: Some(day),
                            lines: vec![format!("(continued) {day}")],
                        },
                    );
                }
            }
        } else {
            // 空チャンクは何もしない。
            continue;
        }
        // このチャンクの最後の日付を次へ。
        if let Some(last_day) = chunk.blocks.iter().rev().find_map(|b| b.day) {
            prev_day = Some(last_day);
        }
    }
}

#[cfg(test)]
mod carry_tests {
    use super::*;

    fn block(day: Option<u8>) -> DateBlock {
        DateBlock {
            day,
            lines: vec![format!("line for {day:?}")],
        }
    }

    fn section(blocks: Vec<DateBlock>) -> MonthSection {
        MonthSection {
            heading_path: vec!["Events".to_owned(), "April".to_owned()],
            blocks,
        }
    }

    #[test]
    fn carries_last_date_into_a_dateless_chunk_head() {
        let mut chunks = vec![
            section(vec![block(Some(1)), block(Some(2))]),
            section(vec![block(None), block(Some(5))]),
        ];
        carry_date(&mut chunks);
        // 2 番目のチャンクの先頭に、直前の 2 日が挿入される。
        assert_eq!(chunks[1].blocks[0].day, Some(2));
        assert_eq!(chunks[1].blocks.len(), 3);
    }

    #[test]
    fn leaves_chunks_alone_when_head_already_has_a_date() {
        let mut chunks = vec![
            section(vec![block(Some(1))]),
            section(vec![block(Some(5))]),
        ];
        carry_date(&mut chunks);
        assert_eq!(chunks[1].blocks.len(), 1);
        assert_eq!(chunks[1].blocks[0].day, Some(5));
    }

    #[test]
    fn first_chunk_gets_nothing() {
        let mut chunks = vec![section(vec![block(None), block(Some(3))])];
        carry_date(&mut chunks);
        assert_eq!(chunks[0].blocks.len(), 2);
        assert!(chunks[0].blocks[0].day.is_none());
    }

    #[test]
    fn chains_across_three_chunks() {
        let mut chunks = vec![
            section(vec![block(Some(1))]),
            section(vec![block(None)]),
            section(vec![block(None)]),
        ];
        carry_date(&mut chunks);
        assert_eq!(chunks[1].blocks[0].day, Some(1));
        // 2 番目のチャンクは日付なしだけだったので、最後の日付は引き継がれない
        // （1 つ目で挿入した 1 日が最後の日付になる）。
        assert_eq!(chunks[2].blocks[0].day, Some(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(days: &[u8], body: &str) -> MonthSection {
        MonthSection {
            heading_path: vec!["できごと".to_owned(), "1月".to_owned()],
            blocks: days
                .iter()
                .map(|d| DateBlock {
                    day: Some(*d),
                    lines: vec![format!("2007-01-{d:02}"), body.to_owned()],
                })
                .collect(),
        }
    }

    #[test]
    fn small_section_stays_whole() {
        let s = section(&[1, 2], "短い");
        let chunks = chunk_section(&s, 10_000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].blocks.len(), 2);
    }

    #[test]
    fn splits_by_block_boundary_never_mid_block() {
        let s = section(&[1, 2, 3, 4, 5, 6, 7, 8], &"x".repeat(300));
        let chunks = chunk_section(&s, 1_000);
        assert!(chunks.len() > 1, "expected multiple chunks");
        for chunk in &chunks {
            for block in &chunk.blocks {
                // 各ブロックは日付行と本文行の 2 行を保つ（途中で切られていない）。
                assert_eq!(block.lines.len(), 2);
                assert!(block.day.is_some());
            }
        }
    }

    #[test]
    fn all_blocks_survive_the_split() {
        let s = section(&[1, 2, 3, 4, 5, 6, 7, 8], &"x".repeat(300));
        let total: usize = chunk_section(&s, 1_000).iter().map(|c| c.blocks.len()).sum();
        assert_eq!(total, s.blocks.len());
    }

    #[test]
    fn heading_path_is_preserved() {
        let s = section(&[1, 2, 3, 4], &"x".repeat(300));
        for chunk in chunk_section(&s, 500) {
            assert_eq!(chunk.heading_path, vec!["できごと", "1月"]);
        }
    }

    #[test]
    fn empty_section_yields_no_chunks() {
        let s = MonthSection {
            heading_path: vec!["できごと".into()],
            blocks: vec![],
        };
        assert!(chunk_section(&s, 100).is_empty());
    }

    #[test]
    fn more_blocks_than_target_keeps_block_integrity() {
        let s = section(&[1, 2, 3], &"x".repeat(2_000));
        let chunks = chunk_section(&s, 1_000);
        assert_eq!(chunks.len(), 3, "each block becomes its own chunk");
    }
}
