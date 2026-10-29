//! Following racers while a finished online player waits for the results.

use super::source::RaceView;
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Rect};
use race_core::RacePhase;

pub fn candidates(v: &RaceView<'_>, you: usize, waiting: bool) -> Vec<usize> {
    if !waiting || v.phase != RacePhase::Racing || v.battle.is_some()
        || !v.racers.get(you).is_some_and(|r| r.finished_at.is_some())
    {
        return Vec::new();
    }
    let mut seats: Vec<_> = v.racers.iter().enumerate()
        .filter(|(i, r)| *i != you && r.finished_at.is_none())
        .map(|(i, _)| i).collect();
    seats.sort_by_key(|&i| (v.racers[i].place, i));
    seats
}

pub fn cycle(seats: &[usize], current: usize, step: isize) -> Option<usize> {
    if seats.is_empty() { return None; }
    let i = seats.iter().position(|&seat| seat == current).unwrap_or(0);
    Some(seats[(i as isize + step).rem_euclid(seats.len() as isize) as usize])
}

pub fn target(seats: &[usize], current: usize) -> Option<usize> {
    seats.iter().copied().find(|&seat| seat == current).or_else(|| seats.first().copied())
}

pub fn ui(ui: &egui::Context, safe: Rect, v: &RaceView<'_>, seats: &[usize], watch: usize) -> Option<usize> {
    let racer = v.racers.get(watch)?;
    let name = &v.cfg.racers.get(watch)?.name;
    let panel = Rect::from_center_size(pos2(safe.center().x, safe.bottom() - 58.0), vec2(340.0_f32.min(safe.width() - 16.0), 82.0));
    let mut selected = None;
    egui::Area::new(egui::Id::new("race-follow")).order(egui::Order::Foreground).fixed_pos(panel.min).show(ui, |ui| {
        ui.set_min_size(panel.size());
        ui.painter().rect_filled(panel, 14.0, Color32::from_rgba_unmultiplied(26, 14, 46, 235));
        let centre = panel.shrink2(vec2(58.0, 4.0));
        let p = ui.painter().with_clip_rect(centre);
        p.text(pos2(centre.center().x, panel.top() + 27.0), Align2::CENTER_CENTER, format!("FOLLOWING {name}"), theme::ui_bold(14.0), theme::ACCENT);
        p.text(pos2(centre.center().x, panel.top() + 53.0), Align2::CENTER_CENTER,
            format!("{} racing · LAP {}/{} · {:.0} KM/H", seats.len(), racer.lap.max(1).min(v.cfg.laps), v.cfg.laps, racer.speed.max(0.0) * 3.6),
            theme::ui_medium(11.0), theme::TEXT);
        for (step, label, x) in [(-1, "<", panel.left() + 8.0), (1, ">", panel.right() - 52.0)] {
            let rect = Rect::from_min_size(pos2(x, panel.top() + 19.0), vec2(44.0, 44.0));
            let sense = if seats.len() > 1 { egui::Sense::click() } else { egui::Sense::hover() };
            let response = ui.interact(rect, egui::Id::new(("race-follow-step", step)), sense);
            let fill = if response.is_pointer_button_down_on() { theme::ACCENT } else { Color32::from_rgb(64, 38, 100) };
            ui.painter().rect_filled(rect, 10.0, fill);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, theme::numbers(22.0), if seats.len() > 1 { theme::TEXT } else { theme::TEXT_DIM });
            if response.clicked() { selected = cycle(seats, watch, step); }
        }
    });
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::source::{LocalRace, RaceSource};
    use race_core::{BotLevel, RaceConfig, TrackId};

    #[test]
    fn only_a_finished_online_player_can_follow_unfinished_racers() {
        let src = LocalRace::new(RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 5), Some(0));
        let mut racers = src.view().racers.to_vec();
        racers[1].place = 3;
        racers[2].place = 2;
        racers[3].finished_at = Some(50.0);
        let mut v = src.view();
        v.racers = &racers;
        v.phase = RacePhase::Racing;
        assert!(candidates(&v, 0, true).is_empty(), "still driving");
        drop(v);
        racers[0].finished_at = Some(49.0);
        let mut v = src.view();
        v.racers = &racers;
        v.phase = RacePhase::Racing;
        assert!(candidates(&v, 0, false).is_empty(), "offline or results received");
        assert_eq!(candidates(&v, 0, true), [2, 1]);
        assert!(candidates(&v, 99, true).is_empty());
        v.phase = RacePhase::Finished;
        assert!(candidates(&v, 0, true).is_empty(), "results take over");
    }

    #[test]
    fn following_a_chosen_racer_survives_position_changes_and_moves_on_when_they_finish() {
        let src = LocalRace::new(RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 5), Some(0));
        let mut racers = src.view().racers.to_vec();
        racers[0].finished_at = Some(50.0);
        let mut watch = 2;
        for finished in [false, true] {
            racers[1].place = 2;
            racers[2].place = 4;
            racers[3].place = 3;
            racers[2].finished_at = finished.then_some(60.0);
            let mut v = src.view();
            v.racers = &racers;
            v.phase = RacePhase::Racing;
            watch = target(&candidates(&v, 0, true), watch).unwrap();
            assert_eq!(watch, if finished { 1 } else { 2 });
        }
        assert_eq!(target(&[], watch), None);
    }

    #[test]
    fn previous_and_next_wrap_without_selecting_finished_or_local_seats() {
        let seats = [3, 1, 5];
        assert_eq!(cycle(&seats, 3, -1), Some(5));
        assert_eq!(cycle(&seats, 5, 1), Some(3));
        assert_eq!(cycle(&seats, 3, 1), Some(1));
        assert_eq!(cycle(&[4], 4, -1), Some(4));
        assert_eq!(cycle(&[], 0, 1), None);
    }
}
