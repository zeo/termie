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
    @location(6) shape: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) box_size: vec2<f32>,
    @location(4) @interpolate(flat) shape: vec3<f32>,
    @location(5) @interpolate(flat) color2: vec4<f32>,
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
    out.color2 = vec4<f32>(0.0);
    // kind 5 carries its bottom color in the uv slots and works in local pixels
    if (inst.kind == 5u) {
        out.uv = corner * inst.size;
        out.color2 = vec4<f32>(inst.uv_min, inst.uv_max);
    }
    out.color = inst.color;
    out.kind = inst.kind;
    out.box_size = inst.size;
    out.shape = inst.shape;
    return out;
}

fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// premultiplied-alpha output so the surface composes correctly when the window
// is translucent (flat per-pixel opacity), and reduces to opaque when alpha == 1
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (in.kind == 0u) {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    // kind 4: rounded rect in local pixels (uv); shape = (radius, blur, ring
    // width). a blurred one is a soft shadow whose rect sits `blur` inside the quad
    if (in.kind == 4u) {
        let blur = in.shape.y;
        let half = in.box_size * 0.5 - vec2<f32>(blur);
        let r = min(in.shape.x, min(half.x, half.y));
        let d = sd_round_box(in.uv - in.box_size * 0.5, half, r);
        var cov: f32;
        if (blur > 0.0) {
            cov = 1.0 - smoothstep(-blur, blur, d);
        } else if (in.shape.z > 0.0) {
            cov = clamp(0.5 - (abs(d + in.shape.z * 0.5) - in.shape.z * 0.5), 0.0, 1.0);
        } else {
            cov = clamp(0.5 - d, 0.0, 1.0);
        }
        let ra = in.color.a * cov;
        return vec4<f32>(in.color.rgb * ra, ra);
    }
    // kind 5: vertical gradient clipped to a rounded rect; shape = (radius,
    // band top, band bottom) in local pixels. only rows inside the band are
    // covered, so a split gel is two instances sharing one rounded outline
    if (in.kind == 5u) {
        let half = in.box_size * 0.5;
        let r = min(in.shape.x, min(half.x, half.y));
        let d = sd_round_box(in.uv - half, half, r);
        var y0 = in.shape.y;
        var y1 = in.shape.z;
        if (y1 <= y0) {
            y0 = 0.0;
            y1 = in.box_size.y;
        }
        let rows = clamp(min(in.uv.y + 0.5, y1) - max(in.uv.y - 0.5, y0), 0.0, 1.0);
        let cov = clamp(0.5 - d, 0.0, 1.0) * rows;
        let c = mix(in.color, in.color2, clamp((in.uv.y - y0) / (y1 - y0), 0.0, 1.0));
        let ra = c.a * cov;
        return vec4<f32>(c.rgb * ra, ra);
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
