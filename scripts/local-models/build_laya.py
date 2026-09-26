"""Export laya-multilingual to ONNX, write laya.json + tokenizer.json, and
generate Rust golden fixtures from the checkpoint's own reference code.

Usage: python scripts/local-models/build_laya.py build/local-models
"""
import json
import os
import shutil
import sys

import numpy as np
import onnxruntime as ort
import torch
from huggingface_hub import snapshot_download

OUT = os.path.join(sys.argv[1], "laya-multilingual")
FIXTURES = os.path.join("crates", "kode-local", "tests", "fixtures", "laya_golden.json")
os.makedirs(OUT, exist_ok=True)

src = snapshot_download("convaiinnovations/laya",
                        allow_patterns=["rl_common.py", "rl_agent_api.py", "multilingual/*"])
sys.path.insert(0, src)
from rl_agent_api import RLAgent  # noqa: E402
from rl_common import QTYPES, build_sequence, render_options  # noqa: E402

agent = RLAgent(os.path.join(src, "multilingual"), device="cpu")
model = agent.model.float().eval()
model.encoder.config._attn_implementation = "eager"
tok = agent.tok
cfg = agent.cfg


class Wrapped(torch.nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids, attention_mask, marker_pos, marker_mask, qtype):
        logits, _ = self.m(input_ids, attention_mask, marker_pos, marker_mask.bool(), qtype)
        return logits


def to_internal(q):
    """Kode QuestionDef json -> rl_common internal question."""
    if q["qtype"] == "choice":
        crit = {k: (v or None) for k, v in q["options"]}
    elif q["qtype"] == "score":
        crit = [v for _, v in q["options"]]
    else:
        crit = None
    return {"t": q["qtype"], "ins": q["instructions"], "crit": crit}


def tensors(state, q):
    ids, markers = build_sequence(tok, state, to_internal(q), cfg["max_len"], cfg["head_max_len"])
    k = len(markers)
    return ids, markers, (
        torch.tensor([ids], dtype=torch.long),
        torch.ones((1, len(ids)), dtype=torch.long),
        torch.tensor([markers], dtype=torch.long),
        torch.ones((1, k), dtype=torch.long),
        torch.tensor([QTYPES[q["qtype"]]], dtype=torch.long),
    )


def probs_for(logits, q):
    k = len(logits)
    qt = QTYPES[q["qtype"]]
    size = "2" if k <= 2 else "3-5" if k <= 5 else "6-10" if k <= 10 else "11+"
    t = cfg.get("temperature_by_options", {}).get("%s:%s" % (q["qtype"], size), cfg["temperature"][qt])
    z = np.asarray(logits, dtype=np.float64) / t
    p = np.exp(z - z.max())
    return (p / p.sum()).tolist()


TIER = {"qtype": "choice", "instructions": "How much engineering work does this coding task need?",
        "options": [["light", "question, explanation, lookup, or a small single-file edit"],
                    ["standard", "a feature or bug fix touching a few files"],
                    ["heavy", "multi-file refactor, architecture, or unclear debugging"]]}
EFFORT = {"qtype": "score", "instructions": "How much reasoning depth does this coding task need?",
          "options": [["low", "quick answer or mechanical change"],
                      ["medium", "ordinary feature or bug fix"],
                      ["high", "subtle debugging, design, or cross-cutting change"]]}
PLAN = {"qtype": "choice", "instructions": "Should the agent write a plan before editing code?",
        "options": [["plan", "needs a written plan before editing"], ["direct", "can be done directly"]]}
MANY = {"qtype": "choice", "instructions": "Which module owns this?",
        "options": [["m%d" % i, "module number %d handles a long list of responsibilities" % i] for i in range(14)]}
NOUL = {"qtype": "noul", "instructions": "Does the task ask to delete data?", "options": []}
SCORE5 = {"qtype": "score", "instructions": "How risky is this change?",
          "options": [[str(i), t] for i, t in enumerate(["none", "low", "some", "high", "severe"])]}


def state(task, project="rust", changed=0):
    return "task: %s\nproject: %s\nuncommitted files: %d" % (task.strip(), project, changed)


STATES = [
    state("what does ContextCompiler::compile return?"),
    state("rename the field `budget_tokens` to `token_budget` everywhere", changed=1),
    state("refactor the provider layer so every model shares one streaming client", changed=4),
    state("tolong jelaskan fungsi run_plan_phase"),
    state("perbaiki bug: TUI tidak scroll ke bawah setelah tool selesai", project="rust", changed=2),
    state("debug why verification sometimes hangs on windows\r\nlogs:\r\n" + "timeout\r\n" * 3),
    state("add an emoji 🚀 to the status line and a <mask> placeholder"),
    state("x" * 20000),
    "",
]
CASES = [(s, q) for s in STATES for q in (TIER, EFFORT, PLAN)]
CASES += [(STATES[2], MANY), (STATES[5], NOUL), (STATES[1], SCORE5)]

# ---- export ---------------------------------------------------------------
_, _, example = tensors(STATES[2], TIER)
onnx_path = os.path.join(OUT, "model.onnx")
torch.onnx.export(
    Wrapped(model), example, onnx_path, opset_version=17, dynamo=False,
    input_names=["input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"],
    output_names=["logits"],
    dynamic_axes={"input_ids": {0: "b", 1: "l"}, "attention_mask": {0: "b", 1: "l"},
                  "marker_pos": {0: "b", 1: "k"}, "marker_mask": {0: "b", 1: "k"},
                  "qtype": {0: "b"}, "logits": {0: "b", 1: "k"}},
)

# ---- parity torch vs onnx + golden fixtures --------------------------------
sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
golden = []
worst = 0.0
with torch.no_grad():
    for s, q in CASES:
        ids, markers, t = tensors(s, q)
        assert len(markers) == len(render_options(to_internal(q))), "options did not fit"
        ref = model(t[0], t[1], t[2], t[3].bool(), t[4])[0][0].numpy()
        got = sess.run(["logits"], {n: v.numpy() for n, v in zip(
            ["input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"], t)})[0][0]
        worst = max(worst, float(np.abs(ref - got).max()))
        golden.append({"state": s, "question": q, "input_ids": ids, "markers": markers,
                       "probs": probs_for(ref.tolist(), q)})
print("max |torch - onnx| =", worst)
assert worst < 1e-4, "ONNX export diverges from PyTorch"

os.makedirs(os.path.dirname(FIXTURES), exist_ok=True)
with open(FIXTURES, "w", encoding="utf-8") as f:
    json.dump(golden, f, ensure_ascii=False)

shutil.copy(os.path.join(src, "multilingual", "tokenizer", "tokenizer.json"), os.path.join(OUT, "tokenizer.json"))
with open(os.path.join(OUT, "laya.json"), "w", encoding="utf-8") as f:
    json.dump({"max_len": cfg["max_len"], "head_max_len": cfg["head_max_len"],
               "temperature": cfg.get("temperature", [1.0, 1.0, 1.0]),
               "temperature_by_options": cfg.get("temperature_by_options", {}),
               "cls_id": tok.cls_token_id, "sep_id": tok.sep_token_id,
               "mask_id": tok.mask_token_id, "mask_token": tok.mask_token}, f, ensure_ascii=False)
print("wrote", OUT, "and", FIXTURES, "(%d cases)" % len(golden))
