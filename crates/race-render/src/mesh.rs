//! Procedural mesh building: vertex format, a builder with transformable
//! primitives, and the shared helpers the ostrich, props and world use.
//!
//! Vertex colour channels: `col.rgb` is sRGB, `col.a` selects the instance
//! tint (0 = fixed colour, 0.5 = tint 0, 1.0 = tint 1, in between blends);
//! `nrm.w` is emissive (0..1, unaffected by lighting).

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Mat4, Vec3};
use std::f32::consts::{PI, TAU};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub nrm: [i8; 4],
    pub col: [u8; 4],
}

pub const VERTEX_ATTRS: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Float32x3, 1 => Snorm8x4, 2 => Unorm8x4];

pub fn vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &VERTEX_ATTRS,
    }
}

/// How a primitive is coloured.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub rgb: [f32; 3],
    pub emissive: f32,
    /// Soft material highlight (0 matte, 1 polished); shares the signed emissive byte.
    pub sheen: f32,
    /// 0 fixed colour, 1 instance tint 0, 2 instance tint 1.
    pub tint: u8,
    /// Darkening towards the bottom of the primitive (0 none, 0.4 strong).
    pub ao: f32,
    pub flat: bool,
}

impl Style {
    pub fn col(r: f32, g: f32, b: f32) -> Style {
        Style { rgb: [r, g, b], emissive: 0.0, sheen: 0.0, tint: 0, ao: 0.0, flat: false }
    }
    pub fn rgb(c: [f32; 3]) -> Style {
        Style::col(c[0], c[1], c[2])
    }
    /// White shaded by `shade`, multiplied by instance tint 0.
    pub fn tint0(shade: f32) -> Style {
        Style { rgb: [shade; 3], emissive: 0.0, sheen: 0.0, tint: 1, ao: 0.0, flat: false }
    }
    /// White shaded by `shade`, multiplied by instance tint 1.
    pub fn tint1(shade: f32) -> Style {
        Style { rgb: [shade; 3], emissive: 0.0, sheen: 0.0, tint: 2, ao: 0.0, flat: false }
    }
    pub fn emit(mut self, e: f32) -> Style {
        self.emissive = e;
        self
    }
    pub fn ao(mut self, a: f32) -> Style {
        self.ao = a;
        self
    }
    pub fn sheen(mut self, s: f32) -> Style {
        self.sheen = s.clamp(0.0, 1.0);
        self
    }
    pub fn flat(mut self) -> Style {
        self.flat = true;
        self
    }
    pub fn scaled(mut self, k: f32) -> Style {
        self.rgb = [self.rgb[0] * k, self.rgb[1] * k, self.rgb[2] * k];
        self
    }
}

fn q8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
fn s8(v: f32) -> i8 {
    (v.clamp(-1.0, 1.0) * 127.0).round() as i8
}

/// Pack a vertex.
pub fn vertex(pos: Vec3, n: Vec3, st: &Style, t: f32) -> Vertex {
    let shade = 1.0 - st.ao * (1.0 - t.clamp(0.0, 1.0));
    let a = match st.tint {
        0 => 0,
        1 => 128,
        _ => 255,
    };
    Vertex {
        pos: pos.to_array(),
        nrm: [s8(n.x), s8(n.y), s8(n.z), s8(if st.emissive > 0.0 { st.emissive } else { -st.sheen })],
        col: [q8(st.rgb[0] * shade), q8(st.rgb[1] * shade), q8(st.rgb[2] * shade), a],
    }
}

#[derive(Clone, Debug, Default)]
pub struct MeshData {
    pub verts: Vec<Vertex>,
    pub idx: Vec<u32>,
}

impl MeshData {
    pub fn new() -> MeshData {
        MeshData::default()
    }
    pub fn tri_count(&self) -> usize {
        self.idx.len() / 3
    }

    /// Append another mesh transformed by `m` (positions and normals).
    pub fn append(&mut self, other: &MeshData, m: &Mat4) {
        self.append_shaded(other, m, 1.0);
    }

