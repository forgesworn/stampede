struct BIn {
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec4<f32>,
    @location(1) b: vec4<f32>,
    @location(2) c: vec4<f32>,
};

struct BOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) v: f32,
    @location(1) along: f32,
    @location(2) color: vec3<f32>,
    @location(3) inten: f32,
    @location(4) fogn: f32,
    @location(5) len: f32,
};

@vertex
fn vs_beam(in: BIn) -> BOut {
    var ts = array<f32, 6>(0.0, 1.0, 1.0, 0.0, 1.0, 0.0);
    var ss = array<f32, 6>(-1.0, -1.0, 1.0, -1.0, 1.0, 1.0);
    let t = ts[in.vi];
    let s = ss[in.vi];
    let p0 = in.a.xyz;
    let p1 = in.b.xyz;
    let d = p1 - p0;
    let len = max(length(d), 1e-4);
    let du = d / len;
    let mid = (p0 + p1) * 0.5;
    var side = cross(du, g.cam_pos.xyz - mid);
    if (length(side) < 1e-4) {
        side = g.cam_right.xyz;
    }
    side = normalize(side);
    let hw = in.a.w * 0.5;
    let pos = mix(p0, p1, t) + side * (s * hw) + du * ((t * 2.0 - 1.0) * hw * 0.5);
    var o: BOut;
    o.clip = g.view_proj * vec4<f32>(pos, 1.0);
    o.v = s;
    o.along = t;
    o.color = in.c.rgb;
    o.inten = in.b.w;
    o.fogn = fog_factor(length(mid - g.cam_pos.xyz));
    o.len = len;
    return o;
}

@fragment
fn fs_beam(in: BOut) -> @location(0) vec4<f32> {
    let core = exp(-in.v * in.v * 28.0);
    let glow = exp(-in.v * in.v * 5.0);
    let flick = 0.88 + 0.12 * sin(in.along * in.len * 2.5 - g.cam_pos.w * 45.0);
    let rgb = mix(in.color, vec3<f32>(1.0, 0.88, 0.72), core * 0.5);
    let a = (core * 0.85 + glow * 0.32) * clamp(in.inten, 0.0, 1.5) * flick * (1.0 - in.fogn * 0.8);
    return vec4<f32>(encode(rgb), clamp(a, 0.0, 1.0));
}
