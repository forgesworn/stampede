//! The Challenges HUD (SPEC 16): a score block at the top centre (score with
//! its label, the clock or target), a bar under it (medal ladder, egg count or
//! the elimination countdown) and floating Drift King points. Everything is
//! read off `World::challenge` every frame (`build`); nothing animates per
//! `ChallengeScore` event.

use super::hud::shadow_text;
use super::icons::stroke;
use crate::ui::screens::challenges::{fmt_threshold, medal_colors};
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Painter, Rect};
use race_core::{ChallengeId, ChallengeState, ELIMINATION_INTERVAL};

/// The bar under the score block.
#[derive(Clone, Debug)]
pub struct Bar {
    /// 0..1.
    pub frac: f32,
    /// Medal marks along the bar: (position 0..1, level 1-3).
    pub ticks: Vec<(f32, u8)>,
    pub text: String,
    pub color: Color32,
}

/// A floating "+4.2" (Drift King).
#[derive(Clone, Debug)]
pub struct Pop {
    pub text: String,
    /// Seconds since it appeared.
    pub age: f32,
    pub color: Color32,
    pub size: f32,
    /// 0..2: which column it floats up in, so pops that arrive together do not overlap.
    pub slot: u8,
}

pub const POP_SECS: f32 = 1.3;

#[derive(Clone, Debug)]
pub struct ChallengeHud {
    pub id: ChallengeId,
    pub label: &'static str,
    pub value: String,
    pub value_color: Color32,
    /// 1 when the score just changed, falling to 0.
    pub pulse: f32,
    pub right_label: &'static str,
    pub right_value: String,
    /// The right value is running out (flash red).
    pub right_warn: bool,
    pub bar: Option<Bar>,
    pub pops: Vec<Pop>,
    /// The race HUD's lap counter stays (Drift King, Turbo Time Attack only).
    pub show_lap: bool,
    /// Medal level the score is at right now (live for the higher-is-better ones).
    pub medal: u8,
    /// 1 right after a Laser Sniper miss (the penalty), falling to 0: the value flashes red.
    pub flash: f32,
}

/// `1:28`, `42`, `7.3`: a countdown that gets finer as it runs out.
pub fn fmt_left(s: f32) -> String {
    let s = s.max(0.0);
    if s >= 60.0 {
        format!("{}:{:02}", (s / 60.0).floor() as u32, (s % 60.0).floor() as u32)
    } else if s >= 10.0 {
        format!("{}", s.ceil() as u32)
    } else {
        format!("{s:.1}")
    }
}

fn medal_word(level: u8) -> &'static str {
    match level {
        1 => "BRONZE",
        2 => "SILVER",
        _ => "GOLD",
    }
}

/// Score text as the panel shows it.
pub fn score_text(id: ChallengeId, score: f32) -> String {
    match id {
        ChallengeId::EggHunt | ChallengeId::TurboTimeAttack | ChallengeId::LaserSniper => super::hud::fmt_time(score),
        ChallengeId::Elimination | ChallengeId::LastFeather => {
            let (n, s) = super::hud::ordinal(score.round().clamp(1.0, 8.0) as u8);
            format!("{n}{s}")
        }
        ChallengeId::DriftKing => format!("{score:.1}"),
        _ => format!("{}", score.floor() as i64),
    }
}

