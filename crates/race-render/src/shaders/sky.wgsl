struct SkyOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_sky(@builtin(vertex_index) vi: u32) -> SkyOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: SkyOut;
    o.clip = vec4<f32>(p[vi], 1.0, 1.0);
    o.ndc = p[vi];
    return o;
}

// Hill silhouette height (elevation, ~radians) for a style and layer.
fn hill_profile(az: f32, layer: f32) -> f32 {
    let style = g.fx0.y;
    if (style > 3.5) {
        // Skyline: blocky towers, taller and denser in the near layer.
        let cells = select(26.0, 38.0, layer > 0.5);
        let n = floor(az * cells + layer * 7.0);
        let h = hash21(vec2<f32>(n, layer + 3.0));
        let base = select(0.05, 0.02, layer > 0.5);
        return base + select(0.07, 0.115, layer > 0.5) * (h * h) + select(0.0, 0.03, layer > 0.5) * step(0.86, h);
    }
    if (style > 2.5) {
        // Peaks: sharp volcanic cones with a flat-ish ridge between.
        let a = abs(sin(az * select(2.5, 3.5, layer > 0.5) + layer * 1.7));
        let cone = pow(1.0 - a, 1.7);
        let big = 0.55 + 0.45 * sin(az * 1.3 + layer * 2.0 + 0.8) + 0.25 * sin(az * 4.7);
        return 0.02 + select(0.2, 0.12, layer > 0.5) * cone * clamp(big, 0.25, 1.15) + 0.012 * sin(az * 13.0 + layer);
    }
    if (style > 1.5) {
        // Mesas: flat tops, steep sides.
        let k = select(3.0, 5.0, layer > 0.5);
        let m1 = smoothstep(-0.12, 0.12, sin(az * k + 0.5 + layer * 2.0));
        let m2 = smoothstep(-0.12, 0.12, sin(az * (k * 2.3) + 2.0 + layer));
        return 0.03 + select(0.085, 0.05, layer > 0.5) * m1 + 0.035 * m2 * m1 + 0.004 * sin(az * 31.0);
    }
    if (style > 0.5) {
        // Dunes: soft crests.
        if (layer < 0.5) {
            return 0.045 + 0.028 * sin(az * 2.0 + 0.5) + 0.016 * sin(az * 4.3 + 2.0);
        }
        return 0.02 + 0.02 * sin(az * 3.0 + 1.0) + 0.012 * sin(az * 6.1 + 4.0);
    }
    if (layer < 0.5) {
        return 0.075 + 0.040 * sin(az * 2.0 + 0.5) + 0.022 * sin(az * 5.0 + 2.0) + 0.010 * sin(az * 11.0 + 1.0);
    }
    return 0.030 + 0.028 * sin(az * 3.0 + 1.0) + 0.016 * sin(az * 7.0 + 4.0) + 0.008 * sin(az * 17.0);
}

