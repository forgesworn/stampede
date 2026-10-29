struct CompU {
    // x = decode gamma for sRGB targets (1) or pass through (0).
    params: vec4<f32>,
    // Mirror rect: centre xy, half size zw (target pixels).
    rect: vec4<f32>,
    // Target size xy, corner radius, bevel width.
    misc: vec4<f32>,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> u: CompU;

struct FOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> FOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: FOut;
    o.clip = vec4<f32>(p[vi], 0.0, 1.0);
    o.uv = vec2<f32>(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return o;
}

fn out_color(c: vec3<f32>) -> vec3<f32> {
    if (u.params.x > 0.5) {
        return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
    }
    return c;
}

@fragment
fn fs_blit(in: FOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(tex, samp, in.uv, 0.0).rgb;
    return vec4<f32>(out_color(c), 1.0);
}

struct RectOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
};

@vertex
fn vs_rect(@builtin(vertex_index) vi: u32) -> RectOut {
    var q = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let c = q[vi];
    let local = c * (u.rect.zw + vec2<f32>(6.0));
    let px = u.rect.xy + local;
    var o: RectOut;
    o.clip = vec4<f32>(px.x / u.misc.x * 2.0 - 1.0, 1.0 - px.y / u.misc.y * 2.0, 0.0, 1.0);
    o.local = local;
    return o;
}

fn sd_round_box(p: vec2<f32>, half_size: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half_size + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_mirror(in: RectOut) -> @location(0) vec4<f32> {
    let hs = u.rect.zw;
    let r = u.misc.z;
    let bev = u.misc.w;
    let d = sd_round_box(in.local, hs, r);
    let e = 1.0;
    let dx = sd_round_box(in.local + vec2<f32>(e, 0.0), hs, r) - d;
    let dy = sd_round_box(in.local + vec2<f32>(0.0, e), hs, r) - d;
    let grad = normalize(vec2<f32>(dx, dy) + vec2<f32>(1e-5));
    let cover = clamp(0.5 - d, 0.0, 1.0);
    if (cover <= 0.0) {
        discard;
    }
    var col: vec3<f32>;
    if (d > -bev) {
        // Bevelled frame: light from the top-left.
        let t = (-d) / bev;
        let lit = clamp(dot(grad, normalize(vec2<f32>(-0.7, -0.7))) * 0.5 + 0.5, 0.0, 1.0);
        var metal = mix(vec3<f32>(0.10, 0.09, 0.11), vec3<f32>(0.78, 0.74, 0.70), lit);
        metal = metal * (0.65 + 0.35 * smoothstep(0.0, 0.5, t));
        // Thin bright rim at the outside and dark groove inside.
        metal = metal + vec3<f32>(0.35) * (1.0 - smoothstep(0.0, 0.12, t)) * lit;
        metal = mix(metal, vec3<f32>(0.03), smoothstep(0.82, 1.0, t));
        col = metal;
    } else {
        let inner = in.local / (hs - vec2<f32>(bev));
        let uv = vec2<f32>(0.5 - inner.x * 0.5, 0.5 + inner.y * 0.5);
        var c = textureSampleLevel(tex, samp, uv, 0.0).rgb;
        let vig = 1.0 - 0.28 * dot(inner, inner) * 0.5;
        c = c * vig;
        // Glass sheen: a faint diagonal highlight.
        let sheen = smoothstep(0.55, 0.0, abs(inner.x * 0.8 + inner.y * 0.9 + 0.35)) * 0.07;
        c = c + vec3<f32>(sheen);
        col = c;
    }
    return vec4<f32>(out_color(col), cover);
}
