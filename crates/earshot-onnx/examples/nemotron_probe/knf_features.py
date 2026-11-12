"""kaldi-native-fbank's own features for the nemotron_probe example's decoder check (T12.23).

In a throwaway venv under target/ holding only hash-pinned wheels (knf-requirements.txt, then
sherpa-onnx from requirements.txt for reference.py), after `nemotron_probe --dump <clips>`:

    python -I knf_features.py <clips> <features>
    cargo run --release -p earshot-onnx --example nemotron_probe --
        --reference crates\\earshot-onnx\\examples\\nemotron_probe\\reference_ort131.jsonl
        --decode-features <features>

The options are the ones sherpa-onnx v2.0.1 sets for NeMo transducers (PostInit), each clip with
0.66 s of zeros and flushed, written as `<id>.fbank` (little-endian f32, frame after frame).
"""

import os, sys
import kaldi_native_fbank as knf
import numpy as np
clips, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
opts = knf.FbankOptions()
opts.frame_opts.dither = 0
opts.frame_opts.snip_edges = False
opts.frame_opts.samp_freq = 16000
opts.frame_opts.remove_dc_offset = False
opts.frame_opts.preemph_coeff = 0.97
opts.frame_opts.window_type = "hann"
opts.mel_opts.num_bins = 128
opts.mel_opts.low_freq = 0
opts.mel_opts.high_freq = 8000
opts.mel_opts.is_librosa = True
for name in sorted(os.listdir(clips)):
    if not name.endswith(".f32"):
        continue
    x = np.fromfile(os.path.join(clips, name), dtype="<f4")
    x = np.concatenate([x, np.zeros(int(0.66 * 16000), dtype=np.float32)])
    f = knf.OnlineFbank(opts)
    f.accept_waveform(16000, x.tolist())
    f.input_finished()
    ref = np.stack([np.array(f.get_frame(i), dtype="<f4") for i in range(f.num_frames_ready)])
    ref.astype("<f4").tofile(os.path.join(out, name[:-4] + ".fbank"))
