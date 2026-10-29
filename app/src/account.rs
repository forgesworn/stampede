//! My Signet owns sign-in and online approval; local play only needs sign-in.

use decented_net::Identity;
use decented_signet::{PermissionAsk, PermissionEvent, SignIn, SignInEvent, Signet, SignetConfig, SignOutOutcome, Task};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const APP_ID: &str = "stampede";

fn config(dir: &Path) -> SignetConfig {
    let mut cfg = SignetConfig::new(dir, APP_ID, "Stampede", Some("stampede://signin".into()));
    // A dependant's guardian has ten minutes to answer in My Signet.
    cfg.pairing_timeout = cfg.permission_wait;
    cfg.signin_origin = Some("https://decented.org".into());
    // My Signet answers on relay.primal.net now; older builds still use nos.lol.
    cfg.signin_relays = ["wss://relay.primal.net", "wss://nos.lol", "wss://relay.damus.io"].map(String::from).to_vec();
    cfg.relays = crate::platform::relays();
    cfg.permission_reason = Some("Stampede shows the other racers that it's really you.".into());
    cfg
}

pub struct Account {
    dir: PathBuf,
    signet: Option<Signet>,
    login: Option<SignIn>,
    permission: Option<PermissionAsk>,
    signing_out: Option<Task<SignOutOutcome>>,
    link_at: Option<Instant>,
    wait_since: Option<Instant>,
    pub message: String,
    pub link_ready: bool,
    open_link: bool,
}

impl Account {
    pub fn open(dir: &Path) -> Self {
        let mut account = Self { dir: dir.into(), signet: None, login: None, permission: None,
            signing_out: None, link_at: None, wait_since: None, message: String::new(), link_ready: false, open_link: false };
        account.reload();
        account
    }

    fn reload(&mut self) {
        match Identity::load_or_create(&self.dir) {
            Ok(identity) => self.signet = Some(Signet::open(config(&self.dir), identity.keys().clone())),
            Err(reason) => { self.signet = None; self.message = format!("Couldn't open this device's account: {reason}"); }
        }
    }

    pub fn signed_in(&self) -> bool { self.signet.as_ref().is_some_and(Signet::is_signed_in) }
    pub fn online_allowed(&self) -> bool { self.signet.as_ref().is_some_and(|s| s.is_signed_in() && s.attestation_json().is_some()) }
    pub fn name(&self) -> Option<String> { self.signet.as_ref().and_then(Signet::display_name) }
    pub fn busy(&self) -> bool { self.login.is_some() || self.permission.is_some() || self.signing_out.is_some() }

    fn retire_key(&mut self) -> Result<(), String> {
        match std::fs::remove_file(self.dir.join("identity.key")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(format!("Couldn't replace this device's sign-in key: {e}")),
        }
        self.reload();
        Ok(())
    }

    pub fn sign_in(&mut self) {
        if self.busy() || self.signed_in() { return; }
        if self.signet.as_ref().is_some_and(|s| s.sign_in_refusal().is_some()) {
            if let Err(reason) = self.retire_key() { self.message = reason; return; }
        }
        if self.signet.is_none() { self.reload(); }
        if let Some(s) = &self.signet {
            self.login = Some(s.sign_in_with_signet());
            self.message = "Connecting to My Signet…".into();
            self.link_ready = false;
        }
    }

    pub fn ask_online(&mut self) {
        if self.busy() || !self.signed_in() { return; }
        self.permission = self.signet.as_ref().map(Signet::ask_online_permission);
        self.wait_since = Some(Instant::now());
        self.link_ready = false;
        self.message = "Preparing the approval request…".into();
    }

    pub fn cancel(&mut self) {
        if let Some(login) = self.login.take() { login.cancel(); }
        if let Some(ask) = self.permission.take() { ask.cancel(); }
        self.link_ready = false;
        self.open_link = false;
        self.message = "Cancelled. You can try again.".into();
    }

    pub fn sign_out(&mut self) {
        self.cancel();
        self.signing_out = self.signet.as_ref().map(Signet::sign_out);
        self.message = "Signing out…".into();
    }

    pub fn resume(&self) {
        if let Some(ask) = &self.permission { ask.reconnect(); }
    }

