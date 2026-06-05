struct Uniforms {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_samp: sampler;
@group(1) @binding(2) var icon_tex: texture_2d<f32>;
@group(1) @binding(3) var icon_samp: sampler;
@group(1) @binding(4) var color_tex: texture_2d<f32>;
@group(1) @binding(5) var color_samp: sampler;

struct InstanceIn {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) kind: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: InstanceIn) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let corner = corners[vi];
    let px = inst.pos + corner * inst.size;
    let clip = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);

    var out: VsOut;
    out.clip = vec4<f32>(clip, 0.0, 1.0);
    out.uv = mix(inst.uv_min, inst.uv_max, corner);
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

// premultiplied-alpha output so the surface composes correctly when the window
// is translucent (flat per-pixel opacity), and reduces to opaque when alpha == 1
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (in.kind == 0u) {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    // kind 2: full-color app-icon texture (straight alpha -> premultiplied)
    if (in.kind == 2u) {
        let t = textureSample(icon_tex, icon_samp, in.uv);
        let ia = t.a * in.color.a;
        return vec4<f32>(t.rgb * ia, ia);
    }
    // kind 3: color (emoji) glyph from the RGBA atlas; carries its own color,
    // so fg is not applied (only the alpha for the startup fade / opacity)
    if (in.kind == 3u) {
        let t = textureSample(color_tex, color_samp, in.uv);
        let ca = t.a * in.color.a;
        return vec4<f32>(t.rgb * ca, ca);
    }
    let cov = textureSample(atlas_tex, atlas_samp, in.uv).r;
    let a = in.color.a * cov;
    return vec4<f32>(in.color.rgb * a, a);
}
