# Unreleased MasterHUB modules (from the SDK and the app)

The MasterHUB app 1.0.0.113 carries SDK libraries and handling code for modules that aren't on sale (as of
2026-10). This is everything that follows from them. Sources: the `CoolTEK_SDK_*.dll` libraries (command tables in
[sdk-commands.md](sdk-commands.md)) and the app's code (USB enumeration, the drawing of the base, input handling).
Some of it is inference, marked as such.

## Naming rule (inferred)

A module with a screen looks named by **its screen's diagonal in tenths of an inch + its number of keys**:

| Code | Reading | Backed by |
| --- | --- | --- |
| M131E (dial, on sale) | 1.3" screen, E = encoder | a 240 × 240 screen is a typical 1.3" |
| M4315 (15 keys, on sale) | 4.3" screen, 15 keys | 4.3" is usually 480 × 272: the SDK's picture buffer (480 × 272 × 4 B) and the module itself (checked on hardware) |
| M5000 | 5.0" screen, no keys | the app: 800 × 480, a typical 5" |
| M4600 | 4.6" screen, no keys | the app: 774 × 298 (or 800 × 320), a bar screen |
| M4300 | 4.3" screen, no fixed keys in the name | — |
| M3700 | 3.7" screen | — |
| M3704 | 3.7" screen, 4 keys | the app handles it as a keypad with 72 × 72 pictures |

Modules without a screen have other codes (M003K, M005S, M032LE, M0064), so the rule covers screen modules only.

## Modules

### M3704: 4 keys with a screen (the furthest along)

| | |
| --- | --- |
| PID | `0x9989` (the app doesn't check a first PID from the `0x01xx` / `0x02xx` series) |
| Size on the base | 4 × 1 cells (the same sockets as the 2xRoller) |
| In the app | fully handled like the 15xKey: 72 × 72 key pictures, colours, key events (the same function as the M4315) |
| SDK commands | brightness `03 08`, screen colour `03 0a`, key colour `03 0b`, logo `03 0c`; screen picture `02 09`, key picture `02 0a`, new logo `02 0b`; reads: firmware `0x0d`, serial number `0x0e`, info `0x0f` |

### M5000: a 5" touch screen

| | |
| --- | --- |
| PID | `0x9992` |
| Screen | 800 × 480 (from the app's code), **touch** |
| Size on the base | 4 × 3 cells (the same sockets as the 15xKey) |
| Input | `[event, ?, x lo, x hi, y lo, y hi]`: event 0 = touch or move (remember the point), anything else = release, which the app takes as a tap at the last point. The app reads it with the M4600's SDK function (probably the same format). |
| In the app | a demo: a colour palette on the screen; a tap (y ≤ 340) picks the lighting colour |
| SDK commands | brightness `03 06`, colour `03 08`, logo `03 0a`; picture `02 07`, new logo `02 09`; reads: firmware `0x0b`, serial number `0x0c`, info `0x0d` |

### M4600: a 4.6" touch screen (bar)

| | |
| --- | --- |
| PID | `0x9993` |
| Screen | 774 × 298 (the code also has 800 × 320), **touch** |
| Size on the base | 4 × 2 cells (the same sockets as the 5xFader) |
| Input | as the M5000 (touch coordinates) |
| In the app | as the M5000: a colour palette for the lighting colour |
| SDK commands | brightness `03 25`, colour `03 27`, logo `03 28`; picture `02 26`, new logo `02 27`; reads: firmware `0x2a`, serial number `0x2b`, info `0x2c` |

### M4300: probably a prototype or forerunner of the 15xKey

| | |
| --- | --- |
| PID | `0x9998`; the app handles **one** such module and doesn't read its serial number |
| Size on the base | one place in a corner of the drawing (`pos11`) |
| Build (from the SDK) | a screen, keys with pictures, a start-up logo; **two chips with firmware of their own** (`Update_sub_FW`, `Test_sub_FW_Recovery_Process`) |
| SDK commands | the report ID and command as one 16-bit word: brightness `03 54`, screen colour `03 56`, key colour `03 57`, logo `03 58`, firmware recovery `03 59` / `03 5c`; screen picture `02 55`, key picture `02 56`, new logo `02 57`, firmware `02 58`; serial number `0x5a` |

The app draws it with **the same picture as the 15xKey** (captioned "15xKey IPS Display"), and its name points to a
4.3" screen, as in the M4315. Its reports follow the 4315 / 131E series (ID `02` / `03` + command) with other
command numbers: most likely an earlier version of the 15-key module.

### M3700: a 3.7" screen

| | |
| --- | --- |
| PID | unknown: **the app doesn't detect it at all**; there's only the SDK library |
| SDK commands | brightness `03 07`, colour `03 09`, logo `03 0a`; picture `02 08`, new logo `02 09`; reads: firmware `0x0c`, serial number `0x0d`, info `0x0e` |

### CoolTEK SDK V01: an earlier device with keys and a screen

A general library without a model code (`CoolTEK_SDK_V01.00.00.dll`, "Check_Button_Status"): a screen, keys with
pictures, a logo. Commands: brightness `03 21`, screen colour `03 23`, key colour `03 24`, logo `03 25`; screen
picture `02 22`, key picture `02 23`, new logo `02 24`; reads: firmware `0x26`, serial number `0x27`, info `0x28`.
The app doesn't use it. Probably a forerunner of the MasterHUB series, or a separate CoolTEK product.

## Pictures in the app

The app has pictures of these modules (used on its drawing of the base), with captions that back the above:

| Module | File in the app (`resources/app.asar` → `img/`) | Caption on the picture |
| --- | --- | --- |
| M5000 | `m5000.png` (507 × 379) | "5" Touch Panel Display" |
| M4600 | `smallScreen.png` (506 × 248) | "4.6" Touch Panel Display" |
| M3704 | `4key.svg`, `4key-folder.svg` | "4xKey IPS Display": a row of 4 keys |
| M4300 | `controlPad.png`, `controlPad-folder.png` | "15xKey IPS Display": a 5 × 3 grid of keys |

The pictures belong to Cooler Master and aren't copied into this repository.

## In common

- **A start-up logo** (`Display_Logo`, `Replace_Logo`) on every screen module but the M131E and M4315, which store
  a "screen picture" instead (`Save_Screen_Picture`).
- **Firmware**: the screen modules' libraries handle 64 KB and 320 KB partitions; the M131E and M4315 also
  1.5 MB (probably for animations and pictures). Modules without a screen (M0): 40 KB.
- Report IDs `0x02` (output) / `0x03` (feature) and picture chunking are the same as in the released modules, so
  supporting them in the plugin would be simple, **once** their PIDs are known (the M3700's aren't) and they're
  tried on hardware.

## For the plugin

Nothing to do now: the modules aren't on sale, and support without hardware would be guesswork. Should they
appear, the M3704 is the easiest (a small 15xKey: the same 72 × 72 pictures and events), then the M4600 / M5000
touch screens (pictures like the dial, touch as a new kind of input: OpenDeck has `touchscreen`).