    pub fn reopen(&mut self) {
        // My Signet rejects an old permission link. Keep the original wait deadline.
        if self.permission.is_some() && self.link_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(240)) {
            let remaining = self.signet.as_ref().map(Signet::permission_wait).unwrap_or_default()
                .saturating_sub(self.wait_since.map(|at| at.elapsed()).unwrap_or_default());
            if remaining < Duration::from_secs(30) {
                self.cancel();
                self.message = "No answer yet. Ask My Signet again.".into();
                return;
            }
            self.permission = self.signet.as_ref().map(|s| s.ask_online_permission_within(remaining));
            self.link_ready = false;
        } else { self.open_link = true; }
    }

    pub fn tick(&mut self, platform: &mut dyn crate::platform::Platform) {
        while let Some(event) = self.login.as_ref().and_then(SignIn::poll) {
            match event {
                SignInEvent::AwaitingApproval => {
                    self.link_ready = true; self.open_link = true;
                    self.message = "Approve the sign-in in My Signet, then come back here.".into();
                }
                SignInEvent::Connected { .. } | SignInEvent::AwaitingSignature => self.message = "Signed in. Finishing with My Signet…".into(),
                SignInEvent::Attested { .. } | SignInEvent::NoBunker { .. } => {
                    self.login = None; self.link_ready = false; self.message = "Signed in with My Signet.".into();
                }
                SignInEvent::AttestationFailed { reason } => {
                    self.login = None; self.link_ready = false;
                    self.message = format!("Signed in. Ask My Signet before playing online: {reason}");
                }
                SignInEvent::Failed { reason } => {
                    self.login = None; self.link_ready = false; self.message = reason;
                }
            }
        }
        while let Some(event) = self.permission.as_ref().and_then(PermissionAsk::poll) {
            match event {
                PermissionEvent::Waiting => {
                    self.link_at = Some(Instant::now()); self.link_ready = true; self.open_link = true;
                    self.message = "In My Signet, allow online play or ask your guardian. Keep My Signet open until they answer, then come back here.".into();
                }
                PermissionEvent::Approved(_) => {
                    self.permission = None; self.link_ready = false; self.message = "Online play approved.".into();
                }
                PermissionEvent::Denied => { self.permission = None; self.link_ready = false; self.message = "Not allowed this time. You can still play solo.".into(); }
                PermissionEvent::TimedOut => { self.permission = None; self.link_ready = false; self.message = "No answer yet. Ask again and keep My Signet open.".into(); }
                PermissionEvent::Failed { reason } => { self.permission = None; self.link_ready = false; self.message = reason; }
            }
        }
        if let Some(outcome) = self.signing_out.as_ref().and_then(Task::poll) {
            self.signing_out = None;
            if outcome.rotate_runtime_key {
                self.signet = None;
                if let Err(reason) = self.retire_key() { self.message = reason; return; }
            }
            self.message = "Signed out. Sign in to play.".into();
        }
        if std::mem::take(&mut self.open_link) {
            let link = self.login.as_ref().map(SignIn::app_link)
                .or_else(|| self.permission.as_ref().map(PermissionAsk::app_link)).unwrap_or("");
            if !link.is_empty() {
                if let Err(reason) = platform.open_url(link) { self.message = reason; }
            }
        }
    }
}

