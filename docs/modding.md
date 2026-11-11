# Modding guide: the WinSense pipe

A game mod talks to WinSense through a named pipe. The mod writes one short line of text per game
event ("a beam starts", "the player got hit"), and WinSense turns it into trigger effects, rumble,
haptics, lights or a sound on the controller. Game logic stays in the mod; WinSense only plays
the effects.

Any language that can open a file can do this, so a mod needs no library and nothing has to be
installed or registered. The WinSense tray must be running.

```
\\.\pipe\WinSense
```

## Rules of the pipe

- Write text lines, each ending in `\n`. Flush after each line so it arrives at once.
- WinSense never writes back. Open the pipe for writing only.
- Words are separated by spaces. Case does not matter.
- A line is at most 255 bytes; a longer line is dropped whole.
- A line with anything unknown or out of range is ignored whole. Nothing else happens, so check
  your lines with the examples below.
- When your mod closes the pipe, or its process ends, everything it set goes off and every light
  it set is handed back. When WinSense quits, everything goes off.
- Only programs of the same Windows user on the same PC can connect.

## Line parts

A line can mix several parts, in any order: `r2 resist 5 8 rumble 40 0` sets a trigger and the
rumble at once.

### Triggers

```
l2|r2|both off
l2|r2|both resist  <start 0-9> <strength 1-8>
l2|r2|both click   <start 2-7> <end start+1..8> <strength 1-8>
l2|r2|both vibrate <start 1-9> <strength 1-8> <hz 1-255>
```

Positions are zones from 0 (released) to 9 (fully pulled).

- `resist`: steady resistance from the start zone on. A high strength from a middle zone makes a
  wall you can still push past.
- `click`: resistance from start to end that gives way at the end, like a gun trigger.
- `vibrate`: the trigger buzzes while it is pulled past the start zone. Use 20 Hz or more, and
  expect players to feel it only with the trigger pulled well in. 8 Hz with a light pull was not
  felt in tests.

Each trigger has one effect at a time.

### Rumble

```
rumble <left 0-255> <right 0-255>
```

Left is the deep motor, right the light one. `rumble 0 0` stops it. Pulses shorter than about
250 ms are easy to miss.

### Haptics

```
haptic left|right|both <hz 1-1000> <level 1-100> for <ms 1-5000>
```

A tone in the grips' haptic actuators. It always needs `for`: a `haptic` line without it is
ignored. A newer tone cuts the one that is playing. While a tone plays, the rumble motors pause
(the tone cannot be felt over rumble) and come back after it. Triggers are not touched.

Over Bluetooth this works directly; on the cable alone it plays through the controller's sound
device. A tone shorter than about 200 ms may not be felt at all. 80 Hz at 50 is a good start.

### Lights

```
light <r 0-255> <g 0-255> <b 0-255>
light off
players <mask 0-31>
players off
flash <r> <g> <b>
```

- `light`: the light bar colour. There is no separate brightness: send darker values.
- `players`: the five small player lights, one bit each from left to right; 4 is the centre one
  alone.
- `off` gives that light back to the controller. Over Bluetooth a given-back light bar goes dark
  rather than returning to its own blue, so set a colour you want to keep first.
- `flash r g b`: that colour for 150 ms, then the lasting colour again. With `for` on the line it
  lasts that long instead.

### Sounds

```
sound <name>
```

Plays one built-in sound on the controller's speaker: `ding`, `ding2`, `off`, `listen`,
`pickup`, `alert`, `radio`, `levelup`. Over Bluetooth only. One plays at a time: a sound that
is playing finishes first, and of the requests that came meanwhile only the newest plays after it. Sounds are dropped (not
played later) while the player is speaking a voice command. A line with only a sound changes
nothing else. A mod cannot send its own audio.

### Timing and stop

```
for <ms 1-5000>
stop
```

- Without `for`, a line changes your mod's **lasting state**: what it names stays until changed;
  what it does not name stays as it was.
- With `for`, the line plays **on top** of the lasting state for that long, then the lasting
  state comes back. Use it for hits and shots.
- `stop` switches your lasting state off, ends a running `for` line and cuts a tone.

## Presets

Words that stand for whole lines, for common game events. They mix with other parts.

