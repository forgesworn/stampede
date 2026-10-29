struct SIn {
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec4<f32>,
    @location(1) b: vec4<f32>,
    @location(2) c: vec4<f32>,
};

struct SOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
};

@vertex
fn vs_shadow(in: SIn) -> SOut {
    var q = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let c = q[in.vi];
    let n = normalize(in.b.xyz);
    let f0 = vec3<f32>(sin(in.b.w), 0.0, cos(in.b.w));
    let f = normalize(f0 - n * dot(f0, n));
    let r = cross(n, f);
    let rad = in.a.w;
    let pos = in.a.xyz + n * 0.05 + r * (c.x * rad) + f * (c.y * rad * in.c.y);
    var o: SOut;
    o.clip = g.view_proj * vec4<f32>(pos, 1.0);
    o.uv = c;
    let dist = length(pos - g.cam_pos.xyz);
    o.alpha = in.c.x * (1.0 - fog_factor(dist));
    return o;
}

@fragment
fn fs_shadow(in: SOut) -> @location(0) vec4<f32> {
    let r = length(in.uv);
    let a = smoothstep(1.0, 0.25, r) * in.alpha;
    return vec4<f32>(0.0, 0.0, 0.02, a);
}