@fragment
fn fs_sky(in: SkyOut) -> @location(0) vec4<f32> {
    let far = g.inv_view_proj * vec4<f32>(in.ndc, 1.0, 1.0);
    let dir = normalize(far.xyz / far.w - g.cam_pos.xyz);
    let t = clamp(dir.y, 0.0, 1.0);
    let time = g.cam_pos.w;
    var col = mix(g.sky_hor.rgb, g.sky_top.rgb, pow(t, 0.5));
    col = mix(g.fog.rgb, col, smoothstep(0.0, 0.16, t));
    let night = g.sun_dir.w;
    let sun = normalize(g.sun_dir.xyz);
    let sd = dot(dir, sun);
    let az = atan2(dir.x, dir.z);
    let e = dir.y;
    // Layered haze bands above the horizon.
    if (g.fx0.z > 0.0) {
        let n = vnoise(vec2<f32>(az * 5.0, e * 26.0));
        let b1 = exp(-pow((e - 0.11) / 0.05, 2.0));
        let b2 = exp(-pow((e - 0.23) / 0.028, 2.0)) * 0.7;
        col = mix(col, g.fx2.rgb, clamp(g.fx0.z * (b1 + b2) * (0.6 + 0.6 * n), 0.0, 1.0));
    }
    // Sun or moon disc (size from the theme).
    let k = g.fx0.w;
    let disc_edge = 1.0 - (1.0 - mix(0.9992, 0.9996, night)) * k * k;
    let disc = smoothstep(disc_edge - 0.0006 * k, disc_edge, sd);
    let glow = pow(max(sd, 0.0), 48.0) * 0.55 + pow(max(sd, 0.0), 6.0) * 0.12;
    col = col + g.sun_color.rgb * (disc * 2.2 + glow * (1.0 - 0.6 * night));
    // Stars.
    if (night > 0.5 && dir.y > 0.0) {
        let p = dir * 70.0;
        let cell = floor(p);
        let h = hash21(cell.xy + cell.z * 17.0);
        let h2 = hash21(cell.yz * 3.1 + cell.x);
        let local = fract(p) - vec3<f32>(0.5);
        let off = vec3<f32>(h - 0.5, h2 - 0.5, fract(h * 7.3) - 0.5) * 0.5;
        let d = length(local - off);
        let tw = 0.7 + 0.3 * sin(g.cam_pos.w * (1.5 + h * 3.0) + h * 40.0);
        let star = step(0.965, h) * smoothstep(0.17, 0.02, d) * tw;
        col = col + vec3<f32>(0.9, 0.93, 1.0) * star * smoothstep(0.0, 0.2, dir.y);
    }
    // Clouds: two octaves on a plane overhead, lit from the sun side.
    if (g.fx0.x > 0.0 && e > 0.015) {
        let uv = dir.xz / (e + 0.14) * 1.5 + vec2<f32>(time * 0.012, time * 0.004);
        let nz = vnoise(uv) * 0.62 + vnoise(uv * 2.7 + 5.0) * 0.38;
        let cov = clamp(g.fx0.x, 0.0, 1.0);
        let cl = smoothstep(1.0 - cov * 0.8, 1.0 - cov * 0.8 + 0.22, nz) * smoothstep(0.015, 0.2, e);
        let lit = 0.72 + 0.28 * clamp(sd * 0.5 + 0.5, 0.0, 1.0);
        col = mix(col, g.fx1.rgb * lit, cl * 0.88);
    }
    // Distant hills.
    let px = 0.0025;
    let far_h = hill_profile(az, 0.0);
    let near_h = hill_profile(az, 1.0);
    let hill_far = mix(g.hill.rgb, g.fog.rgb, 0.62);
    let hill_near = mix(g.hill.rgb, g.fog.rgb, 0.35) * 0.9;
    let above = smoothstep(-0.004, 0.004, e);
    // Glow in the haze over the hills (lava glow, city lights).
    if (g.fx1.w > 0.0) {
        col = col + g.fx3.rgb * g.fx1.w * (1.0 - smoothstep(0.0, 0.2, e)) * (0.75 + 0.25 * sin(az * 9.0 + time * 0.5)) * above;
    }
    col = mix(col, hill_far, (1.0 - smoothstep(far_h - px, far_h + px, e)) * g.hill.w * above);
    var near_c = hill_near;
    if (g.fx0.y > 3.5) {
        // Lit windows on the near skyline.
        let wc = vec2<f32>(floor(az * 38.0 * 6.0), floor(e * 330.0));
        let wl = step(0.7, hash21(wc)) * step(0.35, fract(az * 38.0 * 6.0)) * step(fract(e * 330.0), 0.6);
        let warm = mix(vec3<f32>(1.0, 0.7, 0.3), vec3<f32>(0.5, 0.8, 1.0), step(0.5, hash21(wc + 9.0)));
        near_c = hill_near * 0.5 + warm * wl * 0.9;
    }
    col = mix(col, near_c, (1.0 - smoothstep(near_h - px, near_h + px, e)) * g.hill.w * above);
    return vec4<f32>(encode(col), 1.0);
}