/// All real host/join paths check persisted approval again, before opening sockets.
pub fn identity_for_online(dir: &Path) -> Result<Identity, String> {
    let identity = Identity::load_or_create(dir)?;
    let signet = Signet::open(config(dir), identity.keys().clone());
    if !signet.is_signed_in() { return Err("Sign in with My Signet to play.".into()); }
    let attestation = signet.attestation_json().ok_or("Ask My Signet to approve online play first.")?;
    Ok(identity.with_attestation(attestation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use decented_signet::relay::FakeNetwork;
    use nostr::{EventBuilder, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent, Url};
    use nostr::{nips::nip44, util::JsonUtil};
    use serde_json::{json, Value};
    use crate::platform::os::RecordingPlatform;
    use std::sync::Arc;

    const RELAY: &str = "wss://stampede-test.invalid";
    fn field(link: &str, key: &str) -> String {
        Url::parse(link).unwrap().query_pairs().find(|(k, _)| k == key).unwrap().1.into_owned()
    }
    fn response(net: &FakeNetwork, persona: &Keys, link: &str, permission: bool, allowed: bool) {
        let challenge = field(link, "challenge");
        let session = PublicKey::from_hex(&field(link, "sessionPubkey")).unwrap();
        let (body, status) = if !permission {
            let auth = EventBuilder::new(Kind::Custom(21236), "")
                .tags([Tag::parse(["challenge", &challenge]).unwrap(), Tag::parse(["origin", &field(link, "origin")]).unwrap()])
                .sign_with_keys(persona).unwrap();
            (json!({"type":"signet-auth-response", "requestId":challenge, "authEvent":auth, "displayName":"Test racer"}), "approved")
        } else if allowed {
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(field(link, "template")).unwrap();
            let mut unsigned: Value = serde_json::from_slice(&raw).unwrap();
            unsigned["created_at"] = json!(Timestamp::now().as_secs());
            let event = serde_json::from_value::<UnsignedEvent>(unsigned).unwrap().sign_with_keys(persona).unwrap();
            (json!({"type":"signet-app-permission-response", "permission":"online-play", "challenge":challenge, "event":event}), "approved")
        } else {
            (json!({"type":"signet-app-permission-response", "permission":"online-play", "challenge":challenge, "denied":true}), "rejected")
        };
        let rumor = EventBuilder::new(Kind::Custom(29999), body.to_string())
            .tags([Tag::parse(["session", &challenge]).unwrap(), Tag::parse(["status", status]).unwrap()])
            .build(persona.public_key());
        // My Signet replies within the fresh request's relay filter; the generic
        // NIP-59 builder backdates a wrap by up to two days.
        let sealed = nip44::encrypt(persona.secret_key(), &session, rumor.as_json(), nip44::Version::V2).unwrap();
        let seal = EventBuilder::new(Kind::Seal, sealed).sign_with_keys(persona).unwrap();
        let ephemeral = Keys::generate();
        let wrapped = nip44::encrypt(ephemeral.secret_key(), &session, seal.as_json(), nip44::Version::V2).unwrap();
        let wrap = EventBuilder::new(Kind::GiftWrap, wrapped).tag(Tag::public_key(session)).sign_with_keys(&ephemeral).unwrap();
        assert!(net.hub(RELAY).publish(&wrap));
    }
    fn wait(account: &mut Account, platform: &mut RecordingPlatform, done: impl Fn(&Account, &RecordingPlatform) -> bool) {
        let until = Instant::now() + Duration::from_secs(8);
        while !done(account, platform) {
            account.tick(platform);
            assert!(Instant::now() < until, "timed out: {}", account.message);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn dependant_login_does_not_allow_online_until_a_signed_approval_and_survives_restart() {
        let dir = std::env::temp_dir().join(format!("stampede-signet-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut account = Account::open(&dir);
        let identity = Identity::load_or_create(&dir).unwrap();
        let net = FakeNetwork::new();
        let mut cfg = config(&dir);
        cfg.relays = vec![RELAY.into()]; cfg.signin_relays = vec![RELAY.into()];
        account.signet = Some(Signet::with_connector(cfg, identity.keys().clone(), Arc::new(net.clone())));
        let mut platform = RecordingPlatform::default();
        let persona = Keys::generate();
        account.sign_in();
        wait(&mut account, &mut platform, |_, p| !p.opened.is_empty());
        assert_eq!(field(&platform.opened[0], "name"), "Stampede");
        response(&net, &persona, &platform.opened[0], false, true);
        wait(&mut account, &mut platform, |a, _| a.signed_in() && !a.busy());
        assert_eq!(account.name().as_deref(), Some("Test racer"));
        assert!(!account.online_allowed());
        assert!(identity_for_online(&dir).is_err());
        account.ask_online();
        wait(&mut account, &mut platform, |_, p| p.opened.len() >= 2);
        response(&net, &persona, &platform.opened[1], true, false);
        wait(&mut account, &mut platform, |a, _| !a.busy());
        assert!(!account.online_allowed());
        assert!(account.signed_in());
        account.ask_online();
        wait(&mut account, &mut platform, |_, p| p.opened.len() >= 3);
        response(&net, &persona, &platform.opened[2], true, true);
        wait(&mut account, &mut platform, |a, _| !a.busy());
        assert!(account.online_allowed());
        assert!(identity_for_online(&dir).unwrap().attestation().is_some());
        let restored = Account::open(&dir);
        assert!(restored.signed_in()); assert!(restored.online_allowed());
        drop(restored);
        account.sign_out();
        wait(&mut account, &mut platform, |a, _| !a.busy());
        assert!(!account.signed_in()); assert!(!account.online_allowed());
        assert_ne!(Identity::load_or_create(&dir).unwrap().pubkey(), identity.pubkey());
        assert!(identity_for_online(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
