# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "torch>=2.3",
#     "transformers>=4.48",
#     "safetensors>=0.4",
#     "huggingface_hub>=0.24",
#     "onnx>=1.16",
#     "onnxruntime>=1.22",
#     "numpy>=1.26",
# ]
#
# [[tool.uv.index]]
# name = "pytorch-cu126"
# url = "https://download.pytorch.org/whl/cu126"
# explicit = true
#
# [tool.uv.sources]
# torch = { index = "pytorch-cu126" }
# ///
"""Fine-tune Laya (multilingual) on Kode router labels and export ONNX.

Embedded in the kode binary and run by `kode router train`:
  local:  uv run train_laya.py --train train.jsonl --out <dir>
  remote: hf jobs uv run ... train_laya.py --hub-dataset <repo> --hub-train <path> --push-branch <branch>

Recipe: convaiinnovations/laya fine-tune notebook, on one GPU without DDP.
Export: same as scripts/local-models/build_laya.py (keep the two in sync).
Temperatures stay 1.0 here; Kode calibrates them in Rust.
"""
import argparse
import json
import os
import random
import shutil
import sys
import tempfile
import time

import numpy as np
import torch

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("--train", help="local train.jsonl written by `kode router train`")
ap.add_argument("--out", help="output dir: model.onnx, laya.json, tokenizer.json, train_report.json")
ap.add_argument("--hub-dataset", help="HF dataset repo holding the train file (remote runs)")
ap.add_argument("--hub-train", help="path of the train file inside --hub-dataset")
ap.add_argument("--push-branch", help="branch of --hub-dataset to push outputs to (remote runs)")
ap.add_argument("--epochs", type=int, default=4)
args = ap.parse_args()

if not torch.cuda.is_available():
    sys.exit("train_laya.py needs a CUDA GPU; use `kode router train --remote` to train on HF Jobs")

from huggingface_hub import HfApi, hf_hub_download, snapshot_download  # noqa: E402

remote = bool(args.hub_dataset)
train_path = hf_hub_download(args.hub_dataset, args.hub_train, repo_type="dataset") if remote else args.train
out = args.out or tempfile.mkdtemp(prefix="kode-candidate-")
os.makedirs(out, exist_ok=True)

src = snapshot_download("convaiinnovations/laya",
                        allow_patterns=["rl_common.py", "rl_agent_api.py", "multilingual/*"])
sys.path.insert(0, src)
from rl_agent_api import RLAgent  # noqa: E402
from rl_common import QTYPES, build_sequence, proper_reward, render_options  # noqa: E402

device = torch.device("cuda")
agent = RLAgent(os.path.join(src, "multilingual"), device="cuda")
model, tok, cfg = agent.model, agent.tok, agent.cfg


def to_internal(q):
    """Kode QuestionDef json -> rl_common internal question."""
    if q["qtype"] == "choice":
        crit = {k: (v or None) for k, v in q["options"]}
    elif q["qtype"] == "score":
        crit = [v for _, v in q["options"]]
    else:
        crit = None
    return {"t": q["qtype"], "ins": q["instructions"], "crit": crit}


# ---- data ---------------------------------------------------------------------
items = []
with open(train_path, encoding="utf-8") as f:
    for line in f:
        row = json.loads(line)
        q = to_internal(row["question"])
        ids, markers = build_sequence(tok, row["state"], q, cfg["max_len"], cfg["head_max_len"])
        target = row["target"]
        if len(markers) != len(render_options(q)) or len(target) != len(markers):
            continue
        items.append({"ids": ids, "markers": markers, "qtype": QTYPES[q["t"]], "target": target})
if not items:
    sys.exit("no usable training rows")
print(f"{len(items)} training rows", flush=True)


def collate(batch):
    n, L = len(batch), max(len(it["ids"]) for it in batch)
    kmax = max(len(it["markers"]) for it in batch)
    ids = torch.full((n, L), tok.pad_token_id, dtype=torch.long)
    att = torch.zeros((n, L), dtype=torch.long)
    mpos = torch.zeros((n, kmax), dtype=torch.long)
    mmask = torch.zeros((n, kmax), dtype=torch.bool)
    target = torch.zeros((n, kmax), dtype=torch.float32)
    for i, it in enumerate(batch):
        ids[i, :len(it["ids"])] = torch.tensor(it["ids"])
        att[i, :len(it["ids"])] = 1
        k = len(it["markers"])
        mpos[i, :k] = torch.tensor(it["markers"])
        mmask[i, :k] = True
        target[i, :k] = torch.tensor(it["target"], dtype=torch.float32)
    return {"input_ids": ids, "attention_mask": att, "marker_pos": mpos, "marker_mask": mmask,
            "target": target, "qtype": torch.tensor([it["qtype"] for it in batch])}