| Word | Same as | Tried on a controller |
|---|---|---|
| `beam` | `both vibrate 1 8 25` (lasting, until `stop`) | yes |
| `hit` | `rumble 220 160 for 250` | yes |
| `hitstop` | `both resist 0 8 rumble 255 255 for 90` | no |
| `fire` | `rumble 90 255 for 70` | no |

`hit flash 255 0 0` rumbles and flashes red for the hit's 250 ms.

## Layers: who wins

Three things can drive the controller: the mod's `for` lines, the mod's lasting state, and the
tray's trigger test screen.

- Per trigger, the first that is not `off` wins: the `for` line, then the lasting state, then the
  test screen. An `off` is see-through in every layer: a mod's `off` cannot switch off an effect
  the test screen set, and an `off` in a `for` line changes nothing.
- Each rumble motor takes the strongest value of all of them (and of a game's rumble through the
  optional virtual pad).
- Lights: the `for` line, else the lasting light.
- One mod's lasting state at a time: the newest line wins. A mod that disconnects clears the
  lasting state only if it is still its own.

## Examples

Each sends a beam for four seconds, a hit, then stop. All of them open the pipe for writing,
write a line, and flush.

### Python (run)

```python
import time
with open(r"\\.\pipe\WinSense", "w", buffering=1) as winsense:  # line-buffered: each line goes at once
    winsense.write("beam\n")
    time.sleep(4)
    winsense.write("hit\n")
    time.sleep(0.5)
    winsense.write("stop\n")
```

More in [`examples/`](../examples): `beam.py`, `haptic.py`, `lights.py`.

### C# (run)

```csharp
using System.IO;
using System.IO.Pipes;
using System.Threading;

class Beam
{
    static void Main()
    {
        using (var pipe = new NamedPipeClientStream(".", "WinSense", PipeDirection.Out))
        {
            pipe.Connect(1000);
            var winsense = new StreamWriter(pipe) { AutoFlush = true, NewLine = "\n" };
            winsense.WriteLine("beam");
            Thread.Sleep(4000);
            winsense.WriteLine("hit");
            Thread.Sleep(500);
            winsense.WriteLine("stop");
        }
    }
}
```

Whether this works inside Unity's Mono runtime has not been checked.

### C++ (run)

```cpp
#include <windows.h>
#include <cstring>

static void send(HANDLE pipe, const char* line) {
    DWORD written;
    WriteFile(pipe, line, (DWORD)strlen(line), &written, nullptr);
}

int main() {
    HANDLE pipe = CreateFileW(L"\\\\.\\pipe\\WinSense", GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
    if (pipe == INVALID_HANDLE_VALUE) return 1;  // WinSense is not running
    send(pipe, "beam\n");
    Sleep(4000);
    send(pipe, "hit\n");
    Sleep(500);
    send(pipe, "stop\n");
    CloseHandle(pipe);
}
```

### Lua (not run)

Written from Lua's standard `io` library; no Lua was at hand to run it. Some game Lua sandboxes
remove `io`.

```lua
local winsense = io.open([[\\.\pipe\WinSense]], "w")
if winsense then
  winsense:write("beam\n"); winsense:flush()
  -- ... later, when the hit lands:
  winsense:write("hit\n"); winsense:flush()
  winsense:write("stop\n"); winsense:flush()
  winsense:close()
end
```

## Tips

- Open the pipe once when the game starts and keep it open. If opening fails, WinSense is not
  running: carry on without it, and try again later if you like.
- Send a line when something changes, not every frame.
- Test lines with Python before putting them in your mod: a wrong line is silently ignored.
- Trigger effects only feel right when players actually pull the trigger in the game moment you
  use them for.

## Making clips

The built-in sounds are 200-byte Opus frames, built into the exe. To make a new one (from source
code), write a wav, then:

```
ffmpeg -i in.wav -af "aresample=45000,asetrate=48000,apad=pad_dur=0.04" -ac 2 -ar 48000 -c:a libopus -application lowdelay -frame_duration 10 -b:a 160k -vbr off out.opus
python assets/ogg2frames.py out.opus out.bin
```

Every packet must be 200 bytes (the script prints them). `assets/clips.py` makes the synthesised
bells. Then add the clip to `src/voice.rs`.
