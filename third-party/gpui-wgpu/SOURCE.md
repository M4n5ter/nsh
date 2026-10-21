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
