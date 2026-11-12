"""Reference text for the nemotron_probe example (T12.23): sherpa-onnx's own Python package.

Run in a throwaway venv under target/ holding only hash-pinned wheels (AGENTS.md section 9):

    python -m venv target\\nemotron-ref\\venv
    target\\nemotron-ref\\venv\\Scripts\\python.exe -m pip install --require-hashes --no-deps
        --only-binary :all: -r crates\\earshot-onnx\\examples\\nemotron_probe\\requirements.txt
    cargo run -p earshot-onnx --example nemotron_probe -- --dump target\\nemotron-ref\\clips
    target\\nemotron-ref\\venv\\Scripts\\python.exe -I
        crates\\earshot-onnx\\examples\\nemotron_probe\\reference.py
        <models>\\nemotron\\streaming-0.6b-560ms-int8 target\\nemotron-ref\\clips
        crates\\earshot-onnx\\examples\\nemotron_probe\\reference.jsonl

Each clip is read from `<id>.f32` (little-endian float samples at 16 kHz, written by the probe
from the committed smoke FLAC files) and decoded with sherpa-onnx's own protocol
(`python-api-examples/online-decode-files.py`): the whole clip, 0.66 s of zeros, input finished,
then every ready chunk. One JSON line per clip: its id, the text, the token ids' text and each
token's time in seconds, as sherpa-onnx reports them.
"""

import array
import json
import os
import sys

import sherpa_onnx


def main():
    model, clips, out = sys.argv[1], sys.argv[2], sys.argv[3]
    lines = []
    recognizer = sherpa_onnx.OnlineRecognizer.from_transducer(
        tokens=os.path.join(model, "tokens.txt"),
        encoder=os.path.join(model, "encoder.int8.onnx"),
        decoder=os.path.join(model, "decoder.int8.onnx"),
        joiner=os.path.join(model, "joiner.int8.onnx"),
        num_threads=2,
        sample_rate=16000,
        feature_dim=128,
        decoding_method="greedy_search",
        provider="cpu",
    )
    for name in sorted(os.listdir(clips)):
        if not name.endswith(".f32"):
            continue
        samples = array.array("f")
        with open(os.path.join(clips, name), "rb") as f:
            samples.frombytes(f.read())
        if sys.byteorder != "little":
            samples.byteswap()
        stream = recognizer.create_stream()
        stream.accept_waveform(16000, samples.tolist())
        stream.accept_waveform(16000, [0.0] * int(0.66 * 16000))
        stream.input_finished()
        while recognizer.is_ready(stream):
            recognizer.decode_stream(stream)
        result = json.loads(recognizer.get_result_as_json_string(stream))
        lines.append(
            json.dumps(
                {
                    "id": name[: -len(".f32")],
                    "text": result["text"],
                    "tokens": result["tokens"],
                    "timestamps": result["timestamps"],
                },
                ensure_ascii=True,
            )
        )
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write("".join(line + "\n" for line in lines))


if __name__ == "__main__":
    main()
