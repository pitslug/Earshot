"""Converts a Parakeet TDT encoder for Qualcomm's HTP backend (T8.3).

Run by `cargo xtask qnn-convert` inside a virtual environment built from
`requirements-qnn-convert.txt` (hash-pinned); never on its own. There is no Rust route for
these steps: ONNX Runtime's quantisation tools (and the QNN-specific QDQ configuration) exist
only in its Python package.

Steps (each written to --work, the result to --out):
1. Fix the encoder inputs to one 30 s piece: `audio_signal` [1, n_mels, frames] and `length`
   [1] (the HTP backend needs static shapes); fix the output shapes.
2. Fold constants with ONNX Runtime's basic graph optimisations on the CPU (the shape
   arithmetic of the dynamic export becomes constants). ONNX Runtime's QNN preprocessing
   (fusions) is not run: it changes nothing in these encoders and cannot handle 2 GB models.
3. Quantise to QDQ for QNN: `get_qnn_qdq_config` with the recipe's activation type,
   calibration method and weight granularity (8-bit weights; the recipes are
   `xtask::qnn_convert::RECIPES`), strided calibration over every piece written by the
   `parakeet_calibration` example (LibriSpeech dev-clean minus the pinned subset, never the
   test sets), and the length arithmetic held exact (`length_path_tensors`, T8.3c).
4. Write `encoder.onnx` with its weights in `encoder.weights` (the source's custom metadata
   restored; ONNX Runtime 1.31's QNN preprocessing fails on metadata, so it is removed for the
   quantisation steps), copy the float `decoder.onnx`, `joiner.onnx` and `tokens.txt` (they
   run on the CPU), and record everything in `conversion.json`.
"""

import argparse
import glob
import hashlib
import json
import os
import platform
import shutil
import sys
import time

import numpy as np
import onnx
import onnxruntime as ort
from onnxruntime.quantization import CalibrationDataReader, CalibrationMethod, QuantType, quantize
from onnxruntime.quantization.execution_providers.qnn import get_qnn_qdq_config
from onnxruntime.tools.onnx_model_utils import make_input_shape_fixed

ACTIVATIONS = {"u16": QuantType.QUInt16, "u8": QuantType.QUInt8}
METHODS = {
    "minmax": CalibrationMethod.MinMax,
    "entropy": CalibrationMethod.Entropy,
    "percentile": CalibrationMethod.Percentile,
}


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def log(msg):
    print(f"qnn_convert: {msg}", flush=True)


class Pieces(CalibrationDataReader):
    """The calibration pieces, read one range at a time (`set_range`, ONNX Runtime's strided
    calibration): each range's activations are reduced and merged before the next range is
    run, so memory holds one piece's activations and every piece counts."""

    def __init__(self, directory):
        self.files = sorted(
            f for f in glob.glob(os.path.join(directory, "cal_*.npy")) if not f.endswith("_len.npy")
        )
        if not self.files:
            sys.exit(f"qnn_convert: no calibration pieces in {directory}")
        self.i = 0
        self.end = len(self.files)

    def __len__(self):
        return len(self.files)

    def set_range(self, start_index, end_index):
        self.i = start_index
        self.end = min(end_index, len(self.files))

    def get_next(self):
        if self.i >= self.end:
            return None
        f = self.files[self.i]
        self.i += 1
        return {"audio_signal": np.load(f), "length": np.load(f[:-4] + "_len.npy")}

    def rewind(self):
        self.i = 0
        self.end = len(self.files)


# Scale of every float tensor on the encoder's length path: 2^-4 with zero point 0 holds
# every multiple of 1/16 up to 4095.94 exactly in 16 bits. The length arithmetic (NeMo's
# `calc_length`: (L + 2 * pad - kernel) / stride + 1 per subsampling stage, floored by casts)
# only produces multiples of 1/2 up to the input length (3001 frames), so it stays exact.
LENGTH_SCALE = 1.0 / 16.0
LENGTH_OPS = {"Cast", "Add", "Sub", "Mul", "Div", "Floor", "Ceil"}


