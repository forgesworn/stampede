//! Simple articulated jockeys and the track's mischievous frogs.

use crate::mesh::limb;
use crate::meshes::MeshId;
use crate::ostrich::OstrichPart;
use crate::types::{FrogPose, KnockStyle, OstrichPose, Rgb, RosePose, RugPose};
use glam::{Mat4, Quat, Vec3};
use std::f32::consts::{PI, TAU};

fn tr(p: Vec3, s: Vec3) -> Mat4 {
    Mat4::from_translation(p) * Mat4::from_scale(s)
}

fn limb_ball(a: Vec3, b: Vec3, radius: f32) -> Mat4 {
    // A sphere spans -1..1; the limb frame expects 0..1 along its length.
    limb(a, b, radius) * tr(Vec3::Y * 0.5, Vec3::new(1.0, 0.5, 1.0))
}

/// Seconds a thrown jockey is in the air. Matches race-core's `DISMOUNT_FALL`
/// (the app checks), after which it lands and runs back.
pub const THROW_SECS: f32 = 0.4;
/// How far (radians about the vertical) an eating jockey turns its head towards
/// its +x side, so the face, the cake and the chewing are in profile to a camera
/// behind the ostrich instead of hidden behind the helmet.
pub const CAKE_HEAD_YAW: f32 = 1.35;
/// Seconds a Laser Trump smash holds the ostrich spinning (its spin-out time).
pub const SMASH_SECS: f32 = 1.0;

/// The procedural come-off for one cause at one moment: how the ostrich
/// reacts, and where and how the jockey is flung.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnockFx {
    /// Applied to the ostrich in its own frame.
    pub ostrich: Mat4,
    /// How far a dismounted jockey is along its flight from the saddle to where
    /// it lands (0..1).
    pub travel: f32,
    /// Metres above that flight line.
    pub lift: f32,
    /// Jockey tumble (about x, y and z, radians) about its middle. The jockey
    /// faces the ostrich and flies along its own z axis, so x is end over end
    /// and z is a cartwheel.
    pub tumble: Vec3,
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// 0 up to 1 over `rise`, held until `hold`, back to 0 by `end`.
fn bump(age: f32, rise: f32, hold: f32, end: f32) -> f32 {
    if age < hold {
        smoothstep(age / rise)
    } else {
        smoothstep((end - age) / (end - hold))
    }
}

/// `rot` about the point (0, y, 0).
fn about(y: f32, rot: Mat4) -> Mat4 {
    Mat4::from_translation(Vec3::Y * y) * rot * Mat4::from_translation(-Vec3::Y * y)
}

/// The come-off of `style` `age` seconds in. `side` is +1 when the ostrich is
/// bowled to its right, -1 to its left. Every style gives a different answer
/// at the same age.
pub fn knock_fx(style: KnockStyle, age: f32, side: f32) -> KnockFx {
    let age = age.max(0.0);
    let u = (age / THROW_SECS).clamp(0.0, 1.0);
    let arc = 4.0 * u * (1.0 - u);
    let side = if side < 0.0 { -1.0 } else { 1.0 };
    match style {
        KnockStyle::None => KnockFx {
            ostrich: Mat4::IDENTITY,
            travel: 1.0,
            lift: 0.0,
            tumble: Vec3::ZERO,
        },
        // Stumbling wobble while the jockey slides off the back, arms flailing.
        KnockStyle::Printer => {
            let decay = (-age * 2.2).exp() * smoothstep(age / 0.08);
            KnockFx {
                ostrich: about(
                    1.2,
                    Mat4::from_rotation_z((age * 15.0).sin() * 0.32 * decay)
                        * Mat4::from_rotation_x(0.2 * decay),
                ),
                travel: u,
                lift: 0.5 * arc,
                tumble: Vec3::new(-1.15 * smoothstep(u), 0.0, 0.0),
            }
        }
        // Yanked rug: onto its chest and skidding; the jockey somersaults up and ahead.
        KnockStyle::Rug => {
            let skid = bump(age, 0.2, 0.9, 1.5);
            KnockFx {
                ostrich: Mat4::from_translation(Vec3::new(0.0, -0.5 * skid, 0.6 * skid))
                    * about(0.9, Mat4::from_rotation_x(1.2 * skid)),
                travel: 1.0 - (1.0 - u) * (1.0 - u),
                lift: 3.4 * arc,
                tumble: Vec3::new(TAU * 1.5 * u, 0.0, 0.0),
            }
        }
        // Bowled over sideways, one full roll; the jockey cartwheels away.
        KnockStyle::Pepe => {
            let roll = smoothstep(age / 0.9);
            let shove = bump(age, 0.35, 0.5, 1.2);
            KnockFx {
                ostrich: Mat4::from_translation(Vec3::new(-side * 1.3 * shove, 0.0, 0.0))
                    * about(0.9, Mat4::from_rotation_z(side * TAU * roll)),
                travel: u,
                lift: 2.2 * arc,
                tumble: Vec3::new(0.0, 0.0, side * TAU * 2.0 * u),
            }
        }
        // Spun flat like a coin on its edge; the jockey is thrown straight up.
        KnockStyle::Trump => {
            let flat = bump(age, 0.15, SMASH_SECS - 0.25, SMASH_SECS);
            let w = (age / SMASH_SECS).clamp(0.0, 1.0);
            KnockFx {
                ostrich: Mat4::from_rotation_y(age * TAU * 2.5 * flat)
                    * Mat4::from_translation(Vec3::Y * 0.5 * flat)
                    * about(0.9, Mat4::from_rotation_z(1.5 * flat)),
                travel: 0.0,
                lift: 4.2 * 4.0 * w * (1.0 - w),
                tumble: Vec3::new(0.0, TAU * 1.5 * w, 0.0),
            }
        }
    }
}