/// Everything the panel shows, from the sim's challenge state.
pub fn build(cs: &ChallengeState, racers: usize) -> ChallengeHud {
    let id = cs.id;
    let th = id.medal_thresholds();
    let gold = th[2];
    let value_color = if cs.medal == 3 && id.higher_is_better() { theme::GOLD } else { Color32::WHITE };
    let mut h = ChallengeHud {
        id,
        label: "",
        value: score_text(id, cs.score),
        value_color,
        pulse: 0.0,
        right_label: "",
        right_value: String::new(),
        right_warn: false,
        bar: None,
        pops: Vec::new(),
        show_lap: id.finishing(),
        medal: if id.higher_is_better() { cs.medal } else { 0 },
        flash: 0.0,
    };
    // The medal ladder bar for the "more is better" challenges.
    let ladder = |extra: &str| {
        let next = (1..=3u8).find(|&l| cs.score < th[l as usize - 1]);
        let text = match next {
            Some(l) => format!("NEXT: {} {}", medal_word(l), fmt_threshold(id, th[l as usize - 1])),
            None => "GOLD!".to_string(),
        };
        let text = if extra.is_empty() { text } else { format!("{text}   {extra}") };
        let level = next.map(|l| l - 1).unwrap_or(3).max(1);
        Bar { frac: (cs.score / gold).clamp(0.0, 1.0), ticks: (1..=3u8).map(|l| (th[l as usize - 1] / gold, l)).collect(), text, color: medal_colors(level).0 }
    };
    match id {
        ChallengeId::CoinRush => {
            h.label = "BITCOINS";
            h.right_label = "TIME LEFT";
            h.right_value = fmt_left(cs.time_left);
            h.right_warn = cs.time_left < 10.0 && !cs.done;
            h.bar = Some(ladder(""));
        }
        ChallengeId::BashBrawl => {
            h.label = "BASHES";
            h.right_label = "TIME LEFT";
            h.right_value = fmt_left(cs.time_left);
            h.right_warn = cs.time_left < 10.0 && !cs.done;
            h.bar = Some(ladder(""));
        }
        ChallengeId::DriftKing => {
            h.label = "DRIFT POINTS";
            h.right_label = "GOLD";
            h.right_value = fmt_threshold(id, gold);
            h.bar = Some(ladder(""));
        }
        ChallengeId::EggHunt => {
            h.label = "TIME";
            h.right_label = "GOLD";
            h.right_value = fmt_threshold(id, gold);
            h.bar = Some(Bar { frac: cs.progress as f32 / cs.goal.max(1) as f32, ticks: Vec::new(), text: format!("EGGS {} / {}", cs.progress, cs.goal), color: theme::ACCENT });
        }
        ChallengeId::TurboTimeAttack => {
            h.label = "TIME";
            h.right_label = "GOLD";
            h.right_value = fmt_threshold(id, gold);
        }
        ChallengeId::NightCoinRush => {
            h.label = "BITCOINS";
            h.right_label = "TIME LEFT";
            h.right_value = fmt_left(cs.time_left);
            h.right_warn = cs.time_left < 10.0 && !cs.done;
            h.bar = Some(ladder(""));
        }
        ChallengeId::RampRider => {
            h.label = "TRICK POINTS";
            h.right_label = "TRICKS";
            h.right_value = format!("{}", cs.progress);
            h.bar = Some(ladder(&format!("CHAINS {}", cs.misses)));
        }
        ChallengeId::LaserSniper => {
            h.label = "TIME";
            h.right_label = "GOLD";
            h.right_value = fmt_threshold(id, gold);
            let pen = if cs.misses > 0 { format!("   MISSES {} (+{:.0}s)", cs.misses, cs.penalty) } else { String::new() };
            h.bar = Some(Bar { frac: cs.progress as f32 / cs.goal.max(1) as f32, ticks: Vec::new(), text: format!("EGGS {} / {}{pen}", cs.progress, cs.goal), color: theme::ACCENT });
        }
        ChallengeId::LastFeather => {
            h.label = "PLACE";
            h.right_label = "TIME LEFT";
            h.right_value = fmt_left(cs.time_left);
            h.right_warn = cs.time_left < 10.0 && !cs.done;
            h.bar = Some(Bar { frac: cs.progress as f32 / cs.goal.max(1) as f32, ticks: Vec::new(), text: format!("RIVALS OUT {} / {}", cs.progress, cs.goal), color: Color32::from_rgb(255, 170, 70) });
        }
        ChallengeId::Elimination => {
            h.label = "PLACE";
            h.right_label = "NEXT OUT IN";
            h.right_value = if cs.done { "-".into() } else { fmt_left(cs.time_left) };
            h.right_warn = cs.time_left < 6.0 && !cs.done;
            let left = racers.saturating_sub(cs.eliminated.len());
            h.bar = Some(Bar { frac: (cs.time_left / ELIMINATION_INTERVAL).clamp(0.0, 1.0), ticks: Vec::new(), text: format!("{left} LEFT   OUT {} / {}", cs.progress, cs.goal), color: if h.right_warn { Color32::from_rgb(255, 90, 80) } else { Color32::from_rgb(255, 170, 70) } });
        }
    }
    h
}

