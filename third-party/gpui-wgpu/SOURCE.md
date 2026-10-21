# GPUI Fast WGPU source

This directory receives one published package, `gpui-fast-wgpu` 0.1.10, from
[the official crates.io archive](https://static.crates.io/crates/gpui-fast-wgpu/gpui-fast-wgpu-0.1.10.crate).
The archive SHA-256 is
`cac7d34083b2611b49b2f415a16aa9e33620b127a25e981f835db8fe72f07423`.
Its package-local lockfile is omitted: the consuming workspace owns resolution.
Original license, README and both published manifests are retained.

`upstream.json` records the 26 original package files and their unmodified
SHA-256 hashes. Adaptations are measured against those bytes. NSH does not
receive another GPUI core, platform workspace, application runtime or renderer.

The archive's original tests refer to assets and one shared rounded-clip
fixture outside the published package. Only those referenced fixtures are
received into `test-assets`: IBM Plex Sans Regular, Lilex Regular and Bold,
their two original OFL license texts, and `rounded_clip_fixtures.rs`. They
come byte-for-byte from the upstream Git revision
`b836dd970b26830f2124b7cf91b0e19018fd12da`, not the crate archive. The separate
`test_fixtures` inventory records each original source path, local mapping and
hash. Test include paths are redirected to these files; no fixture assertions
or test-module admission conditions are weakened.

The local production `offscreen` feature enables only the renderer's image
dependency. `WgpuHeadlessRenderer` exposes its own inherent pixel, image,
scene, adapter-information and sprite-atlas methods. The original test/bench
`PlatformHeadlessRenderer` bridge delegates its existing three methods to the
same implementation. No GPUI core feature or trait is extended.

The renderer reuses its GPU target and same-size readback allocation, strips
GPU row padding into caller-owned top-down RGBA8 storage and handles BGRA
formats. All mapping failures and timeouts unmap and discard the staging
buffer. Synchronous readback uses the original bounded GPU wait; success does
not establish terminal publication or a frame-rate guarantee. Adapter policy
is unchanged, and diagnostics distinguish a software-emulated device.

The archive omitted upstream's WGSL-validation test dev dependency; the local
manifest restores Naga at the backend's exact 30.0.1 version with `wgsl-in`.
Production uses `offscreen` without `test-support`; unit tests use
`offscreen,test-support`. Five real headless GPU regressions preserve the reviewed offscreen
implementation's color, odd-stride, RGBA/BGRA, invalid-destination recovery,
target reuse and resize assertions. Their execution belongs to the receiving host.