/// The style to draw: an unseated jockey with no recorded cause (a joiner who
/// missed it) falls like a printer victim; the Le Mans run-up is not a knock-off.
pub(crate) fn effective_style(p: &OstrichPose) -> KnockStyle {
    if p.grid_start {
        KnockStyle::None
    } else if p.knock != KnockStyle::None {
        p.knock
    } else if p.dismounted {
        KnockStyle::Printer
    } else {
        KnockStyle::None
    }
}

/// The ostrich's reaction to being knocked, in its own frame.
pub(crate) fn ostrich_knock(p: &OstrichPose) -> Mat4 {
    let style = effective_style(p);
    if style == KnockStyle::None {
        Mat4::IDENTITY
    } else {
        knock_fx(style, p.knock_age, p.knock_side).ostrich
    }
}

/// First a cartwheel, then lying laughing, then getting back up.
pub(crate) fn fall(progress: f32, aside: f32) -> Mat4 {
    if progress <= 0.0 {
        return Mat4::IDENTITY;
    }
    let p = progress.clamp(0.0, 1.0);
    let down = if p < 0.25 {
        p / 0.25
    } else if p < 0.78 {
        1.0
    } else {
        (1.0 - p) / 0.22
    };
    let down = down * down * (3.0 - 2.0 * down);
    let roll = 1.45 * down + if p < 0.25 { TAU * (p / 0.25) } else { 0.0 };
    Mat4::from_translation(Vec3::new(aside * down, -0.85 * down, 0.0))
        * Mat4::from_translation(Vec3::Y * 1.25)
        * Mat4::from_rotation_z(roll)
        * Mat4::from_translation(-Vec3::Y * 1.25)
}

/// Where the cake is held while eating, in the jockey's frame: ahead of the
/// turned face, a little below the mouth, rising by `bite` (-0.1..0.1) to meet it.
fn cake_spot(head: Vec3, bite: f32) -> Vec3 {
    head + Mat4::from_rotation_y(CAKE_HEAD_YAW).transform_vector3(Vec3::new(0.0, -0.2 + bite, 0.4))
}

