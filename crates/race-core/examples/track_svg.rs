//! Writes one SVG per track (top-down: centreline, road, shoulders, walls,
//! racing line, features, start line) into the directory given as the first
//! argument (default `.`), plus `contact_sheet.svg` (every track in a grid),
//! and prints each track's length and corner stats. Overpass spans are drawn in red.
//!
//! `cargo run -p race-core --example track_svg -- /tmp/tracks`

use std::fmt::Write as _;

use race_core::*;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).expect("create output dir");
    let mut sheet: Vec<(String, String)> = Vec::new(); // (viewBox, body)
    for id in TrackId::ALL {
        let tr = id.track();
        let svg = render(&tr);
        if let (Some(a), Some(nl)) = (svg.find("viewBox=\""), svg.find('\n')) {
            let vb_start = a + "viewBox=\"".len();
            if let Some(len) = svg[vb_start..].find('"') {
                let body = svg[nl + 1..]
                    .trim_end()
                    .trim_end_matches("</svg>")
                    .to_string();
                sheet.push((svg[vb_start..vb_start + len].to_string(), body));
            }
        }
        let path = format!("{dir}/{}.svg", format!("{:?}", id).to_lowercase());
        std::fs::write(&path, svg).expect("write svg");
        let min_r = tr
            .samples
            .iter()
            .map(|s| 1.0 / s.curvature.abs().max(1e-6))
            .fold(f32::INFINITY, f32::min);
        let (lo, hi) = tr.bounds();
        let ys: Vec<f32> = tr.samples.iter().map(|s| s.pos.y).collect();
        let ymin = ys.iter().cloned().fold(f32::INFINITY, f32::min);
        let ymax = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        println!(
            "{:<18} length {:>6.0} m  min radius {:>5.1} m  max half band {:>4.1}  size {:.0}x{:.0} m  height {:.1}..{:.1}  scenery {}  -> {}",
            tr.name,
            tr.length,
            min_r,
            tr.max_half_band,
            hi.x - lo.x,
            hi.z - lo.z,
            ymin,
            ymax,
            tr.scenery().len(),
            path
        );
    }
    // Contact sheet: every track, 3 per row, each scaled to fit a 500x500 cell.
    let (cols, cell) = (3usize, 500.0f32);
    let rows = sheet.len().div_ceil(cols);
    let mut out = String::new();
    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}">"#,
        cols as f32 * cell,
        rows as f32 * cell,
        cols as f32 * cell,
        rows as f32 * cell
    );
    let _ = writeln!(out, r#"<rect width="100%" height="100%" fill="white"/>"#);
    for (i, (vb, body)) in sheet.iter().enumerate() {
        let (cx, cy) = ((i % cols) as f32 * cell, (i / cols) as f32 * cell);
        let _ = writeln!(
            out,
            r#"<svg x="{cx}" y="{cy}" width="{cell}" height="{cell}" viewBox="{vb}" preserveAspectRatio="xMidYMid meet">"#
        );
        out.push_str(body);
        out.push_str("</svg>\n");
    }
    out.push_str("</svg>\n");
    let path = format!("{dir}/contact_sheet.svg");
    std::fs::write(&path, out).expect("write contact sheet");
    println!("contact sheet -> {path}");
}

