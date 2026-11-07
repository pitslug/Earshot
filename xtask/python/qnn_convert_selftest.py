"""Behavioural check of `qnn_convert.py` (T8.3c) on a tiny synthetic encoder.

Run by `cargo test -p xtask --lib qnn_convert_selftest -- --ignored` inside the converter's
hash-pinned venv; never on its own. It builds a two-feature "encoder" with NeMo's length
arithmetic (three subsampling stages of `(L + 2 - 3) / 2 + 1`, floored by casts, as the
FastConformer export has it) and one quantised Mul, writes nine calibration pieces whose
largest values sit in piece 2 (so a calibrator that keeps only the last pieces, as ONNX
Runtime's `CalibMaxIntermediateOutputs` did in T8.3, gets another range), converts it with
the real `qnn_convert.py`, and checks:

1. every piece counts: the `audio_signal` QuantizeLinear scale and zero point equal a single
   MinMax pass over all nine pieces (and differ from one over the last piece alone);
2. the length path keeps scale 1/16 and zero point 0 on every one of its QuantizeLinear
   nodes;
3. `encoded_lengths` of the QDQ model equals the float model's for every length 1..3001.
"""

import argparse
import os
import shutil
import subprocess
import sys

import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto, helper, numpy_helper

sys.dont_write_bytecode = True  # no __pycache__ in the source tree
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import qnn_convert  # noqa: E402

FRAMES = 3001
N_MELS = 2
PIECES = 9
QMAX = 65535


def build_encoder(path):
    inits = [
        numpy_helper.from_array(np.array([1.0], dtype=np.float32), "one"),
        numpy_helper.from_array(np.array([2.0], dtype=np.float32), "two"),
        numpy_helper.from_array(np.array([3.0], dtype=np.float32), "three"),
        # Large enough (6 kB quantised) to be written to `encoder.weights`, as real weights are.
        numpy_helper.from_array(
            np.linspace(0.5, 2.0, N_MELS * FRAMES, dtype=np.float32).reshape(1, N_MELS, FRAMES), "w"
        ),
    ]
    nodes = [helper.make_node("Cast", ["length"], ["l0"], to=TensorProto.FLOAT, name="len_cast")]
    cur = "l0"
    for s in range(3):
        p = f"s{s}_"
        nodes += [
            helper.make_node("Add", [cur, "one"], [p + "a"], name=p + "add"),
            helper.make_node("Add", [p + "a", "one"], [p + "b"], name=p + "add_1"),
            helper.make_node("Sub", [p + "b", "three"], [p + "c"], name=p + "sub"),
            helper.make_node("Div", [p + "c", "two"], [p + "d"], name=p + "div"),
            helper.make_node("Cast", [p + "d"], [p + "i"], to=TensorProto.INT64, name=p + "floor"),
            helper.make_node("Cast", [p + "i"], [p + "f"], to=TensorProto.FLOAT, name=p + "back"),
            helper.make_node("Add", [p + "f", "one"], [p + "out"], name=p + "add_2"),
        ]
        cur = p + "out"
    nodes += [
        helper.make_node("Cast", [cur], ["encoded_lengths"], to=TensorProto.INT64, name="len_out"),
        helper.make_node("Mul", ["audio_signal", "w"], ["outputs"], name="body"),
    ]
    g = helper.make_graph(
        nodes,
        "selftest",
        [
            helper.make_tensor_value_info("audio_signal", TensorProto.FLOAT, [1, N_MELS, "T"]),
            helper.make_tensor_value_info("length", TensorProto.INT64, ["B"]),
        ],
        [
            helper.make_tensor_value_info("outputs", TensorProto.FLOAT, [1, N_MELS, "T"]),
            helper.make_tensor_value_info("encoded_lengths", TensorProto.INT64, ["B"]),
        ],
        inits,
    )
    m = helper.make_model(g, opset_imports=[helper.make_opsetid("", 17)])
    m.ir_version = 9
    onnx.checker.check_model(m)
    onnx.save(m, path)


def write_pieces(directory):
    rng = np.random.default_rng(7)
    lengths = [1500, 3001, 400, 2999, 1, 2048, 777, 1601, 3000]
    pieces = []
    for k, n in enumerate(lengths):
        amp = 7.0 if k == 2 else 1.0 + 0.1 * k
        x = np.zeros((1, N_MELS, FRAMES), dtype=np.float32)
        x[:, :, :n] = rng.uniform(-amp, amp, size=(1, N_MELS, n)).astype(np.float32)
        if k == 2:
            x[0, 0, 0], x[0, 1, 0] = 7.0, -6.5
        np.save(os.path.join(directory, f"cal_{k:04d}.npy"), x)
        np.save(os.path.join(directory, f"cal_{k:04d}_len.npy"), np.array([n], dtype=np.int64))
        pieces.append(x)
    return pieces


