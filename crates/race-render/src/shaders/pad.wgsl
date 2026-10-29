struct PadIn {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) size: vec2<f32>,
};

struct PadOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) wpos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) size: vec2<f32>,
};

@vertex
fn vs_pad(in: PadIn) -> PadOut {
    var o: PadOut;
    o.clip = g.view_proj * vec4<f32>(in.pos, 1.0);
    o.wpos = in.pos;
    o.uv = in.uv;
    o.size = in.size;
    return o;
}

@fragment
fn fs_pad(in: PadOut) -> @location(0) vec4<f32> {
    let ramp = in.size.x < 0.0;
    let hx = abs(in.size.x);
    let len = in.size.y;
    let across = in.uv.x / hx;
    let along = in.uv.y;
    let t = g.cam_pos.w;
    // Chevrons pointing along the pad, scrolling forwards.
    let s = along - abs(in.uv.x) * 0.85;
    let phase = fract((s - t * 3.4) / 1.25);
    let band = smoothstep(0.0, 0.08, phase) * (1.0 - smoothstep(0.38, 0.5, phase));
    let edge = smoothstep(0.80, 0.97, abs(across)) + smoothstep(len - 0.35, len, along) + (1.0 - smoothstep(0.0, 0.35, along));
    let pulse = 0.85 + 0.15 * sin(t * 7.0);
    var col = vec3<f32>(0.16, 0.05, 0.0) + vec3<f32>(1.0, 0.82, 0.22) * band * 1.4 * pulse;
    col = col + vec3<f32>(1.0, 0.45, 0.05) * clamp(edge, 0.0, 1.0) * 0.9 * pulse;
    if (ramp) {
        // Ramp: hazard chevrons (yellow on dark), steady with a slow breathing shimmer.
        let rp = fract((along - abs(in.uv.x) * 0.9) / 1.7);
        let rb = smoothstep(0.0, 0.06, rp) * (1.0 - smoothstep(0.42, 0.5, rp));
        let rim = smoothstep(0.82, 0.96, abs(across));
        col = vec3<f32>(0.04, 0.035, 0.03) + vec3<f32>(1.0, 0.78, 0.1) * rb * (0.9 + 0.1 * sin(t * 3.0 + along)) + vec3<f32>(1.0, 0.95, 0.85) * rim * 0.8;
    }
    let dist = length(in.wpos - g.cam_pos.xyz);
    col = mix(col, g.fog.rgb, fog_factor(dist));
    return vec4<f32>(encode(col), 1.0);
}
