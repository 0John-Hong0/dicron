// Orthographic ray marching for the 3D volume rendering view.
//
// The uniform block mirrors `VolumeUniforms` in `three_d_state.rs`: ten
// `vec4<f32>` members in the same order, so both sides agree on 160 bytes.

struct VolumeUniforms {
    // Camera position in volume millimetres, then the orthographic half height.
    eye_mm_ortho_half_height: vec4<f32>,
    // Screen-right direction, then the viewport's width / height ratio.
    right_aspect: vec4<f32>,
    // Screen-up direction, then the ray marching step in millimetres.
    up_step_mm: vec4<f32>,
    // View direction, then the distance one unit of opacity is defined over.
    forward_opacity_unit_mm: vec4<f32>,
    // Volume extent in millimetres, then the value a texel of 0 stands for.
    size_mm_value_min: vec4<f32>,
    // Texture size in voxels, then the value a texel of 1 stands for.
    dims_value_max: vec4<f32>,
    // Viewport origin and size in physical pixels.
    viewport_min_size_px: vec4<f32>,
    // Transfer function domain, the step limit, and shading on or off.
    lut_range_steps_shading: vec4<f32>,
    // Ambient, diffuse and specular weights, then the specular power.
    ambient_diffuse_spec_shine: vec4<f32>,
    // Background colour, then whether the target expects linear output.
    background_srgb: vec4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: VolumeUniforms;
@group(0) @binding(1) var volume_texture: texture_3d<f32>;
@group(0) @binding(2) var transfer_texture: texture_2d<f32>;
@group(0) @binding(3) var linear_sampler: sampler;

// Compositing stops once the ray is this opaque.
const EARLY_EXIT_ALPHA: f32 = 0.98;
// Gradients shorter than this carry no usable surface direction.
const MIN_GRADIENT_LENGTH: f32 = 1.0e-4;
// Keeps the slab intersection finite for rays parallel to an axis.
const MIN_RAY_DIRECTION: f32 = 1.0e-8;

// One triangle covering the whole viewport; no vertex buffers are bound.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));

    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let background = uniforms.background_srgb.xyz;
    let size_mm = uniforms.size_mm_value_min.xyz;
    let right = uniforms.right_aspect.xyz;
    let up = uniforms.up_step_mm.xyz;
    let forward = uniforms.forward_opacity_unit_mm.xyz;
    let step_mm = max(uniforms.up_step_mm.w, MIN_RAY_DIRECTION);
    let max_steps = i32(uniforms.lut_range_steps_shading.z);

    // `@builtin(position)` counts pixels down from the top of the framebuffer,
    // so the up axis of the image is `0.5 - y`.
    let viewport_size = max(uniforms.viewport_min_size_px.zw, vec2<f32>(1.0, 1.0));
    let fraction = (position.xy - uniforms.viewport_min_size_px.xy) / viewport_size;
    let half_height = uniforms.eye_mm_ortho_half_height.w;
    let across_mm = (fraction.x - 0.5) * 2.0 * half_height * uniforms.right_aspect.w;
    let up_mm = (0.5 - fraction.y) * 2.0 * half_height;

    // Orthographic rays: every one of them runs along the view direction.
    let origin = uniforms.eye_mm_ortho_half_height.xyz + right * across_mm + up * up_mm;
    let direction = normalize(forward);
    let safe_direction = select(
        direction,
        vec3<f32>(MIN_RAY_DIRECTION, MIN_RAY_DIRECTION, MIN_RAY_DIRECTION),
        abs(direction) < vec3<f32>(MIN_RAY_DIRECTION),
    );

    // Slab intersection with the volume's axis-aligned box.
    let to_low = (vec3<f32>(0.0, 0.0, 0.0) - origin) / safe_direction;
    let to_high = (size_mm - origin) / safe_direction;
    let entries = min(to_low, to_high);
    let exits = max(to_low, to_high);
    let entry_mm = max(max(entries.x, max(entries.y, entries.z)), 0.0);
    let exit_mm = min(exits.x, min(exits.y, exits.z));

    if exit_mm <= entry_mm {
        return output_color(background);
    }

    let value_min = uniforms.size_mm_value_min.w;
    let value_max = uniforms.dims_value_max.w;
    let lut_min = uniforms.lut_range_steps_shading.x;
    let lut_span = max(uniforms.lut_range_steps_shading.y - lut_min, MIN_RAY_DIRECTION);
    let opacity_unit_mm = max(uniforms.forward_opacity_unit_mm.w, MIN_RAY_DIRECTION);
    let shading = uniforms.lut_range_steps_shading.w;

