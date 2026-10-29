struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    cam_right: vec4<f32>,
    cam_up: vec4<f32>,
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    amb_sky: vec4<f32>,
    amb_ground: vec4<f32>,
    fog: vec4<f32>,
    sky_top: vec4<f32>,
    sky_hor: vec4<f32>,
    hill: vec4<f32>,
    screen: vec4<f32>,
    // x cloud cover, y hill style, z haze band strength, w sun scale
    fx0: vec4<f32>,
    // cloud colour (linear), w hill glow strength
    fx1: vec4<f32>,
    // haze band colour (linear), w start lights
    fx2: vec4<f32>,
    // hill glow colour (linear)
    fx3: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

fn encode(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
}

fn fog_factor(dist: f32) -> f32 {
    return 1.0 - exp(-g.fog.w * dist);
}

fn surface_light(base: vec3<f32>, n: vec3<f32>, v: vec3<f32>, sheen: f32) -> vec3<f32> {
    let sun = normalize(g.sun_dir.xyz);
    let ndl = dot(n, sun);
    let diffuse = clamp((ndl + 0.22) / 1.22, 0.0, 1.0);
    let hemi = mix(g.amb_ground.rgb, g.amb_sky.rgb, n.y * 0.5 + 0.5);
    let rim = pow(1.0 - clamp(dot(n, v), 0.0, 1.0), 3.0);
    let bounce = 0.12 * max(dot(n, -sun), 0.0);
    var col = base * (g.sun_color.rgb * (diffuse * 0.84 + bounce) + hemi * 0.64 + vec3<f32>(0.045));
    col = col + base * g.amb_sky.rgb * rim * 0.16;
    // Broad, cool fill keeps faces and feathers legible in shade and at night.
    col = col + base * vec3<f32>(0.075, 0.085, 0.11) * max(dot(n, v), 0.0);
    if (sheen > 0.01) {
        let half_light = normalize(sun + v + vec3<f32>(0.0, 0.0001, 0.0));
        let highlight = pow(max(dot(n, half_light), 0.0), mix(16.0, 54.0, sheen));
        col = col + g.sun_color.rgb * highlight * sheen * 0.32 * max(ndl, 0.0);
    }
    return col;
}

fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(123.34, 456.21));
    q = q + dot(q, q + 45.32);
    return fract(q.x * q.y);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}
