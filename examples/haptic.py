"""WinSense mod-port example: real haptics (the buzz in the grips) on the left, the right, then both.

A real mod sends one such line when the game wants a feel: `haptic <left|right|both> <hz> <level %> for <ms>`.
Run: python examples/haptic.py  (the WinSense tray must be running, the controller on Bluetooth).
"""
import sys, time

pipe = sys.argv[1] if len(sys.argv) > 1 else r"\\.\pipe\WinSense"
with open(pipe, "w", buffering=1) as winsense:
    for side in ("left", "right", "both"):
        winsense.write(f"haptic {side} 80 50 for 1500\n")  # 80 Hz at half strength for 1.5 s
        time.sleep(2.5)  # the tone, then a second of quiet