    var color = vec3<f32>(0.0, 0.0, 0.0);
    var alpha = 0.0;
    // Neighbouring rays start at unrelated fractions of a step. A shared start
    // would sample every ray at the same phase, which shows as contour rings
    // on curved surfaces; scattered starts turn that into fine grain.
    var distance_mm = entry_mm + pixel_jitter(position.xy) * step_mm;

    for (var step = 0; step < max_steps; step = step + 1) {
        if distance_mm >= exit_mm || alpha >= EARLY_EXIT_ALPHA {
            break;
        }

        // Voxel i's centre sits at `(i + 0.5) * spacing`, which maps exactly
        // onto texel centre `(i + 0.5) / dims`.
        let texel = (origin + direction * distance_mm) / size_mm;
        let normalized = sample_volume(texel);
        let value = mix(value_min, value_max, normalized);
        let transfer = textureSampleLevel(
            transfer_texture,
            linear_sampler,
            vec2<f32>(clamp((value - lut_min) / lut_span, 0.0, 1.0), 0.5),
            0.0,
        );
        // Opacity is defined over `opacity_unit_mm`, so a longer step is more
        // opaque and the overall opacity stays about the same at any step size.
        let sample_alpha = 1.0 - pow(1.0 - clamp(transfer.a, 0.0, 1.0), step_mm / opacity_unit_mm);

        if sample_alpha > 0.0 {
            var sample_color = transfer.rgb;

            if shading > 0.5 {
                sample_color = shade(sample_color, texel, direction);
            }

            // Front to back compositing.
            color = color + (1.0 - alpha) * sample_alpha * sample_color;
            alpha = alpha + (1.0 - alpha) * sample_alpha;
        }

        distance_mm = distance_mm + step_mm;
    }

    return output_color(color + (1.0 - alpha) * background);
}

// A fraction in [0, 1) that differs from pixel to pixel but not from frame to
// frame, from the lowbias32 integer hash of the pixel.
fn pixel_jitter(pixel: vec2<f32>) -> f32 {
    var hash = u32(pixel.x) + u32(pixel.y) * 65536u;
    hash = hash ^ (hash >> 16u);
    hash = hash * 0x7feb352du;
    hash = hash ^ (hash >> 15u);
    hash = hash * 0x846ca68bu;
    hash = hash ^ (hash >> 16u);

    return f32(hash >> 8u) / 16777216.0;
}

fn sample_volume(texel: vec3<f32>) -> f32 {
    let clamped = clamp(texel, vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(1.0, 1.0, 1.0));

    return textureSampleLevel(volume_texture, linear_sampler, clamped, 0.0).r;
}

// Headlight shading from the value gradient, by central differences one texel
// wide on each axis.
fn shade(color: vec3<f32>, texel: vec3<f32>, direction: vec3<f32>) -> vec3<f32> {
    let texel_step = 1.0 / max(uniforms.dims_value_max.xyz, vec3<f32>(1.0, 1.0, 1.0));
    let gradient = vec3<f32>(
        sample_volume(texel + vec3<f32>(texel_step.x, 0.0, 0.0))
            - sample_volume(texel - vec3<f32>(texel_step.x, 0.0, 0.0)),
        sample_volume(texel + vec3<f32>(0.0, texel_step.y, 0.0))
            - sample_volume(texel - vec3<f32>(0.0, texel_step.y, 0.0)),
        sample_volume(texel + vec3<f32>(0.0, 0.0, texel_step.z))
            - sample_volume(texel - vec3<f32>(0.0, 0.0, texel_step.z)),
    );

    if length(gradient) < MIN_GRADIENT_LENGTH {
        return color;
    }

    let normal = normalize(-gradient);
    let light = -direction;
    let ambient = uniforms.ambient_diffuse_spec_shine.x;
    let diffuse = uniforms.ambient_diffuse_spec_shine.y * max(dot(normal, light), 0.0);
    let specular = uniforms.ambient_diffuse_spec_shine.z
        * pow(
            max(dot(reflect(-light, normal), light), 0.0),
            uniforms.ambient_diffuse_spec_shine.w,
        );

    return color * (ambient + diffuse) + vec3<f32>(specular, specular, specular);
}

// The transfer function is authored in gamma space, so an sRGB target needs the
// linear values it converts back on write.
fn output_color(color: vec3<f32>) -> vec4<f32> {
    if uniforms.background_srgb.w > 0.5 {
        return vec4<f32>(gamma_to_linear(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
    }

    return vec4<f32>(color, 1.0);
}

fn gamma_to_linear(color: vec3<f32>) -> vec3<f32> {
    let lower = color / 12.92;
    let higher = pow((color + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));

    return select(higher, lower, color <= vec3<f32>(0.04045));
}