def length_path_tensors(model_path):
    """The float tensors of the encoder's length arithmetic: everything reachable from the
    `length` input through elementwise ops whose output has one element (the per-utterance
    length), plus their constant inputs. Calibrated like activations, these get a range from
    the calibration lengths and a step of about 0.05 frames, so a value such as 261.0 becomes
    260.98 and the next cast floors it to 260: one encoder frame lost at the end of the piece
    and in every length mask (T8.3c measured it on dev-clean)."""
    g = onnx.shape_inference.infer_shapes(onnx.load(model_path, load_external_data=False)).graph
    shapes = {}
    for v in list(g.value_info) + list(g.input) + list(g.output):
        t = v.type.tensor_type
        dims = [d.dim_value if d.HasField("dim_value") else -1 for d in t.shape.dim]
        shapes[v.name] = (t.elem_type, dims)
    inits = {t.name: t for t in g.initializer}
    # Nodes by index: node names are optional in ONNX, so they cannot key the visited set.
    consumers = {}
    for k, n in enumerate(g.node):
        for i in n.input:
            consumers.setdefault(i, []).append((k, n))

    def one_element(name):
        if name in inits:
            return int(np.prod(inits[name].dims)) == 1
        s = shapes.get(name)
        return s is not None and -1 not in s[1] and int(np.prod(s[1])) == 1

    def is_float(name):
        if name in inits:
            return inits[name].data_type == onnx.TensorProto.FLOAT
        s = shapes.get(name)
        return s is not None and s[0] == onnx.TensorProto.FLOAT

    found, frontier, seen = set(), ["length"], set()
    while frontier:
        nxt = []
        for t in frontier:
            for k, n in consumers.get(t, []):
                if k in seen or n.op_type not in LENGTH_OPS:
                    continue
                if not all(one_element(o) for o in n.output):
                    continue
                seen.add(k)
                for name in list(n.input) + list(n.output):
                    if name and is_float(name) and one_element(name):
                        found.add(name)
                nxt.extend(n.output)
        frontier = nxt
    return sorted(found)


