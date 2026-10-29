//! Sign-in is required for local play; My Signet separately approves online play.

use super::{common, Ctx, Screen, Transition};
use crate::ui::{theme, widgets::{self, Btn}};
use egui::{pos2, vec2, Align2, Rect};

pub struct AccountScreen {
    online: bool,
    mandatory: bool,
    next: Option<Box<dyn Screen>>,
    t: f32,
    manage: bool,
    confirm_signout: bool,
}

impl AccountScreen {
    pub fn login(next: Box<dyn Screen>) -> Self { Self { online: false, mandatory: true, next: Some(next), t: 0.0, manage: false, confirm_signout: false } }
    pub fn online(next: Option<Box<dyn Screen>>) -> Self { Self { online: true, mandatory: false, next, t: 0.0, manage: false, confirm_signout: false } }
    pub fn manage() -> Self { Self { manage: true, ..Self::online(None) } }
    pub fn sample() -> Self { Self { online: false, mandatory: true, next: None, t: 0.0, manage: false, confirm_signout: false } }
}

impl Screen for AccountScreen {
    fn name(&self) -> &'static str { if self.online { "play_online" } else { "signin" } }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        let ready = ctx.account.as_ref().is_some_and(|a| if self.online { a.online_allowed() } else { a.signed_in() && !a.busy() });
        if self.manage {
            if ctx.account.as_ref().is_some_and(|a| !a.signed_in() && !a.busy()) {
                return Transition::Cut(Box::new(Self::login(Box::new(super::home::HomeScreen::new()))));
            }
            return Transition::None;
        }
        if ready {
            return self.next.take().map(|next| if self.mandatory { Transition::Cut(next) } else { Transition::Replace(next) }).unwrap_or(Transition::Pop);
        }
        Transition::None
    }
    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if let Some(account) = ctx.account.as_mut() { account.cancel(); }
        if self.mandatory { Transition::Quit } else { Transition::Pop }
    }
    fn draw(&mut self, ctx: &mut Ctx, egui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, ctx.time as f32, egui, 0.1, |ui, _| {
            let title = if self.manage { "My Signet account" } else if self.online { "Play online" } else { "Sign in with My Signet" };
            let hd = widgets::header(ui, safe, title, widgets::enter(self.t, 0));
            if hd.back.clicked() { out = self.back(ctx); }
            let body = hd.body;
            let card = Rect::from_center_size(body.center(), vec2(body.width().min(610.0), body.height()));
            let p = ui.painter().clone();
            let signed_in = ctx.account.as_ref().is_some_and(|a| a.signed_in());
            let persona;
            let copy = if self.manage {
                let name = ctx.account.as_ref().and_then(|a| a.name()).unwrap_or_else(|| "your persona".into());
                let allowed = ctx.account.as_ref().is_some_and(|a| a.online_allowed());
                persona = format!("Signed in as {name}. {} Sign out to choose another persona. Signing out ends your online room.", if allowed { "Online play is approved." } else { "Ask My Signet before playing online." });
                persona.as_str()
            } else if self.online && signed_in {
                "Other racers will know your My Signet persona plays Stampede. My Signet approves online play. For a dependant persona, your guardian answers on their phone. You can still train and race bots while waiting."
            } else {
                "Stampede requires a My Signet login. Choose your persona in My Signet and approve the sign-in, then return here. Your persona's secret key stays in My Signet."
            };
            let g = p.layout(copy.into(), theme::ui_medium(17.0), theme::TEXT, card.width() - 32.0);
            p.galley(card.min + vec2(16.0, 14.0), g.clone(), theme::TEXT);
            let message = ctx.account.as_ref().map(|a| a.message.as_str()).unwrap_or("");
            let status = p.layout(message.into(), theme::ui_medium(14.0), theme::SAND, card.width() - 32.0);
            p.galley(card.min + vec2(16.0, 28.0 + g.size().y), status, theme::SAND);
            let busy = ctx.account.as_ref().is_some_and(|a| a.busy());
            let link_ready = ctx.account.as_ref().is_some_and(|a| a.link_ready);
            let label = if busy && link_ready { "Open My Signet" } else if busy { "Connecting…" } else if self.online && signed_in { "Ask My Signet" } else { "Sign in with My Signet" };
            let rect = Rect::from_center_size(pos2(card.center().x, card.bottom() - 66.0), vec2(card.width().min(340.0), 44.0));
            if widgets::button(ui, rect, "signet-action", &Btn::primary(label).enabled(!busy || link_ready)).clicked() {
                if let Some(account) = ctx.account.as_mut() {
                    if busy { account.reopen(); } else if self.online && signed_in { account.ask_online(); } else { account.sign_in(); }
                }
            }
            if busy {
                let cancel = Rect::from_center_size(pos2(card.center().x, card.bottom() - 20.0), vec2(140.0, 32.0));
                if widgets::button(ui, cancel, "signet-cancel", &Btn::new("Cancel")).clicked() {
                    if let Some(account) = ctx.account.as_mut() { account.cancel(); }
                }
            } else if self.manage {
                let rect = Rect::from_center_size(pos2(card.center().x, card.bottom() - 20.0), vec2(220.0, 32.0));
                let label = if self.confirm_signout { "Confirm sign out" } else { "Sign out" };
                if widgets::button(ui, rect, "signet-signout", &Btn::new(label)).clicked() {
                    if self.confirm_signout {
                        ctx.online.leave();
                        if let Some(account) = ctx.account.as_mut() { account.sign_out(); }
                    } else { self.confirm_signout = true; }
                }
            } else if self.online {
                p.text(pos2(card.center().x, card.bottom() - 20.0), Align2::CENTER_CENTER, "Back returns to solo play", theme::ui_medium(12.0), theme::TEXT_DIM);
            }
        });
        out
    }
}

/// A tap to host or invite keeps its editor underneath while approval is requested.
pub fn gate(ctx: &Ctx) -> Option<Transition> {
    ctx.account.as_ref().filter(|a| !a.online_allowed())
        .map(|_| Transition::Push(Box::new(AccountScreen::online(None))))
}