pub(crate) fn jockey(p: &OstrichPose, time: f32, seat: Mat4) -> Vec<OstrichPart> {
    let style = effective_style(p);
    let throw = knock_fx(style, p.knock_age, p.knock_side);
    let flying = style != KnockStyle::None && p.knock_age < THROW_SECS;
    let tumble = about(
        1.0,
        Mat4::from_rotation_y(throw.tumble.y)
            * Mat4::from_rotation_z(throw.tumble.z)
            * Mat4::from_rotation_x(throw.tumble.x),
    );
    let yaw = if p.grid_start {
        (-p.chase_offset.x).atan2(-p.chase_offset.z)
    } else if p.dismounted {
        p.chase_yaw
    } else {
        p.yaw + p.visual_yaw + p.spin
    };
    let root = Mat4::from_translation(
        p.pos
            + if p.dismounted {
                p.chase_offset * if flying { throw.travel } else { 1.0 }
            } else {
                Vec3::ZERO
            },
    ) * Mat4::from_rotation_y(yaw)
        * Mat4::from_rotation_z(p.lean)
        * Mat4::from_rotation_x(p.pitch)
        * Mat4::from_scale(Vec3::splat(p.scale));
    let shape = p.rider % 4;
    let height = 0.94 + shape as f32 * 0.045;
    let width = 0.94 + (3 - shape) as f32 * 0.04;
    let skin = [
        [0.94, 0.71, 0.52],
        [0.64, 0.39, 0.25],
        [0.83, 0.58, 0.4],
        [0.4, 0.24, 0.16],
    ][shape as usize];
    let shirt = match p.attire[1] {
        1 => [1.0, 0.43, 0.06],
        2 => [0.35, 0.16, 0.65],
        3 => [0.12, 0.65, 0.55],
        _ => p.accent,
    };
    let laughing = p.tumble > 0.25 && p.tumble < 0.78;
    let giggle = if laughing {
        (time * 20.0).sin() * 0.055
    } else {
        0.0
    };
    let rider = if p.dismounted {
        if flying {
            Mat4::from_translation(Vec3::Y * throw.lift) * tumble
        } else {
            Mat4::IDENTITY
        }
    } else if p.boarding > 0.0 {
        let progress = p.boarding;
        let mut offset = Mat4::from_rotation_y(-yaw).transform_vector3(p.boarding_from);
        offset.y -= seat.w_axis.y;
        seat * Mat4::from_translation(
            offset * (1.0 - progress) + Vec3::Y * (progress * PI).sin() * 0.9,
        )
    } else if style == KnockStyle::Trump && p.knock_age < SMASH_SECS {
        // Thrown straight up out of the saddle, spinning, and back down into it.
        Mat4::from_translation(Vec3::Y * throw.lift)
            * tumble
            * seat
            * Mat4::from_translation(Vec3::new(0.0, 0.0, -0.22))
    } else if p.tumble > 0.0 {
        fall(p.tumble, 1.45) * seat * Mat4::from_translation(Vec3::new(0.0, 0.0, -0.22))
    } else {
        seat * Mat4::from_translation(Vec3::new(0.0, 0.0, -0.22))
    };
    let r = root * rider * Mat4::from_scale(Vec3::new(width, height, 1.0));
    let mut out = Vec::with_capacity(32);
    let mut push = |mesh, model, col: Rgb| {
        out.push(OstrichPart {
            mesh,
            model: r * model,
            tint0: col,
            tint1: col,
        })
    };
    // Saddle, breeches, boots and a slight forward racing crouch.
    if !p.dismounted {
        push(
            MeshId::Saddle,
            Mat4::from_translation(Vec3::new(0.0, 0.48, 0.0)),
            shirt,
        );
    }
    for side in [-1.0, 1.0] {
        let hip = Vec3::new(side * 0.22, 0.62, 0.0);
        let stride = if p.dismounted && p.chase_age >= 0.0 {
            (time * 21.0 + side * PI * 0.5).sin() * 0.45
        } else {
            0.0
        };
        let knee = Vec3::new(
            side * if p.dismounted { 0.22 } else { 0.56 },
            0.29,
            0.32 + stride,
        );
        let boot = Vec3::new(
            side * if p.dismounted { 0.22 } else { 0.57 },
            -0.06,
            0.18 - stride,
        );
        push(
            MeshId::SmallBall,
            limb_ball(hip, knee, 0.13),
            [0.9, 0.88, 0.79],
        );
        push(
            MeshId::SmallBall,
            limb_ball(knee, boot, 0.105),
            [0.12, 0.10, 0.13],
        );
        push(
            MeshId::SmallBall,
            tr(boot, Vec3::new(0.12, 0.09, 0.22)),
            [0.10, 0.08, 0.11],
        );
    }
    let lean = if p.laptop {
        0.23
    } else if p.cake {
        -0.06
    } else {
        p.speed / 32.0 * 0.15
    };
    let body =
        Mat4::from_translation(Vec3::new(0.0, 0.92 + giggle, 0.08)) * Mat4::from_rotation_x(lean);
    push(
        MeshId::Jersey,
        body,
        shirt,
    );
    let head = Vec3::new(0.0, 1.48 + giggle, 0.1 + lean * 0.3);
    // An eating jockey turns its head side-on; everything on the face turns with it.
    let eating = p.cake && !p.dismounted;
    let turn = if eating { Mat4::from_rotation_y(CAKE_HEAD_YAW) } else { Mat4::IDENTITY };
    let head_m = Mat4::from_translation(head) * turn;
    let bite = (time * 5.0).sin() * 0.1;
    push(MeshId::JockeyHead, head_m, skin);
    push(MeshId::JockeyHelmet, head_m, shirt);
    for side in [-1.0, 1.0] {
        let eye = Vec3::new(side * 0.085, 0.005, 0.198);
        push(
            MeshId::SmallBall,
            head_m * tr(eye, Vec3::new(0.034, if laughing { 0.013 } else { 0.04 }, 0.018)),
            [0.05; 3],
        );
        let shoulder = Vec3::new(side * 0.25, 1.13, 0.1);
        let elbow = Vec3::new(side * 0.39, 0.91, 0.29);
        let hand = if p.printing {
            if side > 0.0 {
                Vec3::new(
                    0.55,
                    0.9 + (time * 16.0).sin() * 0.15,
                    0.53 + (time * 16.0).cos() * 0.15,
                )
            } else {
                Vec3::new(-0.27, 0.95, 0.58)
            }
        } else if p.dismounted && flying && style == KnockStyle::Printer {
            // Arms flailing as the jockey slides off.
            Vec3::new(
                side * 0.5,
                1.35 + (time * 30.0 + side * 2.0).sin() * 0.3,
                0.1 + (time * 30.0 + side * 2.0).cos() * 0.3,
            )
        } else if p.dismounted {
            Vec3::new(
                side * 0.3,
                0.82,
                0.2 + if p.chase_age >= 0.0 {
                    (time * 21.0 + side * PI * 0.5).sin() * 0.25
                } else {
                    0.0
                },
            )
        } else if p.laptop {
            Vec3::new(
                side * 0.15,
                0.84 + (time * 35.0 + side * 2.0).sin() * 0.04,
                0.58,
            )
        } else if p.cake {
            // Both hands hold the plate in front of the turned face.
            cake_spot(head, bite) + turn.transform_vector3(Vec3::new(side * 0.2, -0.04, -0.03))
        } else {
            Vec3::new(side * 0.17, 0.85, 0.52)
        };
        push(MeshId::SmallBall, limb_ball(shoulder, elbow, 0.095), shirt);
        push(MeshId::SmallBall, limb_ball(elbow, hand, 0.075), skin);
        push(MeshId::Ball, tr(hand, Vec3::splat(0.09)), [0.96, 0.93, 0.83]);
    }
    // The mouth opens as the cake comes up to it, then chews.
    let mouth = if laughing {
        0.055
    } else if eating {
        0.018 + 0.05 * (bite / 0.1).max(0.0) + 0.012 * (time * 18.0).sin().abs()
    } else {
        0.018
    };
    push(
        MeshId::SmallBall,
        head_m * tr(Vec3::new(0.0, -0.10, 0.19), Vec3::new(0.085, mouth, 0.025)),
        [0.3, 0.07, 0.06],
    );
    if p.laptop {
        push(MeshId::LaptopBase, Mat4::from_translation(Vec3::new(0.0, 0.79, 0.56)), [1.0; 3]);
        let screen = Mat4::from_translation(Vec3::new(0.0, 0.99, 0.75)) * Mat4::from_rotation_x(-0.15);
        push(MeshId::LaptopLid, screen, [1.0; 3]);
    }
    if p.printing {
        push(
            MeshId::Block,
            tr(Vec3::new(0.0, 0.82, 0.59), Vec3::new(0.72, 0.38, 0.5)),
            [0.25, 0.31, 0.29],
        );
        push(
            MeshId::Block,
            tr(Vec3::new(0.0, 1.02, 0.62), Vec3::new(0.5, 0.06, 0.3)),
            [0.9, 0.93, 0.8],
        );
        push(
            MeshId::Block,
            tr(Vec3::new(0.0, 0.85, 0.86), Vec3::new(0.48, 0.07, 0.07)),
            [0.04, 0.08, 0.04],
        );
        let crank =
            Mat4::from_translation(Vec3::new(0.43, 0.9, 0.59)) * Mat4::from_rotation_x(time * 16.0);
        push(
            MeshId::Block,
            crank * tr(Vec3::ZERO, Vec3::new(0.08, 0.34, 0.08)),
            [0.75, 0.76, 0.65],
        );
        push(
            MeshId::SmallBall,
            crank * tr(Vec3::new(0.1, 0.17, 0.0), Vec3::splat(0.09)),
            [0.12, 0.09, 0.07],
        );
        let sheet = ((time * 4.0).fract() * 0.36).max(0.08);
        push(
            MeshId::Block,
            tr(
                Vec3::new(0.0, 0.84, 0.9 + sheet),
                Vec3::new(0.43, 0.025, sheet * 2.0),
            ),
            [0.38, 0.8, 0.35],
        );
    }
    if eating {
        // Held out to the side of the body, level with the mouth, so the camera sees it
        // beside the head; it rises to the mouth with each bite.
        let c = cake_spot(head, bite);
        push(MeshId::Plate, Mat4::from_translation(c - Vec3::Y * 0.035) * turn, [1.0; 3]);
        push(MeshId::CakeSlice, Mat4::from_translation(c) * turn, [1.0; 3]);
    }
    if p.shells {
        for k in 0..3 {
            push(
                MeshId::Shell,
                tr(
                    head + Vec3::new((k as f32 - 1.0) * 0.10, 0.28, 0.0),
                    Vec3::new(0.13, 0.15, 0.13),
                ),
                [1.0, 0.97, 0.87],
            );
        }
    }
    out
}

