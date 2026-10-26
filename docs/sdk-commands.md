# CoolTEK SDK commands (MasterHUB)

Every function of the SDK libraries that ship with the MasterHUB app 1.0.0.113 for Windows
(`resources/build/Release/CoolTEK_SDK_*.dll`) and the HID reports they send. Source: disassembly of the exported
functions (an automatic extractor, with selected functions checked by hand), used only as information for an
implementation of our own. This document contains no Cooler Master code.

## Contents

1. [How the app talks to the device](#1-how-the-app-talks-to-the-device)
2. [SDK conventions](#2-sdk-conventions)
3. [Input reports (events, channel 0)](#3-input-reports-events-channel-0)
4. [Dangerous commands](#4-dangerous-commands)
5. [Commands by library](#5-commands-by-library)

## 1. How the app talks to the device

**The MasterHUB is a standard USB HID device.** Each module is a HID device of its own (VID `0x2516`, its own PID).
Everything goes through the system's HID driver; there's no vendor driver.

- The SDK libraries wrap **hidapi** (they export `hid_open`, `hid_write`, `hid_read_timeout`,
  `hid_get_feature_report`, `hid_send_feature_report`, …). They import only the Windows HID layer: `HID.DLL`
  (`HidD_SetFeature`, `HidD_GetPreparsedData`, `HidP_GetCaps`, …), `SETUPAPI` (finding devices) and
  `ReadFile` / `WriteFile`. No WinUSB, serial port or any other channel.
- The app also uses **libusb** (the `usb` package), but **only to enumerate**: the USB port chain (where a module
  sits on the base) and the serial number from the descriptor. It makes no transfers through it (no
  `controlTransfer`, `claimInterface` or endpoint transfers).
- The app calls the SDK through a Node add-on, `stackaddon.node`, which loads the `CoolTEK_SDK_*.dll` libraries.

### Report types

| Report | Direction | ID | Length | Used for |
| --- | --- | --- | --- | --- |
| Input | module → PC | `0x01` | per module | events and replies: byte 1 is the channel tag, bytes 2–3 the data length (u16 LE), then the data |
| Output | PC → module | `0x02` | the device's output report length (the SDK asks hidapi; M0: 64 B, M4315: 320 B, M131E: from the descriptor) | pictures, LEDs, firmware, data requests |
| Feature (write) | PC → module | `0x03` | 32 B | settings: software mode, brightness, colours, serial number |
| Feature (read) | module → PC | one ID per query (e.g. `0x30`) | 32 B | serial number, firmware versions, brightness, fader positions |

**Input channels** (byte 1 of report `0x01`). Events come on channel 0; the app's `Check_IO_Status` reads only
that one.

| Modules | Channel 0 | 1 | 2 | 3 | 4 |
| --- | --- | --- | --- | --- | --- |
| M0 (base, knobs, faders, rollers) | `0x2a` | `0x2b` | `0x2c` | `0x2d` | `0x2e` |
| 15xKey M4315 | `0x34` | `0x35` | `0x36` | `0x37` | `0x38`\* |
| Dial M131E | `0x44` | `0x45` | `0x46` | `0x47` | `0x48`\* |

\* Follows from the pattern; the SDK only compares channels 0–3.

### What's certain and what isn't

| Level | What |
| --- | --- |
| **Certain** (visible in the app's files) | HID transport through hidapi; libusb only for enumeration; the IDs and command bytes in the tables below (read from the SDK's code) |
| **Very likely** | what the parameters mean: from the function names, the ranges checked and how the app uses them; whatever the plugin uses is checked on hardware (✔) |
| **Unconfirmed** | what the firmware does with commands the plugin doesn't use; conclusions marked "probably" in the notes |

On macOS: the app exists only for Windows, but HID is a standard independent of the system; the same reports work
through hidapi on macOS without drivers (checked on hardware, 2026-10). The exception: the 2xRoller needs the
SET_IDLE request on macOS, see [macos-roller-set-idle.md](macos-roller-set-idle.md).

## 2. SDK conventions

- `*_CM` functions take a device handle. The `*_CM_M131E` / `*_CM_M4315` variants are thin wrappers of the same
  functions (listed together in the tables). The `M0` library serves several modules, told apart by a parameter.
- In the tables `‹aN›` is the function's N-th parameter (a0 is the device handle), `00` a zero byte, `‹?›` a
  computed value.
- Return codes: `0` OK, `3` transfer error, `4` bad reply, `6` bad parameter, `7` / `8` device gone / needs
  reconnecting.
- **Pictures** go in output reports, in chunks of the report length − 8 bytes:
  `02 <command> <chunk number u16 LE> <length u16 LE> <index> <last 0/1> <data…>`. Data format: JPEG. The dial's
  SDK also takes BMP (it recognises the `BM` header and turns it into raw pixels).
- **Firmware version reads** (`*_FW_Infos_by_type`) pick the report ID by type 0–2: three consecutive IDs (the
  table shows the last one).
- **Picture buffer sizes** in `Save_Screen_Picture`: dial 230 400 B (= 240 × 240 × 4, its known resolution),
  M4315 522 240 B (= 480 × 272 × 4). The 15 keys of the M4315 are **one 480 × 272 screen** under see-through keys:
  a picture sent with `Display_Picture_On_Screen` fills it (checked on hardware, the plugin's 15xKey background).

## 3. Input reports (events, channel 0)

Data: `[event id, number of values, values…]`. From the SDK libraries and how the app handles them; all checked
on hardware.

| Module | Event | Meaning |
| --- | --- | --- |
| M4315 | `0x02`, 15 values | keys 0–14 (1 = pressed) |
| M003K | `0x00`, 3 values | knob turns: 1 right, 0 left, 0xFF no change |
| M003K | `0x03`, 3 values | knob presses |
| M005S | `0x02`, 5 values | fader positions 0–100 |
| M032LE | `0x01`, 2 values | right roller, left roller: 0 up, 1 down, 0xFF no change |
| M131E | `0x03` or `0x00`, 1 value | turn: 0 right, 1 left |
| M131E | `0x04`, 1 value | press (1) / release (0); the app times a long press (≥ 600 ms) itself |
| M0064 | channel 1 (`0x2b`), 1 byte | the base's orientation: 1 horizontal, else vertical; sent only at power-up and when the base is turned, so the plugin also reads `0x35` |

## 4. Dangerous commands

The plugin **never** sends these: they can damage a module for good or change its identity.
`Update_FW*` (firmware update), `Test_FW_Recovery_Process*`, `Test_Factory_Reset_Process*`,
`Test_Animation_Recovery_Process*`, `Save_Factory_Data*`, `Set_Serial_Number*`, `Save_Screen_Picture*` (writes to
the module's memory) and `Calibrate_Direction_CM_M0`, marked ⚠ in the tables. The plugin's code refuses their
command bytes (`FORBIDDEN_OUTPUT` and `FORBIDDEN_FEATURE` in `src/device.rs`). That also refuses configuration
data requests it has no use for: the M0's `02 32` / `02 33`, and the dial's `02 49`, which is the M0's firmware
recovery. The LED commands' "store in flash" flag is set
only by **Save and store in the modules**.

## 5. Commands by library

The **Notes** column: ✔ = used by the plugin. The M3700, M3704, M4300, M4600, M5000 and the general V01 libraries
are for modules and devices that aren't on sale (the app knows them as future MasterHUB modules; see
[unreleased-modules.md](unreleased-modules.md)). Their tables come from the automatic extractor only.

### Base M0064, 3xKnob M003K, 5xFader M005S, 2xRoller M032LE (`CoolTEK_SDK_CM_M0_V02.05.00.dll`)

| Function | Report | Length | Bytes | Parameters | Notes |
| --- | --- | --- | --- | --- | --- |
| `Adjust_LED_Brightness_CM_M0` | output | output report | 02 2e ‹a1› ‹a2› ‹a3› | a1 module, a2 index, a3 brightness 0–100 | ✔ |
| `Adjust_LED_Color_CM_M0` | output | output report | 02 2d ‹a1› ‹a2› ‹a3› ‹a4› ‹a5› ‹a6› | a1 module (0 base, 1 faders, 2 knobs), a2 index (base 0; faders 0–4; knobs 0–2; 0xFF all), a3–a5 R G B, a6 store in flash | ✔ (flash only with **Save and store in the modules**) |
| `Adjust_LED_Mode_CM_M0` | output | output report | 02 2c ‹a1› ‹a2› ‹a3› ‹a4› | a1 module, a2 index, a3 effect 0–9, a4 store in flash | ✔ (flash as with the colour). The index works: one 3xKnob LED gets an effect, the others keep theirs (checked 2026-10-25: breathing on one, strobe on another, the third static). The speed (`02 30`) has no index: one per module; strobe at speed 50 is slow. |
| `Adjust_LED_Speed_CM_M0` | output | output report | 02 30 ‹a1› | a1 speed 0–100 | ✔ |
| `Calibrate_Direction_CM_M0` | output | output report | 02 2f | — | ⚠ calibration; refused by the plugin |
| `Check_Rcv_Data_CM_M0` | input | — | `01 <channel> <length u16> <data…>` | channel 0–4, timeout | ✔ events ([input reports](#3-input-reports-events-channel-0)) |
| `Connect_Device_CM_M0` | — | — | — | | calls Disconnect_Device_CM_M0, Get_Device_Infos_CM_M0, hid_enumerate |
| `Connect_Device_byPath_CM_M0` | — | — | — | | calls Disconnect_Device_CM_M0, Get_Device_Infos_CM_M0, hid_open_path |
| `Disconnect_Device_CM_M0` | — | — | — | | calls hid_close |
| `Get_Config_Data_CM_M0` | output | output report | 02 33 | picks 0x32 / 0x33 | asks for configuration data, which comes back in an input report; refused by the plugin |
| `Get_Config_Info_CM_M0` | feature ← | 32 | 36 | — | read 0x36; the bytes' meaning is unknown (base: `36 10 45 71 f3 b1 b0 00…`, the same on both of its USB ports) |
| `Get_Device_Direction_CM_M0` | feature ← | 32 | 35 | — | ✔ read 0x35: `35 00 01` with the base horizontal, byte 2 = 0 vertical. Always answers, on demand (checked 2026-10-25); the plugin asks when it opens the base and every 2 s, since the base reports its orientation by itself only at power-up and when turned. The same whichever of its two USB ports the base is connected by. |
| `Get_Device_Infos_CM_M0` | feature ← | 32 | 31 | — | ✔ (module info) read 0x31, device info |
| `Get_FW_Infos_CM_M0` | — | — | — | | calls get_FW_Infos_by_type_CM_M0 |
| `Get_LED_Brightness_CM_M0` | feature ← | 32 | 32 | — | ✔ (module info) read 0x32, the current brightness |
| `Get_LED_Mode_CM_M0` | feature ← | 32 | 34 | — | ✔ (the effect shown while the plugin leaves the LEDs alone) read 0x34; returns the effect **stored in flash**, not the current one (checked 2026-10-25: knobs set to static without storing report 9) |
| `Get_Serial_Number_CM_M0` | feature ← | 32 | 30 | — | ✔ (module info) read 0x30 |
| `Get_Slider_Value_CM_M0` | feature ← | 32 | 33 | — | ✔ read 0x33, the fader positions in bytes 2–6 (checked 2026-10-26) |
| `Save_Factory_Data_CM_M0` | output | output report | 02 31 | — | ⚠ writes factory data |
| `Set_Serial_Number_CM_M0` | output | output report | 02 2b ‹a2› | a2 length, then the text | ⚠ changes the serial number |
| `Test_FW_Recovery_Process_CM_M0` | output | output report | 02 49 | — | ⚠ firmware recovery mode |
| `Test_Factory_Reset_Process_CM_M0` | output | output report | 02 4a | — | ⚠ factory reset |
| `Update_FW_CM_M0` | — | — | — | | calls Update_FW_by_type_CM_M0 |
| `Update_FW_by_type_CM_M0` | output | output report | 02 2a 00 01 00 00 ‹u16› 01 ‹a3› | data, a3 type | ⚠ firmware update (4 KB pages, 48 B chunks) |
| `get_FW_Infos_by_type_CM_M0` | feature ← | 32 | 2f | a1 type 0–2 | ✔ (module info) read **0x2d + type** (0x2d / 0x2e / 0x2f): firmware version |

### Dial M131E (`CoolTEK_SDK_CM_M131E_V03.02.00.dll`)

| Function | Report | Length | Bytes | Parameters | Notes |
| --- | --- | --- | --- | --- | --- |
| `Adjust_Backlight_CM` (+ `_M131E`) | feature → | 32 | 03 44 ‹a1› | a1 brightness 0–100 | ✔ |
| `Check_Rcv_Data_CM` (+ `_M131E`) | input | — | `01 <channel> <length u16> <data…>` | | ✔ |
| `Connect_Device_CM` (+ `_M131E`) | — | — | — | | calls Disconnect_Device_CM, Get_Device_Infos_CM, hid_enumerate |
| `Connect_Device_byPath_CM` (+ `_M131E`) | — | — | — | | calls Disconnect_Device_CM, Get_Device_Infos_CM, hid_open_path |
| `Disconnect_Device_CM` (+ `_M131E`) | — | — | — | | calls hid_close |
| `Display_Color_On_Screen_CM` (+ `_M131E`) | feature → | 32 | 03 46 ‹a1› ‹a2› ‹a3› | a1–a3 R G B | ✔ (clearing the screen) the whole screen in one colour |
| `Display_Picture_On_Screen_CM` (+ `_M131E`) | output | output report | 02 45 00 00 ‹u16› 00 01 | JPEG (or BMP), length | ✔ chunks: `02 45 <number u16> <length u16> 00 <last> <data>` |
| `Get_Animation_Infos` | feature ← | 32 | 4d | — | read 0x4d |
| `Get_Backlight_CM` (+ `_M131E`) | feature ← | 32 | 4c | — | ✔ (module info) read 0x4c |
| `Get_Config_Data_CM` (+ `_M131E`) | output | output report | 02 49 | picks 0x48 / 0x49 | asks for configuration data |
| `Get_Config_Infos_CM` (+ `_M131E`) | feature ← | 32 | 4e | — | read 0x4e |
| `Get_Device_Infos_CM` (+ `_M131E`) | feature ← | 32 | 4b | — | ✔ (module info) read 0x4b |
| `Get_FW_Infos_CM_M131E` | — | — | — | | calls Get_FW_Infos_by_type_CM |
| `Get_FW_Infos_by_type_CM` (+ `_M131E`) | feature ← | 32 | 49 | a1 type 0–2 | ✔ (module info) read **0x47 + type** |
| `Get_Serial_Number_CM` (+ `_M131E`) | feature ← | 32 | 4a | — | ✔ (module info) read 0x4a |
| `Save_Factory_Data_CM` (+ `_M131E`) | feature → | 32 | 03 4b | — | ⚠ writes factory data |
| `Save_Screen_Picture_CM` | output | output report | 02 47 00 00 ‹u16› ‹?› 01 01 01 ‹?› ‹?› ‹?› ‹?› | picture | ⚠ stores a picture or animation in the module (buffer up to 230 400 B = 240 × 240 × 4); a longer header, not fully worked out |
| `Set_Serial_Number_CM` (+ `_M131E`) | feature → | 32 | 03 45 ‹a2› | a2 length, then the text | ⚠ |
| `Set_Sw_Mode_CM` (+ `_M131E`) | feature → | 32 | 03 49 ‹a1› | a1 0 hardware / 1 software | ✔ (1 at start, 0 when OpenDeck closes) |
| `Test_Animation_Recovery_Process_CM` (+ `_M131E`) | feature → | 32 | 03 4d | — | ⚠ |
| `Test_FW_Recovery_Process_CM` (+ `_M131E`) | feature → | 32 | 03 48 | — | ⚠ |
| `Test_Factory_Reset_Process_CM` (+ `_M131E`) | feature → | 32 | 03 4c | — | ⚠ |
| `Update_FW_CM` (+ `_M131E`) | — | — | — | | calls Update_FW_by_type_CM |
| `Update_FW_by_type_CM` (+ `_M131E`) | output | output report | 02 44 00 01 00 00 ‹u16› 01 ‹a3› | data, a3 type | ⚠ firmware update |

### 15xKey M4315 (`CoolTEK_SDK_CM_M4315_V03.02.00.dll`)

| Function | Report | Length | Bytes | Parameters | Notes |
| --- | --- | --- | --- | --- | --- |
| `Adjust_Backlight_CM` (+ `_M4315`) | feature → | 32 | 03 34 ‹a1› | a1 brightness 0–100 | ✔ |
| `Check_Rcv_Data_CM` (+ `_M4315`) | input | — | `01 <channel> <length u16> <data…>` | | ✔ |
| `Connect_Device_CM` (+ `_M4315`) | — | — | — | | calls Disconnect_Device_CM, Get_Device_Infos_CM, hid_enumerate |
| `Connect_Device_byPath_CM` (+ `_M4315`) | — | — | — | | calls Disconnect_Device_CM, Get_Device_Infos_CM, hid_open_path |
| `Disconnect_Device_CM` (+ `_M4315`) | — | — | — | | calls hid_close |
| `Display_Color_On_Button_CM` (+ `_M4315`) | feature → | 32 | 03 37 ‹a1› ‹a2› ‹a3› ‹a4› | a1 key 0–14, a2–a4 R G B | ✔ (black clears a key) |
| `Display_Color_On_Screen_CM` (+ `_M4315`) | feature → | 32 | 03 36 ‹a1› ‹a2› ‹a3› | a1–a3 R G B | ✔ (clearing everything) |
| `Display_Picture_On_Button_CM` (+ `_M4315`) | output | output report | 02 36 00 00 ‹u16› ‹a1› 01 | a1 key 0–14, JPEG | ✔ chunks: `02 36 <number u16> <length u16> <key> <last> <data>` |
| `Display_Picture_On_Screen_CM` (+ `_M4315`) | output | output report | 02 35 00 00 ‹u16› 00 01 | JPEG | ✔ (the 15xKey background) a 480 × 272 picture over the whole module |
| `Get_Animation_Infos` | feature ← | 32 | 3d | — | read 0x3d |
| `Get_Backlight_CM` (+ `_M4315`) | feature ← | 32 | 3c | — | ✔ (module info) read 0x3c |
| `Get_Config_Data_CM` (+ `_M4315`) | output | output report | 02 3a | picks 0x39 / 0x3a | asks for configuration data |
| `Get_Config_Infos_CM` (+ `_M4315`) | feature ← | 32 | 3e | — | read 0x3e |
| `Get_Device_Infos_CM` (+ `_M4315`) | feature ← | 32 | 3b | — | ✔ (module info) read 0x3b |
| `Get_FW_Infos_CM_M4315` | — | — | — | | calls Get_FW_Infos_by_type_CM |
| `Get_FW_Infos_by_type_CM` (+ `_M4315`) | feature ← | 32 | 39 | a1 type 0–2 | ✔ (module info) read **0x37 + type** |
| `Get_Serial_Number_CM` (+ `_M4315`) | feature ← | 32 | 3a | — | ✔ (module info) read 0x3a |
| `Save_Factory_Data_CM` (+ `_M4315`) | feature → | 32 | 03 3c | — | ⚠ |
| `Save_Screen_Picture_CM` | output | output report | 02 38 00 00 ‹u16› ‹?› 01 01 01 ‹?› ‹?› ‹?› ‹?› | picture | ⚠ stores a picture or animation in the module (buffer up to 522 240 B = 480 × 272 × 4) |
| `Set_Serial_Number_CM` (+ `_M4315`) | feature → | 32 | 03 35 ‹a2› | a2 length, then the text | ⚠ |
| `Set_Sw_Mode_CM` (+ `_M4315`) | feature → | 32 | 03 3a ‹a1› | a1 0 hardware / 1 software | ✔ (1 at start, 0 when OpenDeck closes) |
| `Test_Animation_Recovery_Process_CM` (+ `_M4315`) | feature → | 32 | 03 3e | — | ⚠ |
| `Test_FW_Recovery_Process_CM` (+ `_M4315`) | feature → | 32 | 03 39 | — | ⚠ |
| `Test_Factory_Reset_Process_CM` (+ `_M4315`) | feature → | 32 | 03 3d | — | ⚠ |
| `Update_FW_CM` (+ `_M4315`) | — | — | — | | calls Update_FW_by_type_CM |
| `Update_FW_by_type_CM` (+ `_M4315`) | output | output report | 02 34 00 01 00 00 ‹u16› 01 ‹a3› | data, a3 type | ⚠ firmware update |

### M3704: a module with 4 screen keys, unreleased (`CoolTEK_SDK_CM_M3704_V01.00.00.dll`)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight_CM_M3704` | feature → | 32 | 03 08 ‹a1› |
| `Check_IO_Status_CM_M3704` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device_CM_M3704` | feature ← | 32 | 0f |
| `Connect_Device_byPath_CM` | feature ← | 32 | 0f |
| `Connect_Device_byPath_CM_M3704` | — | — | calls Connect_Device_byPath_CM |
| `Disconnect_Device_CM_M3704` | — | — | calls hid_close |
| `Display_Color_On_Button_CM_M3704` | feature → | 32 | 03 0b ‹a1› ‹a2› ‹a3› ‹a4› |
| `Display_Color_On_Screen_CM_M3704` | feature → | 32 | 03 0a ‹a1› ‹a2› ‹a3› |
| `Display_Logo_CM_M3704` | feature → | 32 | 03 0c |
| `Display_Picture_On_Button_CM_M3704` | output | ? | 02 0a 00 00 ‹u16› ‹a1› 01 |
| `Display_Picture_On_Screen_CM_M3704` | output | ? | 02 09 00 00 ‹u16› 00 01 |
| `Get_Device_Infos_CM_M3704` | feature ← | 32 | 0f |
| `Get_FW_Infos_CM_M3704` | feature ← | 32 | 0d |
| `Get_FW_Infos_by_type_CM_M3704` | feature ← | 32 | 0d |
| `Get_Serial_Number_CM_M3704` | feature ← | 32 | 0e |
| `Replace_Logo_CM_M3704` | output | ? | 02 0b 00 01 00 00 ‹u16› |
| `Set_Serial_Number_CM_M3704` | feature → | 32 | 03 09 ‹a2› |
| `Test_FW_Recovery_Process_CM_M3704` | feature → | 32 | 03 0d |
| `Update_FW_CM_M3704` | — | — | calls Update_FW_by_type_CM_M3704 |
| `Update_FW_by_type_CM_M3704` | output | ? | 02 08 00 01 00 00 ‹u16› 01 ‹a3› |

### M3700: a screen (`CoolTEK_SDK_CM_M3700_V01.00.00.dll`)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight_CM_M3700` | feature → | 32 | 03 07 ‹a1› |
| `Check_IO_Status_CM_M3700` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device_CM_M3700` | feature ← | 32 | 0e |
| `Connect_Device_byPath_CM` | feature ← | 32 | 0e |
| `Connect_Device_byPath_CM_M3700` | — | — | calls Connect_Device_byPath_CM |
| `Disconnect_Device_CM_M3700` | — | — | calls hid_close |
| `Display_Color_On_Screen_CM_M3700` | feature → | 32 | 03 09 ‹a1› ‹a2› ‹a3› |
| `Display_Logo_CM_M3700` | feature → | 32 | 03 0a |
| `Display_Picture_On_Screen_CM_M3700` | output | ? | 02 08 00 00 ‹u16› 00 01 |
| `Get_Device_Infos_CM_M3700` | feature ← | 32 | 0e |
| `Get_FW_Infos_CM_M3700` | feature ← | 32 | 0c |
| `Get_FW_Infos_by_type_CM_M3700` | feature ← | 32 | 0c |
| `Get_Serial_Number_CM_M3700` | feature ← | 32 | 0d |
| `Replace_Logo_CM_M3700` | output | ? | 02 09 00 01 00 00 ‹u16› |
| `Set_Serial_Number_CM_M3700` | feature → | 32 | 03 08 ‹a2› |
| `Test_FW_Recovery_Process_CM_M3700` | feature → | 32 | 03 0b |
| `Update_FW_CM_M3700` | — | — | calls Update_FW_by_type_CM_M3700 |
| `Update_FW_by_type_CM_M3700` | output | ? | 02 07 00 01 00 00 ‹u16› 01 ‹a3› |

### M4300: a small screen with keys (`CoolTEK_SDK_CM_M4300_V01.01.00.dll`)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight_CM_M4300` | feature → | 32 | 03 54 06 |
| `Check_IO_Status_CM_M4300` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device_CM_M4300` | — | — | calls Disconnect_Device_CM_M4300, Get_Device_Infos, hid_enumerate, hid_init, hid_open |
| `Disconnect_Device_CM_M4300` | — | — | calls hid_close |
| `Display_Color_On_Button_CM_M4300` | feature → | 32 | 03 57 ‹?› ‹?› ‹?› ‹a4› |
| `Display_Color_On_Screen_CM_M4300` | feature → | 32 | 03 56 01 ‹?› ‹?› |
| `Display_Logo_CM_M4300` | feature → | 32 | 03 58 |
| `Display_Picture_On_Button_CM_M4300` | output | ? | 02 56 ‹u16› ‹u16› ‹?› 01 |
| `Display_Picture_On_Screen_CM_M4300` | output | ? | 02 55 ‹u16› ‹u16› 00 01 |
| `Get_Backlight_CM_M4300` | feature ← | 32 | ? |
| `Get_Device_Infos` | feature ← | 32 | ? |
| `Get_FW_Infos_CM_M4300` | feature ← | 32 | ? |
| `Get_Serial_Number_CM_M4300` | feature ← | 32 | 5a (+1 more) |
| `Replace_Logo_CM_M4300` | output | ? | 02 57 00 01 ‹u16› ‹u16› |
| `Set_Serial_Number_CM_M4300` | feature → | 32 | ? |
| `Test_FW_Recovery_Process` | feature → | 32 | 03 59 |
| `Test_sub_FW_Recovery_Process` | feature → | 32 | 03 5c |
| `Update_FW_CM_M4300` | — | — | no HID traffic |
| `Update_FW_by_type` | output | ? | 02 58 ‹?› 01 ‹u16› ‹u16› 01 ‹?› |
| `Update_sub_FW` | — | — | no HID traffic |
| `get_FW_Infos_by_type` | feature ← | 32 | ? |

### M4600: a screen (`CoolTEK_SDK_CM_M4600_V02.00.00.dll`)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight_CM_M4600` | feature → | 32 | 03 25 ‹a1› |
| `Check_IO_Status_CM_M4600` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device_CM_M4600` | feature ← | 32 | 2c |
| `Connect_Device_byPath_CM` | feature ← | 32 | 2c |
| `Connect_Device_byPath_CM_M4600` | — | — | calls Connect_Device_byPath_CM |
| `Disconnect_Device_CM_M4600` | — | — | calls hid_close |
| `Display_Color_On_Screen_CM_M4600` | feature → | 32 | 03 27 ‹a1› ‹a2› ‹a3› |
| `Display_Logo_CM_M4600` | feature → | 32 | 03 28 |
| `Display_Picture_On_Screen_CM_M4600` | output | ? | 02 26 00 00 ‹u16› 00 01 |
| `Get_Device_Infos_CM_M4600` | feature ← | 32 | 2c |
| `Get_FW_Infos_CM_M4600` | feature ← | 32 | 2a |
| `Get_FW_Infos_by_type_CM_M4600` | feature ← | 32 | 2a |
| `Get_Serial_Number_CM_M4600` | feature ← | 32 | 2b |
| `Replace_Logo_CM_M4600` | output | ? | 02 27 00 01 00 00 ‹u16› |
| `Set_Serial_Number_CM_M4600` | feature → | 32 | 03 26 ‹a2› |
| `Test_FW_Recovery_Process_CM_M4600` | feature → | 32 | 03 29 |
| `Update_FW_CM_M4600` | — | — | calls Update_FW_by_type_CM_M4600 |
| `Update_FW_by_type_CM_M4600` | output | ? | 02 25 00 01 00 00 ‹u16› 01 ‹a3› |

### M5000: a screen (`CoolTEK_SDK_CM_M5000_V01.00.00.dll`)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight_CM_M5000` | feature → | 32 | 03 06 ‹a1› |
| `Check_IO_Status_CM_M5000` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device_CM_M5000` | — | — | calls Disconnect_Device_CM_M5000, Get_Device_Infos_CM_M5000, hid_enumerate |
| `Disconnect_Device_CM_M5000` | — | — | calls hid_close |
| `Display_Color_On_Screen_CM_M5000` | feature → | 32 | 03 08 ‹a1› ‹a2› ‹a3› |
| `Display_Logo_CM_M5000` | feature → | 32 | 03 0a |
| `Display_Picture_On_Screen_CM_M5000` | output | ? | 02 07 00 00 ‹u16› 00 01 |
| `Get_Device_Infos_CM_M5000` | feature ← | 32 | 0d |
| `Get_FW_Infos_CM_M5000` | — | — | calls get_FW_Infos_by_type_CM_M5000 |
| `Get_Serial_Number_CM_M5000` | feature ← | 32 | 0c |
| `Replace_Logo_CM_M5000` | output | ? | 02 09 00 01 00 00 ‹u16› |
| `Set_Serial_Number_CM_M5000` | feature → | 32 | 03 07 ‹a2› |
| `Test_FW_Recovery_Process_CM_M5000` | feature → | 32 | 03 0b |
| `Update_FW_CM_M5000` | — | — | calls Update_FW_by_type_CM_M5000 |
| `Update_FW_by_type_CM_M5000` | output | ? | 02 06 00 01 00 00 ‹u16› 01 ‹a3› |
| `get_FW_Infos_by_type_CM_M5000` | feature ← | 32 | 0b |

### General CoolTEK SDK (`CoolTEK_SDK_V01.00.00.dll`, an older device with keys and a screen)

| Function | Report | Length | Bytes |
| --- | --- | --- | --- |
| `Adjust_Backlight` | feature → | 32 | 03 21 ‹a1› |
| `Check_Button_Status` | input | — | `01 <channel> <length u16> <data…>` |
| `Connect_Device` | — | — | calls Disconnect_Device, Get_Device_Infos, hid_enumerate |
| `Disconnect_Device` | — | — | calls hid_close |
| `Display_Color_On_Button` | feature → | 32 | 03 24 ‹a1› ‹a2› ‹a3› ‹a4› |
| `Display_Color_On_Screen` | feature → | 32 | 03 23 ‹a1› ‹a2› ‹a3› |
| `Display_Logo` | feature → | 32 | 03 25 |
| `Display_Picture_On_Button` | output | ? | 02 23 00 00 ‹u16› ‹a1› 01 |
| `Display_Picture_On_Screen` | output | ? | 02 22 00 00 ‹u16› 00 01 |
| `Get_Device_Infos` | feature ← | 32 | 28 |
| `Get_FW_Infos` | — | — | calls get_FW_Infos_by_type |
| `Get_Serial_Number` | feature ← | 32 | 27 |
| `Replace_Logo` | output | ? | 02 24 00 01 00 00 ‹u16› |
| `Set_Serial_Number` | feature → | 32 | 03 22 ‹a2› |
| `Update_FW` | — | — | calls Update_FW_by_type |
| `Update_FW_by_type` | output | ? | 02 21 00 01 00 00 ‹u16› 01 ‹a3› |
| `get_FW_Infos_by_type` | feature ← | 32 | 26 |

### Notes on the extractor's tables

- The M4300 writes the report ID and the command as one 16-bit word (split into bytes in the table, e.g.
  `03 54`). Some of the M4300's reads have IDs not worked out (`?`).
- `Get_Config_Data*` shows one of its two commands (see the Parameters column).
- The `Update_FW_by_type*` and `Save_Screen_Picture*` headers are longer (page, chunk number, flags, type); they're
  described only roughly, since the plugin never uses them.
