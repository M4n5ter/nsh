# GPUI terminal transport

This Linux transport embeds the existing NSH GPUI `AppView`, using the host's
original `EngineConfig`. It renders the same scene with the production WGPU
offscreen API. It supplies neither a second application runtime nor terminal IO.
The transport uses the official GPUI Fast 0.1.10 core. Only the WGPU renderer
is received under `third-party/gpui-wgpu`, where its own `offscreen` feature
exposes production pixel readback. It requires no core patch or application
repository, and `test-support` is not a production dependency.

The host constructs `gpui_wgpu::WgpuHeadlessRenderer` and creates `TerminalPlatform`, uses `Application::with_platform` and
`run_embedded`, and pumps foreground work when woken or a timer is due. One
native window is supported. The platform keeps two background executor workers
and at most 64 foreground jobs per pump. Frames are requested when dirty; there
is no imposed 60 Hz timer. GPU readback currently waits synchronously, so this
first API does not establish a frame-rate or input-delivery latency result.
`pump_with_draw(false)` still runs foreground and window lifecycle work while
the output pipeline is full. It preserves the original dirty frame without
re-waking just for that deferred draw; the host allows drawing once a candidate
slot is available. `has_pending()` includes that dirty frame for input barriers.

The sole terminal input and output owners remain in the embedding application.
Use actual terminal pixel dimensions for resize and actual negotiated pixel
mouse reports. Coordinates supplied to this platform are physical pixels at
scale 1; it does not guess cell dimensions or synthesize pixel precision from
cell reports. Keyboard callbacks run before the current native text handler
commits a propagated printable character. A bracketed paste is one text commit.

Each draw produces a `CandidateFrame` containing an issuer-bound `FrameToken`,
viewport generation and tightly packed top-down RGBA8 pixels. In the draw sink,
pair that token with the existing AppView's immutable native input snapshot.
Retain one output transaction in flight and one replaceable pending candidate.
Once any upload byte is written, complete its entire APC sequence.

`KittyFrame::encode` performs bounded PNG compression and admits the complete
transaction under 8 MiB. Upload into the non-displayed owned image slot, accept
only the sole reader's successful matching `(image id, placement 1)` reply,
then flush `retire_previous` before `mark_presented`. An ACK timeout or error
ends this graphics session; reusing its slots could misattribute a late reply.
The positive capability query's nonce must differ from both image slots.
Restoration deletes only those owned slots, never every terminal image.

Resize immediately revokes the old viewport's input geometry. Discard its
pending candidate, finish any already-started output indivisibly, and recreate
the next frame at the new dimensions. A GPU completion is a candidate, not a
terminal publication. A terminal ACK proves protocol acceptance, not display
vsync. The writer's completed receipt is the only caller of `mark_presented`.

For incoming ordinary presses and scrolling, the host first dispatches against
that token's original NSH input snapshot and native owner lease. A rejected
lease cannot fall through to a new hit target. `dispatch_input_exact` is only
the conservative path for opaque input; it is not the ordinary wheel model.
Native window handles, webview portals and OS path pickers are unsupported.
Clipboard writes are returned to the host for its unique output owner.

The GPUI workspace patches only `gpui-fast-wgpu` to that relative source
directory; the main workspace excludes this renderer package. Build the GPUI workspace directly; no Maka
checkout or absolute dependency path is needed. Renderer tests use
`--features offscreen,test-support`; the production terminal dependency uses
only `offscreen` and `font-kit`.
