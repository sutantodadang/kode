"""Export a slim Qwen3-Reranker-0.6B graph ([b, 2] = no/yes logits at the last
position), write reranker.json + tokenizer.json, and Rust golden fixtures.

Usage: python scripts/local-models/build_reranker.py build/local-models
"""
import json
import os
import shutil
import sys
import tempfile

import numpy as np
import onnx
import onnxruntime as ort
import torch
from huggingface_hub import snapshot_download
from transformers import AutoModelForCausalLM, AutoTokenizer

OUT = os.path.join(sys.argv[1], "qwen3-reranker-0.6b")
FIXTURES = os.path.join("crates", "kode-local", "tests", "fixtures", "reranker_golden.json")
os.makedirs(OUT, exist_ok=True)

REPO = "Qwen/Qwen3-Reranker-0.6B"
PREFIX = ("<|im_start|>system\nJudge whether the Document meets the requirements based on the Query "
          "and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n"
          "<|im_start|>user\n")
SUFFIX = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
INSTRUCTION = "Given a coding task, retrieve code or engineering memory relevant to completing it"
MAX_LEN = 1024
DOC_CHAR_CAP = 1024

tok = AutoTokenizer.from_pretrained(REPO)
model = AutoModelForCausalLM.from_pretrained(REPO, torch_dtype=torch.float32).eval()
YES = tok.convert_tokens_to_ids("yes")
NO = tok.convert_tokens_to_ids("no")
pre = tok.encode(PREFIX, add_special_tokens=False)
suf = tok.encode(SUFFIX, add_special_tokens=False)


def input_ids(query, doc):
    content = "<Instruct>: %s\n<Query>: %s\n<Document>: %s" % (INSTRUCTION, query, doc[:DOC_CHAR_CAP])
    body = tok.encode(content, add_special_tokens=False)[: MAX_LEN - len(pre) - len(suf)]
    return pre + body + suf


def reference_score(ids):
    """Official Qwen3-Reranker scoring on one unpadded sequence."""
    with torch.no_grad():
        logits = model(input_ids=torch.tensor([ids])).logits[:, -1, :]
    pair = torch.stack([logits[:, NO], logits[:, YES]], dim=1)
    return float(torch.nn.functional.log_softmax(pair, dim=1)[:, 1].exp()[0])


class YesNo(torch.nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m
        self.register_buffer("rows", torch.tensor([NO, YES]))

    def forward(self, input_ids, attention_mask):
        h = self.m.model(input_ids=input_ids, attention_mask=attention_mask, use_cache=False).last_hidden_state
        w = self.m.lm_head.weight.index_select(0, self.rows)
        return h[:, -1, :] @ w.T


CASES = [
    ("fix the context budget truncation", "fn truncate_to_token_budget(text: &str, token_budget: usize) -> Option<String>"),
    ("fix the context budget truncation", "- Always prefix shell commands with rtk"),
    ("tolong perbaiki login anthropic", "async fn login_anthropic() -> anyhow::Result<()> { /* oauth pkce */ }"),
    ("why does zindeks refresh twice", "zindeks watcher active — index updates automatically"),
    ("add emoji 🚀 support", "status line renders ASCII only\r\nno unicode width handling"),
    ("long doc", "lorem ipsum " * 500),
]
SANITY = [
    {"query": "retry flaky model streams", "relevant": "fn wait_for_model_retry(&self, attempt: u32) -> Duration { exponential backoff }",
     "irrelevant": "The TUI palette uses amber for warnings."},
    {"query": "where are auth tokens stored", "relevant": "Providers read credentials from Kode's own store (~/.kode/auth/).",
     "irrelevant": "fn format_token_count(n: u32) -> String"},
    {"query": "verification reports skipped checks", "relevant": "A skipped check is Skipped, never Passed.",
     "irrelevant": "Sessions persist to .kode/sessions as JSONL."},
    {"query": "perbaiki scroll TUI", "relevant": "fn scroll_to_bottom(state: &mut AppState) { state.scroll = 0; }",
     "irrelevant": "sha256_hex(bytes) returns lowercase hex"},
    {"query": "rename a symbol across files", "relevant": "rename_symbol performs an in-place rename across files (dry-run default)",
     "irrelevant": "ModelTierConfig { provider, model }"},
]

# ---- export ---------------------------------------------------------------
tmp = tempfile.mkdtemp()
raw = os.path.join(tmp, "model.onnx")
ex = torch.tensor([input_ids(*CASES[0])])
torch.onnx.export(
    YesNo(model), (ex, torch.ones_like(ex)), raw, opset_version=17, dynamo=False,
    input_names=["input_ids", "attention_mask"], output_names=["yes_no"],
    dynamic_axes={"input_ids": {0: "b", 1: "l"}, "attention_mask": {0: "b", 1: "l"}, "yes_no": {0: "b"}},
)
m = onnx.load(raw, load_external_data=True)
onnx_path = os.path.join(OUT, "model.onnx")
# onnx appends external data to an existing file; a rerun would double it.
for stale in (onnx_path, os.path.join(OUT, "model.onnx.data")):
    if os.path.exists(stale):
        os.remove(stale)
onnx.save_model(m, onnx_path, save_as_external_data=True, all_tensors_to_one_file=True,
                location="model.onnx.data", size_threshold=1024)
shutil.rmtree(tmp)

# ---- parity + fixtures ------------------------------------------------------
sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])


def onnx_score(ids):
    a = np.asarray([ids], dtype=np.int64)
    z = sess.run(["yes_no"], {"input_ids": a, "attention_mask": np.ones_like(a)})[0][0]
    z = z - z.max()
    return float(np.exp(z[1]) / np.exp(z).sum())


cases, worst = [], 0.0
for q, d in CASES:
    ids = input_ids(q, d)
    ref = reference_score(ids)
    worst = max(worst, abs(ref - onnx_score(ids)))
    cases.append({"query": q, "doc": d, "input_ids": ids, "score": ref})
print("max |reference - onnx| =", worst)
assert worst < 1e-4, "reranker export diverges from the official scoring"
for s in SANITY:
    assert reference_score(input_ids(s["query"], s["relevant"])) > reference_score(input_ids(s["query"], s["irrelevant"])), s

os.makedirs(os.path.dirname(FIXTURES), exist_ok=True)
with open(FIXTURES, "w", encoding="utf-8") as f:
    json.dump({"cases": cases, "sanity": SANITY}, f, ensure_ascii=False)
tok_dir = snapshot_download(REPO, allow_patterns=["tokenizer.json"])
shutil.copy(os.path.join(tok_dir, "tokenizer.json"), os.path.join(OUT, "tokenizer.json"))
with open(os.path.join(OUT, "reranker.json"), "w", encoding="utf-8") as f:
    json.dump({"prefix": PREFIX, "suffix": SUFFIX, "instruction": INSTRUCTION,
               "max_len": MAX_LEN, "doc_char_cap": DOC_CHAR_CAP}, f, ensure_ascii=False)
print("wrote", OUT, "and", FIXTURES)
