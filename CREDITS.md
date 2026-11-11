# Credits

WinSense contains no code copied from other projects. The controller's report layouts were
learned by reading these open projects and notes, then checked on a real controller. Thanks to
everyone who worked them out.

- **Linux kernel driver `hid-playstation.c`** (Sony and contributors, GPL): input and output
  report layout, calibration, battery.
- **SDL** (`SDL_hidapi_ps5.c`, zlib): lights, rumble forms, Bluetooth details.
- **Nielk1's DualSense trigger effect notes** (MIT) and **dualsensectl**: the adaptive trigger
  effect layout.
- **SAxense** by egormanga (MPL-2.0), <https://apps.sdore.me/SAxense>: Bluetooth haptics in
  report 0x32.
- **PadForge**: Bluetooth haptics, speaker and microphone reports.
- **vds**, **DS5Dongle** (MIT) and **LinuxAudio4Dualsense5**: speaker and microphone over
  Bluetooth.
- **ViGEmClient** (MIT, Nefarius Software Solutions): the ViGEmBus protocol for the virtual
  Xbox 360 pad.
- **DS4Windows**, **pydualsense**, **RPCS3**, **DualSense-Windows** and the DualSense community
  wiki: second sources for lights, triggers and Bluetooth disconnect.
- A 2022 wine-devel post and DJm00n's WASAPI notes: haptics as sound on the USB audio device.

## Sounds

- The chimes and the `pickup`, `radio` and `levelup` sounds are synthesised by
  `assets/clips.py`.
- The spoken replies were made with the Kokoro-82M voice "Heart" (Apache-2.0, run once offline
  through kokoro-onnx, MIT; see `assets/voice.py`).
- `assets/alert.bin` is the alert sound from Konami's *Metal Gear Solid*. It belongs to Konami and
  is **not** covered by WinSense's MIT licence.

## Trademarks

DualSense, PlayStation and PS5 are trademarks of Sony Interactive Entertainment. WinSense is not
made by or connected with Sony, Valve, Microsoft or Konami.
