// Draws the marched volume image onto its view, scaled to the view's pixels.
//
// The uniform block mirrors `BlitUniforms` in `volume_render.rs`.

struct BlitUniforms {
    // Viewport origin and size in physical pixels.
    viewport_min_size_px: vec4<f32>,
    // Whether the target expects linear output, then unused.
    flags: vec4<f32>,
}

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(0) @binding(2) var<uniform> blit: BlitUniforms;

// One triangle covering the whole viewport; no vertex buffers are bound.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));

    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let viewport_size = max(blit.viewport_min_size_px.zw, vec2<f32>(1.0, 1.0));
    let fraction = (position.xy - blit.viewport_min_size_px.xy) / viewport_size;
    let color = textureSampleLevel(image, image_sampler, fraction, 0.0).rgb;

    // The image holds the colours as authored, in gamma space; an sRGB target
    // needs the linear values it converts back on write.
    if blit.flags.x > 0.5 {
        return vec4<f32>(gamma_to_linear(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
    }

    return vec4<f32>(color, 1.0);
}

fn gamma_to_linear(color: vec3<f32>) -> vec3<f32> {
    let lower = color / 12.92;
    let higher = pow((color + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));

    return select(higher, lower, color <= vec3<f32>(0.04045));
}
