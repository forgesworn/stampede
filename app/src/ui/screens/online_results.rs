//! After an online race: the results table, an everyone-ready rematch with a
//! cancellable 3-2-1, and (host) back to the lobby to change settings.

use super::common::{self, fmt_time, ordinal};
use super::lobby::{character_chip, LobbyScreen};
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect};
use race_core::{BattleState, Input};
use race_session::{ClientPhase, ResultRow, ResultsView};

pub struct OnlineResultsScreen {
    t: f32,
    /// A table to draw without a session (screenshots).
    sample: Option<(ResultsView, usize, Option<BattleState>)>,
    /// A join link arrived while here: ask before leaving.
    join_prompt: Option<common::LeaveAsk>,
    had_session: bool,
    /// The player chose to leave for another room: stop reading the link we just put back.
    leaving: bool,
    /// The ₿ paid for this race, once the results are in.
    payout: Option<crate::ui::progress_ui::Payout>,
    settled: bool,
    /// Holds back the card for a short wait for the backup.
    mig_delay: super::migration::BehindDelay,
}

impl OnlineResultsScreen {
    /// The results of the session in the app's slot.
    pub fn new() -> OnlineResultsScreen {
        OnlineResultsScreen { t: 0.0, sample: None, join_prompt: None, had_session: false, leaving: false, payout: None, settled: false, mig_delay: Default::default() }
    }

    /// A finished 8-racer race with you 2nd (screenshots).
    pub fn sample() -> OnlineResultsScreen {
        let names = [("Mo", 5u8, false), ("Rider", 2, false), ("Blaze", 1, true), ("Nova", 3, true), ("Boomer", 4, true), ("Ziggy", 6, true), ("Hodl", 7, true), ("Pip", 0, true)];
        let rows = names
            .iter()
            .enumerate()
            .map(|(i, (n, c, bot))| ResultRow { racer: i as u8, name: n.to_string(), character: *c, place: i as u8 + 1, bot: *bot, time: Some(83.4 + i as f32 * 2.31), best_lap: Some(27.1 + i as f32 * 0.6), coins: 9 - i as u32, points: 0 })
            .collect();
        OnlineResultsScreen { t: 0.0, join_prompt: None, had_session: false, leaving: false, payout: None, settled: false, mig_delay: Default::default(), sample: Some((ResultsView { track: race_core::TrackId::Dune, laps: 3, rows }, 1, None)) }
    }

    /// A finished online battle on Mempool Plaza with eight racers, you 2nd (screenshots).
    pub fn battle_sample() -> OnlineResultsScreen {
        let names = [("Mo", 5u8, false), ("Rider", 2, false), ("Blaze", 1, true), ("Nova", 3, true), ("Boomer", 4, true), ("Ziggy", 6, true), ("Hodl", 7, true), ("Pip", 0, true)];
        let mut b = BattleState::new(race_core::BattleRules::default(), 8);
        b.feathers = vec![2, 1, 0, 1, 0, 0, 0, 0];
        b.hits = vec![5, 4, 1, 2, 2, 1, 0, 0];
        b.taken = vec![1, 2, 3, 2, 3, 3, 3, 3];
        b.out_order = vec![7, 6, 5, 4, 2];
        b.done = true;
        b.time_left = 0.0;
        let rank = b.ranking();
        let rows = rank
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                let (n, c, bot) = names[i];
                ResultRow { racer: i as u8, name: n.to_string(), character: c, place: k as u8 + 1, bot, time: None, best_lap: None, coins: 0, points: 0 }
            })
            .collect();
        OnlineResultsScreen { t: 0.0, join_prompt: None, had_session: false, leaving: false, payout: None, settled: false, mig_delay: Default::default(), sample: Some((ResultsView { track: race_core::TrackId::MempoolPlaza, laps: 3, rows }, 0, Some(b))) }
    }

    /// Pay the local player for this race (once).
    fn settle(&mut self, ctx: &mut Ctx, res: &ResultsView, you: usize) {
        use crate::ui::progress::{self, Earn, Kind, Played};
        let Some(row) = res.rows.iter().find(|r| r.racer as usize == you) else { return };
        let humans = res.rows.iter().filter(|r| !r.bot).count();
        let arena = res.track.is_arena();
        let played = Played {
            kind: if arena { Kind::Battle } else { Kind::Online },
            track: Some(res.track),
            place: row.place,
            // a win only counts against another person
            racers: if humans >= 2 { res.rows.len() } else { 1 },
            finished: row.time.is_some() || arena,
            coins: row.coins as u32,
            stats: progress::take_stats(),
            cup_won: None,
        };
        let earn = Earn::race(if arena { 0 } else { row.coins as u32 }, row.place, res.rows.len());
        let report = crate::ui::progress_ui::Report::settle(ctx, &played, earn);
        self.payout = Some(crate::ui::progress_ui::Payout::new(report));
    }

    fn leave(&mut self, ctx: &mut Ctx, say: Option<String>) -> Transition {
        ctx.online.leave();
        if let Some(m) = say {
            ctx.say(m);
        }
        Transition::PopTo("multiplayer")
    }
}