fn render(tr: &Track) -> String {
    let (lo, hi) = tr.bounds();
    let pad = 30.0;
    // Top-down view: screen x = -world x, screen y = -world z (true map, not mirrored).
    let (x0, x1) = (-hi.x - pad, -lo.x + pad);
    let (y0, y1) = (-hi.z - pad, -lo.z + pad);
    let (w, h) = (x1 - x0, y1 - y0);
    let mut s = String::new();
    let _ = writeln!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x0:.1} {y0:.1} {w:.1} {h:.1}" width="{:.0}" height="{:.0}">"#,
        w * 1.2,
        h * 1.2
    );
    let c = |rgb: [f32; 3]| {
        format!(
            "rgb({},{},{})",
            (rgb[0] * 255.0) as u8,
            (rgb[1] * 255.0) as u8,
            (rgb[2] * 255.0) as u8
        )
    };
    let th = &tr.theme;
    let _ = writeln!(
        s,
        r#"<rect x="{x0}" y="{y0}" width="{w}" height="{h}" fill="{}"/>"#,
        c(th.ground)
    );
    let pt = |p: Vec3| format!("{:.1},{:.1}", -p.x, -p.z);
    let edge = |side: f32, extra: bool| -> String {
        tr.samples
            .iter()
            .map(|sm| {
                let hw = 0.5 * sm.width
                    + if extra {
                        if side < 0.0 {
                            sm.shoulder_l
                        } else {
                            sm.shoulder_r
                        }
                    } else {
                        0.0
                    };
                pt(sm.pos + sm.right * side * hw)
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    // Shoulder band, road band (as polygons between edges).
    for (extra, col) in [(true, c(th.shoulder)), (false, c(th.road))] {
        let left = edge(-1.0, extra);
        let right = edge(1.0, extra);
        let _ = writeln!(
            s,
            r#"<polyline points="{left}" fill="none" stroke="{col}" stroke-width="0.6"/>"#
        );
        let _ = writeln!(
            s,
            r#"<polyline points="{right}" fill="none" stroke="{col}" stroke-width="0.6"/>"#
        );
        // Filled ribbon: quads per sample.
        let n = tr.samples.len();
        let mut d = String::new();
        for i in 0..n {
            let a = &tr.samples[i];
            let b = &tr.samples[(i + 1) % n];
            let hw = |sm: &TrackSample, side: f32| {
                0.5 * sm.width
                    + if extra {
                        if side < 0.0 {
                            sm.shoulder_l
                        } else {
                            sm.shoulder_r
                        }
                    } else {
                        0.0
                    }
            };
            let _ = write!(
                d,
                "M{} L{} L{} L{} Z ",
                pt(a.pos - a.right * hw(a, -1.0)),
                pt(b.pos - b.right * hw(b, -1.0)),
                pt(b.pos + b.right * hw(b, 1.0)),
                pt(a.pos + a.right * hw(a, 1.0))
            );
        }
        let _ = writeln!(
            s,
            r#"<path d="{d}" fill="{col}" stroke="{col}" stroke-width="0.3"/>"#
        );
    }
    // Walls.
    for side in [-1.0f32, 1.0] {
        let _ = writeln!(
            s,
            r#"<polyline points="{} {}" fill="none" stroke="{}" stroke-width="1.2"/>"#,
            edge(side, true),
            pt(tr.samples[0].pos + tr.samples[0].right * side * (0.5 * tr.samples[0].width + 4.0)),
            c(th.wall)
        );
    }
    // Racing line and centreline.
    let rl = tr
        .racing_line
        .iter()
        .map(|l| pt(l.pos))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(
        s,
        r#"<polygon points="{rl}" fill="none" stroke="rgb(40,200,255)" stroke-width="0.7" stroke-dasharray="3,2"/>"#
    );
    let cl = tr
        .samples
        .iter()
        .map(|sm| pt(sm.pos))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(
        s,
        r#"<polygon points="{cl}" fill="none" stroke="white" stroke-width="0.4"/>"#
    );
    // Overpass spans (the deck of each bridge) in red over the road.
    for b in &tr.bridges {
        let mut d = Vec::new();
        let mut x = 0.0;
        while x <= b.s1 - b.s0 {
            d.push(pt(tr.sample_at(b.s0 + x).pos));
            x += tr.ds;
        }
        let _ = writeln!(
            s,
            r#"<polyline points="{}" fill="none" stroke="rgba(220,40,40,0.55)" stroke-width="{:.1}" stroke-linecap="butt"/>"#,
            d.join(" "),
            tr.sample_at(b.s0).width
        );
    }
    // Features.
    for f in &tr.features {
        match *f {
            Feature::Ramp { s: fs, len, .. } => {
                let a = tr.sample_at(fs);
                let b = tr.sample_at(fs + len);
                let hw = 0.5 * a.width;
                let _ = writeln!(
                    s,
                    r#"<polygon points="{} {} {} {}" fill="rgb(150,90,30)" stroke="black" stroke-width="0.4"/>"#,
                    pt(a.pos - a.right * hw),
                    pt(b.pos - b.right * hw),
                    pt(b.pos + b.right * hw),
                    pt(a.pos + a.right * hw)
                );
            }
            Feature::Gap { s: fs, len } => {
                let a = tr.sample_at(fs);
                let b = tr.sample_at(fs + len);
                let hw = 0.5 * a.width + a.shoulder_l;
                let _ = writeln!(
                    s,
                    r#"<polygon points="{} {} {} {}" fill="black"/>"#,
                    pt(a.pos - a.right * hw),
                    pt(b.pos - b.right * hw),
                    pt(b.pos + b.right * hw),
                    pt(a.pos + a.right * hw)
                );
            }
            Feature::BoostPad {
                s: fs,
                lateral,
                len,
            } => {
                let a = tr.sample_at(fs);
                let b = tr.sample_at(fs + len);
                let h = 0.5 * PAD_WIDTH;
                let _ = writeln!(
                    s,
                    r#"<polygon points="{} {} {} {}" fill="rgb(255,140,0)"/>"#,
                    pt(a.pos + a.right * (lateral - h)),
                    pt(b.pos + b.right * (lateral - h)),
                    pt(b.pos + b.right * (lateral + h)),
                    pt(a.pos + a.right * (lateral + h))
                );
            }
            Feature::TargetRow { s: fs, count } => {
                let a = tr.sample_at(fs);
                let c = count as f32;
                let sp = (4.0f32).min((a.width - 3.0) / (c - 1.0).max(1.0));
                for k in 0..count {
                    let p = a.pos + a.right * ((k as f32 - (c - 1.0) * 0.5) * sp);
                    let _ = writeln!(
                        s,
                        r#"<circle cx="{:.1}" cy="{:.1}" r="1.2" fill="rgb(247,147,26)" stroke="black" stroke-width="0.3"/>"#,
                        -p.x, -p.z
                    );
                }
            }
            Feature::CoinLine {
                s: fs,
                lateral,
                count,
                spacing,
            } => {
                for k in 0..count {
                    let a = tr.sample_at(fs + k as f32 * spacing);
                    let p = a.pos + a.right * lateral;
                    let _ = writeln!(
                        s,
                        r#"<circle cx="{:.1}" cy="{:.1}" r="0.7" fill="gold"/>"#,
                        -p.x, -p.z
                    );
                }
            }
            Feature::Hurdle { s: fs } | Feature::Gate { s: fs } => {
                let a = tr.sample_at(fs);
                let hw = 0.5 * a.width;
                let col = if matches!(f, Feature::Hurdle { .. }) {
                    "saddlebrown"
                } else {
                    "blue"
                };
                let _ = writeln!(
                    s,
                    r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{col}" stroke-width="1.2"/>"#,
                    -(a.pos - a.right * hw).x,
                    -(a.pos - a.right * hw).z,
                    -(a.pos + a.right * hw).x,
                    -(a.pos + a.right * hw).z
                );
            }
        }
    }
    // Start line and direction arrow.
    let a = &tr.samples[0];
    let hw = 0.5 * a.width;
    let _ = writeln!(
        s,
        r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="red" stroke-width="2"/>"#,
        -(a.pos - a.right * hw).x,
        -(a.pos - a.right * hw).z,
        -(a.pos + a.right * hw).x,
        -(a.pos + a.right * hw).z
    );
    let b = tr.sample_at(25.0);
    let _ = writeln!(
        s,
        r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="red" stroke-width="1.2"/>"#,
        -a.pos.x, -a.pos.z, -b.pos.x, -b.pos.z
    );
    let _ = writeln!(
        s,
        r#"<text x="{:.1}" y="{:.1}" font-size="14" fill="black">{}  {:.0} m</text>"#,
        x0 + 5.0,
        y0 + 16.0,
        tr.name,
        tr.length
    );
    s.push_str("</svg>\n");
    s
}
