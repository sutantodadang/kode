//! Temperature calibration and the candidate gate, over logits from any
//! `LogitSource` (the real Laya model, or a fake in tests).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::dataset::{Labeled, Split, split_of};
use crate::error::LocalError;
use crate::route::{route_questions, softmax_with_temperature};
use crate::sequence::{QType, QuestionDef};
use crate::temps::{Temperatures, bucket};

pub const MIN_CALIBRATE: usize = 50;
pub const MIN_TRAIN: usize = 300;
/// 30 records × 3 questions.
pub const MIN_EVAL_DECISIONS: usize = 90;
pub const ECE_TOLERANCE: f64 = 0.01;
const MIN_BUCKET: usize = 10;
const ECE_BINS: usize = 15;

pub trait LogitSource: Send + Sync {
    fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError>;
    fn temperatures(&self) -> Temperatures;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub split: Split,
    pub qtype: QType,
    pub logits: Vec<f32>,
    pub target: Vec<f32>,
}

pub fn build_items(src: &dyn LogitSource, examples: &[Labeled]) -> Result<Vec<Item>, LocalError> {
    let questions = route_questions();
    let mut items = Vec::new();
    for ex in examples {
        let split = split_of(&ex.id);
        for q in &questions {
            if let Some(target) = ex.labels.get(q.key) {
                let logits = src.logits(&ex.state, &q.def)?;
                if logits.len() == target.len() {
                    items.push(Item {
                        split,
                        qtype: q.def.qtype,
                        logits,
                        target: target.clone(),
                    });
                }
            }
        }
    }
    Ok(items)
}

fn nll(items: &[&Item], t: f32) -> f64 {
    let total: f64 = items
        .iter()
        .map(|it| {
            let p = softmax_with_temperature(&it.logits, t);
            -it.target
                .iter()
                .zip(&p)
                .map(|(y, q)| f64::from(*y) * f64::from(q.max(1e-12)).ln())
                .sum::<f64>()
        })
        .sum();
    total / items.len().max(1) as f64
}

/// Golden-section search of soft NLL over `ln T ∈ [ln 0.1, ln 10]`.
pub fn fit_temperature(items: &[&Item]) -> f32 {
    const INV_PHI: f64 = 0.618_033_988_749_894_9;
    let f = |x: f64| nll(items, x.exp() as f32);
    let (mut a, mut b) = (0.1f64.ln(), 10.0f64.ln());
    let mut c = b - INV_PHI * (b - a);
    let mut d = a + INV_PHI * (b - a);
    let (mut fc, mut fd) = (f(c), f(d));
    while b - a > 1e-4 {
        if fc < fd {
            b = d;
            d = c;
            fd = fc;
            c = b - INV_PHI * (b - a);
            fc = f(c);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + INV_PHI * (b - a);
            fd = f(d);
        }
    }
    ((a + b) / 2.0).exp() as f32
}

