struct VIn {
    @location(0) pos: vec3<f32>,
    @location(1) nrm: vec4<f32>,
    @location(2) col: vec4<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) t0: vec4<f32>,
    @location(8) t1: vec4<f32>,
};

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) wpos: vec3<f32>,
    @location(1) wnrm: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) extra: vec3<f32>,
};

@vertex
fn vs_lit(in: VIn) -> VOut {
    var o: VOut;
    let model = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let wp = model * vec4<f32>(in.pos, 1.0);
    let sx = max(dot(in.m0.xyz, in.m0.xyz), 1e-8);
    let sy = max(dot(in.m1.xyz, in.m1.xyz), 1e-8);
    let sz = max(dot(in.m2.xyz, in.m2.xyz), 1e-8);
    let n = in.nrm.xyz;
    o.wnrm = normalize(in.m0.xyz * (n.x / sx) + in.m1.xyz * (n.y / sy) + in.m2.xyz * (n.z / sz));
    o.wpos = wp.xyz;
    o.clip = g.view_proj * wp;
    let c = pow(in.col.rgb, vec3<f32>(2.2));
    let a = in.col.a;
    let k0 = clamp(a * 2.0, 0.0, 1.0);
    let k1 = clamp(a * 2.0 - 1.0, 0.0, 1.0);
    o.color = mix(mix(c, c * in.t0.rgb, k0), c * in.t1.rgb, k1);
    o.extra = vec3<f32>(max(in.nrm.w, 0.0), in.t0.w, max(-in.nrm.w, 0.0));
    return o;
}

@fragment
fn fs_lit(in: VOut) -> @location(0) vec4<f32> {
    let n = normalize(in.wnrm);
    let to_cam = g.cam_pos.xyz - in.wpos;
    let dist = length(to_cam);
    let v = to_cam / max(dist, 1e-4);
    var col = surface_light(in.color, n, v, in.extra.z);
    col = mix(col, in.color * 1.6 + vec3<f32>(0.04), in.extra.x);
    col = col + vec3<f32>(in.extra.y);
    col = mix(col, g.fog.rgb, fog_factor(dist));
    return vec4<f32>(encode(col), 1.0);
}

@fragment
fn fs_shield(in: VOut) -> @location(0) vec4<f32> {
    let n = normalize(in.wnrm);
    let to_cam = g.cam_pos.xyz - in.wpos;
    let dist = length(to_cam);
    let v = to_cam / max(dist, 1e-4);
    let fres = pow(1.0 - abs(dot(n, v)), 2.2);
    let band = 0.5 + 0.5 * sin(in.wpos.y * 9.0 + g.cam_pos.w * 3.0);
    let a = (0.10 + 0.62 * fres + 0.08 * band) * in.extra.y;
    var col = in.color * (0.55 + 0.9 * fres) + vec3<f32>(0.05, 0.12, 0.2) * fres;
    col = mix(col, g.fog.rgb, fog_factor(dist));
    return vec4<f32>(encode(col), clamp(a, 0.0, 1.0));
}
