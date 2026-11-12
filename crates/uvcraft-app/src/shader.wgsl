// UvCraft viewport shader. Static geometry, UVs and colors have separate buffers:
// params.x = 0 → 3D view (position), 1 → UV view (uv as position).

struct Uniforms {
    view_proj: mat4x4<f32>,
    // x: view mode, y: tint amount, z: checker cells per UV unit, w: line depth offset
    params: vec4<f32>,
    // xyz: light direction (towards the light), w: distortion heatmap
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var image_tex: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

fn place(pos: vec3<f32>, uv: vec2<f32>) -> vec4<f32> {
    if (u.params.x > 0.5) {
        return u.view_proj * vec4<f32>(uv, 0.0, 1.0);
    }
    return u.view_proj * vec4<f32>(pos, 1.0);
}

struct TriIn {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) normal: vec3<f32>,
    @location(3) color: vec4<f32>,
};

struct TriOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs_tri(v: TriIn) -> TriOut {
    var o: TriOut;
    o.clip = place(v.pos, v.uv);
    o.uv = v.uv;
    o.normal = v.normal;
    o.color = v.color;
    return o;
}

fn checker(uv: vec2<f32>) -> vec3<f32> {
    let c = floor(uv * u.params.z);
    let s = c.x + c.y;
    let parity = s - 2.0 * floor(s * 0.5);
    return mix(vec3<f32>(0.82), vec3<f32>(0.42), parity);
}

@fragment
fn fs_tri(i: TriOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    // color.a: 1 = selected, otherwise 0.5.
    let selected = i.color.a > 0.75 && i.color.a < 1.5;
    let heatmap = u.light.w > 0.5;
    if (heatmap && i.color.a < 0.0) {
        discard;
    }
    var base = mix(checker(i.uv), i.color.rgb, clamp(u.params.y, 0.0, 1.0));
    if (u.params.y < -0.5) {
        base = textureSample(image_tex, image_sampler, i.uv).rgb;
    }
    if (u.params.x > 0.5) {
        let cells = floor(i.uv * u.params.z);
        let parity = cells.x + cells.y - 2.0 * floor((cells.x + cells.y) * 0.5);
        var c = mix(vec3<f32>(0.12), vec3<f32>(0.17), parity);
        if (heatmap) {
            c = mix(c, i.color.rgb, clamp(u.params.y, 0.0, 1.0));
        } else if (u.params.y > 0.5) {
            c = mix(c, i.color.rgb, 0.65);
        }
        if (u.params.y < -0.5) {
            c = textureSample(image_tex, image_sampler, i.uv).rgb;
        }
        var a = 0.92;
        if (selected && !heatmap) {
            c = mix(c, vec3<f32>(1.0, 0.55, 0.12), 0.55);
            a = 1.0;
        }
        return vec4<f32>(c, a);
    }
    if (heatmap) {
        return vec4<f32>(mix(checker(i.uv), i.color.rgb,
            clamp(u.params.y, 0.0, 1.0)), 1.0);
    }
    let n = normalize(i.normal);
    let d = abs(dot(n, normalize(u.light.xyz)));
    var col = base * (0.3 + 0.7 * d);
    if (!front) {
        col = col * vec3<f32>(0.55, 0.6, 0.95);
    }
    if (selected) {
        col = mix(col, vec3<f32>(1.0, 0.55, 0.12), 0.45);
    }
    return vec4<f32>(col, 1.0);
}

struct LineIn {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_line(v: LineIn) -> LineOut {
    var o: LineOut;
    o.clip = place(v.pos, v.uv);
    // Pull lines slightly towards the camera so they win against their faces.
    o.clip.z = o.clip.z - u.params.w * o.clip.w;
    o.color = v.color;
    return o;
}

@fragment
fn fs_line(i: LineOut) -> @location(0) vec4<f32> {
    return i.color;
}
