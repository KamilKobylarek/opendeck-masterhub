# The modules' physical layout in OpenDeck (PR #359)

Today OpenDeck draws every device a plugin registers as a grid of keys (`rows` × `columns`) and a row of encoders.
The three M003K knobs, set in a triangle, show as a line, and the faders and rollers as knobs. The draft PR
[nekename/OpenDeck#359 "Explicit device layout"](https://github.com/nekename/OpenDeck/pull/359) lets a plugin
describe each control's place, size and shape. The plugin already sends it (since 2026-10-26); OpenDeck without the
PR ignores it. This document is how.

## Status of the PR (2026-10-26)

- A draft, open, not merged; the author is gathering opinions on the approach. Last changed 2026-10-20.
- The latest OpenDeck release, 2.14.2 (2026-10-21), has no layouts.
- The format may still change before it's merged; the plugin follows it then.

## The format in the PR (`layoutVersion: 1`)

Two new, optional fields in `registerDevice`, beside the existing ones (`rows`, `columns`, `encoders`, …):

```json
"layoutVersion": 1,
"layout": {
  "canvas": { "width": 400, "height": 300, "unit": "px" },
  "surfaces": [
    { "id": "screen", "kind": "display", "x": 0, "y": 0, "width": 400, "height": 300, "pixelWidth": 480, "pixelHeight": 272 }
  ],
  "controls": [
    { "controller": "Keypad", "position": 0, "shape": "rect", "x": 10, "y": 10, "width": 70, "height": 70, "surface": "screen" },
    { "controller": "Encoder", "position": 0, "shape": "circle", "cx": 200, "cy": 150, "r": 40 }
  ]
}
```

- `canvas`: the coordinate system (`u16` integers).
- `surfaces`: optional areas, `display` (grouping only) or `touchpanel` (drawn).
- `controls`: what actions are dropped on. `controller` is `Keypad` or `Encoder`; `position` is the same number
  the plugin already sends in its events. Shape `rect` (`x`, `y`, `width`, `height`) or `circle` (`cx`, `cy`, `r`).
  Any shape goes with any controller, so a fader can be a narrow `Encoder` rectangle.
- A version other than 1: OpenDeck ignores the layout and draws as before.

**Backward compatibility:** `DeviceInfo` in today's OpenDeck has no `deny_unknown_fields`, so 2.14 and older just
skip both fields. The plugin can always send them, without checking OpenDeck's version.

## Each module's layout

One OpenDeck device per module, as now. The controls and their position numbers don't change, so users' profiles
stay valid. The plugin already knows the controls' sizes and places: it draws them in the settings page's
**Pictures** view (`module_picture` in `src/placement.rs`), so the layout can use the same proportions.

| Module | Controls in the layout | Notes |
| --- | --- | --- |
| 15xKey M4315 | 15 × `Keypad` `rect` in a 5 × 3 grid, one `display` surface of 480 × 272 | one screen under the keys, as the plugin's keypad background already uses |
| 3xKnob M003K | 3 × `Encoder` `circle` **in a triangle** | the apex at the top: 0 bottom left, 1 bottom right, 2 top (checked on hardware) |
| 5xFader M005S | 5 × `Encoder` `rect`, narrow and tall, side by side | no "fader" shape; a rectangle does, and the events stay as now (turns by the change in position) |
| 2xRoller M032LE | 2 × `Encoder` `rect`, wide, side by side | position 0 in OpenDeck is the left roller (the module reports the right one first; `encoder_index` swaps them) |
| Dial M131E | 1 × `Encoder` `circle` + a 240 × 240 `display` surface | |

### A turned module

The plugin already knows every module's turn (its socket and the base's orientation) and registers the device
again when it changes. The layout is worked out at 0° and its coordinates turned by the same angle (90° / 180° /
270° about the `canvas` centre, swapping width and height at 90° / 270°). One function covers every module.
Directions and pictures stay as they are, as in the official app.

### Functions per knob (1–4)

With several functions a knob or the dial has `encoders × functions` positions but only `encoders` physical
knobs. The PR's renderer shows only the positions in `controls` (others are hidden), so every one is there:
function 1 where the physical knob is, functions 2–4 as smaller circles in a row each below the module, in
OpenDeck's order. The same goes for the dial's key: a small circle under its screen.

## In the plugin

- `layout(kind, rot, functions)` in `src/placement.rs` builds it from the shapes of the settings page's module
  pictures (`control_shapes`), turned with the module, each control by its OpenDeck position.
- `register()` in `src/main.rs` sends `"layoutVersion": 1, "layout": …` with every `registerDevice`, again when a
  module is turned.
- A test checks that for every module, turn and number of functions each OpenDeck position is in the layout once,
  inside the canvas, and that the coordinates are whole and non-negative (the PR's types are `u16`; a bad value
  would fail the whole registration there).

To look at it: build OpenDeck from the `BrendanGrant/OpenDeck:explicit-device-layout` branch (Deno, then
`deno task tauri build`). It uses the same settings folder as an installed OpenDeck, so keep a copy of that first.

## Later: the whole base as one device

A layout describes one registered device. Showing the whole base with its modules where they lie would mean
registering it as one device with every module's controls. That changes the position numbers (profiles would stop
matching) and needs registering again whenever a module is plugged in. To be considered only after single-module
layouts are in, as a separate option.
