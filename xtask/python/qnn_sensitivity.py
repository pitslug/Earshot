"""Precision sensitivity and dev error of Parakeet NPU conversions (T8.3c, a measurement tool).

Not part of `cargo xtask qnn-convert`. Run with the converter's hash-pinned venv
(`target\\qnn-convert\\venv\\Scripts\\python.exe -I xtask\\python\\qnn_sensitivity.py ...`).

The dev pieces are the first dev-clean utterance of each of the first 16 speakers of the
pinned subset (`fixtures/librispeech/subset.toml`, in file order; the dev WER set, never
calibration data), as single pieces at the static shape:

    qnn_sensitivity.py dev-plan --subset fixtures\\librispeech\\subset.toml
        --fixtures <EARSHOT_FIXTURES> --count 16 --out target\\qnn-convert\\devplan.txt
    target\\release\\examples\\parakeet_calibration.exe 80 target\\qnn-convert\\devplan.txt
        target\\qnn-convert\\dev-80

Error measure everywhere: the mean over dev pieces of the mean over valid output frames of
the relative L2 error of the encoder output against the float static model (`prepared.onnx`
of a conversion's work directory).

    qnn_sensitivity.py compare --model <work>\\prepared.onnx --dev <dev dir>
        --encoder <label>=<conversion dir>\\encoder.onnx ... --out <json>

measures finished conversions (`qnn-convert --out`), the T8.3c tie-break.

    qnn_sensitivity.py sweep --model <work>\\prepared.onnx --calibration <cal dir>
        --pieces 80 --dev <dev dir> --work <scratch> [--groups layer|role] [--promote a,b]
        [--method minmax|entropy|percentile] [--per-channel] [--a16-only]

starts from 8-bit activations everywhere (A8W8, the converter's exact length path, the
given calibration method and weight granularity) and promotes one group of activations at a
time to 16 bit, next to uniform A8W8 and A16W8 (`--a16-only`: only the A16W8 reference).
Groups: `layer` = the pre-encoder and each conformer layer (node name prefix); `role` = the
activations feeding each operator type across the whole encoder. Calibration runs once per
method (its ranges do not depend on the activation type or the weights) and is cached in
--work. The entropy and percentile calibrators write a temporary `<model>-inferred.onnx`
next to --model: never run two of them on the same model file at once.
"""

import argparse
import glob
import json
import os
import re
import sys
import time
import tomllib

import numpy as np
import onnx
import onnxruntime as ort
from onnxruntime.quantization import QuantType, quantize_static
from onnxruntime.quantization.execution_providers.qnn import get_qnn_qdq_config

sys.dont_write_bytecode = True  # no __pycache__ in the source tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import qnn_convert  # noqa: E402


def pieces(directory):
    files = sorted(f for f in glob.glob(os.path.join(directory, "cal_*.npy")) if not f.endswith("_len.npy"))
    if not files:
        sys.exit(f"no pieces in {directory}")
    return [(np.load(f), np.load(f[:-4] + "_len.npy")) for f in files]


def session(path, threads=8):
    so = ort.SessionOptions()
    so.intra_op_num_threads = threads
    so.log_severity_level = 3
    return ort.InferenceSession(path, so, providers=["CPUExecutionProvider"])


def reference(model, dev):
    s = session(model)
    ref = []
    for x, l in dev:
        o, n = s.run(["outputs", "encoded_lengths"], {"audio_signal": x, "length": l})
        ref.append((o[0, :, : int(n[0])], int(n[0])))
    return ref


def rel_error(ref, sess, data):
    errs, mismatched = [], 0
    for (x, l), (oa, n) in zip(data, ref):
        ob, nb = sess.run(["outputs", "encoded_lengths"], {"audio_signal": x, "length": l})
        mismatched += int(nb[0]) != n
        ob = ob[0, :, :n]
        e = np.linalg.norm(oa - ob, axis=0) / (np.linalg.norm(oa, axis=0) + 1e-9)
        errs.append(float(e.mean()))
    return float(np.mean(errs)), mismatched


def dev_plan(a):
    u = [e for e in tomllib.load(open(a.subset, "rb"))["utterance"] if e["split"] == "dev-clean"]
    seen, pick = set(), []
    for e in u:
        if e["speaker"] not in seen:
            seen.add(e["speaker"])
            pick.append(e)
    pick = pick[: a.count]
    with open(a.out, "w", encoding="utf-8", newline="\n") as f:
        for e in pick:
            s, c, _ = e["id"].split("-")
            f.write(os.path.join(a.fixtures, "librispeech/dev-clean/LibriSpeech/dev-clean", s, c,
                                 e["id"] + ".flac") + "\n")
    print(f"{len(pick)} dev pieces planned in {a.out}")


