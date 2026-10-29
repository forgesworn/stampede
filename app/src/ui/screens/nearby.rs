//! Nearby games: rooms heard on this Wi-Fi's beacon, live. Tap one to join it
//! directly (no internet needed). The Wi-Fi multicast lock is held while the
//! list is open (Android drops multicast packets without it).

use super::common;
use super::join::JoinScreen;
use super::{Ctx, Screen, Transition};
use crate::online::{self, OnlineSession};
use crate::ui::art::{self, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, S2, S3};
use crate::ui::{feedback, theme};
use decented_net::{LanBrowser, LanRoom};
use egui::{pos2, vec2, Align2, Rect, Stroke};

pub struct NearbyScreen {
    browser: Option<LanBrowser>,
    rooms: Vec<LanRoom>,
    started: bool,
    /// Holding the multicast lock.
    locked: bool,
    listening: bool,
    t: f32,
    scroll: f32,
}

impl Default for NearbyScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl NearbyScreen {
    pub fn new() -> NearbyScreen {
        NearbyScreen { browser: None, rooms: Vec::new(), started: false, locked: false, listening: true, t: 0.0, scroll: 0.0 }
    }

    /// A fixed list (screenshots).
    pub fn sample() -> NearbyScreen {
        let room = |name: &str, code: &str, n: u8, last: u8| LanRoom {
            name: name.into(),
            code: code.into(),
            player_count: n,
            max_players: 8,
            addr: std::net::SocketAddr::from(([192, 168, 1, last], 41000 + last as u16)),
            token: "00".into(),
        };
        NearbyScreen { rooms: vec![room("Mo's race", "KWTQPM", 2, 24), room("Friday race", "HDXNZB", 5, 31), room("Rider's race", "PCRVAS", 1, 40)], started: true, ..NearbyScreen::new() }
    }

    /// Nothing nearby yet (screenshots).
    pub fn empty_sample() -> NearbyScreen {
        NearbyScreen { started: true, ..NearbyScreen::new() }
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.browser = None;
        if self.locked {
            self.locked = false;
            ctx.platform.set_multicast_lock(false);
        }
    }
}