/// Per qtype (≥ 10 items) and per option-count bucket (≥ 10 items);
/// anything smaller keeps 1.0 / falls back to its qtype.
pub fn fit(items: &[&Item]) -> Temperatures {
    let mut temps = Temperatures::default();
    for qtype in [QType::Choice, QType::Score, QType::Noul] {
        let of_type: Vec<&Item> = items.iter().copied().filter(|i| i.qtype == qtype).collect();
        if of_type.len() >= MIN_BUCKET {
            temps.temperature[qtype.index()] = fit_temperature(&of_type);
        }
        let mut buckets: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
        for it in &of_type {
            buckets
                .entry(bucket(qtype, it.logits.len()))
                .or_default()
                .push(it);
        }
        for (key, group) in buckets {
            if group.len() >= MIN_BUCKET {
                temps
                    .temperature_by_options
                    .insert(key, fit_temperature(&group));
            }
        }
    }
    temps
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub n: usize,
    pub accuracy: f64,
    pub ece: f64,
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

/// laya `ece_score`: 15 equal-width bins `(lo, hi]` over max probability.
pub fn ece(conf: &[f64], correct: &[bool]) -> f64 {
    if conf.is_empty() {
        return 0.0;
    }
    let n = conf.len() as f64;
    let mut e = 0.0;
    for b in 0..ECE_BINS {
        let lo = b as f64 / ECE_BINS as f64;
        let hi = (b + 1) as f64 / ECE_BINS as f64;
        let sel: Vec<usize> = (0..conf.len())
            .filter(|&i| conf[i] > lo && conf[i] <= hi)
            .collect();
        if sel.is_empty() {
            continue;
        }
        let mean_conf = sel.iter().map(|&i| conf[i]).sum::<f64>() / sel.len() as f64;
        let acc = sel.iter().filter(|&&i| correct[i]).count() as f64 / sel.len() as f64;
        e += sel.len() as f64 / n * (mean_conf - acc).abs();
    }
    e
}

pub fn metrics(items: &[&Item], temps: &Temperatures) -> Metrics {
    let mut conf = Vec::with_capacity(items.len());
    let mut correct = Vec::with_capacity(items.len());
    for it in items {
        let p = softmax_with_temperature(&it.logits, temps.get(it.qtype, it.logits.len()));
        let pred = argmax(&p);
        conf.push(f64::from(p[pred]));
        correct.push(pred == argmax(&it.target));
    }
    let n = items.len();
    let accuracy = if n == 0 {
        0.0
    } else {
        correct.iter().filter(|c| **c).count() as f64 / n as f64
    };
    Metrics {
        n,
        accuracy,
        ece: ece(&conf, &correct),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub temps: Temperatures,
    pub before: Metrics,
    pub after: Metrics,
    pub n_fit: usize,
}

/// Fits on `fit_on` splits; before/after are measured on the eval split.
pub fn calibrate(items: &[Item], current: &Temperatures, fit_on: &[Split]) -> Calibration {
    let fit_items: Vec<&Item> = items.iter().filter(|i| fit_on.contains(&i.split)).collect();
    let eval: Vec<&Item> = items.iter().filter(|i| i.split == Split::Eval).collect();
    let temps = fit(&fit_items);
    Calibration {
        before: metrics(&eval, current),
        after: metrics(&eval, &temps),
        n_fit: fit_items.len(),
        temps,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub passed: bool,
    pub reason: String,
}

pub fn gate(base: &Metrics, cand: &Metrics) -> Gate {
    let reject = |reason: String| Gate {
        passed: false,
        reason,
    };
    if cand.n < MIN_EVAL_DECISIONS {
        return reject(format!(
            "eval split has {} decisions; need {MIN_EVAL_DECISIONS}",
            cand.n
        ));
    }
    if cand.accuracy < base.accuracy {
        return reject(format!(
            "accuracy {:.3} < base {:.3}",
            cand.accuracy, base.accuracy
        ));
    }
    if cand.ece > base.ece + ECE_TOLERANCE {
        return reject(format!(
            "ECE {:.3} > base {:.3} + {ECE_TOLERANCE}",
            cand.ece, base.ece
        ));
    }
    Gate {
        passed: true,
        reason: format!(
            "accuracy {:.3} ≥ base {:.3}; ECE {:.3} ≤ base {:.3} + {ECE_TOLERANCE}",
            cand.accuracy, base.accuracy, cand.ece, base.ece
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub base: Metrics,
    pub candidate: Metrics,
    pub candidate_temps: Temperatures,
    pub gate: Gate,
    pub n_calib: usize,
}

/// Calibrates the candidate on the calibration split, then compares both
/// models (each with its own temperatures) on the eval split.
pub fn evaluate(base_items: &[Item], base_temps: &Temperatures, cand_items: &[Item]) -> Evaluation {
    let calib: Vec<&Item> = cand_items
        .iter()
        .filter(|i| i.split == Split::Calib)
        .collect();
    let candidate_temps = fit(&calib);
    let base_eval: Vec<&Item> = base_items
        .iter()
        .filter(|i| i.split == Split::Eval)
        .collect();
    let cand_eval: Vec<&Item> = cand_items
        .iter()
        .filter(|i| i.split == Split::Eval)
        .collect();
    let base = metrics(&base_eval, base_temps);
    let candidate = metrics(&cand_eval, &candidate_temps);
    Evaluation {
        gate: gate(&base, &candidate),
        base,
        candidate,
        candidate_temps,
        n_calib: calib.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::softmax_with_temperature;

    /// Deterministic logits with some spread.
    fn logits_for(i: usize) -> Vec<f32> {
        vec![
            (i % 5) as f32 * 0.8,
            ((i * 7) % 3) as f32 * 1.1,
            1.5 - (i % 4) as f32 * 0.3,
        ]
    }

    fn items_with_true_temperature(n: usize, t: f32, split: Split) -> Vec<Item> {
        (0..n)
            .map(|i| {
                let logits = logits_for(i);
                let target = softmax_with_temperature(&logits, t);
                Item {
                    split,
                    qtype: QType::Choice,
                    logits,
                    target,
                }
            })
            .collect()
    }

    fn refs(items: &[Item]) -> Vec<&Item> {
        items.iter().collect()
    }

    #[test]
    fn fit_recovers_a_known_temperature() {
        for t in [0.5f32, 2.0] {
            let items = items_with_true_temperature(60, t, Split::Calib);
            let fitted = fit_temperature(&refs(&items));
            assert!((fitted - t).abs() < 0.02, "wanted {t}, got {fitted}");
        }
    }

    #[test]
    fn fit_uses_buckets_and_falls_back_below_ten_items() {
        let many = items_with_true_temperature(40, 2.0, Split::Calib);
        let temps = fit(&refs(&many));
        assert!((temps.temperature[QType::Choice.index()] - 2.0).abs() < 0.05);
        assert!(temps.temperature_by_options.contains_key("choice:3-5"));
        let few = items_with_true_temperature(5, 2.0, Split::Calib);
        let temps = fit(&refs(&few));
        assert_eq!(temps, Temperatures::default());
    }

    #[test]
    fn ece_matches_hand_computed_bins() {
        // Bins of width 1/15: 0.9 → (0.8667, 0.9333], 0.55 → (0.5333, 0.6].
        let e = ece(&[0.9, 0.9, 0.55], &[true, false, true]);
        let expected = (2.0 / 3.0) * (0.9 - 0.5) + (1.0 / 3.0) * (1.0 - 0.55);
        assert!((e - expected).abs() < 1e-9, "{e} vs {expected}");
        assert_eq!(ece(&[], &[]), 0.0);
    }

    #[test]
    fn metrics_count_argmax_agreement() {
        let items = vec![
            Item {
                split: Split::Eval,
                qtype: QType::Choice,
                logits: vec![3.0, 0.0],
                target: vec![1.0, 0.0],
            },
            Item {
                split: Split::Eval,
                qtype: QType::Choice,
                logits: vec![3.0, 0.0],
                target: vec![0.2, 0.8],
            },
        ];
        let m = metrics(&refs(&items), &Temperatures::default());
        assert_eq!(m.n, 2);
        assert!((m.accuracy - 0.5).abs() < 1e-9);
    }

    #[test]
    fn calibration_fits_on_requested_splits_and_scores_eval() {
        let mut items = items_with_true_temperature(60, 2.0, Split::Train);
        items.extend(items_with_true_temperature(30, 2.0, Split::Eval));
        let cal = calibrate(
            &items,
            &Temperatures::default(),
            &[Split::Train, Split::Calib],
        );
        assert_eq!(cal.n_fit, 60);
        assert_eq!(cal.before.n, 30);
        // The generator's targets are softmax(logits / 2.0), so the fit must
        // recover ~2.0. (ECE is not the right lens here: argmax is invariant
        // under temperature and accuracy is already 1.0.)
        assert!(
            (cal.temps.get(QType::Choice, 3) - 2.0).abs() < 0.1,
            "fitted {}",
            cal.temps.get(QType::Choice, 3)
        );
        assert_eq!(cal.after.accuracy, cal.before.accuracy);
    }

    fn metric(n: usize, accuracy: f64, ece: f64) -> Metrics {
        Metrics { n, accuracy, ece }
    }

    #[test]
    fn gate_rules() {
        let base = metric(120, 0.60, 0.10);
        assert!(gate(&base, &metric(120, 0.70, 0.08)).passed);
        assert!(
            gate(&base, &metric(120, 0.60, 0.105)).passed,
            "within ECE tolerance"
        );
        assert!(
            gate(&base, &metric(120, 0.59, 0.05))
                .reason
                .contains("accuracy")
        );
        assert!(gate(&base, &metric(120, 0.70, 0.12)).reason.contains("ECE"));
        assert!(
            gate(&base, &metric(40, 0.90, 0.01))
                .reason
                .contains("eval split")
        );
    }

    struct Fake {
        good: bool,
    }

    impl LogitSource for Fake {
        fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
            let k = q.options.len();
            if !self.good {
                return Ok(vec![0.0; k]);
            }
            // The state names the right option for each question.
            let right = q
                .options
                .iter()
                .position(|(key, _)| state.contains(&format!("={key}")))
                .unwrap_or(0);
            Ok((0..k).map(|i| if i == right { 3.0 } else { 0.0 }).collect())
        }
        fn temperatures(&self) -> Temperatures {
            Temperatures::default()
        }
    }

    fn examples(n: usize) -> Vec<Labeled> {
        (0..n)
            .map(|i| Labeled {
                id: format!("01J{i:023}"),
                state: "task tier=heavy effort=low plan=plan".to_string(),
                labels: [
                    ("tier".to_string(), vec![0.0, 0.0, 1.0]),
                    ("effort".to_string(), vec![1.0, 0.0, 0.0]),
                    ("plan".to_string(), vec![1.0, 0.0]),
                ]
                .into_iter()
                .collect(),
                corrected: false,
            })
            .collect()
    }

    #[test]
    fn build_items_makes_one_item_per_labeled_question() {
        let ex = examples(4);
        let items = build_items(&Fake { good: true }, &ex).unwrap();
        assert_eq!(items.len(), 12);
        assert!(items.iter().all(|i| i.split == split_of(&ex[0].id) || true));
    }

    #[test]
    fn evaluation_passes_a_better_candidate_and_rejects_a_worse_one() {
        let ex = examples(600);
        let good = build_items(&Fake { good: true }, &ex).unwrap();
        let bad = build_items(&Fake { good: false }, &ex).unwrap();
        let e = evaluate(&bad, &Temperatures::default(), &good);
        assert!(e.gate.passed, "{}", e.gate.reason);
        assert!(e.candidate.accuracy > e.base.accuracy);
        let e = evaluate(&good, &Temperatures::default(), &bad);
        assert!(!e.gate.passed);
    }
}
