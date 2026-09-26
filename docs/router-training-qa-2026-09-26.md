# Router training QA — 2026-09-26

Validated against `docs/superpowers/plans/2026-09-26-router-training.md` on
Windows with an RTX 4070 SUPER and ONNX Runtime 1.22.0.

## Checks

| Check | Result |
| --- | --- |
| `rtk cargo test --workspace` | 843 passed, 8 ignored |
| `rtk cargo fmt --all -- --check` | Passed |
| `rtk cargo clippy --workspace --all-targets` | Passed |
| Real Laya golden tests (`--ignored`) | 2 passed: tokenizer sequences and CPU inference match Python |
| CLI status, correct, thresholds, invalid labels, unfinished publish | Passed |
| Real `router calibrate --write`, then recalibrate | Passed; repeat uses the saved calibration for the “before” metrics |
| Real `router train` through the embedded script and `uv` | Passed: 720 training rows, 4 CUDA epochs, 47.3 seconds of training |
| ONNX export parity | Maximum absolute difference `0.0000190735`, below `0.0001` |
| Rust candidate evaluation and gate | Passed: 90 evaluation decisions, report saved |
| Real `router publish --to path:…` and subsequent status | Passed; checkpoint manifest and all three model files written |
| Published checkpoint loading and routing (`--ignored`) | 1 passed; loaded the team checkpoint and produced valid probabilities on CPU |
| Training without CUDA | Refused with the expected CUDA/remote-training hint |
| HF Jobs invocation | CLI dry-run passed with a placeholder token; no job submitted |

## Fixes found during QA

- Dataset label resolution accepted negative probabilities and distributions
  whose sum overflowed. It now excludes those distributions; valid user
  corrections can still supply labels. A regression test reproduced the
  failure before the fix.
- Training accepted calibration manifests for old router questions, and
  recalibration reported the shipped temperatures as its baseline even when
  a compatible calibration was active. Both commands now share the same
  temperature selection, checking model revision and questions version.
  A regression test covers compatible calibration and incompatible manifests.

## Local evidence

The isolated fixture is at `G:\kode-work\router-training-qa-20260926`.
It has 300 synthetic records: 240 train, 30 calibration, 30 evaluation.
The records repeat a synthetic state to exercise the pipeline; they are
**not** a benchmark of routing quality or generalization to unseen tasks.
The Kode workspace's router dataset and active manifest were not changed.

Candidate: `01M3EHBQ5N1W0QFJ2Q30S23V0A`.

- `.kode/router/candidates/<candidate>/model/train_report.json`: Python
  training duration, loss, and export parity.
- `.kode/router/candidates/<candidate>/report.json`: Rust gate; baseline
  accuracy `0.666667`, ECE `0.125560`; candidate accuracy `1.0`, ECE `0.0`.
- `.kode/router/team-model.json` and `shared/<candidate>/`: published model.
- `check_cli.py`: CLI assertions used for this isolated fixture.

To rerun the committed model-loading smoke test against this checkpoint:

```powershell
$env:KODE_ROUTER_QA_ROOT = 'G:/kode-work/router-training-qa-20260926'
rtk cargo test -p kode --bin kode published_checkpoint -- --ignored
```

Real HF Jobs training, private HF publishing/downloading, a live paid
teacher call, and the separate Rust/Burn research spike were not run.
Teacher capture, secret filtering, invalid answers, and cancellation are
covered by the existing mocked tests. PyTorch emitted scheduler/export
warnings during training; the process, parity check, and Rust gate succeeded.
