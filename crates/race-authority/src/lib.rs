//! Import-free binding of the existing race rules to Fray's authority world.
#![cfg(any(target_arch = "wasm32", test))]
use fray_session::authority::{Authority, AuthorityError};
use race_session::authority::RaceAuthority;

enum State {
    Fresh,
    Live(Box<RaceAuthority>),
    Faulted,
}

impl State {
    fn start(
        &mut self,
        build: impl FnOnce() -> Result<RaceAuthority, AuthorityError>,
    ) -> Result<(), AuthorityError> {
        let previous = std::mem::replace(self, Self::Faulted);
        if !matches!(previous, Self::Fresh) {
            return Err(AuthorityError::fault(
                "instance already initialised or faulted",
            ));
        }
        *self = Self::Live(Box::new(build()?));
        Ok(())
    }

    fn call<T>(
        &mut self,
        call: impl FnOnce(&mut RaceAuthority) -> Result<T, AuthorityError>,
    ) -> Result<T, AuthorityError> {
        let mut authority = match std::mem::replace(self, Self::Faulted) {
            Self::Live(authority) => authority,
            _ => return Err(AuthorityError::fault("instance not live")),
        };
        let result = call(&mut authority)?;
        *self = Self::Live(authority);
        Ok(result)
    }
}

#[cfg(target_arch = "wasm32")]
mod component {
    use super::*;
    use fray_session::authority as native;
    use std::sync::Mutex;

    wit_bindgen::generate!({
        path: "../../../forgesworn/fray/crates/fray-session/wit",
        world: "game-authority",
    });
    use exports::fray::authority::authority as wire;

    static STATE: Mutex<State> = Mutex::new(State::Fresh);
    struct Component;

    fn error(error: native::AuthorityError) -> wire::AuthorityError {
        wire::AuthorityError {
            code: match error.code {
                native::ErrorCode::InvalidConfig => wire::ErrorCode::InvalidConfig,
                native::ErrorCode::InvalidCheckpoint => wire::ErrorCode::InvalidCheckpoint,
                native::ErrorCode::UnsupportedVersion => wire::ErrorCode::UnsupportedVersion,
                native::ErrorCode::Fault => wire::ErrorCode::Fault,
            },
            message: error.message,
        }
    }
    fn time(time: wire::Time) -> native::Time {
        native::Time {
            tick: time.tick,
            elapsed_us: time.elapsed_us,
        }
    }
    fn delivery(delivery: wire::Delivery) -> native::Delivery {
        match delivery {
            wire::Delivery::Reliable => native::Delivery::Reliable,
            wire::Delivery::Lossy => native::Delivery::Lossy,
        }
    }
    fn event(event: wire::Event) -> Result<native::Event, native::AuthorityError> {
        Ok(match event {
            wire::Event::Joined(player) => native::Event::Joined(native::Player {
                id: native::PlayerId(player.id),
                identity: native::identity_from_bytes(player.identity.as_deref())?,
                metadata: player.metadata,
            }),
            wire::Event::Left(leave) => native::Event::Left {
                player: native::PlayerId(leave.player),
                reason: leave.reason,
            },
            wire::Event::Message(message) => native::Event::Message {
                player: native::PlayerId(message.player),
                delivery: delivery(message.delivery),
                bytes: message.bytes,
            },
            wire::Event::Control(bytes) => native::Event::Control(bytes),
        })
    }
    fn commands(commands: Vec<native::Command>) -> Vec<wire::Command> {
        commands
            .into_iter()
            .map(|command| match command {
                native::Command::Send {
                    target,
                    delivery,
                    bytes,
                } => wire::Command::Send(wire::SendCommand {
                    target: match target {
                        native::Target::Player(player) => wire::Target::Player(player.0),
                        native::Target::Broadcast => wire::Target::Broadcast,
                    },
                    delivery: match delivery {
                        native::Delivery::Reliable => wire::Delivery::Reliable,
                        native::Delivery::Lossy => wire::Delivery::Lossy,
                    },
                    bytes,
                }),
                native::Command::Disconnect { player, reason } => {
                    wire::Command::Disconnect(wire::Leave {
                        player: player.0,
                        reason,
                    })
                }
                native::Command::SetOpen(open) => wire::Command::SetOpen(open),
            })
            .collect()
    }
    impl wire::Guest for Component {
        fn init(config: Vec<u8>, seed: u64) -> Result<(), wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .start(|| RaceAuthority::init(&config, seed))
                .map_err(error)
        }
        fn on_event(
            now: wire::Time,
            input: wire::Event,
        ) -> Result<Vec<wire::Command>, wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .call(|authority| authority.on_event(time(now), event(input)?))
                .map(commands)
                .map_err(error)
        }
        fn tick(now: wire::Time) -> Result<Vec<wire::Command>, wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .call(|authority| authority.tick(time(now)))
                .map(commands)
                .map_err(error)
        }
        fn checkpoint() -> Result<Vec<u8>, wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .call(|authority| authority.checkpoint())
                .map_err(error)
        }
        fn restore(config: Vec<u8>, checkpoint: Vec<u8>) -> Result<(), wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .start(|| RaceAuthority::restore(&config, &checkpoint))
                .map_err(error)
        }
        fn state_hash() -> Result<Vec<u8>, wire::AuthorityError> {
            STATE
                .lock()
                .unwrap()
                .call(|authority| authority.state_hash().map(|hash| hash.to_vec()))
                .map_err(error)
        }
    }
    export!(Component);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fray_session::authority::Time;
    use race_session::authority::AuthorityConfig;

    #[test]
    fn calls_before_init_and_failed_init_are_terminal() {
        let config = AuthorityConfig::default().to_bytes().unwrap();
        let mut state = State::Fresh;
        assert!(state.call(|a| a.checkpoint()).is_err());
        assert!(state.start(|| RaceAuthority::init(&config, 42)).is_err());
        let mut state = State::Fresh;
        assert!(state.start(|| RaceAuthority::init(&[], 42)).is_err());
        assert!(state.start(|| RaceAuthority::init(&config, 42)).is_err());
    }

    #[test]
    fn repeated_init_and_authority_errors_are_terminal() {
        let config = AuthorityConfig::default().to_bytes().unwrap();
        let mut state = State::Fresh;
        state.start(|| RaceAuthority::init(&config, 42)).unwrap();
        assert!(state.start(|| RaceAuthority::init(&config, 42)).is_err());
        assert!(state.call(|a| a.state_hash()).is_err());
        let mut state = State::Fresh;
        state.start(|| RaceAuthority::init(&config, 42)).unwrap();
        assert!(state
            .call(|a| a.tick(Time::at_tick(2, 60).unwrap()))
            .is_err());
        assert!(state.call(|a| a.checkpoint()).is_err());
    }
}