    /// Append with the (non-emissive) colours scaled by `shade`.
    pub fn append_shaded(&mut self, other: &MeshData, m: &Mat4, shade: f32) {
        let base = self.verts.len() as u32;
        let nm = normal_matrix(m);
        for v in &other.verts {
            let p = m.transform_point3(Vec3::from_array(v.pos));
            let n = nm * Vec3::new(v.nrm[0] as f32, v.nrm[1] as f32, v.nrm[2] as f32);
            let n = n.normalize_or_zero() * 127.0;
            let sh = |c: u8| ((c as f32 * shade).clamp(0.0, 255.0)) as u8;
            self.verts.push(Vertex { pos: p.to_array(), nrm: [n.x as i8, n.y as i8, n.z as i8, v.nrm[3]], col: [sh(v.col[0]), sh(v.col[1]), sh(v.col[2]), v.col[3]] });
        }
        let flip = m.determinant() < 0.0;
        for t in other.idx.chunks_exact(3) {
            if flip {
                self.idx.extend_from_slice(&[base + t[0], base + t[2], base + t[1]]);
            } else {
                self.idx.extend_from_slice(&[base + t[0], base + t[1], base + t[2]]);
            }
        }
    }

    pub fn bounds(&self) -> (Vec3, Vec3) {
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        for v in &self.verts {
            let p = Vec3::from_array(v.pos);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        (lo, hi)
    }

    /// Raw triangles with per-vertex data already in final space. Normals are
    /// used to orient the winding (counter-clockwise seen from outside).
    pub fn push_tris(&mut self, ps: &[Vec3], ns: &[Vec3], ts: &[f32], tris: &[[u32; 3]], st: &Style) {
        if st.flat {
            for t in tris {
                let (a, b, c) = (ps[t[0] as usize], ps[t[1] as usize], ps[t[2] as usize]);
                let fnrm = (b - a).cross(c - a);
                if fnrm.length_squared() < 1e-14 {
                    continue;
                }
                let mut fnrm = fnrm.normalize();
                let avg = ns[t[0] as usize] + ns[t[1] as usize] + ns[t[2] as usize];
                let flip = fnrm.dot(avg) < 0.0;
                if flip {
                    fnrm = -fnrm;
                }
                let base = self.verts.len() as u32;
                let tt = (ts[t[0] as usize], ts[t[1] as usize], ts[t[2] as usize]);
                self.verts.push(vertex(a, fnrm, st, tt.0));
                if flip {
                    self.verts.push(vertex(c, fnrm, st, tt.2));
                    self.verts.push(vertex(b, fnrm, st, tt.1));
                } else {
                    self.verts.push(vertex(b, fnrm, st, tt.1));
                    self.verts.push(vertex(c, fnrm, st, tt.2));
                }
                self.idx.extend_from_slice(&[base, base + 1, base + 2]);
            }
        } else {
            let base = self.verts.len() as u32;
            for i in 0..ps.len() {
                self.verts.push(vertex(ps[i], ns[i], st, ts[i]));
            }
            for t in tris {
                let (a, b, c) = (ps[t[0] as usize], ps[t[1] as usize], ps[t[2] as usize]);
                let fnrm = (b - a).cross(c - a);
                if fnrm.length_squared() < 1e-14 {
                    continue;
                }
                let avg = ns[t[0] as usize] + ns[t[1] as usize] + ns[t[2] as usize];
                if fnrm.dot(avg) < 0.0 {
                    self.idx.extend_from_slice(&[base + t[0], base + t[2], base + t[1]]);
                } else {
                    self.idx.extend_from_slice(&[base + t[0], base + t[1], base + t[2]]);
                }
            }
        }
    }

    /// Transform local data by `m` then push.
    fn push_local(&mut self, m: &Mat4, ps: &[Vec3], ns: &[Vec3], ts: &[f32], tris: &[[u32; 3]], st: &Style) {
        let nm = normal_matrix(m);
        let wp: Vec<Vec3> = ps.iter().map(|p| m.transform_point3(*p)).collect();
        let wn: Vec<Vec3> = ns.iter().map(|n| (nm * *n).normalize_or_zero()).collect();
        self.push_tris(&wp, &wn, ts, tris, st);
    }

    // ------------------------------------------------------- primitives ----

    /// Unit box with softly bevelled edges. Six small grids share smooth corner normals.
    pub fn bevel_box(&mut self, m: &Mat4, bevel: f32, st: &Style) {
        let r = bevel.clamp(0.001, 0.24);
        let inner = 0.5 - r;
        let steps = [-0.5, -inner, inner, 0.5];
        for (n, u, v) in [(Vec3::X, Vec3::Z, Vec3::Y), (-Vec3::X, Vec3::Y, Vec3::Z),
            (Vec3::Y, Vec3::X, Vec3::Z), (-Vec3::Y, Vec3::Z, Vec3::X),
            (Vec3::Z, Vec3::Y, Vec3::X), (-Vec3::Z, Vec3::X, Vec3::Y)] {
            let mut ps = Vec::new();
            let mut ns = Vec::new();
            let mut ts = Vec::new();
            for y in steps { for x in steps {
                let p = n * 0.5 + u * x + v * y;
                let q = p.clamp(Vec3::splat(-inner), Vec3::splat(inner));
                let normal = (p - q).normalize();
                let p = q + normal * r;
                ps.push(p); ns.push(normal); ts.push(p.y + 0.5);
            }}
            let mut tris = Vec::new();
            for y in 0..3 { for x in 0..3 {
                let a = y * 4 + x;
                tris.extend([[a, a + 1, a + 4], [a + 1, a + 5, a + 4]]);
            }}
            self.push_local(m, &ps, &ns, &ts, &tris, st);
        }
    }

    /// Unit sphere (radius 1) scaled/placed by `m`.
    pub fn sphere(&mut self, m: &Mat4, segs: u32, rings: u32, st: &Style) {
        self.sphere_jit(m, segs, rings, st, 0.0, 0);
    }

    /// Sphere with a deterministic radial jitter (rocks).
    pub fn sphere_jit(&mut self, m: &Mat4, segs: u32, rings: u32, st: &Style, jit: f32, seed: i32) {
        let mut ps = vec![Vec3::Y];
        let mut ts = vec![1.0];
        for k in 1..rings {
            let th = PI * k as f32 / rings as f32;
            for j in 0..segs {
                let ph = TAU * j as f32 / segs as f32;
                ps.push(Vec3::new(th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()));
                ts.push(0.5 + 0.5 * th.cos());
            }
        }
        ps.push(-Vec3::Y);
        ts.push(0.0);
        if jit != 0.0 {
            for (i, p) in ps.iter_mut().enumerate() {
                *p *= 1.0 + jit * (hash01(i as i32, seed, 77) * 2.0 - 1.0);
            }
        }
        let bottom = (ps.len() - 1) as u32;
        let mut tris = Vec::new();
        let ring = |k: u32, j: u32| 1 + (k - 1) * segs + (j % segs);
        for j in 0..segs {
            tris.push([0, ring(1, j + 1), ring(1, j)]);
            tris.push([bottom, ring(rings - 1, j), ring(rings - 1, j + 1)]);
        }
        for k in 1..rings - 1 {
            for j in 0..segs {
                tris.push([ring(k, j), ring(k, j + 1), ring(k + 1, j)]);
                tris.push([ring(k, j + 1), ring(k + 1, j + 1), ring(k + 1, j)]);
            }
        }
        let ns: Vec<Vec3> = ps.iter().map(|p| p.normalize_or_zero()).collect();
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Cylinder or frustum along +Y from y=0 (radius `r0`) to y=1 (radius `r1`).
    pub fn cyl(&mut self, m: &Mat4, segs: u32, r0: f32, r1: f32, caps: bool, st: &Style) {
        let mut ps = Vec::new();
        let mut ns = Vec::new();
        let mut ts = Vec::new();
        let slope = r0 - r1;
        for j in 0..segs {
            let ph = TAU * j as f32 / segs as f32;
            let (c, s) = (ph.cos(), ph.sin());
            let n = Vec3::new(c, slope, s).normalize();
            ps.push(Vec3::new(c * r0, 0.0, s * r0));
            ns.push(n);
            ts.push(0.0);
            ps.push(Vec3::new(c * r1, 1.0, s * r1));
            ns.push(n);
            ts.push(1.0);
        }
        let mut tris = Vec::new();
        for j in 0..segs {
            let a = 2 * j;
            let b = 2 * ((j + 1) % segs);
            tris.push([a, b, a + 1]);
            tris.push([b, b + 1, a + 1]);
        }
        let base = ps.len() as u32;
        if caps && r0 > 0.0 {
            ps.push(Vec3::ZERO);
            ns.push(-Vec3::Y);
            ts.push(0.0);
            for j in 0..segs {
                let ph = TAU * j as f32 / segs as f32;
                ps.push(Vec3::new(ph.cos() * r0, 0.0, ph.sin() * r0));
                ns.push(-Vec3::Y);
                ts.push(0.0);
            }
            for j in 0..segs {
                tris.push([base, base + 1 + (j + 1) % segs, base + 1 + j]);
            }
        }
        let base = ps.len() as u32;
        if caps && r1 > 0.0 {
            ps.push(Vec3::Y);
            ns.push(Vec3::Y);
            ts.push(1.0);
            for j in 0..segs {
                let ph = TAU * j as f32 / segs as f32;
                ps.push(Vec3::new(ph.cos() * r1, 1.0, ph.sin() * r1));
                ns.push(Vec3::Y);
                ts.push(1.0);
            }
            for j in 0..segs {
                tris.push([base, base + 1 + j, base + 1 + (j + 1) % segs]);
            }
        }
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Cone along +Y (base radius 1 at y=0, tip at y=1).
    pub fn cone(&mut self, m: &Mat4, segs: u32, st: &Style) {
        self.cyl(m, segs, 1.0, 0.0, true, st);
    }

    /// Unit cube centred on the origin (side 1). Flat normals.
    pub fn cube(&mut self, m: &Mat4, st: &Style) {
        let faces: [(Vec3, Vec3, Vec3); 6] = [
            (Vec3::X, Vec3::Y, Vec3::Z),
            (-Vec3::X, Vec3::Y, -Vec3::Z),
            (Vec3::Y, Vec3::Z, Vec3::X),
            (-Vec3::Y, Vec3::Z, -Vec3::X),
            (Vec3::Z, Vec3::Y, -Vec3::X),
            (-Vec3::Z, Vec3::Y, Vec3::X),
        ];
        let mut ps = Vec::new();
        let mut ns = Vec::new();
        let mut ts = Vec::new();
        let mut tris = Vec::new();
        for (n, u, v) in faces {
            let b = ps.len() as u32;
            for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let p = n * 0.5 + u * (0.5 * su) + v * (0.5 * sv);
                ts.push(p.y + 0.5);
                ps.push(p);
                ns.push(n);
            }
            tris.push([b, b + 1, b + 2]);
            tris.push([b, b + 2, b + 3]);
        }
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Box between two corner points in final space, no transform.
    pub fn aabb(&mut self, lo: Vec3, hi: Vec3, st: &Style) {
        let m = Mat4::from_translation((lo + hi) * 0.5) * Mat4::from_scale(hi - lo);
        self.cube(&m, st);
    }

    /// Flat disc, radius 1, in the XZ plane, normal +Y.
    pub fn disc(&mut self, m: &Mat4, segs: u32, st: &Style) {
        let mut ps = vec![Vec3::ZERO];
        for j in 0..segs {
            let ph = TAU * j as f32 / segs as f32;
            ps.push(Vec3::new(ph.cos(), 0.0, ph.sin()));
        }
        let ns = vec![Vec3::Y; ps.len()];
        let ts = vec![1.0; ps.len()];
        let tris: Vec<[u32; 3]> = (0..segs).map(|j| [0, 1 + (j + 1) % segs, 1 + j]).collect();
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Torus in the XZ plane: ring radius `major`, tube radius `minor`.
    pub fn torus(&mut self, m: &Mat4, segs: u32, tube: u32, major: f32, minor: f32, st: &Style) {
        let mut ps = Vec::new();
        let mut ns = Vec::new();
        let mut ts = Vec::new();
        for i in 0..segs {
            let a = TAU * i as f32 / segs as f32;
            let (ca, sa) = (a.cos(), a.sin());
            for k in 0..tube {
                let b = TAU * k as f32 / tube as f32;
                let (cb, sb) = (b.cos(), b.sin());
                let n = Vec3::new(ca * cb, sb, sa * cb);
                ps.push(Vec3::new(ca * major, 0.0, sa * major) + n * minor);
                ns.push(n);
                ts.push(0.5 + 0.5 * sb);
            }
        }
        let mut tris = Vec::new();
        for i in 0..segs {
            for k in 0..tube {
                let a = i * tube + k;
                let b = ((i + 1) % segs) * tube + k;
                let a1 = i * tube + (k + 1) % tube;
                let b1 = ((i + 1) % segs) * tube + (k + 1) % tube;
                tris.push([a, b, a1]);
                tris.push([b, b1, a1]);
            }
        }
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Surface of revolution about +Y. `profile` is `(radius, y)` from bottom
    /// to top; radius 0 at an end closes it to a point.
    pub fn lathe(&mut self, m: &Mat4, segs: u32, profile: &[(f32, f32)], st: &Style) {
        let n = profile.len();
        let ymin = profile.iter().map(|p| p.1).fold(f32::MAX, f32::min);
        let ymax = profile.iter().map(|p| p.1).fold(f32::MIN, f32::max);
        let mut ps = Vec::new();
        let mut ns = Vec::new();
        let mut ts = Vec::new();
        for (i, &(r, y)) in profile.iter().enumerate() {
            let prev = profile[i.saturating_sub(1)];
            let next = profile[(i + 1).min(n - 1)];
            let (dr, dy) = (next.0 - prev.0, next.1 - prev.1);
            // Outward normal of the profile in the (r, y) plane.
            let (nr, ny) = (dy, -dr);
            let l = (nr * nr + ny * ny).sqrt().max(1e-6);
            let (nr, ny) = (nr / l, ny / l);
            for j in 0..segs {
                let ph = TAU * j as f32 / segs as f32;
                let (c, s) = (ph.cos(), ph.sin());
                ps.push(Vec3::new(c * r, y, s * r));
                ns.push(Vec3::new(c * nr, ny, s * nr));
                ts.push((y - ymin) / (ymax - ymin).max(1e-6));
            }
        }
        let mut tris = Vec::new();
        for i in 0..n - 1 {
            for j in 0..segs {
                let a = (i as u32) * segs + j;
                let b = (i as u32) * segs + (j + 1) % segs;
                let a1 = (i as u32 + 1) * segs + j;
                let b1 = (i as u32 + 1) * segs + (j + 1) % segs;
                tris.push([a, b, a1]);
                tris.push([b, b1, a1]);
            }
        }
        self.push_local(m, &ps, &ns, &ts, &tris, st);
    }

    /// Arbitrary quad (a, b, c, d around) in final space; flat normal oriented
    /// along `toward` (the side that should face the viewer).
    pub fn quad4(&mut self, p: [Vec3; 4], toward: Vec3, st: &Style) {
        let n = (p[1] - p[0]).cross(p[2] - p[0]);
        let n = if n.dot(toward) < 0.0 { -n } else { n };
        let n = n.normalize_or_zero();
        let ns = [n; 4];
        let ts = [0.0, 0.0, 1.0, 1.0];
        self.push_tris(&p, &ns, &ts, &[[0, 1, 2], [0, 2, 3]], &Style { flat: true, ..*st });
    }

    /// Triangle in final space oriented to face `toward`.
    pub fn tri(&mut self, p: [Vec3; 3], toward: Vec3, st: &Style) {
        let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        let n = if n.dot(toward) < 0.0 { -n } else { n };
        self.push_tris(&p, &[n; 3], &[0.0, 0.5, 1.0], &[[0, 1, 2]], &Style { flat: true, ..*st });
    }

    /// Extruded five-pointed (or n-pointed) star in the XY plane facing +Z,
    /// outer radius 1, thickness `depth` centred on z=0.
    pub fn star(&mut self, m: &Mat4, points: u32, inner: f32, depth: f32, st: &Style) {
        let n = points * 2;
        let ring = |i: u32, z: f32| {
            let a = PI * 0.5 + TAU * i as f32 / n as f32;
            let r = if i % 2 == 0 { 1.0 } else { inner };
            Vec3::new(a.cos() * r, a.sin() * r, z)
        };
        let mut ps = vec![Vec3::new(0.0, 0.0, depth * 0.5), Vec3::new(0.0, 0.0, -depth * 0.5)];
        for i in 0..n {
            ps.push(ring(i, depth * 0.5));
        }
        for i in 0..n {
            ps.push(ring(i, -depth * 0.5));
        }
        let mut ns = Vec::new();
        let mut tris = Vec::new();
        for i in 0..n {
            let j = (i + 1) % n;
            tris.push([0, 2 + i, 2 + j]);
            tris.push([1, 2 + n + j, 2 + n + i]);
            tris.push([2 + i, 2 + n + i, 2 + j]);
            tris.push([2 + j, 2 + n + i, 2 + n + j]);
        }
        // Normals: only used for orientation; outward from the axis, +Z / -Z on faces.
        for (i, p) in ps.iter().enumerate() {
            ns.push(if i == 0 {
                Vec3::Z
            } else if i == 1 {
                -Vec3::Z
            } else {
                Vec3::new(p.x, p.y, p.z * 4.0)
            });
        }
        let ts = vec![0.5; ps.len()];
        // Always flat shaded.
        self.push_local(m, &ps, &ns, &ts, &tris, &Style { flat: true, ..*st });
    }
}

pub fn normal_matrix(m: &Mat4) -> Mat3 {
    let m3 = Mat3::from_mat4(*m);
    if m3.determinant().abs() < 1e-12 {
        Mat3::IDENTITY
    } else {
        m3.inverse().transpose()
    }
}

/// A model matrix mapping a +Y-axis primitive (cylinder, cone) onto the
/// segment `a -> b` with the given radius scale.
pub fn limb(a: Vec3, b: Vec3, radius: f32) -> Mat4 {
    let d = b - a;
    let len = d.length().max(1e-5);
    let y = d / len;
    let helper = if y.x.abs() < 0.9 { Vec3::X } else { Vec3::Z };
    let x = helper.cross(y).normalize();
    let z = y.cross(x);
    Mat4::from_cols((x * radius).extend(0.0), d.extend(0.0), (z * radius).extend(0.0), a.extend(1.0))
}

/// Matrix from an origin and three axes.
pub fn basis(origin: Vec3, x: Vec3, y: Vec3, z: Vec3) -> Mat4 {
    Mat4::from_cols(x.extend(0.0), y.extend(0.0), z.extend(0.0), origin.extend(1.0))
}

/// Cheap deterministic hash to 0..1.
pub fn hash01(a: i32, b: i32, c: i32) -> f32 {
    let mut h = (a as u32).wrapping_mul(0x9E37_79B1) ^ (b as u32).wrapping_mul(0x85EB_CA77) ^ (c as u32).wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h & 0xFFFFFF) as f32 / 16_777_216.0
}

/// Smooth 2D value noise, 0..1.
pub fn noise2(x: f32, y: f32, seed: i32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (xi, yi) = (xi as i32, yi as i32);
    let a = hash01(xi, yi, seed);
    let b = hash01(xi + 1, yi, seed);
    let c = hash01(xi, yi + 1, seed);
    let d = hash01(xi + 1, yi + 1, seed);
    (a + (b - a) * sx) + ((c + (d - c) * sx) - (a + (b - a) * sx)) * sy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(m: &MeshData, convex_about: Option<Vec3>) {
        assert!(!m.verts.is_empty());
        assert_eq!(m.idx.len() % 3, 0);
        for &i in &m.idx {
            assert!((i as usize) < m.verts.len(), "index out of range");
        }
        for v in &m.verts {
            assert!(v.pos.iter().all(|c| c.is_finite()));
            let n = Vec3::new(v.nrm[0] as f32, v.nrm[1] as f32, v.nrm[2] as f32) / 127.0;
            assert!((n.length() - 1.0).abs() < 0.06, "normal not unit: {n:?}");
        }
        if let Some(c) = convex_about {
            for t in m.idx.chunks_exact(3) {
                let p = |k: usize| Vec3::from_array(m.verts[t[k] as usize].pos);
                let (a, b, cc) = (p(0), p(1), p(2));
                let fnrm = (b - a).cross(cc - a);
                let cen = (a + b + cc) / 3.0 - c;
                assert!(fnrm.dot(cen) > 0.0, "inward winding");
            }
        }
    }

    #[test]
    fn primitives_valid() {
        let m = Mat4::from_scale(Vec3::new(1.0, 2.0, 0.5));
        let mut b = MeshData::new();
        b.sphere(&m, 12, 8, &Style::tint0(1.0));
        check(&b, Some(Vec3::ZERO));
        let mut b = MeshData::new();
        b.sphere(&m, 8, 6, &Style::tint0(1.0).flat());
        check(&b, Some(Vec3::ZERO));
        let mut b = MeshData::new();
        b.cube(&Mat4::IDENTITY, &Style::col(1.0, 0.0, 0.0));
        check(&b, Some(Vec3::ZERO));
        let mut b = MeshData::new();
        b.cyl(&Mat4::from_translation(Vec3::Y * -0.5), 10, 1.0, 0.7, true, &Style::col(0.5, 0.5, 0.5));
        check(&b, Some(Vec3::ZERO));
        let mut b = MeshData::new();
        b.cone(&Mat4::from_translation(Vec3::Y * -0.25), 8, &Style::col(0.5, 0.5, 0.5));
        check(&b, Some(Vec3::new(0.0, 0.1, 0.0)));
        let mut b = MeshData::new();
        b.torus(&Mat4::IDENTITY, 12, 6, 1.0, 0.2, &Style::col(1.0, 1.0, 0.0));
        check(&b, None);
        let mut b = MeshData::new();
        b.disc(&Mat4::IDENTITY, 12, &Style::col(1.0, 1.0, 0.0));
        check(&b, None);
        let mut b = MeshData::new();
        b.lathe(&Mat4::IDENTITY, 10, &[(0.0, 0.0), (0.6, 0.2), (0.8, 0.8), (0.4, 1.4), (0.0, 1.6)], &Style::col(1.0, 1.0, 1.0));
        check(&b, Some(Vec3::new(0.0, 0.8, 0.0)));
        let mut b = MeshData::new();
        b.star(&Mat4::IDENTITY, 5, 0.45, 0.2, &Style::col(1.0, 1.0, 0.0));
        check(&b, None);
    }

    #[test]
    fn mirrored_matrix_keeps_winding() {
        let m = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)) * Mat4::from_translation(Vec3::X * 3.0);
        let mut b = MeshData::new();
        b.sphere(&m, 10, 6, &Style::tint0(1.0));
        check(&b, Some(Vec3::new(-3.0, 0.0, 0.0)));
        let mut c = MeshData::new();
        let mut src = MeshData::new();
        src.sphere(&Mat4::IDENTITY, 10, 6, &Style::tint0(1.0));
        c.append(&src, &Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)));
        check(&c, Some(Vec3::ZERO));
    }

