//! The online [`RaceSource`]: a `race-session` client (the host's own player
//! included) behind the interface the race screen already uses, so the race
//! screen runs unchanged. The session does the work: it predicts your racer,
//! interpolates the others (100 ms behind the host) and raises your own
//! motion events itself; this adapter only feeds it one input per rendered
//! frame and maps its world to the screen's [`RaceView`].
//!
//! The adapter owns the [`OnlineSession`] while the race runs and hands it to
//! the results screen (or drops it, leaving the room) through
//! [`RaceSource::exit_transition`].

use super::source::{NetStatus, RaceSource, RaceView};
use crate::flow::{RaceKind, RaceLaunch};
use crate::online::{self, OnlineSlot};
use crate::ui::screens::online_results::OnlineResultsScreen;
use crate::ui::screens::{Ctx, Screen, Transition};
use race_core::*;
use race_session::ClientPhase;

/// Seconds between the periodic `net:` log lines.
const LOG_EVERY: f32 = 5.0;

/// The last state the session showed, copied once per frame: the screen reads
/// it through `&self` while the session itself lives in the shared slot. It
/// also serves the frame or two after the session dropped its view (the host
/// went back to the lobby) and before the screen leaves, so the racers stay
/// where they were instead of jumping back to the grid.
struct Shadow {
    world: World,
    race_time: f32,
    countdown: f32,
    finished: bool,
}

impl Shadow {
    fn of(v: &race_session::RaceView) -> Shadow {
        Shadow { world: v.world.clone(), race_time: v.hud.race_time, countdown: v.hud.countdown, finished: v.hud.finished }
    }
}

pub struct NetRace {
    slot: OnlineSlot,
    shadow: Shadow,
    events: Vec<Event>,
    you: usize,
    log_t: f32,
    /// The race screen is paused online: the room races on, a bot steers.
    hands_off: bool,
}

impl NetRace {
    /// A race source for the slot's session whose race has started; `None`
    /// when there is no race view yet.
    pub fn new(slot: &OnlineSlot) -> Option<NetRace> {
        let (shadow, you) = slot.with(|s| s.me().view().map(|v| (Shadow::of(v), v.you))).flatten()?;
        Some(NetRace { slot: slot.clone(), shadow, events: Vec::new(), you, log_t: 0.0, hands_off: false })
    }

    /// The launch the race screen is built with: the host's config for this
    /// race and your racer index.
    pub fn launch(&self) -> RaceLaunch {
        RaceLaunch { config: self.shadow.world.cfg.clone(), kind: RaceKind::Online, local_seat: self.you }
    }
}

/// The race screen for the slot's started online race, or `None` if the race
/// has not started. The session stays in the slot.
pub fn race_screen(slot: &OnlineSlot) -> Option<Box<dyn Screen>> {
    let src = NetRace::new(slot)?;
    let launch = src.launch();
    Some(Box::new(super::race::with_source(launch, super::race::Staging::default(), Box::new(src))))
}

impl RaceSource for NetRace {
    fn view(&self) -> RaceView<'_> {
        let w = &self.shadow.world;
        RaceView {
            cfg: &w.cfg,
            track: &w.track,
            tun: &w.tun,
            racers: &w.racers,
            // The session already eases your racer and interpolates the others
            // per rendered frame: show its state as is (no second smoothing).
            prev: &w.racers,
            alpha: 1.0,
            projectiles: &w.projectiles,
            hazards: &w.hazards,
            coins: &w.coins,
            loose_coins: &w.loose_coins,
            targets: &w.targets,
            play: &w.play,
            phase: w.phase,
            race_time: self.shadow.race_time,
            countdown_left: self.shadow.countdown,
            lesson: None,
            challenge: None,
            battle: w.battle.as_ref(),
            lap_times: &w.lap_times,
            hurdles_active: false,
        }
    }

    fn local_seat(&self) -> Option<usize> {
        Some(self.you)
    }

    fn advance(&mut self, dt: f32, input: &mut dyn FnMut() -> Input) -> u32 {
        let mut i = input();
        let hands_off = self.hands_off;
        let slot = self.slot.clone();
        let seen = slot.with(|s| {
            if s.autopilot || s.is_away() || hands_off {
                // Dev hook, backgrounded, or paused: the bot AI drives (it holds
                // the throttle until the start window).
                i = s.bot_drive();
            }
            s.pump(i);
            let seen = s.me().view().map(|v| (Shadow::of(v), v.events.clone()));
            self.log_t += dt;
            if self.log_t >= LOG_EVERY {
                self.log_t = 0.0;
                let st = s.me().stats();
                log::info!(
                    "net: rtt {:?} ms snapshots {} hard snaps {} last/max correction {:.2}/{:.2} m others {:.0} ms back, past the newest snapshot {}/{} frames phase {:?}",
                    s.rtt_ms(),
                    st.snapshots,
                    st.hard_snaps,
                    st.last_correction_m,
                    st.max_correction_m,
                    st.interp_delay_ms,
                    st.interp_starved_frames,
                    st.interp_frames,
                    s.me().phase()
                );
            }
            seen
        });
        if let Some(Some((shadow, events))) = seen {
            self.shadow = shadow;
            self.events.extend(events);
        }
        1
    }

    fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn net_status(&self) -> Option<NetStatus> {
        let finished = self.shadow.finished;
        let st = self.slot.with(|s| {
            let rtt = s.rtt_ms();
            NetStatus { rtt_ms: rtt, quality: online::quality(rtt), lost: s.dead_reason(), waiting: s.me().results().is_none() && finished, host: s.is_host(), notice: s.me().notice() }
        });
        Some(st.unwrap_or_default())
    }

    fn pauses_sim(&self) -> bool {
        false
    }

    fn set_hands_off(&mut self, on: bool) {
        self.hands_off = on;
    }

    fn ready_to_finish(&self) -> bool {
        // The host's results, or the link/room changed under us (or the room is gone).
        self.slot.with(|s| s.me().results().is_some() || !matches!(s.me().phase(), ClientPhase::Racing)).unwrap_or(true)
    }

    fn exit_transition(&mut self, ctx: &mut Ctx, quit: bool) -> Option<Transition> {
        if quit {
            ctx.online.leave();
            return Some(Transition::PopTo("multiplayer"));
        }
        let (dead, lobby) = ctx.online.with(|s| (s.dead_reason(), matches!(s.me().phase(), ClientPhase::Lobby))).unwrap_or((None, false));
        if let Some(why) = dead {
            // Host gone (or removed): say so and go back to the menu.
            ctx.say(why);
            ctx.online.leave();
            return Some(Transition::PopTo("multiplayer"));
        }
        if !ctx.online.is_some() {
            return Some(Transition::PopTo("multiplayer"));
        }
        if lobby {
            // The host went back to the lobby while we waited: straight there.
            return Some(Transition::Replace(Box::new(crate::ui::screens::lobby::LobbyScreen::new())));
        }
        Some(Transition::Replace(Box::new(OnlineResultsScreen::new())))
    }
}

#[cfg(test)]
mod tests;
