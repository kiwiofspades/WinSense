"""WinSense mod-port example: a colour on the light bar, a damage flash on top, a sound, then stop.

A real mod sends the same lines when the game changes state (health colour), takes a hit, picks
something up. Run: python examples/lights.py  (the WinSense tray must be running; the sound needs
the controller on Bluetooth).
"""
import sys, time

pipe = sys.argv[1] if len(sys.argv) > 1 else r"\\.\pipe\WinSense"
with open(pipe, "w", buffering=1) as winsense:  # one line per game event; line-buffered sends each at once
    winsense.write("light 0 80 255 players 4\n")  # lasting: a blue bar and the centre player light
    time.sleep(2)
    winsense.write("flash 255 0 0\n")             # hit: red for a moment, then the blue comes back
    time.sleep(1)
    winsense.write("hit flash 255 0 0\n")         # the same with the hit's rumble, for as long as it lasts
    time.sleep(1)
    winsense.write("sound pickup\n")              # a chime from the controller's speaker
    time.sleep(1)
    winsense.write("stop\n")                      # lights handed back to the controller
# Closing the pipe (or the game exiting) also hands back every light this script set.