# ---- train (fine-tune notebook recipe, single GPU) -----------------------------
EPOCHS, MICRO_BATCH, GRAD_ACCUM, GROUP_SIZE = args.epochs, 8, 4, 4
LR_ENCODER, LR_HEAD, SIGMA_START, SIGMA_END = 2.5e-5, 1.0e-4, 0.4, 0.1

model.encoder.gradient_checkpointing_enable(gradient_checkpointing_kwargs={"use_reentrant": False})
model.head_checkpointing = True
model.train()
enc_params = [p for n, p in model.named_parameters() if n.startswith("encoder.")]
head_params = [p for n, p in model.named_parameters() if not n.startswith("encoder.")]
opt = torch.optim.AdamW([{"params": enc_params, "lr": LR_ENCODER},
                         {"params": head_params, "lr": LR_HEAD}], weight_decay=0.01)
total_updates = max(1, (len(items) // (MICRO_BATCH * GRAD_ACCUM)) * EPOCHS)
sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=total_updates, eta_min=1e-6)
scaler = torch.amp.GradScaler("cuda")

t0 = time.time()
final_loss = float("nan")
for epoch in range(EPOCHS):
    random.Random(42 + epoch).shuffle(items)
    sigma = SIGMA_START + (SIGMA_END - SIGMA_START) * (epoch / max(1, EPOCHS - 1))
    opt.zero_grad(set_to_none=True)
    steps, epoch_loss = 0, 0.0
    for i in range(0, len(items), MICRO_BATCH):
        b = collate(items[i:i + MICRO_BATCH])
        with torch.autocast("cuda", dtype=torch.float16):
            logits, act = model(b["input_ids"].to(device), b["attention_mask"].to(device),
                                b["marker_pos"].to(device), b["marker_mask"].to(device), b["qtype"].to(device))
        logits = logits.float()
        mask = b["marker_mask"].to(device)
        k = mask.sum(-1, keepdim=True).float()
        target = b["target"].to(device)
        eps = torch.randn((GROUP_SIZE,) + logits.shape, device=device) * sigma * mask
        eps = (eps - eps.sum(-1, keepdim=True) / k) * mask
        z = logits.detach().unsqueeze(0) + eps
        q = torch.softmax(z.masked_fill(~mask, -1e4), -1)
        with torch.no_grad():
            r = proper_reward(q, target.unsqueeze(0), b["qtype"].to(device), mask, w_sph=0.75, w_rps=1.0)
            adv = r - r.mean(0, keepdim=True)
            adv = adv / (adv.std() + 1e-6)
        logp = -(((z - logits.unsqueeze(0)) ** 2) * mask).sum(-1) / (2 * sigma ** 2)
        loss_rl = -(adv * logp).mean()
        loss_ce = -(target * torch.log_softmax(logits.masked_fill(~mask, -1e4), -1)).sum(-1).mean()
        loss = (loss_rl + loss_ce) / GRAD_ACCUM + 0.0 * act.sum()
        scaler.scale(loss).backward()
        steps += 1
        epoch_loss += loss.item() * GRAD_ACCUM
        if steps % GRAD_ACCUM == 0 or i + MICRO_BATCH >= len(items):
            scaler.unscale_(opt)
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            scaler.step(opt)
            scaler.update()
            sched.step()
            opt.zero_grad(set_to_none=True)
    final_loss = epoch_loss / max(1, steps)
    print(f"epoch {epoch + 1}/{EPOCHS} loss {final_loss:.4f} ({time.time() - t0:.0f}s)", flush=True)
train_seconds = time.time() - t0


# ---- export (keep in sync with scripts/local-models/build_laya.py) -------------
model.encoder.gradient_checkpointing_disable()
model.head_checkpointing = False
model = model.float().cpu().eval()
model.encoder.config._attn_implementation = "eager"


def head_layer(layer, x, keep):
    """Pre-LN nn.TransformerEncoderLayer with length-free reshapes (see build_laya.py)."""
    attn = layer.self_attn
    nh = attn.num_heads
    hd = attn.embed_dim // nh
    q, k, v = torch.nn.functional.linear(layer.norm1(x), attn.in_proj_weight, attn.in_proj_bias).chunk(3, dim=-1)
    q, k, v = (t.unflatten(-1, (nh, hd)).transpose(1, 2) for t in (q, k, v))
    a = torch.nn.functional.scaled_dot_product_attention(q, k, v, attn_mask=keep)
    x = x + attn.out_proj(a.transpose(1, 2).flatten(2))
    return x + layer.linear2(layer.activation(layer.linear1(layer.norm2(x))))


class Wrapped(torch.nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids, attention_mask, marker_pos, marker_mask, qtype):
        m = self.m
        h = m.encoder(input_ids=input_ids, attention_mask=attention_mask).last_hidden_state
        h = h + m.type_emb(qtype)[:, None, :]
        keep = attention_mask.bool()[:, None, None, :]
        for layer in m.head.layers:
            h = head_layer(layer, h, keep)
        idx = marker_pos.clamp(min=0)[:, :, None].expand(-1, -1, h.size(-1))
        logits = m.scorer(torch.gather(h, 1, idx)).squeeze(-1).float()
        return logits.masked_fill(~marker_mask.bool(), -1e4)


def tensors(it):
    k = len(it["markers"])
    return (torch.tensor([it["ids"]], dtype=torch.long),
            torch.ones((1, len(it["ids"])), dtype=torch.long),
            torch.tensor([it["markers"]], dtype=torch.long),
            torch.ones((1, k), dtype=torch.long),
            torch.tensor([it["qtype"]], dtype=torch.long))


# References before export: torch.onnx.export perturbs the in-process model.
probe = items[:16]
with torch.no_grad():
    refs = []
    for it in probe:
        t = tensors(it)
        refs.append((t, model(t[0], t[1], t[2], t[3].bool(), t[4])[0][0].numpy()))

onnx_path = os.path.join(out, "model.onnx")
torch.onnx.export(
    Wrapped(model), tensors(items[0]), onnx_path, opset_version=17, dynamo=False,
    input_names=["input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"],
    output_names=["logits"],
    dynamic_axes={"input_ids": {0: "b", 1: "l"}, "attention_mask": {0: "b", 1: "l"},
                  "marker_pos": {0: "b", 1: "k"}, "marker_mask": {0: "b", 1: "k"},
                  "qtype": {0: "b"}, "logits": {0: "b", 1: "k"}},
)

import onnxruntime as ort  # noqa: E402

sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
names = ["input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"]
worst = 0.0
for t, ref in refs:
    got = sess.run(["logits"], {n: v.numpy() for n, v in zip(names, t)})[0][0]
    worst = max(worst, float(np.abs(ref - got).max()))
print(f"max |torch - onnx| = {worst}", flush=True)
if worst >= 1e-4:
    sys.exit(f"ONNX export diverges from PyTorch ({worst})")

shutil.copy(os.path.join(src, "multilingual", "tokenizer", "tokenizer.json"), os.path.join(out, "tokenizer.json"))
with open(os.path.join(out, "laya.json"), "w", encoding="utf-8") as f:
    json.dump({"max_len": cfg["max_len"], "head_max_len": cfg["head_max_len"],
               "temperature": [1.0, 1.0, 1.0], "temperature_by_options": {},
               "cls_id": tok.cls_token_id, "sep_id": tok.sep_token_id,
               "mask_id": tok.mask_token_id, "mask_token": tok.mask_token}, f, ensure_ascii=False)
with open(os.path.join(out, "train_report.json"), "w", encoding="utf-8") as f:
    json.dump({"rows": len(items), "epochs": EPOCHS, "final_loss": final_loss,
               "train_seconds": round(train_seconds, 1), "parity": worst}, f)

if remote:
    api = HfApi()
    api.create_branch(args.hub_dataset, repo_type="dataset", branch=args.push_branch, exist_ok=True)
    api.upload_folder(folder_path=out, repo_id=args.hub_dataset, repo_type="dataset",
                      revision=args.push_branch, path_in_repo="candidate",
                      commit_message="kode router candidate")
print("wrote", out, flush=True)
