---
title: Terminal
description: Run Native Shell apps in a terminal, package console applications, and embed an independent NSH surface in a Ratatui app.
order: 15.75
group: platforms
---

# Terminal

Native Shell renders apps in terminals on macOS, Windows and Linux with Ratatui and Crossterm. The same TSX and
Rust components, reactive graph, Taffy flex and grid layout, virtualized lists, networking and sandbox run behind
the terminal view. A terminal application can embed an independent `AppView` in any rectangular region it owns.

## Running an app

```sh
nsh run my-app --target tui
```

The host uses the terminal's alternate screen, keyboard, mouse and bracketed paste, and restores the terminal when
the app closes. TSX edits arrive through the same development server as on graphical platforms. Ctrl+Q closes the
app. The distributed host is in `runtime/tui/`; a checkout builds `nsh-tui-host` on demand.

The operations workbench combines sidebar navigation,
search and status filters, service metrics, a detail inspector, canvas charts, an activity timeline and a deployment
dialog. Its compact 120×34 layout shows seven complete service rows with metrics and a chart; 160×50 also shows
the inspector and activity timeline. Shorter terminals fold away secondary content to keep the fleet visible:

```sh
nsh run examples/terminal-workbench --target tui
```

The host can also load a built bundle directly:

```sh
nsh build my-app --release -o build
dist/nsh/runtime/tui/nsh-tui-host --bundle build/bundle.nshc --size 120x34
```

`--size` uses columns and rows. In an interactive session the current terminal size determines the viewport;
scripts use the supplied size as their initial viewport.

## Layout and drawing

NSH keeps layout in points. By default, one terminal cell represents **8×16 points**, so a 120×40 terminal supplies
a 960×640-point viewport to Taffy. Padding, flex and grid, scroll offsets, image sizes and pointer hit testing
share that coordinate system. Resizing the terminal relays the new viewport to the engine and reflows the app.

Text wraps by terminal column width: Chinese and many emoji occupy two columns, and combining sequences stay
together. Text spans preserve colors, bold and italic, underlines and strikethrough; bidirectional paragraphs
reorder their runs for display. The terminal's font determines the face and glyph size. Explicit line height and
letter spacing are rounded to terminal rows and columns.

