// Decor: static-looking meshes whose vertices animate from a per-vertex `anim` word
// (crowd bob, flag flutter), plus water, lava and the start lights. Opaque, one draw.
struct DIn {
    @location(0) pos: vec3<f32>,
    @location(1) nrm: vec4<f32>,
    @location(2) col: vec4<f32>,
    @location(3) anim: vec4<f32>,
};

struct DOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) wpos: vec3<f32>,
    @location(1) wnrm: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) extra: vec2<f32>,
    @location(4) @interpolate(flat) kind: f32,
    @location(5) @interpolate(flat) aux: vec3<f32>,
};

@vertex
fn vs_decor(in: DIn) -> DOut {
    var o: DOut;
    var p = in.pos;
    let t = g.cam_pos.w;
    let kind = floor(in.anim.x * 255.0 + 0.5);
    var emissive = max(in.nrm.w, 0.0);
    if (kind == 1.0) {
        // Cheering crowd: hop with the whole body, stretch the upper body more.
        let ph = in.anim.y * 6.2831;
        let hop = abs(sin(t * 5.2 + ph)) * (0.55 + 0.45 * sin(t * 1.7 + ph * 3.0));
        p.y = p.y + hop * (0.12 + 0.5 * in.anim.z * in.anim.z);
        p.x = p.x + sin(t * 3.1 + ph) * 0.05 * in.anim.z;
    } else if (kind == 2.0) {
        // Flag: ripple along the wind (the facing normal of the flag), growing away from the pole.
        let ph = in.anim.y * 6.2831;
        let yaw = in.anim.w * 6.2831;
        let face = vec3<f32>(sin(yaw), 0.0, cos(yaw));
        let w = sin(t * 5.5 + ph + in.anim.z * 5.0) + 0.5 * sin(t * 9.3 + ph * 2.0 + in.anim.z * 11.0);
        p = p + face * (w * 0.2 * in.anim.z);
        p.y = p.y - 0.06 * in.anim.z * in.anim.z * (1.0 + sin(t * 4.0 + ph));
    } else if (kind == 6.0) {
        let ph = in.anim.y * 6.2831;
        emissive = emissive * (0.82 + 0.18 * sin(t * 2.6 + ph));
    }
    let wp = vec4<f32>(p, 1.0);
    o.wnrm = normalize(in.nrm.xyz);
    o.wpos = p;
    o.clip = g.view_proj * wp;
    o.color = pow(in.col.rgb, vec3<f32>(2.2));
    o.extra = vec2<f32>(emissive, in.anim.y);
    o.kind = kind;
    o.aux = pow(in.anim.yzw, vec3<f32>(2.2));
    return o;
}

fn water_col(in: DOut) -> vec3<f32> {
    let p = in.wpos.xz;
    let t = g.cam_pos.w;
    let w1 = sin(p.x * 0.9 + t * 1.1 + sin(p.y * 0.6 + t * 0.55) * 1.7);
    let w2 = sin(p.y * 1.2 - t * 1.3 + sin(p.x * 0.9 - t * 0.7) * 1.3);
    let w3 = sin((p.x + p.y) * 1.9 + t * 2.1);
    let ripple = 0.5 + 0.25 * (w1 + w2);
    let tone = mix(in.color, in.aux, clamp(ripple * 0.55 + 0.35, 0.0, 1.0));
    let n = normalize(vec3<f32>(w1 * 0.09 + w3 * 0.03, 1.0, w2 * 0.09));
    let to_cam = g.cam_pos.xyz - in.wpos;
    let dist = length(to_cam);
    let v = to_cam / max(dist, 1e-4);
    let fres = pow(1.0 - clamp(dot(n, v), 0.0, 1.0), 3.0);
    let sky = mix(g.sky_hor.rgb, g.sky_top.rgb, 0.35);
    var col = mix(tone, sky * 0.55 + tone * 0.45, clamp(0.04 + 0.6 * fres, 0.0, 0.5));
    let sun = normalize(g.sun_dir.xyz);
    let r = reflect(-v, n);
    let glint = pow(max(dot(r, sun), 0.0), 140.0);
    col = col + g.sun_color.rgb * glint * 2.6 * (1.0 - g.sun_dir.w * 0.5);
    col = col + g.sun_color.rgb * 0.05 * smoothstep(0.88, 1.0, w1 * w2 * 0.5 + 0.5);
    return col;
}

fn lava_col(in: DOut) -> vec3<f32> {
    let p = in.wpos.xz * 0.11;
    let t = g.cam_pos.w;
    let a = sin(p.x * 3.1 + t * 0.32 + sin(p.y * 2.3 - t * 0.21) * 2.0);
    let b = sin(p.y * 2.7 + t * 0.27 + sin(p.x * 1.9 + t * 0.3) * 2.2);
    let c = sin((p.x - p.y) * 5.3 - t * 0.4);
    let vein = smoothstep(0.38, 0.0, abs(a * b)) * 0.9 + smoothstep(0.2, 0.0, abs(c * a)) * 0.35;
    let heat = clamp(vein + 0.3 + 0.12 * sin(t * 1.6 + p.x * 9.0), 0.0, 1.0);
    return mix(in.color, in.aux, heat) * (0.9 + heat * 1.6);
}

@fragment
fn fs_decor(in: DOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    let dist0 = length(g.cam_pos.xyz - in.wpos);
    var col: vec3<f32>;
    if (in.kind == 3.0) {
        col = water_col(in);
    } else if (in.kind == 4.0) {
        col = lava_col(in);
    } else if (in.kind == 5.0) {
        let k = in.extra.y * 5.0 - 0.5;
        let stage = g.fx2.w;
        var on = 0.0;
        var tint = vec3<f32>(1.0, 0.05, 0.03);
        if (stage > 5.5) {
            on = 1.0;
            tint = vec3<f32>(0.1, 1.0, 0.25);
        } else if (stage >= k + 0.5) {
            on = 1.0;
        }
        col = mix(vec3<f32>(0.03, 0.02, 0.02), tint * 2.6, on);
    } else {
        var n = normalize(in.wnrm);
        if (!front) {
            n = -n;
        }
        let v = (g.cam_pos.xyz - in.wpos) / max(dist0, 1e-4);
        col = surface_light(in.color, n, v, 0.0);
        col = mix(col, in.color * 1.6 + vec3<f32>(0.04), in.extra.x);
    }
    col = mix(col, g.fog.rgb, fog_factor(dist0));
    return vec4<f32>(encode(col), 1.0);
}
