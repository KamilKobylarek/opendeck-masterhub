# The official MasterHUB app

Source: the **MasterHUB 1.0.0.113** app for Windows (a copy of the installation provided by the device's owner),
studied without running it and only for information on the protocol, for an implementation of our own (the
interoperability exception: Polish Copyright Act art. 75(2)(3), EU Directive 2009/24/EC art. 6). This document
holds facts and tables of our own, no Cooler Master code.

## Contents

1. [The app's parts](#1-the-apps-parts)
2. [Talking to the device](#2-talking-to-the-device)
3. [Where modules sit and how they're turned](#3-where-modules-sit-and-how-theyre-turned)
4. [The dial M131E](#4-the-dial-m131e)
5. [The other modules](#5-the-other-modules)
6. [What the app does (summary)](#6-what-the-app-does-summary)
7. [Appendix: socket → turn](#appendix-socket--turn)

## 1. The app's parts

| Part | What it is |
| --- | --- |
| `MasterHUB.exe` + `resources/app.asar` | An Electron (Vue) app. The package carries a source map with the interface's original code. |
| `resources/build/Release/CoolTEK_SDK_CM_*.dll` | The hardware SDK: one library per module (`M0`, `M131E`, `M4315`, …), wrapping hidapi. |
| `stackaddon.node` | A Node add-on through which the app calls the SDK (`Kr.*` in the main process). |
| `worker/MCSDK.dll`, `mcsdk-api.node` | The MasterCTRL SDK (lighting of other Cooler Master devices); not about the modules. |
| `daemon/MHDaemon.exe` | A separate Electron process (socket.io; CPU and GPU sensors through `cpuidsdk64.dll`). |

The app also knows modules that aren't on sale or documented: `M3704` (keys), `M4300`, `M4600`, `M5000` (screens);
see [unreleased-modules.md](unreleased-modules.md).

## 2. Talking to the device

The MasterHUB is a **plain USB HID device** using the system's driver (no vendor driver): the SDK
(`CoolTEK_SDK_*.dll`) wraps hidapi and uses only `HID.DLL`, `SETUPAPI` and `ReadFile` / `WriteFile`. The app uses
libusb only to enumerate (the port chain, the serial number), with no data transfers. Reports: input `0x01`
(events), output `0x02` (pictures, LEDs, firmware), feature `0x03` and feature reads (settings, information).
Details, how certain each part is and every command: [sdk-commands.md](sdk-commands.md).

## 3. Where modules sit and how they're turned

The app works out where a module sits **and** how it's turned from the USB port chain alone; it doesn't use the
base's report `0x36` for it.

1. USB enumeration (the `usb` package; `portNumbers` is the port chain from the controller).
2. **The base's hub** is the USB device `VID 0x067B` / `PID 0x2586` (a Prolific hub).
3. The base M0064 (`0x2516:0x01F6`) belongs to the hub when its chain starts with the hub's and is at least 2
   longer.
4. A module belongs to that base when its chain starts with the hub's (on the same controller).
5. **The socket key** is the module's ports below the hub, joined without a separator:
   `portNumbers.slice(len(hub)).join("")`, e.g. hub `[1,1]`, module `[1,1,4,3]` → `"43"`.
6. The interface places the module on its drawing of the base with the CSS class `pos<key>`. That class sets **both
   place and turn**, since each contact pad of the base leads to a different port of the hub (see the
   [appendix](#appendix-socket--turn)).

**The whole base's orientation** (the VERTICAL / HORIZONTAL label): the base M0064 reports it in an input report,
first data byte `1` = horizontal, anything else vertical. Horizontal, the interface turns its view by 270°. **On
hardware (2026-10-25):** with the base horizontal a module's turn is the table's turn **+ 90°** clockwise (the
app's 270° is the other way round): a 15xKey at socket 23 (270° in the table) stands upright with the base lying
flat. The orientation can also be read on demand (`0x35`, see [sdk-commands.md](sdk-commands.md)). The user can
also switch it by hand in the app.

## 4. The dial M131E

IDs: `0x2516:0x0206`.

| Function | Report |
| --- | --- |
| Software mode | feature, 32 B: `03 49 <0 hardware, 1 software>` |
| Screen brightness | feature, 32 B: `03 44 <0–100>` |
| Picture on the screen | output: `02 45 <chunk number u16 LE> <length u16 LE> 00 <last 0/1> <data…>` |

- **Picture format:** the app sends **240 × 240 JPEG**. The SDK also takes BMP: it recognises the `BM` header and
  turns it into raw pixels.
- **Chunk length** = the device's output report length − 8 bytes (the SDK asks hidapi). The M4315's report is
  320 bytes; for the M131E the plugin reads the length from the device's HID descriptor.
- The M4315's keys use the same scheme with command `0x36`, the key number in place of the `00` byte.

**Input** (event data: `[id, count, values…]`):

| `id` | Meaning |
| --- | --- |
| `4` | press: `data[2]` = 1 pressed, 0 released. The app times a long press itself (from the press). |
| `3` or `0` | turn: `data[2]` = 0 right, 1 left (**the other way round from the M003K knobs**) |

## 5. The other modules

- **Knobs M003K:** turns in `data[2..4]`, 1 = right, 0 = left. Press: event `3`. As in the plugin.
- **Rollers M032LE:** `data[2]` is the **right** roller, `data[3]` the **left** one; 1 = down. The plugin swaps
  them, so OpenDeck's position 1 is the left roller (checked on hardware).
- **Faders M005S:** values in `data[2..6]` (0–100). As in the plugin (checked on hardware, 2026-10-26).
- **Second product ids:** the app takes each module under two PIDs; so does the plugin.

| Module | PID | Second PID |
| --- | --- | --- |
| 15xKey M4315 | `0x01FE` | `0x9997` |
| 3xKnob M003K | `0x0202` | `0x9996` |
| 2xRoller M032LE | `0x0204` | `0x9995` |
| Dial M131E | `0x0206` | `0x9994` |
| 5xFader M005S | `0x0200` | `0x9991` |
| Base M0064 | `0x01F6` | `0x9999` |

## 6. What the app does (summary)

A full survey was made on the app's code; here only what matters for the plugin.

**Input and polling.** Each module is polled about every 20 ms. A long press of the dial is 600 ms or more (timed
by the main process). On the M4315 the first key pressed in a report is handled.

**Orientation and placement only serve the interface.** The socket and the base's orientation turn only the
app's drawing. The app doesn't remap keys, reverse directions or turn pictures. A key's "direction" field is about
scroll actions (up / down), not turning its icon. → The plugin rearranges only the **layout** in OpenDeck (the
counterpart of the turned drawing) and leaves directions and pictures alone (pictures can be turned upright by a
setting).

**M4315 keys.**

- A press shows the icon shrunk to 36 × 36 on black; the release brings the picture back (done the same in the
  plugin).
- Up to 5 pages, folders, multi-step actions (280 ms between steps), a timer on a key, GIFs (a frame every
  30 ms).

**M003K knobs and the M131E dial.**

- A press switches to the next function in the knob's list (wrapping round).
- After switching, the dial shows a "function card" (a 140 px icon and the title), then after 2 s goes back to
  its display mode.
- A long press of the dial starts a timer.
- Turn steps: volume and brightness ±5, scrolling ±100, OBS in dB.

**M005S faders.** Absolute values 0–100. The app drives only the fader that moved last, with no pick-up when
values match.

**The dial's screen.** Modes: default (icon and title), picture / GIF, CPU and GPU load and temperature (sensors
from the cpuid daemon, refreshed every second), clock, timer.

**LEDs.** Hardware effects 0–9: off, static, breathing, strobe, spring, cycling, random, wave, water, rainbow.
The colour applies to effects 1–4, the brightness to effects 1–6 and 8–9. The base, knobs and faders have LEDs,
each on its own or all together. The app sends the settings again every 60 s.

**PC integrations** (OBS, Twitch, Discord, Streamlabs, Adobe, MasterCTRL, shortcuts, volume through `nircmd`) are
covered in OpenDeck by existing Stream Deck plugins; the plugin doesn't copy them.

**Profiles.** JSON tied to the base's serial number; the `.mhz` export is a ZIP. The app doesn't switch profiles
by application (OpenDeck can).

## Appendix: socket → turn

From the app's interface, vertical view. The positions in px are on the app's drawing of the base (a grid every
~129 px from 25 px). For turned elements the coordinates depend on the CSS turning point, so they don't map
directly onto grid cells.

**Note:** the tables reproduce the app's CSS classes and have errors: faders at sockets 31 and 32, the dial at
121, rollers at 34 and 124, plus entries for sockets the base doesn't have (11, 13, 14). The plugin
(`src/placement.rs`) derives from them one position per socket: where a 2 × 2 module sits and how it's turned;
the other modules sit in a fixed place beside it. All 91 sound entries agree with that, while the wrong ones
contradict the other modules in the same socket. The arrangements checked on the unit agree too (15xKey 23,
2xRoller 33, dial 134, 3xKnob 143; 15xKey 23, 2xRoller 33, 5xFader 113).

**3xKnob (M003K)** (24 sockets)

| Socket | Turn | Position in the app (px) |
| --- | --- | --- |
| 21 | 180° | top=284px left=283px |
| 22 | 270° | top=283px left=153px |
| 23 | 270° | top=283px left=283px |
| 24 | 0° | top=154px left=282px |
| 31 | 0° | top=282px left=25px |
| 32 | 180° | top=284px left=155px |
| 33 | 180° | top=284px left=26px |
| 34 | 0° | left=25px |
| 41 | 0° | top=410px left=282px |
| 42 | 0° | top=282px left=282px |
| 43 | 270° | top=412px left=283px |
| 44 | 270° | top=541px left=283px |
| 111 | 270° | top=283px left=25px |
| 112 | 270° | top=154px left=283px |
| 113 | 0° | left=283px |
| 122 | 0° | top=540px left=283px |
| 123 | 0° | top=540px left=25px |
| 124 | 270° | top=541px left=25px |
| 131 | 180° | top=28px left=285px |
| 132 | 270° | top=25px left=154px |
| 134 | 270° | top=25px left=283px |
| 141 | 180° | top=25px left=26px |
| 142 | 180° | top=26px left=155px |
| 143 | 270° | top=25px left=25px |

**Dial (M131E)** (24 sockets)

| Socket | Turn | Position in the app (px) |
| --- | --- | --- |
| 21 | 180° | top=284px left=283px |
| 22 | 270° | top=283px left=153px |
| 23 | 270° | top=283px left=283px |
| 24 | 0° | top=154px left=282px |
| 31 | 0° | top=282px left=25px |
| 32 | 180° | top=284px left=155px |
| 33 | 180° | top=284px left=26px |
| 34 | 0° | left=25px |
| 41 | 0° | top=410px left=282px |
| 42 | 0° | top=282px left=282px |
| 43 | 270° | top=412px left=283px |
| 44 | 270° | top=541px left=283px |
| 111 | 270° | top=283px left=25px |
| 112 | 270° | top=154px left=283px |
| 121 | 0° | top=293px left=18px |
| 122 | 0° | top=540px left=283px |
| 123 | 0° | top=540px left=25px |
| 124 | 270° | top=541px left=25px |
| 131 | 180° | top=28px left=285px |
| 132 | 270° | top=25px left=154px |
| 134 | 270° | top=25px left=283px |
| 141 | 180° | top=25px left=26px |
| 142 | 180° | top=26px left=155px |
| 143 | 270° | top=25px left=25px |

**2xRoller (M032LE)** (23 sockets)

| Socket | Turn | Position in the app (px) |
| --- | --- | --- |
| 13 | 0° | top=150px left=-147px |
| 14 | 0° | top=150px left=-147px |
| 21 | 270° | top=475px left=89px |
| 23 | 0° | top=282px left=25px |
| 31 | 90° | top=217px left=-40px |
| 32 | 270° | top=475px left=-40px |
| 33 | 270° | top=475px left=-169px |
| 34 | 0° | top=150px left=-147px |
| 41 | 90° | top=315px left=202px |
| 42 | 90° | top=217px left=218px |
| 43 | 0° | top=411px left=25px |
| 44 | 0° | top=540px left=25px |
| 112 | 0° | top=153px left=24px |
| 114 | 270° | top=475px left=217px |
| 121 | 0° | top=669px left=25px |
| 122 | 90° | top=475px left=218px |
| 123 | 90° | top=475px left=-40px |
| 124 | 270° | top=219px left=86px |
| 131 | 270° | top=216px left=90px |
| 133 | 270° | top=218px left=218px |
| 134 | 0° | top=23px left=25px |
| 141 | 270° | top=218px left=-169px |
| 142 | 270° | top=218px left=-39px |

**5xFader (M005S)** (16 sockets)

| Socket | Turn | Position in the app (px) |
| --- | --- | --- |
| 22 | 270° | top=411px left=25px |
| 23 | 270° | top=411px left=155px |
| 24 | 0° | bottom=411px left=25px |
| 31 | 270° | top=420px left=180px |
| 32 | 270° | top=165px left=180px |
| 33 | 180° | top=281px left=25px |
| 41 | 0° | bottom=153px left=25px |
| 42 | 0° | bottom=282px left=25px |
| 111 | 270° | top=411px left=-104px |
| 112 | 270° | top=282px left=154px |
| 113 | 0° | top=25px left=25px |
| 122 | 0° | bottom=25px left=25px |
| 132 | 270° | top=153px left=25px |
| 134 | 270° | top=153px left=155px |
| 141 | 180° | top=24px left=25px |
| 143 | 270° | top=153px left=-104px |

**15xKey (M4315)** (12 sockets)

| Socket | Turn | Position in the app (px) |
| --- | --- | --- |
| 11 | 0° | bottom=0 left=0 |
| 22 | 270° | top=346px left=-40px |
| 23 | 270° | top=346px left=88px |
| 24 | 0° | top=25px left=25px |
| 33 | 180° | top=281px left=23px |
| 41 | 0° | top=282px left=23px |
| 42 | 0° | top=153px left=25px |
| 112 | 270° | top=217px left=89px |
| 122 | 0° | top=411px left=25px |
| 132 | 270° | top=88px left=-40px |
| 134 | 270° | top=89px left=88px |
| 141 | 180° | top=23px left=23px |