pub(crate) fn rug(p: &RugPose, time: f32) -> Vec<OstrichPart> {
    // Lie on the banked, sloping road: a level rug sinks under it at the high edge.
    let yaw = Mat4::from_rotation_y(p.yaw);
    let local_up = yaw.inverse().transform_vector3(p.up).normalize_or_zero();
    let bank = if local_up == Vec3::ZERO { Mat4::IDENTITY } else { Mat4::from_quat(Quat::from_rotation_arc(Vec3::Y, local_up)) };
    let root = Mat4::from_translation(p.pos) * yaw * Mat4::from_rotation_x(-p.slope.clamp(-1.0, 1.0).asin()) * bank;
    let mut out = Vec::new();
    let mut push = |mesh, model, colour| {
        out.push(OstrichPart {
            mesh,
            model: root * model,
            tint0: colour,
            tint1: colour,
        })
    };
    let edge = p.side * p.width * 0.5;
    let width = p.width * p.extension;
    let centre = edge - p.side * width * 0.5;
    if width > 0.01 {
        push(
            MeshId::Block,
            tr(Vec3::new(centre, 0.055, 0.0), Vec3::new(width, 0.08, 3.4)),
            [0.55, 0.08, 0.25],
        );
        for z in [-1.45, 1.45] {
            push(
                MeshId::Block,
                tr(Vec3::new(centre, 0.105, z), Vec3::new(width, 0.025, 0.18)),
                [0.98, 0.7, 0.16],
            );
        }
        for k in 0..7 {
            let x = edge - p.side * width * (k as f32 + 0.5) / 7.0;
            push(
                MeshId::Block,
                tr(
                    Vec3::new(x, 0.11, 0.0),
                    Vec3::new((width / 14.0).min(0.5), 0.025, 1.4),
                ),
                [0.96, 0.52, 0.12],
            );
        }
    }
    push(
        MeshId::SmallBall,
        tr(
            Vec3::new(edge - p.side * width, 0.22, 0.0),
            Vec3::new(0.28, 0.28, 1.75),
        ),
        [0.65, 0.12, 0.3],
    );
    let x = edge + p.side * 1.2;
    let tug = if p.pulling {
        (time * 25.0).sin() * 0.15 + 0.3
    } else {
        0.0
    };
    push(
        MeshId::Jersey,
        tr(Vec3::new(x + p.side * tug, 0.95, 0.0), Vec3::ONE),
        [0.2, 0.45, 0.6],
    );
    push(
        MeshId::JockeyHead,
        Mat4::from_translation(Vec3::new(x + p.side * tug, 1.52, 0.0)),
        [0.7, 0.44, 0.28],
    );
    push(
        MeshId::JockeyHelmet,
        Mat4::from_translation(Vec3::new(x + p.side * tug, 1.52, 0.0)),
        [0.85, 0.12, 0.19],
    );
    for z in [-0.18, 0.18] {
        push(
            MeshId::SmallBall,
            limb_ball(
                Vec3::new(x, 0.65, z),
                Vec3::new(x + p.side * 0.1, 0.08, z * 1.5),
                0.13,
            ),
            [0.17, 0.15, 0.22],
        );
        push(
            MeshId::SmallBall,
            limb_ball(Vec3::new(x, 1.12, z), Vec3::new(edge, 0.4 + tug, z), 0.09),
            [0.7, 0.44, 0.28],
        );
    }
    out
}

