// Modified by Longbridge for gpui-fast.
//! Exercise the WebGL instance-texture transport on the native GPU as well:
//! masks, paths and nested scenes must use the same absolute record indices.

use super::{assert_same_pixels, device_size, pixel, quad, read_back, sp};

#[path = "../../../test-assets/rounded_clip_fixtures.rs"]
mod fixtures;

fn texture_renderer() -> crate::wgpu_renderer::WgpuHeadlessRenderer {
    let mut renderer = crate::wgpu_renderer::WgpuHeadlessRenderer::new()
        .expect("instance-texture tests require a GPU");
    let core = &mut renderer.core;
    let device = &core.resources.device;
    let layouts = crate::wgpu_renderer::WgpuRendererCore::create_bind_group_layouts(device, true);
    let pipelines = crate::wgpu_renderer::WgpuRendererCore::create_pipelines(
        device,
        &layouts,
        core.target_format,
        wgpu::CompositeAlphaMode::Opaque,
        core.rendering_params.path_sample_count,
        false,
        true,
    );
    let (instances, capacity) = crate::wgpu_renderer::WgpuRendererCore::create_instance_texture(
        device,
        // Short rows make the existing real mask, path and nested-scene
        // fixtures cross texture rows while retaining the normal shaders.
        37 * 37 * 16,
        37,
    );
    core.resources.bind_group_layouts = layouts;
    core.resources.pipelines = pipelines;
    core.resources.instance_data = instances;
    core.instance_data_capacity = capacity;
    core.instance_data_alignment = 16;
    core.uses_webgl_instance_data = true;
    core.fast_frame = Default::default();
    renderer
}

#[gpui::test]
fn rounded_overflow_retained_child_follows_ancestor_clip(cx: &mut gpui::TestAppContext) {
    let mut renderer = texture_renderer();
    fixtures::retained_frames(cx, |scene, size| {
        gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, scene, size)
            .expect("retained texture-transport frame rendered")
            .into_raw()
    });
}

fn render_div_fixture(cx: &mut gpui::TestAppContext, build: fn() -> gpui::Div) -> (Vec<u8>, usize) {
    let mut renderer = texture_renderer();
    fixtures::render_with(cx, build, |scene, size| {
        gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, scene, size)
            .expect("texture-transport frame rendered")
            .into_raw()
    })
}

#[gpui::test]
fn rounded_overflow_inside_scroll_tiles_matches_direct_pixels(cx: &mut gpui::TestAppContext) {
    let mut renderer = texture_renderer();
    fixtures::render_with(cx, fixtures::square_fixture, |scene, size| {
        let direct =
            gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, scene, size)
                .unwrap()
                .into_raw();
        let tiled = fixtures::tiled_scene(scene, size);
        let actual =
            gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, &tiled, size)
                .unwrap()
                .into_raw();
        assert_same_pixels(&actual, &direct, size.width.0 as usize);
        actual
    });
}

#[test]
fn instance_texture_keeps_mixed_primitives_across_reused_frames() {
    use gpui::{ContentMask, Path, Scene, point, px};

    let mut renderer = texture_renderer();
    for count in [9, 1, 9] {
        let mut scene = Scene::default();
        let color = if count == 1 {
            gpui::green()
        } else {
            gpui::red()
        };
        for index in 0..count {
            scene.insert_primitive(quad(sp(index as f32 * 2., 0., 2., 8.), color));
        }
        let mut path = Path::new(point(px(2.), px(12.)));
        path.line_to(point(px(14.), px(12.)));
        path.line_to(point(px(14.), px(24.)));
        path.line_to(point(px(2.), px(24.)));
        path.line_to(point(px(2.), px(12.)));
        path.content_mask = ContentMask {
            bounds: sp(0., 0., 32., 32.).map(|value| px(value.0)),
        };
        path.color = gpui::blue().into();
        scene.insert_primitive(path.scale(1.));
        scene.finish();

        let pixels = renderer
            .render_scene_to_image(&scene, device_size(32, 32))
            .expect("mixed instance-texture frame rendered")
            .into_raw();
        assert_eq!(
            pixel(&pixels, 32, 1, 3),
            if count == 1 {
                [0, 128, 0, 255]
            } else {
                [255, 0, 0, 255]
            },
        );
        assert_eq!(
            pixel(&pixels, 32, 17, 3),
            if count == 1 {
                [0, 0, 0, 255]
            } else {
                [255, 0, 0, 255]
            },
        );
        assert_eq!(pixel(&pixels, 32, 8, 18), [0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, 32, 30, 30), [0, 0, 0, 255]);
        assert!(matches!(
            &renderer.core.resources.instance_data,
            crate::wgpu_renderer::InstanceData::Texture { width: 37, .. }
        ));
    }
}

#[test]
fn instance_texture_upload_preserves_bits_and_partial_texels_across_rows() {
    use crate::wgpu_renderer::{InstanceData, WgpuRendererCore};

    let mut renderer = texture_renderer();
    let resources = &mut renderer.core.resources;
    let texture = resources.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("instance_texture_upload_readback"),
        size: wgpu::Extent3d {
            width: 7,
            height: 5,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    resources.instance_data = InstanceData::Texture {
        view: texture.create_view(&Default::default()),
        texture: texture.clone(),
        width: 7,
        height: 5,
    };
    let mut expected = vec![0xa5; 7 * 5 * 16];
    WgpuRendererCore::write_instance_texture(resources, 0, &expected);
    // The first write begins inside a row and ends inside a texel. The
    // second starts at zero and ends in a partial row, as packed frames do.
    for (offset, length) in [(3 * 16, (3 * 7 + 2) * 16 + 7), (0, (3 * 7 + 1) * 16)] {
        let data: Vec<u8> = (0..length).map(|index| (index * 73 + 29) as u8).collect();
        WgpuRendererCore::write_instance_texture(resources, offset as u64, &data);
        expected[offset..offset + length].copy_from_slice(&data);
        expected[offset + length..offset + length.next_multiple_of(16)].fill(0);
        let actual = read_back(
            &resources.device,
            &resources.queue,
            &texture,
            wgpu::TextureFormat::Rgba32Uint,
        );
        assert_eq!(
            actual, expected,
            "upload at byte {offset} of {length} bytes"
        );
    }
}