def save_external(model, path, data_name):
    onnx.save(model, path, save_as_external_data=True, all_tensors_to_one_file=True, location=data_name)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--source", required=True)
    p.add_argument("--calibration", required=True)
    p.add_argument("--n-mels", type=int, required=True)
    p.add_argument("--frames", type=int, required=True)
    p.add_argument("--activations", choices=sorted(ACTIVATIONS), default="u16")
    p.add_argument("--calibration-method", choices=sorted(METHODS), default="minmax")
    p.add_argument("--per-channel", action="store_true")
    p.add_argument(
        "--a16-roles",
        default="",
        help="comma-separated operator types whose activation inputs stay 16 bit when the "
        "activations are 8 bit (mixed precision)",
    )
    p.add_argument("--recipe", default="unnamed")
    p.add_argument("--calibration-set", default="unspecified")
    p.add_argument("--float-only", action="store_true")
    p.add_argument("--pieces", type=int, required=True)
    p.add_argument("--work", required=True)
    p.add_argument("--out", required=True)
    a = p.parse_args()
    os.makedirs(a.work, exist_ok=True)
    os.makedirs(a.out, exist_ok=True)
    started = time.time()

    src = os.path.join(a.source, "encoder.onnx")
    log(f"loading {src}")
    # The graph only: the 0.6B encoders keep 2.4 GB of weights in `encoder.weights`, beyond
    # protobuf's 2 GB message limit. The edited graph keeps its references to that file,
    # which is copied next to it.
    m = onnx.load(src, load_external_data=False)
    meta = {e.key: e.value for e in m.metadata_props}
    make_input_shape_fixed(m.graph, "audio_signal", [1, a.n_mels, a.frames])
    make_input_shape_fixed(m.graph, "length", [1])
    del m.metadata_props[:]
    external = sorted({
        e.value for t in m.graph.initializer for e in t.external_data if e.key == "location"
    })
    for f in external:
        log(f"copying {f}")
        shutil.copyfile(os.path.join(a.source, f), os.path.join(a.work, f))
    unfixed = os.path.join(a.work, "inputs-fixed.onnx")
    onnx.save(m, unfixed)
    # The output shapes follow from the fixed inputs (onnxruntime's `fix_output_shapes`, done
    # through files because the in-memory form cannot hold more than 2 GB).
    inferred = os.path.join(a.work, "inferred.onnx")
    onnx.shape_inference.infer_shapes_path(unfixed, inferred)
    shapes = onnx.load(inferred, load_external_data=False)
    for o, n in zip(m.graph.output, shapes.graph.output):
        dims = n.type.tensor_type.shape.dim
        if all(d.HasField("dim_value") for d in dims):
            o.type.tensor_type.shape.CopyFrom(n.type.tensor_type.shape)
    log("outputs: " + ", ".join(
        f"{o.name} {[d.dim_value or d.dim_param for d in o.type.tensor_type.shape.dim]}"
        for o in m.graph.output
    ))
    fixed = os.path.join(a.work, "fixed.onnx")
    onnx.save(m, fixed)
    del m, shapes

    log("folding constants (ONNX Runtime basic optimisations)")
    folded = os.path.join(a.work, "folded.onnx")
    so = ort.SessionOptions()
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
    so.optimized_model_filepath = folded
    so.add_session_config_entry("session.optimized_model_external_initializers_file_name", "folded.weights")
    so.add_session_config_entry("session.optimized_model_external_initializers_min_size_in_bytes", "1024")
    ort.InferenceSession(fixed, so, providers=["CPUExecutionProvider"])

    # `qnn_preprocess_model` (QNN-specific fusions) is not run: it changed nothing in the
    # FastConformer encoder (measured on the 110M, 2026-10-24: its LayerNorms are already
    # fused by the export and it has no Gelu, LpNorm or SpaceToDepth patterns), and it
    # serialises the whole model in memory, which fails beyond 2 GB (the 0.6B encoders).
    model_in = folded

    # 16-bit QDQ needs opset 21. The quantiser would convert the opset itself, but in memory
    # (which fails beyond 2 GB): convert the graph alone here, keeping its weight file.
    g = onnx.load(folded, load_external_data=False)
    opset = next(o.version for o in g.opset_import if o.domain in ("", "ai.onnx"))
    if opset < 21:
        log(f"opset {opset} -> 21")
        g = onnx.version_converter.convert_version(g, 21)
    model_in = os.path.join(a.work, "prepared.onnx")
    onnx.save(g, model_in)
    del g

    stale = os.path.join(a.out, "encoder.weights")
    if os.path.exists(stale):
        os.remove(stale)

    if a.float_only:
        # Diagnostic: the static float model, unquantised (separates the cost of the static
        # shape from the cost of quantisation). The graph only, its weight file copied under
        # the output name (the 0.6B encoders are over 2 GB).
        f = onnx.load(model_in, load_external_data=False)
        for k, v in meta.items():
            e = f.metadata_props.add()
            e.key, e.value = k, v
        files = set()
        for t in f.graph.initializer:
            for e in t.external_data:
                if e.key == "location":
                    files.add(e.value)
                    e.value = "encoder.weights"
        if len(files) > 1:
            sys.exit(f"qnn_convert: weights in several files {sorted(files)}")
        for name in files:
            shutil.copyfile(os.path.join(a.work, name), stale)
        onnx.save(f, os.path.join(a.out, "encoder.onnx"))
        for name in ["decoder.onnx", "joiner.onnx", "tokens.txt"]:
            shutil.copyfile(os.path.join(a.source, name), os.path.join(a.out, name))
        log(f"wrote the static float model to {a.out} (diagnostic, not for the NPU)")
        return

    reader = Pieces(a.calibration)
    if len(reader.files) != a.pieces:
        sys.exit(
            f"qnn_convert: {a.calibration} holds {len(reader.files)} calibration pieces, "
            f"{a.pieces} expected"
        )
    log(
        f"quantising (recipe {a.recipe}: A{a.activations[1:]}W8, {a.calibration_method}, "
        f"weights per {'channel' if a.per_channel else 'tensor'}) with {len(reader.files)} "
        "calibration pieces"
    )
    # Strided calibration, one piece per stride: ONNX Runtime reduces and merges each
    # stride's activations before running the next, so every piece counts and memory holds
    # one piece's activations (the 0.6B encoders over 3001 frames). Never
    # `CalibMaxIntermediateOutputs`: ONNX Runtime 1.31's MinMaxCalibrater.collect_data drops
    # the buffered outputs when that limit is reached without reducing them, so only the
    # pieces after the last flush were calibrated (T8.3 calibrated on 1 of 89 pieces, and
    # every length past that piece's was clipped: T8.3c).
    lengths = length_path_tensors(model_in)
    if not lengths:
        sys.exit("qnn_convert: found no length arithmetic from the `length` input")
    log(f"length path: {len(lengths)} tensors held exact (16 bit, scale 1/16)")
    overrides = {}
    roles = [r for r in a.a16_roles.split(",") if r]
    if roles:
        g = onnx.load(model_in, load_external_data=False).graph
        inits = {t.name for t in g.initializer}
        sixteen = {
            i for n in g.node if n.op_type in roles for i in n.input if i and i not in inits
        }
        log(f"mixed precision: {len(sixteen)} activation inputs of {', '.join(roles)} at 16 bit")
        overrides.update({t: [{"quant_type": QuantType.QUInt16}] for t in sixteen})
        del g
    overrides.update({
        t: [{
            "quant_type": QuantType.QUInt16,
            "scale": np.array(LENGTH_SCALE, dtype=np.float32),
            "zero_point": np.array(0, dtype=np.uint16),
        }]
        for t in lengths
    })
    cfg = get_qnn_qdq_config(
        model_in,
        reader,
        calibrate_method=METHODS[a.calibration_method],
        activation_type=ACTIVATIONS[a.activations],
        weight_type=QuantType.QUInt8,
        per_channel=a.per_channel,
        init_overrides=overrides,
        stride=1,
    )
    cfg.use_external_data_format = True
    qdq = os.path.join(a.work, "qdq.onnx")
    quantize(model_in, qdq, cfg)

    q = onnx.load(qdq)
    for k, v in meta.items():
        e = q.metadata_props.add()
        e.key, e.value = k, v
    out_encoder = os.path.join(a.out, "encoder.onnx")
    if os.path.exists(os.path.join(a.out, "encoder.weights")):
        os.remove(os.path.join(a.out, "encoder.weights"))
    save_external(q, out_encoder, "encoder.weights")
    for f in ["decoder.onnx", "joiner.onnx", "tokens.txt"]:
        shutil.copyfile(os.path.join(a.source, f), os.path.join(a.out, f))

    outputs = ["encoder.onnx", "encoder.weights", "decoder.onnx", "joiner.onnx", "tokens.txt"]
    sources = [f for f in ["encoder.onnx", "encoder.weights", "decoder.onnx", "joiner.onnx", "tokens.txt"]
               if os.path.exists(os.path.join(a.source, f))]
    record = {
        "task": "T8.3c",
        "recipe": a.recipe,
        "tool": "xtask/python/qnn_convert.py",
        "python": platform.python_version(),
        "onnx": onnx.__version__,
        "onnxruntime": ort.__version__,
        "numpy": np.__version__,
        "static_input": {"audio_signal": [1, a.n_mels, a.frames], "length": [1]},
        "quantisation": {
            "format": "QDQ (get_qnn_qdq_config)",
            "activations": ACTIVATIONS[a.activations].name,
            "activations_16_bit_inputs_of": roles,
            "weights": "QUInt8, per channel (Conv)" if a.per_channel else "QUInt8, per tensor",
            "calibration": METHODS[a.calibration_method].name,
            "calibration_stride": 1,
            "calibration_pieces": len(reader.files),
            "calibration_data": a.calibration_set,
        },
        "sources": {f: {"sha256": sha256(os.path.join(a.source, f)),
                        "size": os.path.getsize(os.path.join(a.source, f))} for f in sources},
        "outputs": {f: {"sha256": sha256(os.path.join(a.out, f)),
                        "size": os.path.getsize(os.path.join(a.out, f))} for f in outputs},
        "seconds": round(time.time() - started, 1),
    }
    with open(os.path.join(a.out, "conversion.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(record, f, indent=2)
        f.write("\n")
    log(f"wrote {a.out} in {record['seconds']} s")


if __name__ == "__main__":
    main()