    #[test]
    fn limb_cylinder_is_outward() {
        let a = Vec3::new(0.3, 1.0, 0.2);
        let b = Vec3::new(0.35, 1.2, 0.3);
        let m = limb(a, b, 0.1);
        let mut c = MeshData::new();
        c.cyl(&m, 8, 1.0, 0.88, false, &Style::tint0(1.0));
        for t in c.idx.chunks_exact(3) {
            let p = |k: usize| Vec3::from_array(c.verts[t[k] as usize].pos);
            let n = |k: usize| {
                let v = c.verts[t[k] as usize].nrm;
                Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32)
            };
            let (p0, p1, p2) = (p(0), p(1), p(2));
            let cen = (p0 + p1 + p2) / 3.0;
            let ax = (b - a).normalize();
            let out = (cen - a) - ax * (cen - a).dot(ax);
            assert!((p1 - p0).cross(p2 - p0).dot(out) > 0.0, "inward winding");
            assert!(n(0).dot(out) > 0.0, "inward normal");
            let r = out.length();
            assert!(r > 0.07 && r < 0.11, "radius {r}");
        }
    }

    #[test]
    fn limb_maps_segment() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(1.5, 1.0, 3.5);
        let m = limb(a, b, 0.2);
        assert!((m.transform_point3(Vec3::ZERO) - a).length() < 1e-5);
        assert!((m.transform_point3(Vec3::Y) - b).length() < 1e-5);
    }
}