Backgrounds, borders and clipping follow the retained view tree. On startup, the interactive host detects image
protocols and the terminal's physical cell dimensions. Images and recorded canvas drawings use
[`ratatui-image`](https://docs.rs/ratatui-image/11.1.0/ratatui_image/) with Kitty, Sixel or iTerm2 graphics when
available. Encoding runs on the graphics worker. Colored upper and lower half blocks provide a portable drawing
path and preserve composition where text or overlays overlap a graphic. The terminal controls how RGB colors map
to its palette.

`NSH_IMAGE_PROTOCOL` selects `auto` (the default), `halfblocks`, `kitty`, `sixel` or `iterm2`. `halfblocks` starts
directly with ordinary text rendering. Selecting a graphics protocol uses the terminal's reported pixel dimensions.
Headless scripts render reproducible text snapshots. An embedding host selects its protocol explicitly through
`AppView::set_image_protocol`; creating an embedded view leaves terminal IO with its host.

The existing [`nsh/ui/mobile`](../components/index.md) components render through these primitives. Give each
interactive row enough height to occupy at least one terminal row, use clear color contrast and keep important
labels legible at the terminal's column width.

Canvas paths, gradients, clipping, transforms, compositing and image drawing are rasterized on a worker; canvas
text uses shaped system-font outlines. Regular `Text` remains terminal glyphs. Images support PNG, JPEG, GIF and
WebP, including animated GIF and WebP within the decoder's resource bounds.

## Keyboard and mouse

| Keys | Action |
|---|---|
| Tab, Shift+Tab | Focus the next or previous input, pressable or scroll view, within the newest modal |
| Enter, Space | Activate the focused pressable |
| Escape | Leave an input; otherwise go back or request modal dismissal |
| Arrows, Page Up / Down, Home / End | Scroll the focused scroll view or a scrollable ancestor |
| Ctrl+Q | Close the terminal host |
| Ctrl+C | Copy an input selection; outside an input, close the host |

Click to focus or activate controls. The mouse wheel scrolls vertically; Shift+wheel scrolls horizontally. Pointer
positions are converted from cells into the same virtual points used by layout.

Inputs support grapheme-aware cursor movement and deletion, Shift selection, word movement with Ctrl/Alt+arrows,
Home / End, Ctrl+Home / End, select all, undo and redo, and a clipboard owned by the surface. Ctrl+A/C/X/V select,
copy, cut and paste; Ctrl+Z and Ctrl+Shift+Z or Ctrl+Y undo and redo. Secure fields mask their contents and protect
copy and cut. Input methods enter committed text through the terminal; bracketed paste enters text as one edit.
Enter follows `submitBehavior`, and Shift/Alt+Enter inserts a newline into a multiline input.

The app's [`nsh/keymap`](../modules/nsh-keymap.md) bindings run before default handling. Terminal emulators decide
which keys and modifiers reach an app; choose shortcuts that your supported terminal forwards.

Selectable text supports mouse selection and Ctrl+A/C, and inline text links activate through pointer hit testing.
Native alert and action-sheet overlays use Tab or the arrows to choose a button and Enter to activate it.

## Packaging

```sh
nsh package my-app --target tui
nsh package my-app --target tui --run
```

The console runner and its resources travel together:

```text
out/<Name>/
  <Name>                 executable; <Name>.exe on Windows
  nsh/bundle.nshc        compiled TSX application
  nsh/assets/…           imported assets
  nsh/app.json           application metadata
  nsh/capabilities.json  application capabilities, when configured
```

The same directory is archived as `<Name>.tar.gz` on macOS and Linux and `<Name>.zip` on Windows. Package on the
operating system where the executable will run. Keep the `nsh/` directory beside the executable. The terminal
runtime uses Rust libraries and the operating system's console facilities. `--run` launches it in the current
terminal and waits until it closes. Rust root applications use the same package layout with their root compiled
into the runner.

## Embedding in a Ratatui app

`nsh-platform-tui` exposes an `AppView` with its own engine, retained surface and input state. Your host owns the
terminal, event loop and other Ratatui widgets. Load a bundle, then poll commits, advance animations, forward
events and draw into the region assigned to the view:

```rust
use nsh_platform_tui::{AppOptions, AppView, Bundle};

let mut view = AppView::new(AppOptions::default());
let code = std::fs::read_to_string("app/bundle.js")?;
view.load(Bundle::source(code).into())?;

// In the host's event loop:
view.poll()?;
view.tick(start.elapsed().as_secs_f64())?;
view.resize(area.width, area.height);
view.render(area, frame.buffer_mut());
```

Forward keyboard and paste events with `view.handle_event(event)`. Translate mouse positions to coordinates
relative to the view's region before forwarding them; route events only to the surface that should receive them.
Dropping the view or calling `stop()` ends its engine.

`AppOptions::asset_base` resolves `asset:/` URIs from an asset directory or HTTP base.
`AppOptions::limits` configures the VM limits; embedding defaults to `Limits::default()`, while the standalone
hosts use `Limits::standalone_desktop()`. `AppOptions::metrics` accepts a validated `CellMetrics` value for hosts
that choose a different virtual point scale. See [Sandbox and capabilities](./sandbox-and-capabilities.md).

For [native components](./native-components.md), implement `NativeComponent` and register its factory with
`AppView::register_component(name, factory)` before loading the app. Each mounted leaf gets its own instance,
receives props and commands, paints into its clipped region, and can return native events to TSX. Hosts can supply
terminal presentations for content such as maps, browser pages and media controls. The built-in `WebView` and
`Video` elements display descriptive tiles and report an error event for the app to handle.

## Reproducible interaction

The host's `--script` runs against an in-memory Ratatui buffer and saves UTF-8 text snapshots. Steps are separated
by semicolons or newlines:

```sh
nsh-tui-host --bundle build/bundle.nshc --size 120x40 \
  --script "wait 300; shot home.txt; key Tab; type 你好 👋; resize 100 35; wait 300; shot resized.txt; quit"
```

| Step | Action |
|---|---|
| `wait <ms>` | Process commits and animations for the duration |
| `shot <file.txt>` | Save the current text buffer |
| `tap <column> <row>` | Click a cell |
| `hover <column> <row>` | Move the pointer to a cell |
| `wheel <column> <row> <dx> <dy>` | Scroll in wheel detents at a cell |
| `type <text>` | Paste UTF-8 text into the focused input |
| `key <name>` | Send a key, such as `Tab`, `Enter`, `Esc`, `Ctrl+a` or `Shift+Left` |
| `selectall` | Select all text in the focused input |
| `resize <columns> <rows>` | Change the viewport and relayout |
| `dark`, `light` | Set the surface appearance |
| `expect <text>` | Assert that the snapshot contains text |
| `expect-focus` | Assert that a node has keyboard focus |
| `quit` | Stop the app |

Snapshots preserve characters and layout. Inspect colors and half-block graphics in an interactive terminal.

## See also

- [CLI](./cli.md)
- [Packaging](./packaging.md)
- [Images and assets](./images-and-assets.md)
- [Canvas](./canvas.md)
- [Platform overview](./platforms.md)