def compare(a):
    dev = pieces(a.dev)
    ref = reference(a.model, dev)
    results = {"measure": "mean relative L2 error of the encoder output vs the float static "
                          "model over valid frames, mean over dev pieces",
               "dev_pieces": len(dev), "encoders": {}}
    for item in a.encoder:
        label, path = item.split("=", 1)
        e, mism = rel_error(ref, session(path), dev)
        results["encoders"][label] = {"relative_error": e, "length_mismatches": mism}
        print(f"{label}: {e:.4f} ({mism}/{len(dev)} length mismatches)", flush=True)
    with open(a.out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(results, f, indent=1)
        f.write("\n")


def sweep(a):
    os.makedirs(a.work, exist_ok=True)
    dev = pieces(a.dev)
    ref = reference(a.model, dev)
    g = onnx.load(a.model, load_external_data=False).graph
    inits = {t.name for t in g.initializer}
    lengths = qnn_convert.length_path_tensors(a.model)
    groups = {}
    if a.groups == "layer":
        for n in g.node:
            m = re.match(r"/(layers\.\d+|pre_encode)/", n.name)
            key = m.group(1) if m else "other"
            groups.setdefault(key, set()).update(o for o in n.output if o)
    else:
        for n in g.node:
            groups.setdefault(n.op_type, set()).update(i for i in n.input if i and i not in inits)
    promoted = [x for x in a.promote.split(",") if x]

    reader = qnn_convert.Pieces(a.calibration)
    if len(reader) != a.pieces:
        sys.exit(f"{len(reader)} calibration pieces, {a.pieces} expected")
    cache = os.path.join(a.work, f"calibration-{a.method}.json")
    method = qnn_convert.METHODS[a.method]

    def convert(name, act, sixteen):
        ov = {t: [{"quant_type": QuantType.QUInt16}] for t in sixteen if t not in lengths}
        ov.update({
            t: [{"quant_type": QuantType.QUInt16,
                 "scale": np.array(qnn_convert.LENGTH_SCALE, dtype=np.float32),
                 "zero_point": np.array(0, dtype=np.uint16)}]
            for t in lengths
        })
        cfg = get_qnn_qdq_config(a.model, reader, calibrate_method=method,
                                 activation_type=act, weight_type=QuantType.QUInt8,
                                 per_channel=a.per_channel, init_overrides=ov, stride=1)
        out = os.path.join(a.work, f"{name}.onnx")
        for f in (out, out + ".data"):
            if os.path.exists(f):
                os.remove(f)
        quantize_static(a.model, out, cfg.calibration_data_reader,
                        calibrate_method=cfg.calibrate_method, quant_format=cfg.quant_format,
                        activation_type=cfg.activation_type, weight_type=cfg.weight_type,
                        op_types_to_quantize=cfg.op_types_to_quantize,
                        nodes_to_exclude=cfg.nodes_to_exclude, per_channel=cfg.per_channel,
                        use_external_data_format=True, extra_options=cfg.extra_options,
                        calibration_cache_path=cache)
        e, _ = rel_error(ref, session(out), dev)
        for f in (out, out + ".data"):
            if os.path.exists(f):
                os.remove(f)
        return e

    tag = f"{a.method}{'-pc' if a.per_channel else ''}"
    results = {"method": a.method, "per_channel": a.per_channel, "dev_pieces": len(dev)}
    t = time.time()
    results["A16W8 (all)"] = convert("a16", QuantType.QUInt16, set())
    print(json.dumps(results), flush=True)
    if not a.a16_only:
        base16 = set().union(*(groups[k] for k in promoted)) if promoted else set()
        results["A8W8 base" + (f" + {a.promote}" if promoted else "")] = convert("a8", QuantType.QUInt8, base16)
        for k in sorted(groups):
            if k in promoted:
                continue
            results[k] = convert("g", QuantType.QUInt8, base16 | groups[k])
            print(f"{k}: {results[k]:.4f} ({len(groups[k])} tensors, {time.time() - t:.0f} s)", flush=True)
    name = f"a16-{tag}.json" if a.a16_only else f"sensitivity-{a.groups}-{tag}.json"
    with open(os.path.join(a.work, name), "w", encoding="utf-8", newline="\n") as f:
        json.dump(results, f, indent=1)
        f.write("\n")


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("dev-plan")
    d.add_argument("--subset", required=True)
    d.add_argument("--fixtures", required=True)
    d.add_argument("--count", type=int, default=16)
    d.add_argument("--out", required=True)
    c = sub.add_parser("compare")
    c.add_argument("--model", required=True)
    c.add_argument("--dev", required=True)
    c.add_argument("--encoder", action="append", required=True, help="<label>=<encoder.onnx>")
    c.add_argument("--out", required=True)
    s = sub.add_parser("sweep")
    s.add_argument("--model", required=True)
    s.add_argument("--calibration", required=True)
    s.add_argument("--pieces", type=int, required=True)
    s.add_argument("--dev", required=True)
    s.add_argument("--work", required=True)
    s.add_argument("--groups", choices=["layer", "role"], default="layer")
    s.add_argument("--promote", default="", help="comma-separated groups already at 16 bit")
    s.add_argument("--method", choices=sorted(qnn_convert.METHODS), default="minmax")
    s.add_argument("--per-channel", action="store_true")
    s.add_argument("--a16-only", action="store_true")
    a = p.parse_args()
    {"dev-plan": dev_plan, "compare": compare, "sweep": sweep}[a.cmd](a)


if __name__ == "__main__":
    main()
