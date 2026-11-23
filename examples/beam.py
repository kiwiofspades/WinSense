"""WinSense mod-port example: fire a beam for 4 seconds, land a hit, then stop.

A real mod sends the same lines from wherever the game decides a beam starts or a hit lands.
Run: python examples/beam.py  (the WinSense tray must be running). Squeeze L2 and R2 about halfway:
the beam pulses in the triggers once they are pulled past the first zone.
"""
import sys, time

pipe = sys.argv[1] if len(sys.argv) > 1 else r"\\.\pipe\WinSense"
try:
    winsense = open(pipe, "w", buffering=1)  # line-buffered: each line goes at once
except OSError as e:
    sys.exit(f"Could not open {pipe}: {e.strerror}. The WinSense tray must be running.")
with winsense:
    winsense.write("beam\n")      # beam starts: both triggers pulse
    time.sleep(4)
    winsense.write("hit\n")       # the beam lands: a short rumble on top
    time.sleep(0.5)
    winsense.write("stop\n")      # beam over
# Closing the pipe (or the game exiting) also ends every effect this script started.
