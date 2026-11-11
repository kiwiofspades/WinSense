# Voice replies with Kokoro-82M "Heart" (pip install kokoro-onnx soundfile; model files from the kokoro-onnx releases).
# usage: python voice.py <folder with kokoro-v1.0.onnx and voices-v1.0.bin> <out folder>; then the clip recipe (see docs/modding.md).
import sys, numpy as np, soundfile as sf
from kokoro_onnx import Kokoro
m, out = sys.argv[1:3]
k = Kokoro(m + '/kokoro-v1.0.onnx', m + '/voices-v1.0.bin')
for name, text in [('opening','Opening.'),('sorry',"Didn't quite catch that."),('switching','Switching.'),('closing','Closing.'),('okay','Okay.'),('battery_full','Battery full.'),('battery_high','Battery high.'),('battery_half','Battery about half.'),('battery_low','Battery low.'),('charging','Charging.')]:
    a, sr = k.create(text, voice='af_heart', speed=1.0, lang='en-us')
    a = a / np.abs(a).max() * 0.89  # -1 dBFS peak, like a loud Zira clip
    sf.write(f'{out}/{name}.wav', a, sr)