pub(crate) fn frog(p: &FrogPose, time: f32) -> Vec<OstrichPart> {
    let down = if p.tumble > 0.0 {
        (p.tumble * PI).sin().abs().min(1.0)
    } else {
        0.0
    };
    let root = Mat4::from_translation(p.pos + Vec3::Y * 0.35)
        * Mat4::from_rotation_y(p.yaw)
        * Mat4::from_rotation_z(if p.tumble > 0.0 {
            down * 1.5 + (p.tumble * 4.0).min(1.0) * TAU
        } else {
            0.0
        });
    let laugh = if p.tumble > 0.25 && p.tumble < 0.78 {
        (time * 20.0).sin() * 0.04
    } else {
        0.0
    };
    let mut out = Vec::new();
    let mut ball = |pos, scale, color| {
        out.push(OstrichPart {
            mesh: MeshId::SkinBall,
            model: root * tr(pos, scale),
            tint0: color,
            tint1: color,
        })
    };
    let green = [0.29, 0.65, 0.25];
    ball(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.43, 0.32, 0.38),
        [0.16, 0.34, 0.63],
    );
    ball(
        Vec3::new(0.0, 0.29 + laugh, 0.05),
        Vec3::new(0.50, 0.3, 0.35),
        green,
    );
    ball(
        Vec3::new(0.0, 0.19 + laugh, 0.33),
        Vec3::new(0.35, 0.07, 0.09),
        [0.60, 0.26, 0.13],
    );
    ball(
        Vec3::new(0.0, 0.19 + laugh, 0.4),
        Vec3::new(0.27, if down > 0.1 { 0.035 } else { 0.015 }, 0.015),
        [0.12, 0.08, 0.05],
    );
    ball(Vec3::new(0.0, 0.22 + laugh, 0.22), Vec3::new(0.42, 0.15, 0.20), [0.52, 0.74, 0.30]);
    for side in [-1.0, 1.0] {
        ball(
            Vec3::new(side * 0.23, 0.51, 0.14),
            Vec3::new(0.22, 0.21, 0.18),
            green,
        );
        ball(
            Vec3::new(side * 0.23, 0.52, 0.29),
            Vec3::new(0.15, 0.13, 0.07),
            [1.0, 0.98, 0.88],
        );
        ball(
            Vec3::new(side * 0.23, 0.51, 0.35),
            Vec3::new(0.05, 0.08, 0.035),
            [0.03; 3],
        );
        ball(
            Vec3::new(side * 0.48, -0.12, 0.0),
            Vec3::new(0.27, 0.15, 0.28),
            green,
        );
        ball(
            Vec3::new(side * 0.47, 0.14, 0.3),
            Vec3::new(0.12, 0.14, 0.13),
            green,
        );
    }
    for side in [-1.0, 1.0] {
        out.push(OstrichPart { mesh: MeshId::FrogFoot,
            model: root * Mat4::from_translation(Vec3::new(side * 0.49, -0.24, 0.24)) * Mat4::from_rotation_y(side * 0.35),
            tint0: green, tint1: green });
    }
    out
}

/// Strides a second of the rose seller's cartoon run.
pub const ROSE_CADENCE: f32 = 4.5;
/// The seller is drawn a little larger than life, to stand up beside an ostrich.
const ROSE_SCALE: f32 = 1.15;

/// Turban cloth of the rose seller.
const TURBAN: Rgb = [0.96, 0.52, 0.10];

