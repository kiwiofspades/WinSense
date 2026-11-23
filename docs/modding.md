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
- WinSense writes nothing back unless you ask for it with `hello` (see "Hearing back"). Without
  it, open the pipe for writing only.
- Words are separated by spaces. Case does not matter.
- A line is at most 255 bytes; a longer line is dropped whole.
- A line with anything unknown or out of range is ignored whole. After `hello` you get an `err`
  line saying why; without it nothing else happens.
- When your mod closes the pipe, or its process ends, everything it set goes off and every light
  it set is handed back, at once: a `for` line, flash or tone of yours that is still running ends
  too (unless another mod started a newer one since). When WinSense quits, everything goes off.
- If the pipe cannot be made (another program holds its name), the tray says so once and
  WinSense keeps trying, from every second up to every 30 seconds.
- Only programs of the same Windows user on the same PC can connect.

## Hearing back

Needs WinSense 0.18.0 or later, like `curve`, `haptic stream`, `release` and `reclaim`.
An older WinSense ignores these lines.

```
hello
hello ack
```

After `hello`, WinSense writes lines back to your connection only, each ending in `\n`:

| Line | When |
|---|---|
| `winsense <version> proto <n> caps <list>` | the answer to `hello`, for example `winsense 0.18.0 proto 1 caps triggers,curve,rumble,haptic,stream,lights,players,sound,release,events,ack` |
| `pad <id> connected usb` or `pad <id> connected bt [full]` | for each controller at `hello`, and when one connects. `full`: over Bluetooth it is on the long report, which its lights, triggers and haptics need; WinSense's first write switches it |
| `pad <id> full` | a Bluetooth controller just switched to the long report |
| `pad <id> gone` | it disconnected, or WinSense could not open it again after a failed write |
| `err <line> <why>` | your line number `<line>` (counting every line you sent, from 1) was ignored: `unknown <word>`, `bad <word>` (a value missing or out of range after that word), `haptic needs for`, `too long` |
| `writefail <id> <message>` | a report could not be written to that controller (WinSense opens it again and tries once more) |
| `haptic stream bt` or `haptic stream none` | a `haptic stream` started, or its path changed: it plays over Bluetooth, or there is no Bluetooth controller to play it |
| `haptic underrun <n>` | at most once a second, and when the stream closes: `<n>` reports of the stream went out silent because the next block came late (a gap longer than 100 ms counts as a pause, not an underrun) |
| `haptic overflow <n>` | the same, for blocks dropped because more than 8 were waiting. Counts at close reach the other `hello` connections only (the stream's own is closed by then) |
| `ok <line>` | with `hello ack` only: that line was taken (the `hello ack` line itself gets none) |

- Only the first `hello` on a connection counts; a later one is just taken (`ok` with acks on).
- `caps` lists what this WinSense understands; check it before using a newer word. `proto` goes
  up only if a word changes meaning. `winsense --version` prints the version too.
- Ids count up from 1 while WinSense runs; a controller that reconnects gets a new one.
- Read what WinSense writes. If you never read, the pipe's buffer (4 KB) and then 64 more lines
  wait for you and later ones are dropped; WinSense itself never waits for you.
- If you read and write from two threads, open the pipe for overlapped (asynchronous) use, for
  example `PipeOptions.Asynchronous` in C#: with a plain handle a waiting read holds up your
  writes. From one thread, check for waiting data first (`PeekNamedPipe`), then read.
- A second connection opened at the same moment as another can fail with "pipe busy": wait a
  little (`WaitNamedPipe`) and try again.
- From pipe write to the report being ready to send took a median of 0.14 ms (95 % under 0.5 ms)
  in a test run with no controller (2026-11-22). Writing it to the controller comes on top; not
  measured yet.

## Line parts

A line can mix several parts, in any order: `r2 resist 5 8 rumble 40 0` sets a trigger and the
rumble at once.

### Triggers

```
l2|r2|both off
l2|r2|both resist  <start 0-9> <strength 1-8>
l2|r2|both click   <start 2-7> <end start+1..8> <strength 1-8>
l2|r2|both vibrate <start 1-9> <strength 1-8> <hz 1-255>
l2|r2|both curve   <start 0-9> <strength 1-8> [strength ...]
```

Positions are zones from 0 (released) to 9 (fully pulled).

- `resist`: steady resistance from the start zone on. A high strength from a middle zone makes a
  wall you can still push past.
- `click`: resistance from start to end that gives way at the end, like a gun trigger.
- `curve` (0.18.0 and later): resistance from the start zone with its own strength in each zone, one number per
  zone from the start on; the last one carries on to zone 9. `r2 curve 1 2 2 3 4 5 6 7 8` grows
  stiffer through the pull; `r2 curve 3 1 1 5` is light, then firm from zone 5. `curve <start> <s>`
  is the same as `resist <start> <s>`. At most one number per zone from the start (10 - start).
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
device. 80 Hz at 50 is a good start. Over Bluetooth the controller loses the first 150-200 ms of
every new haptic stream, so WinSense puts 150 ms of silence in front of each tone: a tone is felt
about 150 ms after its line (with a client stream open, at once, see below).

#### Your own samples: `haptic stream`

Needs WinSense 0.18.0 or later.

```
haptic stream
```

Send this as the last thing on a line; everything after it on that connection is samples, until
you close the connection. Use a connection of its own for it and keep your text lines on another.

- Samples are blocks of 64 bytes: 32 frames of two signed 8-bit samples (left grip, right grip),
  3000 frames a second, so one block every 10.67 ms. Each block is one report 0x32.
- Send blocks at that pace. WinSense plays each block in the next report slot. If none is waiting
  it sends silence, so the stream stays warm and your next block is felt at once, with no
  lost start. More than 8 waiting (85 ms): the oldest are dropped.
- While the stream is open, `haptic ... for` tones (yours and other clients') are added on top
  of your samples, `stop` cuts only the tone, and game sound as haptics waits. The newest stream
  wins: a second connection that opens one takes over (`haptic stream bt` is said again). A
  tone already playing when a stream opens over Bluetooth is cut.
- Closing the connection ends the stream with one silent report. A Bluetooth DualSense coming
  or going restarts it, which can cost a moment of samples; other devices change nothing.
- While a game holds the controller (`release`), the stream waits and its blocks are dropped
  (not counted as overflow). Tones and taps sent meanwhile are dropped too, not played later.
- Bluetooth only for now; with only the cable, the blocks are read and dropped (`haptic stream
  none`) and tones play on the cable as without a stream. Like tones, the samples are not felt
  while classic rumble runs.

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

### Handing the controller to a game

Needs WinSense 0.18.0 or later.

```
release
reclaim
```

- `release`: WinSense sends nothing to the controller until you `reclaim` it or your connection
  closes. That covers every client, the trigger test screen, game sound as haptics, the mute
  light, notification taps and sounds. Use it when your game drives the DualSense itself. Tones
  and sounds asked for meanwhile are dropped; lasting state and lights you send meanwhile are kept
  and go out when the controller is taken back.
- `reclaim`, or closing your connection: WinSense sends its whole current state once (triggers,
  rumble, lights), since the game may have changed it. If WinSense had sent nothing before and
  nothing changed meanwhile, it stays quiet.
- Several clients can hold it at once; it is free again once all have let go. The tray item
  "Hands off the controller while a game is in front" holds it the same way for games that never
  open the pipe.

What WinSense writes when it is not held, so you can judge whether to release it: every trigger or
rumble change is one output report that sets both triggers (an `off` trigger included) and the
whole rumble state. A report with both motors at 0 carries no rumble bits, which stops any classic
rumble and hands the grips back to haptics. Lights ride in the same report. Haptic tones and game
sound as haptics are report 0x32 over Bluetooth, sounds report 0x35.

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
- Test lines with Python before putting them in your mod, or say `hello` and read the `err`
  lines: without `hello` a wrong line is silently ignored.
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