def u16_scale_zp(lo, hi):
    lo, hi = min(0.0, lo), max(0.0, hi)
    scale = (hi - lo) / QMAX
    return scale, int(round(-lo / scale))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--work", required=True)
    a = p.parse_args()
    if os.path.exists(a.work):
        shutil.rmtree(a.work)
    src, cal, work, out = (os.path.join(a.work, d) for d in ("source", "cal", "work", "out"))
    for d in (src, cal):
        os.makedirs(d)
    build_encoder(os.path.join(src, "encoder.onnx"))
    for f in ("decoder.onnx", "joiner.onnx", "tokens.txt"):
        with open(os.path.join(src, f), "wb") as fh:
            fh.write(f.encode())
    pieces = write_pieces(cal)

    subprocess.run(
        [sys.executable, "-I", os.path.join(HERE, "qnn_convert.py"),
         "--source", src, "--calibration", cal, "--pieces", str(PIECES),
         "--n-mels", str(N_MELS), "--frames", str(FRAMES), "--recipe", "selftest",
         "--work", work, "--out", out],
        check=True,
    )

    q = onnx.load(os.path.join(out, "encoder.onnx"))
    inits = {t.name: numpy_helper.to_array(t) for t in q.graph.initializer}
    qdq = [n for n in q.graph.node if n.op_type in ("QuantizeLinear", "DequantizeLinear")]

    def qparams(tensor):
        # ONNX Runtime names a tensor's parameters `<tensor>_scale` / `<tensor>_zero_point`,
        # on the QuantizeLinear of an activation and on the DequantizeLinear of a constant
        # (constants are quantised offline).
        found = [n for n in qdq if n.input[1] == f"{tensor}_scale"]
        assert found, f"no quantisation of {tensor}"
        return [(float(inits[n.input[1]]), int(inits[n.input[2]])) for n in found]

    # 1. Every piece counts.
    allx = np.concatenate([x.ravel() for x in pieces])
    want = u16_scale_zp(float(allx.min()), float(allx.max()))
    last = u16_scale_zp(float(pieces[-1].min()), float(pieces[-1].max()))
    assert abs(want[0] - last[0]) > 1e-3 * want[0], "the data must tell the two apart"
    for scale, zp in qparams("audio_signal"):
        assert abs(scale - want[0]) <= 1e-6 * want[0] and abs(zp - want[1]) <= 1, (
            f"audio_signal scale {scale} zp {zp}; all pieces give {want}, the last alone {last}"
        )

    # 2. The length path keeps its fixed scale.
    lengths = qnn_convert.length_path_tensors(os.path.join(work, "prepared.onnx"))
    expect = {"l0", "one", "two", "three"} | {
        f"s{s}_{t}" for s in range(3) for t in ("a", "b", "c", "d", "f", "out")
    }
    assert set(lengths) == expect, f"length path {sorted(lengths)}, expected {sorted(expect)}"
    assert qnn_convert.LENGTH_SCALE == 1.0 / 16.0
    for t in lengths:
        for scale, zp in qparams(t):
            assert scale == 1.0 / 16.0 and zp == 0, f"{t}: scale {scale} zp {zp}"

    # 3. Exact lengths for every input length.
    so = ort.SessionOptions()
    so.log_severity_level = 3
    fs = ort.InferenceSession(os.path.join(work, "prepared.onnx"), so, providers=["CPUExecutionProvider"])
    qs = ort.InferenceSession(os.path.join(out, "encoder.onnx"), so, providers=["CPUExecutionProvider"])
    x = np.zeros((1, N_MELS, FRAMES), dtype=np.float32)
    bad = []
    for n in range(1, FRAMES + 1):
        feed = {"audio_signal": x, "length": np.array([n], dtype=np.int64)}
        f = int(fs.run(["encoded_lengths"], feed)[0][0])
        g = int(qs.run(["encoded_lengths"], feed)[0][0])
        if f != g:
            bad.append((n, f, g))
    assert not bad, f"{len(bad)} lengths differ, first {bad[:5]}"
    print(f"qnn_convert selftest: ok ({len(lengths)} length tensors, {PIECES} pieces)")


if __name__ == "__main__":
    main()