/// The rose seller: a cheerful hawker in a turban and a waistcoat, a basket on
/// one arm and one huge red rose held up in the other, legs a blur.
pub(crate) fn rose_seller(p: &RosePose, time: f32) -> Vec<OstrichPart> {
    let phase = time * ROSE_CADENCE;
    let bob = (phase * TAU).sin().abs() * 0.06;
    // Gone with a pop at the end of the run off the road.
    let size = (1.0 - smoothstep((p.leave - 0.7) / 0.3)) * ROSE_SCALE;
    let root = Mat4::from_translation(p.pos)
        * Mat4::from_rotation_y(p.yaw)
        * Mat4::from_scale(Vec3::splat(size.max(0.001)));
    let hip_y = 0.86;
    // Leaning into the run, about the hips.
    let body = root * Mat4::from_translation(Vec3::new(0.0, hip_y + bob, 0.0)) * Mat4::from_rotation_x(0.22) * Mat4::from_translation(Vec3::new(0.0, -hip_y, 0.0));
    let mut out = Vec::new();
    let mut part = |mesh: MeshId, model: Mat4, c: Rgb| out.push(OstrichPart { mesh, model, tint0: c, tint1: c });
    let skin = [0.62, 0.42, 0.28];
    let shirt = [0.97, 0.96, 0.92];
    let waistcoat = [0.18, 0.42, 0.30];
    let trousers = [0.30, 0.26, 0.24];
    let gold = [1.0, 0.78, 0.25];
    let ball = MeshId::SkinBall;

    // Legs, a whirl: the real stride and two fainter ghosts trailing it.
    let hip_root = root * Mat4::from_translation(Vec3::Y * bob);
    for side in [-1.0f32, 1.0] {
        let hip = Vec3::new(side * 0.11, hip_y, 0.0);
        for (ghost, (lag, thin, fade)) in [(0.0, 1.0, 1.0), (0.09, 0.75, 1.25), (0.18, 0.5, 1.5)].into_iter().enumerate() {
            let f = (phase - lag + if side > 0.0 { 0.0 } else { 0.5 }) * TAU;
            let foot = Vec3::new(side * 0.12, 0.06 + f.cos().max(0.0) * 0.3, f.sin() * 0.46);
            let knee = (hip + foot) * 0.5 + Vec3::new(0.0, 0.04, 0.16 + f.cos().max(0.0) * 0.1);
            let c = trousers.map(|v: f32| (v * fade).min(1.0));
            part(ball, hip_root * limb_ball(hip, knee, 0.075 * thin), c);
            part(ball, hip_root * limb_ball(knee, foot, 0.06 * thin), c);
            if ghost == 0 {
                part(ball, hip_root * tr(foot + Vec3::new(0.0, -0.02, 0.05), Vec3::new(0.065, 0.05, 0.12)), [0.12, 0.09, 0.08]);
            }
        }
    }

    // Body: shirt, waistcoat with gold buttons, a red bow tie.
    part(ball, body * tr(Vec3::new(0.0, 1.14, 0.0), Vec3::new(0.2, 0.3, 0.15)), shirt);
    part(ball, body * tr(Vec3::new(0.0, 1.09, -0.004), Vec3::new(0.214, 0.26, 0.162)), waistcoat);
    part(ball, body * tr(Vec3::new(0.0, 1.27, 0.125), Vec3::new(0.07, 0.09, 0.04)), shirt);
    for k in 0..3 {
        part(MeshId::SmallBall, body * tr(Vec3::new(0.0, 1.0 + k as f32 * 0.075, 0.158), Vec3::splat(0.017)), gold);
    }
    for s in [-1.0f32, 1.0] {
        part(ball, body * tr(Vec3::new(s * 0.035, 1.36, 0.13), Vec3::new(0.035, 0.022, 0.018)), [0.85, 0.1, 0.12]);
    }
    part(ball, body * tr(Vec3::new(0.0, 1.43, 0.0), Vec3::new(0.06, 0.07, 0.06)), skin);

    // Head, cheerful: warm cheeks, a moustache and a grin.
    let head = Vec3::new(0.0, 1.57, 0.02);
    part(ball, body * tr(head, Vec3::new(0.15, 0.165, 0.15)), skin);
    part(ball, body * tr(head + Vec3::new(0.0, -0.01, 0.15), Vec3::splat(0.034)), [0.56, 0.37, 0.24]);
    for s in [-1.0f32, 1.0] {
        part(MeshId::SmallBall, body * tr(head + Vec3::new(s * 0.055, 0.035, 0.13), Vec3::splat(0.021)), [0.06, 0.05, 0.05]);
        part(ball, body * tr(head + Vec3::new(s * 0.085, -0.035, 0.11), Vec3::new(0.032, 0.022, 0.015)), [0.74, 0.40, 0.32]);
    }
    part(ball, body * tr(head + Vec3::new(0.0, -0.075, 0.13), Vec3::new(0.055, 0.016, 0.02)), [0.55, 0.12, 0.14]);
    for s in [-1.0f32, 1.0] {
        part(ball, body * Mat4::from_translation(head + Vec3::new(s * 0.04, -0.045, 0.142)) * Mat4::from_rotation_z(s * 0.3) * Mat4::from_scale(Vec3::new(0.048, 0.017, 0.022)), [0.10, 0.07, 0.06]);
    }
    // Turban: a wrapped band round the head, a dome over it, two folds crossing
    // at the front and a jewel where they meet.
    part(ball, body * tr(head + Vec3::new(0.0, 0.085, -0.005), Vec3::new(0.172, 0.095, 0.17)), TURBAN);
    part(ball, body * tr(head + Vec3::new(0.0, 0.16, -0.02), Vec3::new(0.15, 0.11, 0.152)), TURBAN.map(|v| v * 0.92));
    for s in [-1.0f32, 1.0] {
        part(ball, body * Mat4::from_translation(head + Vec3::new(s * 0.05, 0.13, 0.11)) * Mat4::from_rotation_z(s * 0.55) * Mat4::from_scale(Vec3::new(0.11, 0.045, 0.06)), TURBAN.map(|v| v * 1.04));
    }
    part(MeshId::SmallBall, body * tr(head + Vec3::new(0.0, 0.115, 0.165), Vec3::splat(0.026)), gold);

    // The rose arm, on the customer's side: held high while running, offered
    // out when selling, with a little shake.
    let s = if p.toward < 0.0 { -1.0 } else { 1.0 };
    let shoulder = Vec3::new(s * 0.22, 1.33, 0.0);
    let shake = (time * 9.0).sin() * 0.04;
    let hand = if p.selling {
        // Up at the jockey's face, over the ostrich's back.
        Vec3::new(s * 0.5, 2.05 + shake, 0.3)
    } else {
        Vec3::new(s * 0.26, 1.95 + shake, 0.12)
    };
    let elbow = (shoulder + hand) * 0.5 + Vec3::new(s * 0.08, -0.08, -0.04);
    part(ball, body * limb_ball(shoulder, elbow, 0.06), shirt);
    part(ball, body * limb_ball(elbow, hand, 0.05), shirt);
    part(ball, body * tr(hand, Vec3::splat(0.055)), skin);
    let tip = hand + Vec3::new(s * 0.06, 0.38, 0.06);
    part(ball, body * limb_ball(hand - Vec3::Y * 0.05, tip, 0.014), [0.16, 0.48, 0.17]);
    part(ball, body * tr((hand + tip) * 0.5 + Vec3::new(s * 0.05, 0.0, 0.0), Vec3::new(0.06, 0.02, 0.035)), [0.18, 0.55, 0.2]);
    part(MeshId::RoseBloom, body * Mat4::from_translation(tip + Vec3::Y * 0.14) * Mat4::from_rotation_z(-s * 0.25) * Mat4::from_scale(Vec3::splat(1.4)), [1.0; 3]);

    // The basket arm swings with the run.
    let swing = (phase * TAU).sin() * 0.12;
    let shoulder = Vec3::new(-s * 0.22, 1.33, 0.0);
    let hand = Vec3::new(-s * 0.3, 0.98, 0.06 + swing);
    let elbow = (shoulder + hand) * 0.5 + Vec3::new(-s * 0.06, 0.0, -0.06);
    part(ball, body * limb_ball(shoulder, elbow, 0.06), shirt);
    part(ball, body * limb_ball(elbow, hand, 0.05), shirt);
    part(ball, body * tr(hand, Vec3::splat(0.05)), skin);
    // The basket hangs from the hand, steadied rather than leaning with the body.
    let at = body.transform_point3(hand);
    part(MeshId::Basket, Mat4::from_translation(at) * Mat4::from_rotation_y(p.yaw) * Mat4::from_scale(Vec3::splat(size.max(0.001))), [1.0; 3]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rose_seller_offers_the_rose_on_the_customers_side_and_vanishes_at_the_end() {
        let pose = |toward, selling, leave| RosePose { pos: Vec3::ZERO, yaw: 0.0, toward, selling, leave };
        let rose_x = |p: &RosePose| {
            rose_seller(p, 0.3)
                .iter()
                .find(|q| q.mesh == MeshId::RoseBloom)
                .map(|q| q.model.w_axis.x)
                .unwrap()
        };
        assert!(rose_x(&pose(1.0, true, 0.0)) > 0.4);
        assert!(rose_x(&pose(-1.0, true, 0.0)) < -0.4);
        assert!(rose_x(&pose(1.0, false, 0.0)).abs() < rose_x(&pose(1.0, true, 0.0)));
        // A turban, and gone by the end of the run off the road.
        let parts = rose_seller(&pose(1.0, false, 0.0), 0.3);
        assert!(parts.iter().filter(|q| q.tint0 == TURBAN).count() >= 1);
        let gone = rose_seller(&pose(1.0, false, 1.0), 0.3);
        assert!(gone.iter().all(|q| q.model.x_axis.length() < 0.02));
        // The legs whirl: the feet are somewhere else a moment later.
        let a = rose_seller(&pose(1.0, false, 0.0), 0.0);
        let b = rose_seller(&pose(1.0, false, 0.0), 0.05);
        assert!(a.iter().zip(&b).any(|(x, y)| (x.model.w_axis - y.model.w_axis).length() > 0.1));
    }


    #[test]
    fn every_knock_style_moves_the_ostrich_and_the_jockey_differently() {
        for age in [0.1, 0.2, 0.3, 0.4, 0.6, 0.9] {
            for (i, a) in KnockStyle::ALL.iter().enumerate() {
                let fa = knock_fx(*a, age, 1.0);
                assert_ne!(fa.ostrich, Mat4::IDENTITY, "{a:?} at {age} s leaves the ostrich alone");
                for b in &KnockStyle::ALL[i + 1..] {
                    let fb = knock_fx(*b, age, 1.0);
                    assert_ne!(fa.ostrich, fb.ostrich, "{a:?} and {b:?} react alike at {age} s");
                    assert!(
                        fa.lift != fb.lift || fa.tumble != fb.tumble || fa.travel != fb.travel,
                        "{a:?} and {b:?} throw the jockey alike at {age} s"
                    );
                }
            }
        }
        // The jockey's launch alone tells them apart mid-flight.
        let launches: Vec<_> = KnockStyle::ALL
            .iter()
            .map(|s| {
                let f = knock_fx(*s, 0.2, 1.0);
                (f.lift, f.tumble.to_array())
            })
            .collect();
        for (i, a) in launches.iter().enumerate() {
            for b in &launches[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn a_style_with_no_knock_changes_nothing_and_the_throw_lands_and_the_ostrich_recovers() {
        let none = knock_fx(KnockStyle::None, 0.2, 1.0);
        assert_eq!(none.ostrich, Mat4::IDENTITY);
        assert_eq!((none.travel, none.lift, none.tumble), (1.0, 0.0, Vec3::ZERO));
        for style in [KnockStyle::Printer, KnockStyle::Rug, KnockStyle::Pepe] {
            let landed = knock_fx(style, THROW_SECS, 1.0);
            assert!((landed.travel - 1.0).abs() < 1e-5, "{style:?} lands where the sim put the jockey");
            assert!(landed.lift.abs() < 1e-4, "{style:?} lands on the ground");
        }
        for style in KnockStyle::ALL {
            let done = knock_fx(style, 3.0, 1.0).ostrich;
            assert!(
                done.abs_diff_eq(Mat4::IDENTITY, 1e-3),
                "{style:?} leaves the ostrich bent out of shape"
            );
        }
    }

    #[test]
    fn the_rug_lies_on_a_banked_sloping_road() {
        let (yaw, bank, slope) = (0.7f32, 0.12f32, 0.08f32);
        let right = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
        let up = Vec3::Y * bank.cos() + right * bank.sin();
        let p = RugPose { pos: Vec3::new(5.0, 2.0, -3.0), yaw, up, slope, width: 14.0, extension: 1.0, pulling: false, side: 1.0 };
        let carpet = &rug(&p, 0.0)[0];
        // Both ends of the carpet's width sit just above the road plane through `pos`.
        for x in [-6.5f32, 6.5] {
            let w = carpet.model.transform_point3(Vec3::new(x, 0.0, 0.0));
            let (dx, dz) = (w.x - p.pos.x, w.z - p.pos.z);
            let plane = p.pos.y - (up.x * dx + up.z * dz) / up.y;
            assert!((w.y - plane).abs() < 0.3, "x={x}: carpet {} vs road {plane}", w.y);
            assert!(w.y > plane - 0.2, "x={x} sunk under the road");
        }
    }

    #[test]
    fn pepe_bowls_the_ostrich_the_way_the_jockey_flies() {
        let left = knock_fx(KnockStyle::Pepe, 0.4, -1.0);
        let right = knock_fx(KnockStyle::Pepe, 0.4, 1.0);
        assert_ne!(left.ostrich, right.ostrich);
        assert_eq!(left.tumble.z, -right.tumble.z);
    }

    #[test]
    fn thrown_jockeys_and_reacting_ostriches_stay_finite() {
        for style in KnockStyle::ALL {
            for k in 0..40 {
                let pose = OstrichPose {
                    knock: style,
                    knock_age: k as f32 * 0.05,
                    dismounted: style != KnockStyle::Trump,
                    chase_offset: Vec3::new(2.0, 0.0, -6.0),
                    chase_age: k as f32 * 0.05,
                    ..Default::default()
                };
                assert!(ostrich_knock(&pose).is_finite());
                for part in jockey(&pose, k as f32 * 0.1, Mat4::IDENTITY) {
                    assert!(part.model.is_finite(), "{style:?} at step {k}");
                }
            }
        }
    }

    #[test]
    fn an_eating_jockey_holds_the_cake_out_beside_a_head_turned_side_on() {
        let parts = |cake: bool| {
            let pose = OstrichPose { cake, ..OstrichPose::default() };
            jockey(&pose, 0.7, Mat4::IDENTITY)
        };
        let pos = |ps: &[OstrichPart], mesh| {
            ps.iter().find(|q| q.mesh == mesh).map(|q| q.model.w_axis.truncate())
        };
        assert!(pos(&parts(false), MeshId::CakeSlice).is_none(), "no cake unless eating");
        let eating = parts(true);
        let head = pos(&eating, MeshId::JockeyHead).unwrap();
        let cake = pos(&eating, MeshId::CakeSlice).unwrap();
        // The camera looks along +z from behind: the cake must clear the helmet sideways
        // (the head is about 0.2 m wide each side), not sit in front of it.
        assert!((cake.x - head.x).abs() > 0.3, "cake at {cake:?}, head {head:?}");
        // The face points sideways: the head's forward axis (+z) now lies mostly along x.
        let head_part = eating.iter().find(|q| q.mesh == MeshId::JockeyHead).unwrap();
        let face = head_part.model.transform_vector3(Vec3::Z).normalize();
        assert!(face.x.abs() > 0.9 && face.z.abs() < 0.45, "face {face:?}");
        // And it faces forward again when not eating.
        let plain = parts(false);
        let forward = plain.iter().find(|q| q.mesh == MeshId::JockeyHead).unwrap();
        assert!(forward.model.transform_vector3(Vec3::Z).normalize().z > 0.95);
    }
}