impl Screen for NearbyScreen {
    fn name(&self) -> &'static str {
        "nearby"
    }

    fn on_background(&mut self, ctx: &mut Ctx) {
        feedback::on_background();
        // Release the Wi-Fi multicast lock and the LAN port while away; the list
        // starts again on the next frame back.
        self.stop(ctx);
        self.started = false;
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !self.started {
            self.started = true;
            ctx.platform.set_multicast_lock(true);
            self.locked = true;
            let net = crate::platform::net_config(ctx.data_dir);
            let b = race_session::browse_nearby(&net);
            self.listening = b.is_listening();
            log::info!("nearby: browsing (listening {})", self.listening);
            self.browser = Some(b);
        }
        if let Some(b) = self.browser.as_mut() {
            let rooms = b.poll();
            if rooms.len() != self.rooms.len() {
                log::info!("nearby: {} room(s) in range", rooms.len());
            }
            self.rooms = rooms;
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        self.stop(ctx);
        Transition::Pop
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let rooms = self.rooms.clone();
        let listening = self.listening;
        let mut join: Option<usize> = None;
        let (mut leave, mut by_code) = (false, false);
        let mut scroll = self.scroll;
        common::page(ctx.viewport.screen, tm, ui, 0.30, |ui, _| {
            let hd = widgets::header(ui, safe, "Nearby games", widgets::enter(t, 0));
            if hd.back.clicked() {
                leave = true;
            }
            let body = hd.body;
            let portrait = common::Layout::new(safe).portrait;
            let p = ui.painter().clone();
            let foot_h = 46.0;
            let list = Rect::from_min_max(body.min, pos2(body.right(), body.bottom() - foot_h - S3));
            // ── the rows ──
            let row_h = 62.0;
            let gap = S2;
            if rooms.is_empty() {
                let card = Rect::from_center_size(list.center(), vec2(list.width().min(520.0), 150.0));
                widgets::panel(&p, card, 22.0);
                // radar sweep
                let c = pos2(card.left() + 70.0, card.center().y);
                for k in 0..3 {
                    let ph = ((tm * 0.8 + k as f32 / 3.0) % 1.0).clamp(0.0, 1.0);
                    p.circle_stroke(c, 12.0 + ph * 36.0, Stroke::new(2.0_f32, rgba(0xF7, 0x93, 0x1A, 0.7 * (1.0 - ph))));
                }
                icons::draw(&p, Icon::Nearby, c, 34.0, theme::SAND);
                let tx = card.left() + 130.0;
                let (head, sub) = if listening {
                    ("Looking for games...", "Both phones need to be on the same Wi-Fi. Ask a friend to open Host a game.")
                } else {
                    ("Can't listen on this network", "Something else is using the Nearby port, or Wi-Fi is off. Try Join by code instead.")
                };
                let size = art::fit_size(&p, head, theme::title, 22.0, card.right() - tx - 16.0);
                p.text(pos2(tx, card.top() + 50.0), Align2::LEFT_CENTER, head, theme::title(size), theme::TEXT);
                let g = p.layout(sub.to_string(), theme::ui_medium(12.5), theme::TEXT_DIM, card.right() - 24.0 - tx);
                p.galley(pos2(tx, card.top() + 74.0), g, theme::TEXT_DIM);
            } else {
                let total = rooms.len() as f32 * (row_h + gap);
                let max_scroll = (total - list.height()).max(0.0);
                let drag = ui.interact(list, egui::Id::new("nearby-list"), egui::Sense::drag());
                if drag.dragged() {
                    scroll = (scroll - drag.drag_delta().y).clamp(0.0, max_scroll);
                }
                let clip = p.with_clip_rect(list.expand(4.0));
                for (i, room) in rooms.iter().enumerate() {
                    let r = Rect::from_min_size(pos2(list.left(), list.top() + i as f32 * (row_h + gap) - scroll), vec2(list.width(), row_h));
                    if r.bottom() < list.top() || r.top() > list.bottom() {
                        continue;
                    }
                    let e = widgets::enter(t, 1 + i);
                    widgets::staged(ui, e, vec2(0.0, 28.0), |ui, off| {
                        let r = r.translate(off);
                        let full = room.player_count >= room.max_players;
                        let resp = ui.interact(r.intersect(list), egui::Id::new(("nearby-row", i)), egui::Sense::click());
                        let (scale, _) = widgets::press_fx(ui, egui::Id::new(("nearby-row", i)), &resp);
                        let rr = Rect::from_center_size(r.center(), r.size() * scale);
                        widgets::panel(&clip, rr, 18.0);
                        icons::draw(&clip, Icon::Nearby, pos2(rr.left() + 34.0, rr.center().y), 30.0, theme::SAND);
                        let name_size = art::fit_size(&clip, &room.name, theme::title, 21.0, rr.width() - 210.0);
                        clip.text(pos2(rr.left() + 66.0, rr.center().y - 10.0), Align2::LEFT_CENTER, &room.name, theme::title(name_size), theme::TEXT);
                        clip.text(pos2(rr.left() + 66.0, rr.center().y + 13.0), Align2::LEFT_CENTER, format!("{}  \u{b7}  {}", online::spaced_code(&room.code), room.addr.ip()), theme::ui_medium(11.5), theme::TEXT_DIM);
                        let count = format!("{}/{}", room.player_count, room.max_players);
                        clip.text(pos2(rr.right() - 100.0, rr.center().y), Align2::RIGHT_CENTER, count, theme::numbers(22.0), if full { theme::LASER } else { theme::GOLD });
                        clip.text(pos2(rr.right() - 100.0, rr.center().y + 17.0), Align2::RIGHT_CENTER, "racers", theme::ui_medium(9.5), theme::TEXT_DIM);
                        let jb = Rect::from_center_size(pos2(rr.right() - 52.0, rr.center().y), vec2(78.0, 38.0));
                        let (kind_full, label) = if full { (true, "Full") } else { (false, "Join") };
                        let b = if kind_full { Btn::new(label).enabled(false) } else { Btn::primary(label) };
                        if widgets::button(ui, jb, &format!("nearby-join-{i}"), &b.size(17.0)).clicked() || (resp.clicked() && !full) {
                            join = Some(i);
                        }
                    });
                }
            }
            // ── the foot ──
            let fy = body.bottom() - foot_h;
            p.text(pos2(body.left() + 4.0, fy + foot_h / 2.0), Align2::LEFT_CENTER, if portrait { "" } else { "Games appear as soon as a nearby host opens a room." }, theme::ui_medium(11.5), theme::TEXT_DIM);
            let bw = 190.0;
            if widgets::button(ui, Rect::from_min_size(pos2(body.right() - bw, fy), vec2(bw, foot_h)), "nearby-code", &Btn::new("Join by code").icon(Icon::Join).size(16.0)).clicked() {
                by_code = true;
            }
        });
        self.scroll = scroll;
        let _ = art::hash01(0);
        if leave {
            return self.back(ctx);
        }
        if by_code {
            feedback::select(ctx);
            self.stop(ctx);
            return Transition::Replace(Box::new(JoinScreen::new()));
        }
        if let Some(i) = join {
            if let Some(room) = rooms.get(i) {
                feedback::select(ctx);
                match OnlineSession::join_nearby(ctx.data_dir, room, &ctx.profile.name, ctx.profile.character) {
                    Ok(s) => {
                        self.stop(ctx);
                        return Transition::Replace(Box::new(JoinScreen::connecting(s, &room.name)));
                    }
                    Err(e) => ctx.say(e),
                }
            }
        }
        Transition::None
    }
}
