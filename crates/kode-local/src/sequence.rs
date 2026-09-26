//! 1:1 port of laya `rl_common.render_options` / `build_sequence`. Must stay
//! token-for-token identical to the Python reference; the golden fixtures in
//! `tests/fixtures/laya_golden.json` pin it.

use serde::{Deserialize, Serialize};

/// Laya question primitive. Discriminants match `rl_common.QTYPES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QType {
    Choice = 0,
    Score = 1,
    Noul = 2,
}

impl QType {
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            QType::Choice => "choice",
            QType::Score => "score",
            QType::Noul => "noul",
        }
    }
}

/// A question in Kode's shape. `options` is `(key, description)`:
/// choice renders `key: description` (bare key when empty), score renders
/// `level i: description` in order, noul ignores it (fixed false/true pair).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionDef {
    pub qtype: QType,
    pub instructions: String,
    pub options: Vec<(String, String)>,
}

/// The tokenizer surface `build_sequence` needs. `encode` must not add
/// special tokens (Python: `add_special_tokens=False`).
pub trait SeqTokenizer {
    fn encode(&self, text: &str) -> Vec<u32>;
    fn cls_id(&self) -> u32;
    fn sep_id(&self) -> u32;
    fn mask_id(&self) -> u32;
    fn mask_token(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sequence {
    pub ids: Vec<u32>,
    /// Position of each option's mask marker, in option order.
    pub markers: Vec<usize>,
}

const OPTION_TOKEN_CAP: usize = 48;
const MIN_OPTION_BUDGET: usize = 16;
/// ponytail: the state is char-capped before tokenizing so a pasted
/// multi-MB log cannot stall the tokenizer. 32 chars per token of headroom
/// keeps the cap from ever cutting into the `max_len` window for real text.
const STATE_CHARS_PER_TOKEN_CAP: usize = 32;

pub fn render_options(q: &QuestionDef) -> Vec<String> {
    match q.qtype {
        QType::Choice => q
            .options
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}: {v}")
                }
            })
            .collect(),
        QType::Score => q
            .options
            .iter()
            .enumerate()
            .map(|(i, (_, v))| format!("level {i}: {v}"))
            .collect(),
        QType::Noul => vec![
            "false: no, the statement does not hold".to_string(),
            "true: yes, the statement holds".to_string(),
        ],
    }
}