/// Draw the score block, its bar and the pops. `mirror`: the rear-view mirror
/// owns the top centre, so only the pops show.
pub fn draw(p: &Painter, safe: Rect, h: &ChallengeHud, mirror: bool, t: f32) {
    let w = 238.0;
    let panel = Rect::from_center_size(pos2(safe.center().x, safe.top() + 8.0 + 31.0), vec2(w, 62.0));
    if !mirror {
        p.rect_filled(panel.translate(vec2(0.0, 2.0)), 16.0, Color32::from_black_alpha(70));
        p.rect_filled(panel, 16.0, Color32::from_rgba_unmultiplied(14, 8, 28, 178));
        let rim = if h.medal > 0 { medal_colors(h.medal).0 } else { theme::ACCENT.gamma_multiply(0.75) };
        p.rect_stroke(panel, 16.0, stroke(2.0, rim), egui::StrokeKind::Inside);
        // score (left)
        let lc = pos2(panel.left() + w * 0.30, panel.center().y);
        p.text(lc + vec2(0.0, -21.0), Align2::CENTER_CENTER, h.label, theme::ui_bold(9.5), theme::SAND);
        let k = 1.0 + 0.30 * h.pulse * h.pulse;
        // long values (a 0:22.00 clock) shrink to fit the left half
        let fit = |text: &str, size: f32, max_w: f32| {
            let w = p.layout_no_wrap(text.to_string(), theme::numbers(size), Color32::WHITE).size().x;
            if w > max_w { size * max_w / w } else { size }
        };
        let vs = fit(&h.value, 33.0, w * 0.50);
        let vcol = if h.flash > 0.0 { theme::mix(h.value_color, Color32::from_rgb(255, 80, 70), h.flash.clamp(0.0, 1.0)) } else { h.value_color };
        shadow_text(p, lc + vec2(0.0, 8.0), Align2::CENTER_CENTER, &h.value, theme::numbers(vs * k), vcol);
        // divider
        p.vline(panel.left() + w * 0.58, (panel.top() + 10.0)..=(panel.bottom() - 10.0), stroke(1.0, Color32::from_white_alpha(40)));
        // clock or target (right)
        let rc = pos2(panel.left() + w * 0.79, panel.center().y);
        p.text(rc + vec2(0.0, -21.0), Align2::CENTER_CENTER, h.right_label, theme::ui_bold(9.5), theme::SAND);
        let warn = h.right_warn;
        let col = if warn { Color32::from_rgb(255, 96, 84).gamma_multiply(0.75 + 0.25 * (t * 10.0).sin().abs()) } else { Color32::from_rgb(255, 232, 170) };
        let col = if h.right_label == "GOLD" { medal_colors(3).0 } else { col };
        shadow_text(p, rc + vec2(0.0, 8.0), Align2::CENTER_CENTER, &h.right_value, theme::numbers(fit(&h.right_value, 25.0, w * 0.36)), col);
        // the bar
        if let Some(b) = &h.bar {
            let bar = Rect::from_center_size(pos2(panel.center().x, panel.bottom() + 12.0), vec2(w - 24.0, 14.0));
            p.rect_filled(bar.expand(2.0), 9.0, Color32::from_black_alpha(140));
            p.rect_filled(bar, 7.0, Color32::from_rgba_unmultiplied(40, 24, 66, 200));
            let f = b.frac.clamp(0.0, 1.0);
            if f > 0.0 {
                p.rect_filled(Rect::from_min_size(bar.min, vec2((bar.width() * f).max(8.0), bar.height())), 7.0, b.color);
            }
            for (pos, lvl) in &b.ticks {
                let x = bar.left() + bar.width() * pos.clamp(0.0, 1.0);
                let reached = f + 0.001 >= *pos;
                p.circle_filled(pos2(x.min(bar.right() - 3.0), bar.center().y), 3.6, Color32::from_black_alpha(160));
                p.circle_filled(pos2(x.min(bar.right() - 3.0), bar.center().y), 2.6, if reached { medal_colors(*lvl).0 } else { medal_colors(*lvl).1.gamma_multiply(0.8) });
            }
            shadow_text(p, bar.center(), Align2::CENTER_CENTER, &b.text, theme::ui_bold(9.0), Color32::WHITE);
        }
    }
    // floating drift points, rising out of the middle
    for (i, pop) in h.pops.iter().enumerate() {
        let a = pop.age / POP_SECS;
        if !(0.0..1.0).contains(&a) {
            continue;
        }
        let rise = 1.0 - (1.0 - a) * (1.0 - a);
        let _ = i;
        let pos = pos2(safe.center().x + 62.0 + pop.slot as f32 * 58.0, safe.top() + safe.height() * 0.56 - rise * 58.0 - (pop.slot % 2) as f32 * 14.0);
        let pop_in = race_render::ease_out_elastic((pop.age / 0.3).clamp(0.0, 1.0));
        let fade = (1.0 - ((a - 0.65) / 0.35).clamp(0.0, 1.0)).clamp(0.0, 1.0);
        shadow_text(p, pos, Align2::CENTER_CENTER, &pop.text, theme::title(pop.size * (0.5 + 0.5 * pop_in)), pop.color.gamma_multiply(fade));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_content_per_challenge() {
        let mut cs = ChallengeState::new(ChallengeId::CoinRush);
        cs.score = 104.0;
        cs.time_left = 31.2;
        cs.medal = 2;
        let h = build(&cs, 1);
        assert_eq!((h.label, h.value.as_str(), h.right_value.as_str()), ("BITCOINS", "104", "32"));
        assert_eq!(h.bar.as_ref().unwrap().text, "NEXT: GOLD 130");
        assert!(!h.show_lap && h.medal == 2);

        let mut e = ChallengeState::new(ChallengeId::Elimination);
        e.score = 3.0;
        e.time_left = 4.25;
        e.eliminated = vec![5, 2];
        e.progress = 2;
        let h = build(&e, 8);
        assert_eq!((h.value.as_str(), h.right_value.as_str()), ("3rd", "4.2"));
        assert!(h.right_warn);
        assert_eq!(h.bar.as_ref().unwrap().text, "6 LEFT   OUT 2 / 7");

        assert!(build(&ChallengeState::new(ChallengeId::DriftKing), 1).show_lap);
        assert!(build(&ChallengeState::new(ChallengeId::TurboTimeAttack), 1).show_lap);
        for id in ChallengeId::ALL {
            let h = build(&ChallengeState::new(id), 8);
            assert!(!h.label.is_empty() && !h.right_label.is_empty(), "{id:?}");
        }
        assert_eq!(fmt_left(88.0), "1:28");
        assert_eq!(fmt_left(9.96), "10.0");
    }
}
