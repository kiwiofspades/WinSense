# Credits

WinSense contains no code copied from other projects. The controller's report layouts were
learned by reading these open projects and notes, then checked on a real controller. Thanks to
everyone who worked them out.

Each licence below is the one stated in that project's own repository. Nothing from these
projects is part of WinSense, so their licences do not apply to it.

- **Linux kernel driver `hid-playstation.c`** (Sony and contributors, GPL-2.0-or-later): input and
  output report layout, calibration, battery.
- **SDL** (`SDL_hidapi_ps5.c`, zlib licence), <https://github.com/libsdl-org/SDL>: lights, rumble
  forms, Bluetooth details.
- **Nielk1's DualSense trigger effect notes** (MIT), the gist "Factories for all DualSense trigger
  effects", and **dualsensectl** (GPL-2.0), <https://github.com/nowrep/dualsensectl>: the adaptive
  trigger effect layout.
- **SAxense** by egormanga (MPL-2.0), <https://github.com/egormanga/SAxense>: Bluetooth haptics in
  report 0x32.
- **PadForge**, <https://github.com/hifihedgehog/PadForge> (CC BY-NC-SA 4.0): Bluetooth haptics,
  speaker and microphone reports.
- **vds**, <https://github.com/hurryman2212/vds> (MIT), **DS5Dongle**,
  <https://github.com/awalol/DS5Dongle> (MIT), and **LinuxAudio4Dualsense5**,
  <https://github.com/GeorgLegato/LinuxAudio4Dualsense5> (AGPL-3.0): speaker and microphone over
  Bluetooth.
- **ViGEmClient** (MIT, Nefarius Software Solutions), <https://github.com/nefarius/ViGEmClient>:
  the ViGEmBus protocol for the virtual Xbox 360 pad.
- **DS4Windows** (GPL-3.0), <https://github.com/ds4windowsapp/DS4Windows>, **pydualsense** (MIT),
  <https://github.com/flok/pydualsense>, **RPCS3** (GPL-2.0), <https://github.com/RPCS3/rpcs3>,
  **DualSense-Windows** (MIT for the implementation only),
  <https://github.com/Ohjurot/DualSense-Windows>, and the DualSense community wiki: second sources
  for lights, triggers and Bluetooth disconnect.
- A 2022 wine-devel post and DJm00n's WASAPI notes: haptics as sound on the USB audio device.

## Libraries

- **hidapi** crate (MIT), <https://github.com/ruabmbua/hidapi-rs>: access to the controller's HID
  interface.
- **windows** and **windows-sys** crates (MIT OR Apache-2.0),
  <https://github.com/microsoft/windows-rs>: the Windows APIs.

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