pub fn build_sequence(
    tok: &dyn SeqTokenizer,
    state: &str,
    q: &QuestionDef,
    max_len: usize,
    head_max_len: usize,
) -> Sequence {
    let mask = tok.mask_token();
    let scrub = |s: &str| {
        if mask.is_empty() {
            s.to_string()
        } else {
            s.replace(mask, " ")
        }
    };

    let mut head_ids = tok.encode(&format!(
        "{} question: {}",
        q.qtype.name(),
        scrub(&q.instructions)
    ));
    let mut opt_ids: Vec<Vec<u32>> = render_options(q)
        .iter()
        .map(|o| {
            let mut v = vec![tok.mask_id()];
            v.extend(
                tok.encode(&format!(" {}", scrub(o)))
                    .into_iter()
                    .take(OPTION_TOKEN_CAP),
            );
            v
        })
        .collect();

    let used = |o: &[Vec<u32>]| o.iter().map(Vec::len).sum::<usize>() as isize;
    let mut opt_budget = head_max_len as isize - used(&opt_ids);
    if opt_budget < MIN_OPTION_BUDGET as isize {
        let per = std::cmp::max(
            4,
            head_max_len.saturating_sub(MIN_OPTION_BUDGET) / opt_ids.len().max(1),
        );
        for o in &mut opt_ids {
            o.truncate(per);
        }
        opt_budget = head_max_len as isize - used(&opt_ids);
    }
    head_ids.truncate(std::cmp::max(8, opt_budget) as usize);

    let mut ids = Vec::with_capacity(max_len);
    ids.push(tok.cls_id());
    ids.extend(head_ids);
    ids.push(tok.sep_id());
    let mut markers = Vec::with_capacity(opt_ids.len());
    for o in opt_ids {
        markers.push(ids.len());
        ids.extend(o);
    }
    ids.push(tok.sep_id());

    let room = max_len.saturating_sub(ids.len() + 1);
    let capped: String = state
        .chars()
        .take(max_len.saturating_mul(STATE_CHARS_PER_TOKEN_CAP))
        .collect();
    let mut st = tok.encode(&scrub(&capped));
    st.truncate(room);
    ids.extend(st);
    ids.push(tok.sep_id());

    ids.truncate(max_len);
    markers.retain(|&m| m < max_len);
    Sequence { ids, markers }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One token per char (id = codepoint + 100). cls=1, sep=2, mask=3.
    struct CharTok;
    impl SeqTokenizer for CharTok {
        fn encode(&self, text: &str) -> Vec<u32> {
            text.chars().map(|c| c as u32 + 100).collect()
        }
        fn cls_id(&self) -> u32 {
            1
        }
        fn sep_id(&self) -> u32 {
            2
        }
        fn mask_id(&self) -> u32 {
            3
        }
        fn mask_token(&self) -> &str {
            "<mask>"
        }
    }

    fn q(qtype: QType, ins: &str, options: &[(&str, &str)]) -> QuestionDef {
        QuestionDef {
            qtype,
            instructions: ins.to_string(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn render_options_matches_reference_shapes() {
        assert_eq!(
            render_options(&q(QType::Choice, "x", &[("a", ""), ("b", "desc")])),
            vec!["a".to_string(), "b: desc".to_string()]
        );
        assert_eq!(
            render_options(&q(QType::Score, "x", &[("low", "slow"), ("high", "fast")])),
            vec!["level 0: slow".to_string(), "level 1: fast".to_string()]
        );
        assert_eq!(
            render_options(&q(QType::Noul, "x", &[])),
            vec![
                "false: no, the statement does not hold".to_string(),
                "true: yes, the statement holds".to_string()
            ]
        );
    }

    #[test]
    fn short_sequence_layout_matches_reference() {
        // head "choice question: pick" = 21 chars; options " a" / " b: x".
        let seq = build_sequence(
            &CharTok,
            "hi",
            &q(QType::Choice, "pick", &[("a", ""), ("b", "x")]),
            64,
            32,
        );
        assert_eq!(seq.markers, vec![23, 26]);
        assert_eq!(seq.ids.len(), 36);
        assert_eq!(seq.ids[0], 1);
        assert_eq!(seq.ids[22], 2);
        assert_eq!(seq.ids[23], 3);
        assert_eq!(seq.ids[26], 3);
        assert_eq!(seq.ids[32], 2);
        assert_eq!(seq.ids[33], 'h' as u32 + 100);
        assert_eq!(seq.ids[35], 2);
    }

    #[test]
    fn many_long_options_shrink_evenly_and_head_keeps_eight_tokens() {
        let long = "y".repeat(60);
        let options: Vec<(String, String)> =
            (0..20).map(|i| (format!("k{i}"), long.clone())).collect();
        let question = QuestionDef {
            qtype: QType::Choice,
            instructions: "a long instruction that will be cut".to_string(),
            options,
        };
        let seq = build_sequence(&CharTok, "s", &question, 1024, 64);
        // per = max(4, (64 - 16) / 20) = 4; head = max(8, 64 - 80) = 8.
        assert_eq!(seq.markers.len(), 20);
        assert_eq!(seq.markers[0], 10);
        for (i, m) in seq.markers.iter().enumerate() {
            assert_eq!(*m, 10 + 4 * i);
        }
    }

    #[test]
    fn state_is_truncated_to_fill_max_len_exactly() {
        let seq = build_sequence(
            &CharTok,
            &"z".repeat(1000),
            &q(QType::Noul, "x", &[]),
            40,
            16,
        );
        assert_eq!(seq.ids.len(), 40);
        assert_eq!(*seq.ids.last().unwrap(), 2);
        assert_eq!(seq.markers, vec![10, 14]);
    }

    #[test]
    fn build_sequence_caps_huge_state() {
        let huge = "log line\n".repeat(600_000); // ~5.4 MB
        let started = std::time::Instant::now();
        let seq = build_sequence(&CharTok, &huge, &q(QType::Noul, "x", &[]), 1024, 256);
        assert!(seq.ids.len() <= 1024);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn mask_token_in_user_text_is_scrubbed() {
        let seq = build_sequence(
            &CharTok,
            "a<mask>b",
            &q(
                QType::Choice,
                "is <mask> here?",
                &[("yes", "<mask>"), ("no", "")],
            ),
            256,
            64,
        );
        let masks = seq.ids.iter().filter(|&&id| id == 3).count();
        assert_eq!(masks, 2, "only the two option markers may be mask ids");
    }

    #[test]
    fn empty_state_ends_with_two_separators() {
        let seq = build_sequence(&CharTok, "", &q(QType::Noul, "x", &[]), 256, 64);
        let n = seq.ids.len();
        assert_eq!(&seq.ids[n - 2..], &[2, 2]);
    }
}
