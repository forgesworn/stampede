//! Sound and haptics for the menus: boot the audio once, keep the music, the
//! volumes and the mute state in step with the profile, and give every tap its
//! click and buzz.

use crate::haptics::Pulse;
use crate::sound;
use crate::ui::screens::Ctx;
use race_audio::{MusicTrack, Sfx};
use std::cell::Cell;

thread_local! {
    /// What the audio was last told about `ctx.muted`.
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

/// Call at the top of every menu screen's `update`: opens the audio (silent in
/// `--shot`), applies the profile volumes and haptics switch, follows
/// `ctx.muted`, and keeps the menu music playing.
pub fn boot(ctx: &mut Ctx) {
    sound::init(ctx.opts.shot.is_some());
    let (music, effects, muted) = (ctx.profile.music_volume, ctx.profile.effects_volume, ctx.muted);
    sound::with(|a| {
        a.set_volumes(1.0, music, effects);
        if MUTED.with(|m| m.replace(muted)) != muted {
            if muted {
                a.pause_all();
            } else {
                a.resume_all();
            }
        }
        a.set_music(Some(MusicTrack::Menu));
    });
    ctx.haptics.set_enabled(ctx.profile.haptics);
}

fn sfx(s: Sfx) {
    sound::with(|a| a.play(s, 1.0, 1.0));
}

/// A button press: click and a light buzz.
pub fn tap(ctx: &mut Ctx) {
    sfx(Sfx::MenuTap);
    ctx.haptics.pulse(Pulse::Tap);
}

/// A confirming choice (Go, Select).
pub fn select(ctx: &mut Ctx) {
    sfx(Sfx::MenuSelect);
    ctx.haptics.pulse(Pulse::Tap);
}

/// A ₿ tick; `chain` climbs the pentatonic scale (0..7) for a run of them.
pub fn coin(chain: u32) {
    sound::with(|a| a.play(Sfx::Coin, 0.85, race_audio::coin_pitch(chain)));
}

/// An achievement unlocked.
pub fn unlock(ctx: &mut Ctx) {
    sfx(Sfx::ItemGet);
    ctx.haptics.pulse(Pulse::Coin);
}

/// Something bought: a rising pair of coin dings and a thump.
pub fn bought(ctx: &mut Ctx) {
    coin(3);
    coin(6);
    sfx(Sfx::MenuSelect);
    ctx.haptics.pulse(Pulse::Boost);
}

/// Going back.
pub fn back(ctx: &mut Ctx) {
    sfx(Sfx::MenuBack);
    ctx.haptics.pulse(Pulse::Tap);
}

/// The app went to the background: silence music and sounds.
pub fn on_background() {
    MUTED.with(|m| m.set(true));
    sound::with(|a| a.pause_all());
}