impl Default for OnlineResultsScreen {
    fn default() -> Self {
        Self::new()
    }
}

enum Next {
    Dead(String),
    Lobby,
    Race,
    Stay,
}

enum Act {
    Leave,
    Rematch(bool),
    ToLobby,
}

impl Screen for OnlineResultsScreen {
    fn name(&self) -> &'static str {
        "online_results"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn uses_online(&self) -> bool {
        true
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if self.leaving {
            return Transition::None;
        }
        match ctx.take_join_code() {
            // A link that isn't an invite is no reason to leave the room.
            Some(crate::keeps::JoinTarget::BadInvite) => ctx.say(crate::keeps::NOT_AN_INVITE),
            Some(code) => self.join_prompt = Some(common::LeaveAsk::new(ctx.data_dir, code)),
            None => {}
        }
        if !ctx.online.is_some() {
            if self.had_session {
                self.had_session = false;
                return Transition::PopTo("multiplayer");
            }
            return Transition::None;
        }
        self.had_session = true;
        if !self.settled {
            let got = ctx.online.with(|s| s.me().results().cloned().map(|r| (r, s.me().view().map_or(0, |v| v.you)))).flatten();
            if let Some((res, you)) = got {
                self.settled = true;
                self.settle(ctx, &res, you);
            }
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
        }
        let next = ctx
            .online
            .with(|s| {
                s.pump(Input::default());
                if let Some(why) = s.dead_reason() {
                    return Next::Dead(why);
                }
                match s.me().phase() {
                    // The host went back to the lobby.
                    ClientPhase::Lobby => Next::Lobby,
                    // Everyone was ready: the rematch is on.
                    ClientPhase::Racing if s.me().view().is_some() => Next::Race,
                    _ => Next::Stay,
                }
            })
            .unwrap_or(Next::Stay);
        match next {
            Next::Dead(why) => self.leave(ctx, Some(why)),
            Next::Lobby => Transition::Replace(Box::new(LobbyScreen::new())),
            Next::Race => match crate::game::source_net::race_screen(ctx.online) {
                Some(screen) => Transition::Replace(screen),
                None => Transition::None,
            },
            Next::Stay => Transition::None,
        }
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if self.join_prompt.take().is_some() {
            return Transition::None;
        }
        self.leave(ctx, None)
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut acts: Vec<Act> = Vec::new();
        // What to draw.
        let live = ctx.online.with(|s| {
            let lobby = s.me().lobby();
            let humans: Vec<_> = lobby.map(|l| l.seats.iter().filter(|x| !x.bot).collect()).unwrap_or_default();
            let ready_n = humans.iter().filter(|x| x.ready).count();
            let you = s.me().view().map_or(0, |v| v.you);
            (s.me().results().cloned(), you, ready_n, humans.len(), s.me().my_lobby_seat().is_some_and(|m| m.ready), s.me().rematch_ms_left(s.now_ms()), s.is_host(), s.me().view().and_then(|v| v.world.battle.clone()))
        });
        // Players who left mid-race: listed last, as Left.
        let left: Vec<u8> = ctx.online.with(|s| s.me().left_racers().to_vec()).unwrap_or_default();
        let (res, you, ready_n, humans, me_ready, count_ms, is_host, battle) = match (live, &self.sample) {
            (Some(l), _) => l,
            (None, Some((r, you, b))) => (Some(r.clone()), *you, 1, 2, false, None, true, b.clone()),
            (None, None) => (None, 0, 0, 0, false, None, false, None),
        };
        // A battle's rows carry feathers and hits instead of times.
        let battle = battle.filter(|_| res.as_ref().is_some_and(|r| r.track.is_arena()));
        let payout = self.payout.as_ref();
        common::page(ctx.viewport.screen, tm, ui, 0.30, |ui, brand| {
            let hd = widgets::header(ui, safe, "Results", widgets::enter(t, 0));
            if hd.back.clicked() {
                acts.push(Act::Leave);
            }
            let body = hd.body;
            let rw = (body.width() * 0.34).clamp(220.0, 290.0);
            let (table, side) = common::Layout::new(safe).split(body, rw, 400.0, S3);
            let p = ui.painter().clone();
            // ── the table ──
            let Some(res) = &res else {
                p.text(table.center(), Align2::CENTER_CENTER, "Waiting for the results...", theme::title(22.0), theme::TEXT_DIM);
                return;
            };
            let n = res.rows.len().max(1);
            let gap = 3.0;
            let rh = ((table.height() - gap * (n as f32 - 1.0)) / n as f32).min(34.0);
            for (i, row) in res.rows.iter().enumerate() {
                let r = Rect::from_min_size(pos2(table.left(), table.top() + i as f32 * (rh + gap)), vec2(table.width(), rh));
                let e = widgets::enter(t, 1 + i / 2);
                widgets::staged(ui, e, vec2(-24.0, 0.0), |ui, off| {
                    let r = r.translate(off);
                    let p = ui.painter();
                    let mine = row.racer as usize == you;
                    widgets::well(p, r, 10.0);
                    if mine {
                        p.rect_stroke(r, 10.0, egui::Stroke::new(2.0_f32, theme::GOLD), egui::StrokeKind::Inside);
                    }
                    let medal = match row.place {
                        1 => theme::GOLD,
                        2 => Color32::from_rgb(0xD5, 0xD9, 0xE0),
                        3 => Color32::from_rgb(0xD8, 0x8A, 0x4A),
                        _ => theme::TEXT_DIM,
                    };
                    p.text(pos2(r.left() + 30.0, r.center().y), Align2::CENTER_CENTER, ordinal(row.place), theme::title(15.0), medal);
                    character_chip(p, pos2(r.left() + 62.0, r.center().y), (rh * 0.34).min(9.5), row.character);
                    let nx = r.left() + 80.0;
                    let size = art::fit_size(p, &row.name, theme::ui_bold, 12.5, r.width() * 0.34);
                    let nr = p.text(pos2(nx, r.center().y), Align2::LEFT_CENTER, &row.name, theme::ui_bold(size), if mine { theme::GOLD } else { theme::TEXT });
                    if row.bot {
                        widgets::pill(p, pos2(nr.right() + 20.0, r.center().y), "BOT", rgba(0x6A, 0x36, 0x96, 0.9), theme::TEXT, 8.5);
                    }
                    if left.contains(&row.racer) {
                        p.text(pos2(r.right() - 12.0, r.center().y), Align2::RIGHT_CENTER, "LEFT", theme::ui_bold(12.0), theme::TEXT_DIM);
                    } else if let Some(b) = &battle {
                        let i = row.racer as usize;
                        let f = b.feathers.get(i).copied().unwrap_or(0);
                        let hits = b.hits.get(i).copied().unwrap_or(0);
                        if f == 0 {
                            p.text(pos2(r.right() - 12.0, r.center().y), Align2::RIGHT_CENTER, "OUT", theme::ui_bold(12.0), Color32::from_rgb(255, 120, 110));
                        } else {
                            let shown = f.min(5);
                            for k in 0..shown {
                                crate::game::battle_hud::feather(p, pos2(r.right() - 14.0 - (shown - 1 - k) as f32 * 11.0, r.center().y), 15.0, 0.0, 1.0, true);
                            }
                        }
                        p.text(pos2(r.right() - 82.0, r.center().y), Align2::RIGHT_CENTER, format!("{hits} hit{}", if hits == 1 { "" } else { "s" }), theme::ui_medium(11.5), theme::TEXT_DIM);
                    } else {
                    let time = row.time.map(fmt_time).unwrap_or_else(|| "DNF".into());
                    p.text(pos2(r.right() - 12.0, r.center().y), Align2::RIGHT_CENTER, time, theme::numbers(14.5), if row.time.is_some() { theme::TEXT } else { theme::TEXT_DIM });
                    if let Some(b) = row.best_lap {
                        p.text(pos2(r.right() - 96.0, r.center().y), Align2::RIGHT_CENTER, fmt_time(b), theme::ui_medium(11.0), theme::TEXT_DIM);
                    }
                    }
                });
            }

            // ── the side panel ──
            widgets::staged(ui, widgets::enter(t, 2), vec2(40.0, 0.0), |ui, off| {
                let col = side.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, col, 20.0);
                let inner = col.shrink2(vec2(14.0, 14.0));
                let mine = res.rows.iter().find(|r| r.racer as usize == you);
                if let Some(m) = mine {
                    let head = if m.time.is_none() && battle.is_none() { "Did not finish".to_string() } else { format!("You came {}", ordinal(m.place)) };
                    let size = art::fit_size(&p, &head, theme::title, 26.0, inner.width());
                    widgets::heading(&p, pos2(inner.left(), inner.top() + 16.0), Align2::LEFT_CENTER, &head, size, if m.place == 1 { theme::GOLD } else { theme::TEXT });
                }
                p.text(pos2(inner.left(), inner.top() + 42.0), Align2::LEFT_CENTER, if battle.is_some() { format!("{} \u{b7} Battle", super::tracks::info(res.track).name) } else { format!("{} \u{b7} {} lap{}", super::tracks::info(res.track).name, res.laps, if res.laps == 1 { "" } else { "s" }) }, theme::ui_medium(11.5), theme::TEXT_DIM);

                // the rematch
                let mid = Rect::from_min_size(pos2(inner.left(), inner.top() + 54.0), vec2(inner.width(), 56.0));
                widgets::well(&p, mid, 14.0);
                if let Some(ms) = count_ms {
                    let secs = (ms as f32 / 1000.0).ceil().max(1.0) as u32;
                    art::outlined(&p, pos2(mid.left() + 36.0, mid.center().y), Align2::CENTER_CENTER, &format!("{secs}"), theme::title(40.0), theme::GOLD, theme::OUTLINE, 3.5, 0.0);
                    p.text(pos2(mid.left() + 78.0, mid.center().y - 8.0), Align2::LEFT_CENTER, "Rematch in", theme::ui_bold(13.0), theme::TEXT);
                    p.text(pos2(mid.left() + 78.0, mid.center().y + 10.0), Align2::LEFT_CENTER, "tap Cancel to stop it", theme::ui_medium(10.5), theme::TEXT_DIM);
                } else {
                    icons::draw(&p, Icon::Flag, pos2(mid.left() + 26.0, mid.center().y), 26.0, theme::SAND);
                    p.text(pos2(mid.left() + 52.0, mid.center().y - 8.0), Align2::LEFT_CENTER, format!("{ready_n} of {humans} ready"), theme::ui_bold(13.0), theme::TEXT);
                    p.text(pos2(mid.left() + 52.0, mid.center().y + 10.0), Align2::LEFT_CENTER, "to race again", theme::ui_medium(10.5), theme::TEXT_DIM);
                }
                // what the race paid
                if let Some(po) = payout {
                    po.draw_chip(&p, brand, pos2(inner.left(), mid.bottom() + 18.0), 28.0, tm);
                }
                // buttons
                let bh = 36.0;
                let mut y = inner.bottom() - bh;
                if widgets::button(ui, Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), bh)), "res-leave", &Btn::ghost("Leave room").size(15.0)).clicked() {
                    acts.push(Act::Leave);
                }
                if is_host {
                    y -= bh + 6.0;
                    if widgets::button(ui, Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), bh)), "res-lobby", &Btn::new("Back to lobby").size(15.0)).clicked() {
                        acts.push(Act::ToLobby);
                    }
                }
                y -= bh + 6.0;
                let label = if count_ms.is_some() {
                    "Cancel"
                } else if me_ready {
                    "Ready! (tap to undo)"
                } else {
                    "Race again"
                };
                let b = if count_ms.is_some() || me_ready { Btn::new(label).selected(true) } else { Btn::primary(label) };
                if widgets::button(ui, Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), bh)), "res-again", &b.size(16.0)).clicked() {
                    acts.push(Act::Rematch(!(me_ready || count_ms.is_some())));
                }
            });
            if let Some(po) = payout {
                po.draw_toasts(&p, brand, safe);
            }
        });
        // How host migration stands (protected, paused, reconnecting...).
        if let Some((state, host)) = ctx.online.with(|s| (s.migration_state(), s.is_host())) {
            let p = ui.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("migration-chip")));
            let state = self.mig_delay.state(&state, decented_signet::sharing::now_ms());
            let _ = super::migration::results_status(ui, &p, ctx.viewport.screen, safe, &state, host);
        }
        if let Some(mut ask) = self.join_prompt.take() {
            acts.clear();
            ask.tick(ctx.dt);
            let answer = common::leave_prompt(ctx, ui, &ask);
            match answer {
                Some(true) => {
                    feedback::select(ctx);
                    ctx.online.leave();
                    self.leaving = true;
                    *ctx.join_code = Some(ask.target);
                    return Transition::PopTo("multiplayer");
                }
                Some(false) => feedback::back(ctx),
                None => self.join_prompt = Some(ask),
            }
        }
        let mut out = Transition::None;
        for a in acts {
            match a {
                Act::Leave => {
                    feedback::back(ctx);
                    out = self.leave(ctx, None);
                }
                Act::Rematch(on) => {
                    feedback::select(ctx);
                    ctx.online.with(|s| s.client_command(move |client| client.set_rematch_ready(on)));
                }
                Act::ToLobby => {
                    feedback::tap(ctx);
                    ctx.online.with(|s| s.to_lobby());
                }
            }
        }
        out
    }
}
