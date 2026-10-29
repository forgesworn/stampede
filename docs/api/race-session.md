# race-session API

Stampede's multiplayer layer (SPEC section 10): lobby, a host-authoritative
race protocol with client prediction, over Fray's shared transport
(loopback hub for tests, QUIC over `decented-net` for real). Deps: `race-core`,
`decented-net` (Cargo alias for `fray-net` in `../forgesworn/fray`), bincode 1.3, serde,
log, rand 0.8, rand_pcg 0.3, getrandom 0.3. No threads of its own: you drive it
with explicit `now_ms` (milliseconds on any monotonic clock).

```
use race_session::*;
```

## Pick the path, then drive one trait

Whatever the mode, the race screen reads a `&RaceView` from a `RaceDriver`:

```rust
trait RaceDriver {
    fn update(&mut self, now_ms: u64, input: Input);   // once per rendered frame
    fn view(&self) -> Option<&RaceView>;                // this frame's state
    fn results(&self) -> Option<&ResultsView>;          // Some once the race is over
}
```

Implemented by `LocalRace` (offline) and `ClientSession` (online, including the
host's own player). So the app has **one** race screen.

### Solo vs bots, training (no network)

```rust
let cfg = RaceConfig::quick(TrackId::Savanna, 5, BotLevel::Normal, seed);   // race_core
// or RaceConfig::training(LessonId::RunSteer, RacerKind::Human, seed)
let mut race = LocalRace::new(cfg, 0);     // 0 = your racer index
// per frame:
race.update(now_ms, input);                // steps World at 60 Hz from frame times
let v = race.view().unwrap();              // draw v; play v.events
if let Some(r) = race.results() { /* podium / results screen */ }
race.set_paused(true);                     // pause menu
race.world();                              // &World (lesson state, tunables)
race.record_ghost(true);                   // Time Trial: before the first update
race.ghost(); race.take_ghost();           // Some(Ghost) once your racer finished
```
The grid countdown starts on the first `update`. With `record_ghost(true)`
the ghost is sampled after every world tick inside `update`, so it is the
same at any frame rate (tested at 33 ms frames with stalls against a per-tick
recording); keep the best per track (`Ghost::to_bytes`) and draw
`Ghost::from_bytes(..)?.pose_at(view.world.race_time())`. Frame steps are capped at 100 ms
and 12 ticks, so a stalled frame cannot spiral. The sim keeps running after the
finish with every racer under bot control (something for the camera to follow).
`LocalRace` events are the world's own, **including `Footstep`**.

### Host a room (Online code + Nearby at once)

```rust
let net = race_session::net_config(data_dir);               // app tag "stampede"
let id  = race_session::load_identity(&net.data_dir)?;       // key in data_dir
let quic = race_session::host_room(&net, &id, "Friday race");   // QuicHost
// quic.is_ready() -> quic.code() = the room code to show; the same room is
// advertised to Nearby browsers on the LAN.
let cfg = HostConfig { track: TrackId::Dune, laps: 3, bots: 5, bot_level: BotLevel::Normal,
                       ..HostConfig::default() };
let (mut host, mut me) = host_with_local_player(Some(Box::new(quic)), cfg, "Ann", 2);
// no remote side (practice with a lobby, or Nearby off): pass None.
```
`host_with_local_player` joins the host's own player through a zero-latency
loopback beside the remote transport (`MuxHostTransport`). `me` is a normal
`ClientSession`: it appears in the lobby, readies up, races with prediction.

Per frame (host app), in this order:

```rust
host.update(now_ms);                 // read the net, step due world ticks, send snapshots/events
me.update(now_ms, input);            // the host's own player (input ignored outside a race)
```

Lobby controls (host UI; lobby phase only, return `false` otherwise):
`host.set_track(TrackId)` (race tracks only: not Training, not an arena, not bare Custom), `set_laps(1..=5)` (1..=9 for Custom), `set_bots(0..=7)`,
`set_battle(Option<BattleSetup { arena, rules }>)` (`Some`: the next race and its
rematches are a battle on an arena, see below; `None`: back to racing),
`set_bot_level(Easy|Normal|Hard)`, `kick(seat_id)`, `start_race(now_ms)` (force,
needs a human, and for a battle two participants incl. bots: see `can_start()`,
which the lobby should use to enable Start; auto-start and rematches obey it too), `to_lobby()` (from the results, to change settings),
`shutdown()` (tells everyone, drops links; call on leaving). Read-only:
`phase()` (`Lobby | Racing | Results`), `lobby_view()`, `world()`, `results()`,
`config()`, `battle()`, `stats()`, `rematch_ends_at()`, `human_count()`, `rtt_ms(seat)`, `can_start()`.

Guests' battle state: the client merges the reliable `FeatherLost` / `FeatherStolen` /
`Out` / `BattleOver` events over the newest (lossy, often older) snapshot, so
`view.world.battle` is never behind the events in `view.events` (right winner,
right "N left"). `invuln_ds` is clamped to 25 (the snapshot sends u8 centiseconds).

Starting: with `auto_start` (default) the race starts the frame every human is
ready.

### Dedicated test host and benchmark

`host.set_le_mans_start(bool)` is lobby-only and selects the shared playable
run-and-jump rule for subsequent races. The app enables it for phone hosts;
headless hosts enable it by default.

`HostAuthority::new_headless(transport, cfg)` creates an authority without a
local player. It permits eight bots and explicitly started bot-only races.
Automatic starts and rematches require a human; when the last human leaves a
running race, bots carry on. The ordinary constructor and phone-host path retain
their existing human requirement and seven-bot setting limit. A network authority
also needs `net.host_has_player = false` so all eight seats are available remotely.

From the Stampede workspace:

```sh
cargo run --locked -p race-session --bin stampede-headless -- --bench --bots 8 --rooms 1 --seconds 5
cargo run --locked -p race-session --bin stampede-headless -- --smoke --players 8
cargo run --locked -p race-session --bin stampede-headless -- --lan --test-identity --name "Fray test host" --players 2 --bots 6 --laps 1 --seconds 300
cargo run --locked -p race-session --bin stampede-headless -- --migration-host --test-identity --data-dir "$(mktemp -d)" --standby <PUBKEY_HEX> --players 7 --bots 1 --laps 1 --seconds 300
cargo run --locked -p race-session --bin stampede-headless -- --online --test-identity --data-dir "$HOME/.stampede-keep" --quic-port 47800 --status 192.168.1.5:8780 --seconds 0
```

`--online` is a Keep test host: a `--lan` room that is also open to phones anywhere by
its six-letter room code, through the relays (`--relay wss://URL`, repeatable; the
default is the app's list). `ready` is printed once a relay has been connected for
1.2 s, with the code, the relays and the announced addresses. The room stays open between
races: 30 s after the results with no rematch, or as soon as everyone has left, it
returns to the lobby so the code can be joined again, and a `phase` line says so.
`--quic-port` fixes the UDP port; `--advertise IP:PORT` (repeatable) also announces a
router or VPN port forward, and `--advertise-only` announces nothing else. A race starts
once `--players` humans are in and every human is ready, with bots filling the grid; the
default is 1 with `--online` (8 with `--lan`). `--seconds 0`
runs until SIGTERM or SIGINT, which close the room cleanly and exit 0. `--status ADDR:PORT`
serves a status page (and `/status.json`) with the code, the phase, who is in and a New
code button (`POST /new-code`, which disconnects the players); it shows the room code to
anyone who can reach it, so bind it to the home network only. It answers only requests
whose `Host` is an IP address or `localhost` (or the name given by `--status-host`), so a web
page cannot reach it through a DNS name of its own. If a new room cannot be hosted (a fixed
port still busy), the page says why and the Keep keeps retrying until it can.

#### Invites (`--data-dir`)

With `--online`, `--data-dir DIR` makes the Keep keep its identity and an invite
table in `DIR`, and turns on the invite routes of the status page. Without it the
Keep is as before: a new identity every start, joined by its room code only.

- **Identity.** `DIR/identity.key` (the runtime key, written by Fray) and
  `DIR/persona.key` (the synthetic persona behind `--test-identity`) are created on
  first use, mode 0600, in a directory made 0700. The attestation is signed again at
  each start. Invite links name the Keep by its runtime key, so a Keep that loses
  `DIR` loses every invite (and gets a new key). `ready` prints `host_pubkey`.
- **Invite table.** `DIR/invites.json`, written atomically (a temp file in `DIR`,
  mode 0600, synced, then a rename, then a sync of `DIR` so the rename survives a
  power cut). A file that cannot be read does not stop the Keep: it starts with
  invites off (room-code play works), logs an `invites_off` line with the reason,
  and the status page shows a red line, "Invites are off: invites.json couldn't be
  read: <reason>. Fix or move the file and restart." The file is never replaced by an
  empty table. The same goes for a damaged `identity.key` or `persona.key`: invites
  are off and the run uses a temporary identity (the line says so); the damaged key
  file is never overwritten. Each invite has `id` (hex of
  `InviteId::of(secret)`), `kind` (`guest` or `personal`), `secret` (hex, needed to
  hold the invite again after a restart), `holder` (personal: the persona public key,
  hex; the form takes an npub or hex; guest: empty until the first person uses the
  pass, then that person), `label` (at most 40 characters), `created_at`,
  `expires_at`, `max_uses` (none for both kinds now; a pass from before, single-use, becomes its
  user's on load), `uses`, `last_used_at`,
  `used_by` (the last 16 `{persona, name}`), and `revoked`.
- **Guest pass.** 24 hours. The label is optional. It works for the first person who
  uses it: on the first admission `holder` becomes that joiner's verified persona, and
  from then on it behaves like a personal invite for that persona (the same persona
  may rejoin any number of times until it expires; anyone else is refused as
  `someone_else`). It is not removed from the room at first use, so a blip that makes
  the guest paste the link again does not lock them out.
- **Personal invite.** Bound to one persona: the joiner's verified persona
  (from the My Signet approval the host checks on every join) must equal `holder`,
  or the peer is turned away. Unlimited uses until it expires: 30 days by default, 1 to
  365 in the form's `days` field.
- **What the room holds.** On start, after every New code, and whenever the table
  changes, the room's host holds exactly the live invites (not revoked, not
  expired, uses left): `HostHandle::add_invite` for the new ones,
  `remove_invite` for expired and revoked ones. A sweep every 30 s catches expiry.
- **Admission.** After the persona check, a host gate (see below) is told which
  admission the joiner proved. Room-code and LAN joins pass untouched. An
  invite join is checked against the table: unknown, revoked, expired, or the
  wrong persona (a personal invite's holder, or the first person who used a guest
  pass) is disconnected before the game sees it (not banned:
  the person can come back with a right invite), and the use is
  written to disk before the joiner is let in (a use that cannot be saved is
  refused). Revoking an invite also disconnects the players it let in. An expired
  invite does not remove a player already in. The turned-away client sees the host
  leave; the reason is in the Keep's log only.
- **Events** (one JSON line each): `invite_created`, `invite_admitted`
  (`invite`, `kind`, `label`, `persona`, `uses`, `exhausted`), `invite_refused`
  (`invite`, `persona`, `reason`: `unknown`, `revoked`, `expired`, `used`,
  `someone_else`; `used` only for an old single-use entry), `invite_revoked`,
  `invite_kick`, `invite_add_failed`, `invites_off` (`reason`). Secrets are never
  logged.
- **Status page.** `GET /` has an Invites section (the list, a New guest pass
  button, a link to the forms) and states "A guest pass works for the first person who
  uses it, until it expires." A guest pass shows as "unused" or "used by <name>".
  `GET /invites` is the same page with the forms, and does not reload while
  you type. `POST /invites/guest` (`label`), `POST /invites/personal` (`persona`,
  `label`, `days`) and `POST /invites/<32 hex id>/revoke` take
  `application/x-www-form-urlencoded` bodies of at most 4 KB, behind the same `Host`
  allow-list and same-origin (`Origin`) check as New code. A new invite answers
  `303` to `GET /invites/new/<id>`, which shows its link in a read-only text box
  (not in the list) while the invite is live and, for a guest pass, still unused; a
  refresh shows the same invite and makes no other. The page says "Anyone on your
  home network who opens this page can see live invite links."
  `/status.json` gains `invites_enabled` and `invites`: per invite `id`, `kind`,
  `label`, `holder`, `holder_short`, `created_at`, `expires_at`, `expires_in`,
  `uses`, `max_uses`, `last_used_at`, `used_by`, `status` (`live`, `used`, `expired`,
  `revoked`), `usage` (`unused` or `used`) and, for a live one, `link` (the page is
  for the home network only). `invites_problem` is the reason invites are off, else null.
  Without `--online` and `--data-dir` the routes are 404 and the page says invites need them.

```sh
# make a guest pass from a script, and read its link
curl -s -X POST -d 'label=Cousin' http://127.0.0.1:8780/invites/guest >/dev/null  # 303 to /invites/new/<id>
curl -s http://127.0.0.1:8780/status.json | jq -r '.invites[] | select(.status=="live") | .link'
```

The benchmark completes races with eight bots and zero humans, advances a virtual
game clock, and measures native execution with a monotonic clock. JSON reports
per-room update cost per advanced tick, p50/p99, the combined room burst, completed
races and ticks. A 250 ms warm-up is excluded from cost samples, with a bounded
16,384-sample reservoir. It finishes each current race after the requested wall
duration. It includes simulation and authority bookkeeping, without network
clients or graphics; it does not establish network fan-out or a safe room limit.
Linux memory fields are process RSS, peak RSS and growth, including benchmark
bookkeeping and allocators; they are null on systems without those proc fields.

The smoke check connects eight clients through real localhost QUIC and the
unchanged signed-attestation verifier. It checks eight human lobby seats, no
host-player seat and the authenticated host key. Both sides use fresh synthetic
signed identities held only in memory.

The bounded LAN mode requires `--test-identity` and prints its room name and
ephemeral public host key when ready. On each phone choose Multiplayer → Nearby,
join **Fray test host**, then Ready. CLI settings determine the race; it starts
when the requested human count is present and every human is ready. This host
disables online relays, STUN and router mapping, so its code cannot be looked up
through online Join by code. Test identities satisfy the existing verifier; they
do not define production Keep ownership or service authorisation.

The migration host mode (`--migration-host`) is a headless host with no player
seat for the host-migration kill trial. It requires `--test-identity`,
`--data-dir DIR` (its network state; the trial passes a fresh temporary
directory) and `--standby PUBKEY_HEX` (the runtime key of the device to make
the backup). `--bots` defaults to 0 in this mode; players plus bots is at most
eight. It waits for `--players` humans with the standby among them, arms the
room over the room's own messages (`HostArming`), switches every human to
protected play (`HostSwitch`), starts the race in the first armed batch and
then runs the armed original as the app's host frame does (`PlayerHost` and
`LivePrimary` on a `RunnerClock`, probing on every poll), until it is killed or
`--seconds` pass. Session time is held from the frozen lobby to the switch.
It prints one JSON line per step, flushed at once, which a test reads as its
process-control barrier:

- `migration_ready`: `addr`, `token`, `room_code`, `host_pubkey`, `pid`;
- `armed`: `room_code`, `session`, `standby`, `members`, `humans`,
  `host_seats` (0), `host_member` (false), `lan_token_rotated`,
  `all_protected`, `unprotected`, `base_ms`;
- `racing`: the armed race is one second past GO (`racing_commits`, `tick`,
  `racers`, `logical_ms`);
- `race_completed`, `paused`, `stopped`.

It exits non-zero if arming or the switch fails or any human is unprotected.
A lobby that fills to eight humans closes its listener, which rotates the LAN
admission token (`lan_token_rotated: true`); `Switch` carries the listener's
current token, so the players still re-dial and are protected.

The kill trial (`tests/migration/subprocess_kill.rs`, in the `migration`
target) runs ten independent trials, each with a fresh host process and fresh
synthetic identities in temporary directories: a full lobby of eight humans
driven in process as the app drives them (the standby device with its own
seat, seven remote humans), each device's driver loop on its own thread as on
its own phone, and no bot, so the LAN token has rotated (the
trial requires `lan_token_rotated: true`). Each trial waits for `armed` and `racing`
and for every client to be racing, then sends SIGKILL (`Child::kill`) and
times, on the monotonic clock, each client's authoritative `Resume` from the
successor. It asserts a resume within 1000 ms for every client, the same room
code, session and match (`RaceConfig`), stable seats, no host seat or member,
and that the race completes under the successor, and prints one line per
trial and one summary line. Each trial is a real-time race (about 40 s), so
it is ignored by default. Its acceptance runs use a release build, as phones
run (the default test build's per-batch signing and hashing is slow enough to
stall the host's frames):

```sh
cargo test --release --locked -p race-session --test migration sigkilled -- --ignored --nocapture
```

Run it on an otherwise idle machine. A frame stall in the host process or the
standby device over the 250 ms game-clock budget (`MAX_GAME_GAP_MS`) pauses the
armed room as skew before the kill, or ends the successor after it; the trial
then fails with the host's `paused` line (`max_frame_gap_ms`, `committed_ms`,
`game_ms`) or the device error.

This is a manually launched native development process. It has no Keep picker,
remote launch API, backup access or production keys. Real Keep hosting alongside
private Signet data requires the separate sandbox and control design.

### Custom tracks online

`host.set_custom_track(&CustomTrack) -> Result<(), String>` validates with
`custom::to_def`, selects Custom and the authored laps, clears readies and sends
the canonical bytes to every human. New joiners receive the selection and bytes
too. `LobbyView::custom: Option<u64>` carries the selected hash; `RaceStart`
binds its world to that same hash. `host.custom_track()` and
`client.custom_track()` expose the checked track (the client returns it only
when it matches the current selection).

Transfers use at most four reliable chunks of at most 900 bytes and 2,830 bytes
in total. Deserialisation rejects oversized byte-vector lengths before reserving
capacity. Reassembly accepts identical duplicates and out-of-order delivery;
inconsistent headers, conflicting duplicates, bad sizes, malformed canonical
bytes, wrong hashes and invalid geometry never produce `TrackReady`.
`client.downloading_track()` and `client.track_error()` drive the guest's status
and disabled Ready button. The app may save `client.custom_track()` in My Tracks.

`can_start()` is false until every human, including the host's loopback player,
acknowledges the current hash. Manual Start, auto-start and rematches obey this
gate. Checked geometry belongs to each session, so another registry installation
cannot change a room's world. Returning to the lobby keeps the checked track;
changing to another custom track invalidates acknowledgements and transfers it
again. Choosing a built-in track or a battle clears the custom selection.

**Seat ids** (`SeatView::id`): humans get `0..100`, the lowest free id after the
last one given (kept while seated; not reused at once). Bots are
`BOT_ID_BASE (100) + racer index`, the racer the bot drives: in the lobby the
index it will take at the start (humans first, in join order), so a bot's id
shifts when a person joins or leaves the lobby; from `RaceStart` on it is
fixed, and a person who leaves mid-race reappears as bot `100 + their racer
index`. Key people on `id`; key bots on `SeatView::racer()` (`Some(id - 100)`
for bots, `None` for humans) or on `world.cfg.racers` during a race. Seats = humans in join order, then `min(bots, 8 - humans)` bots (the
bot *wish* is kept: when a person leaves, the bot comes back). **A person
joining a full lobby takes a bot's seat**; a ninth person is refused
(`Closed("lobby full")`). The room is closed to newcomers (`set_open(false)`)
from the race start until the host returns to the lobby.

**Leaving mid-race.** The bot that takes over drives the racer on, so the race is
not emptied, but its result is not the player's: unless the player had already
finished, the results list them as Left, after every finisher and every bot, with
no time, best lap or points, and everyone behind them moves up a place
(`view::rank_leavers_last`; `HostAuthority::left_racers()`). The host tells every
client with `ToClient::Left { racer, finished }` (variant 11, appended like
`KeepPass`, so no `PROTO_VERSION` bump: an older app drops it and shows the row as an
unplaced racer). The client keeps `ClientSession::left_racers()` for the results
screen and shows `ClientSession::notice()`, "<name> left", for `NOTICE_MS` (4 s).
A player who finished, who is out of a battle, or who is eliminated keeps their
place and is not listed. The list travels in the migration checkpoint, whose schema
is now 2: arming a backup needs the same app build on the host and every device,
and a device on another build refuses with "run different versions of the app"
(`migration::version_refusal`), so a room is never armed with a backup that could
not restore it.

### Join by room code

```rust
let net = race_session::net_config(data_dir);
let id  = race_session::load_identity(&net.data_dir)?;
let link = race_session::join_code(&net, &id, "ABCDEF");           // QuicClient
let mut s = ClientSession::new(Box::new(link), "Ann", 3);          // name, character id
// per frame:
s.update(now_ms, input);
match s.phase() { ... }
```

### Join by invite

A Keep with `--data-dir` hands out invite links (see "Invites" above). A link
holds the Keep's runtime public key and a 32-byte secret, both as hex, and **no address**:

```text
stampede://keep/<64 hex: the Keep's runtime public key>/<64 hex: the invite secret>
```

The prefix is exactly `stampede://keep/`, the two parts are 64 hex digits each (either
case when read, lower case when written) separated by one `/`, and nothing follows.
`parse_invite_link` trims surrounding whitespace and returns friendly text for
anything else. It is pure: it opens no connection.

```rust
use race_session::{parse_invite_link, format_invite_link, join_invite, ClientSession};
let (host_key, secret) = parse_invite_link(pasted_text)?;      // ([u8; 32], [u8; 32])
let link = join_invite(&net, &id, &secret, host_key);            // QuicClient
let mut s = ClientSession::new(Box::new(link), "Ann", 3);
// text for a link: format_invite_link(&host_key, &secret)
```

`join_invite` finds the Keep on relay tags derived from the secret, and pins the Keep:
only a host whose welcome is signed by `host_key` is joined, otherwise the join fails
before the player says who they are (`QuicClient::failed_join`). The session then asks
the player to confirm the host as for a code join. Failure is the same
"Code not found" text as a code that does not exist when the invite was revoked or
expired (the Keep stops answering), and the host leaving when it refused the player
after the handshake (an invite that belongs to someone else, or a revoke).
The joiner must send a My Signet approval as for any join. Other exports for the
Keep's side: `Admission`, `InviteId`, `JoinInfo`, `QuicHost::set_admission_gate`
(a closure run after the persona check that may refuse, with the joiner's verified
persona and which admission it proved) and `QuicHost::peers_admitted_by`. A host with
no gate, such as the phone app's, admits exactly as before.

**Return pass.** A Keep can send a player it has admitted (by code or on the local
network) the same pair a link carries, so the app can save the Keep to My Keeps
without the player holding a link: `HostAuthority::send_keep_pass(peer, KeepPass { host_key,
secret })` (`peer` from `peer_of_seat` or the transport's `Joined` event), once. The
client surfaces it with `ClientSession::take_keep_passes()`; the app saves it like a
completed invite join and joins nothing. It is `ToClient::KeepPass`, the last variant
(index 10), and `PROTO_VERSION` stays 11: an older app cannot decode a variant it does
not know and drops the message, where a version bump would make it refuse the host.
A pass naming a host other than the one the link proved (QUIC) is dropped.

### Nearby (LAN)

```rust
let mut browser = race_session::browse_nearby(&net);     // decented_net::LanBrowser
let rooms: Vec<LanRoom> = browser.poll();                // name, code, player_count, max_players, addr
let link = race_session::join_nearby(&net, &id, &rooms[0]);
let mut s = ClientSession::new(Box::new(link), "Ann", 3);
```
Hosting once is both: `host_room` gives the code and the beacon.

### Battle online ("Feather Frenzy")

```rust
host.set_battle(Some(BattleSetup::new(TrackId::DustBowl)));          // default rules: 3 feathers, 3 min
host.set_battle(Some(BattleSetup { arena: TrackId::MempoolPlaza,
    rules: BattleRules { feathers: 4, time_limit: 240, ..BattleRules::default() } }));
// LobbyView::battle shows it to everyone; `track` / `laps` are then unused.
```
`HostConfig` is unchanged (the battle choice lives on the host, so app code
that builds `HostConfig` literals still compiles). Rules are clamped (feathers
1-9, max feathers up to 15, 30-900 s, invulnerability up to 5 s, egg respawn
0.5-20 s); a non-arena is refused. `RaceStart.config.mode` is
`RaceMode::Battle(rules)`, so every client builds the same `World::battle`.
Snapshots carry the battle (`Snapshot::battle: Option<BattleNet>`: clock,
feathers, invulnerability, time since out, hits, hits taken, out order, done;
50 B typical, 90 B worst, never dropped by the budget) and the client writes it
onto `view.world.battle` every frame, so the HUD and renderer read the same
`BattleState` online and offline (feathers, `flashing(i)`, `fade(i)`,
`ranking()`). `FeatherLost`, `FeatherStolen`, `Out` and `BattleOver` are
reliable events like hits. Your own racer, once out, is no longer predicted
(the host froze it; it is drawn from snapshots). `ResultsView` rows are in the
battle ranking (`time` / `best_lap` are `None`; `points` by place as usual).

## ClientSession

```rust
ClientSession::new(Box<dyn ClientTransport>, name: &str, character: u8)
update(now_ms, local_input)
phase() -> &ClientPhase     // Connecting(text) | Lobby | Racing | Results | Failed(text)
                            // | Closed(reason) | HostLeft | Disconnected(reason); phase.is_dead()
host_identity() -> Option<&HostIdentity>      // QUIC: who the host proved to be (name, npub, npub_prefix())
lobby() -> Option<&LobbyView>                 // seats {id, name, character, ready, bot, host}, track, custom, laps, bots, bot_level, phase, battle
                                              // SeatView::racer() -> Option<usize> (bots)
my_seat() -> Option<u8>; my_lobby_seat() -> Option<&SeatView>
set_character(u8); set_ready(bool)            // lobby
set_rematch_ready(bool)                       // results screen
rematch_ms_left(now_ms) -> Option<u64>        // the cancellable 3-2-1, while it runs
leave()
view() -> Option<&RaceView>; results() -> Option<&ResultsView>
rtt_ms(); clock_offset_ms(); host_now(now_ms); stats() -> &ClientStats   // hard_snaps = corrections over 10 m
```
Flow: `Connecting -> Lobby` (after `Welcome`) `-> Racing` (on `RaceStart`: `view()` exists
from this frame, the 3-2-1-GO runs from the synced clock) `-> Results` (host sent
`Results`; `view()` stays, frozen to the results' places) `-> Lobby` (host
`to_lobby`) or `-> Racing` (rematch). Show the "Join X's game?" confirm from
`host_identity()` when it is `Some`; backing out is `leave()`.

## RaceView (what you draw)

```rust
pub struct RaceView {
    pub world: World,          // display state, see below
    pub you: usize,            // your racer index
    pub hud: Hud,
    pub standings: Vec<usize>, // racer indices, 1st first
    pub events: Vec<Event>,    // since the previous update(): sounds / FX
    pub correction: Vec3,      // online: visual offset already applied to your racer
    pub over: bool,
}
pub struct Hud { place, racers, lap (1-based), laps, final_lap, race_time, countdown (s to GO),
    speed, coins, item, item_uses, heat, overheated, drift_tier, boosting, wrong_way,
    finished, finish_time, ping_ms, lesson }
view.own() -> &Racer;  view.others() -> impl Iterator<Item = &Racer>
```
`view.world` is a full `race_core::World`, so the code that maps sim state to
`FrameScene` is the same online and offline: `world.racers` (index = seat; yours
last predicted, plus the easing offset; others interpolated), `projectiles`,
`hazards`, `coins[i].alive` and `targets[i].alive` (coins sit at their home
position), `loose_coins`, `phase`, `tick` (so `race_time()` /
`countdown_left()` work), `cfg` (names, characters, laps). **Online it is a
shadow: never call `step` on it.** Other racers carry what `RacerNet` carries
plus `s / lateral / progress / track_idx / ground_y` projected locally; their
`finished_at` is the host's finish time once the `Finished` event arrived.
`hud.race_time` / `countdown` are continuous (not tick-quantised). Item kind of
other racers is not sent (`flags` bit only).

Events: `Countdown{n}` / `Go` come from the synced clock; `Footstep` for others is
derived from their gait phase; for **your own racer** the motion events (jump,
drift start/tier, boost, boost pad, trick, landing, wall hit, fell, respawn,
overheat, footstep) and the launch (`StartBoost` / `Stumble` at GO) are raised by
the prediction the instant they happen and the host's copies are dropped (a
launch the host decided differently, e.g. after lost countdown inputs, is still
played when its event arrives). Host events whose racer/target indices are out
of range are dropped, so every index in `view.events` is valid for `view.world`;
a `Results` with a racer index or place out of range is ignored likewise. Everything else (hits, bashes, items, coins, laps,
finishes, shields, laser zaps, lightning, race over) is the host's, reliable.
The host rate-limits `WallHit` to 4 per second per racer and never sends
`Footstep`, `Countdown`, `Go`, `Gate` or lesson events.

## Timeline, prediction, interpolation

World tick `t` happens at host time `start_host_ms + t * 1000 / 60` and consumes
input number `t + 1`. `RaceStart.start_host_ms` is `now + 700 ms`. A client sends
input `n` at about `start + (n-1)/60 s - (RTT/2 + 2 ticks)`: it runs ahead of
the host by half an RTT plus the 2-tick jitter buffer, so inputs normally wait
2 ticks in the host's queue. Every datagram carries the last 5 inputs (with
consecutive numbers). A tick whose input never arrived repeats the seat's last
applied input **as it was**: buttons are held levels that the sim edge-detects,
so a repeat can never fire a press, and a button held through the gap does not
fire again when the real inputs resume (clearing it would). Inputs arriving
after their tick are dropped, except that a jump/item/bash press in a late copy
that the repeat missed fires on the next tick, once (duplicates and later
copies cannot re-fire it). A seat silent for 6 s mid-race is dropped to a bot. `ack_seq` counts applied tick slots (real or
repeated), so it equals the snapshot's `tick` while the seat is stepped.

On every snapshot the client resets to `own`, replays the unacked inputs
(`Racer::integrate`, the same quantised `Input::pack/unpack` form the host sees)
and puts the position difference into a visual offset that eases to zero over
`correction_ms(d)`: 150 ms up to 1 m, 20 ms more per metre, at most 250 ms
(`CORRECTION_MS`, `CORRECTION_MAX_MS`). Only a difference over 10 m
(`SNAP_DISTANCE`: respawns, teleports) snaps, so a hit or bash that arrives half
an RTT late slides. The countdown launch and coin pick-up are replayed with
race-core's own `Racer::countdown_step` and `Racer::reaches_coin` /
`pick_coin`, the same code `World::step` runs (they are the only world effects
that change your own motion on a clear track). Bumps, bashes, eggs and hurdles
still arrive via the snapshot. A host-sent `own` racer is sanitised before the
replay (drift tier clamped), and a `RaceStart` whose `you` is not a racer of the
world it builds is ignored. Others are drawn
at least 100 ms behind the host's timeline (`tick` of the snapshots against the synced
host clock), further when snapshots arrive late: the 97th percentile of how late the
last 4 s of snapshots were, plus a snapshot gap, at most 350 ms
(`interp_target_ms`). The drawn time is a clock of its own that follows that target
at 0.8 to 1.08 of real time, changes speed gradually, slows when it nears the newest
snapshot and jumps only when more than 500 ms off, so a late packet, a burst or a
clock-sync round never makes the others jump. Past the newest snapshot they are
extrapolated up to 100 ms; teleports over 20 m are not interpolated. Eggs, the
frogs and the rose sellers likewise; coins, targets and dropped coins come from the newest snapshot; cacti
from the nearest snapshot. `ClientStats` counts the frames drawn past the newest
snapshot and the delay in use; `tests/jitter.rs` measures it all under a modelled
mobile-data path.

Clock: Twang's ping/pong sync (5 pings per round, lowest-RTT sample), every
2.5 s. `host_now = local + offset`. The first round starts with the lobby; if
it has not completed when `RaceStart` arrives (an instant ready at a high RTT),
the race view waits for it (well inside the 3 s countdown) and the host repeats
the seat's idle input meanwhile.

## Migration bootstrap in development

The app drives these APIs from its online slot (`app/src/online/armed.rs`; see
"App wiring" in `docs/host-migration.md`). Arming is LAN-only (a room with no
relays). Armed rooms need game protocol 11 (see below). Real
host-process termination trials and updated-phone recovery are still required
before migration acceptance.

The app contract, in short:

- One `OnlineSession` per device owns every migration object in one role
  (arming, switch, `LivePrimary` + `PlayerHost`, `RemotePlayer`, `LiveStandby`,
  takeover, `LiveSuccessor`). Screens read `me()` (the active client: the
  ordinary session, the armed route's, the recovering or successor seat's),
  `authority()` and `migration_state()`; they change things only through
  `client_command` (run inside the next armed frame's client transaction),
  the host controls (queued into the next staged batch) and
  `choose_backup(seat)`, `answer_consent(yes)`, `cancel_arming()`.
  A `client_command` issued while the role is `ClientArming`, `HostArming`
  or `HostSwitch` is not deferred: it reaches the frozen lobby, and if the
  switch succeeds it is dropped. Host controls are refused while arming or
  switching.
- `pump` runs every frame, whichever screen is on top, in the documented
  order: the host's authority and `HostArming`, then `HostSwitch`, then the
  own seat, `PlayerHost::poll`, `LivePrimary::probe`, `LivePrimary::poll` (`publish` on
  `Committed`, `certify` on `Membership`), one `stage`, `flush`; a client's
  `ClientArming`, `ClientSwitch`, then `RemotePlayer::frame` and
  `watch_standby` → `recover` → `step` → `into_resumed`; the standby device's
  `LiveStandby::poll`, repeated until everything queued on the pair link is
  installed, then its watch (promote once `expired()` or the original
  `Left`), then its own seat, then the takeover and the successor.
- `LivePrimary::poll` drops pair-link frames Fray refuses as `Stale`/`Gap`
  (`refused_frames()`), owes a waiting batch a resend every
  `RETRY_MIN_GAP_MS` (150 ms, `retries()`), and reads Fray's standby
  watch: while it reports `Paused`,
  `backup_lost()` holds, a soft pause the app shows as "Backup lost — the
  room is paused" that clears by itself when the watch reads `Live`, with
  no game-side tolerance. Hard latches (`failure()`) stay for genuine
  faults, and none is reported after `Promote`. The driver calls
  `LivePrimary::probe` every frame before `poll` (forgetting costs only a
  false soft pause); `LiveStandby::poll` probes by itself, so a forgetful
  standby driver cannot cause a false takeover; Fray counts the original's
  late answers (`LiveStandby::late_answers()`) instead of refusing them. `activity()` (and
  `LiveStandby::activity()`) is derived from the committed checkpoint:
  `Advancing` in every phase whose timer runs (`Racing`, `Results`), `Idle`
  in the lobby and at index 0; there is no setter. `stage` sends the staged
  candidate in the same frame.
  See "Owned original runtime" in `docs/host-migration.md`.
- The standby device's player leaving keeps its standby running in the
  background (`OnlineSession::in_background`, `OnlineSlot::pump_background`,
  driven by the engine) until the room ends; its seat departs like any
  human's. `SurvivorLinks::drive` takes `Option<&mut OwnSeat>` and refuses
  `None` while that player is still a member at C, seated or seatless (kept
  by its `Leave` batch; `OwnSeat::new` accepts its `Closed("left")` client,
  and the successor's first batch retires it). A takeover is seatless only
  once the seat was retired before C.
- Session time is held from the freeze (host) or from the device's consent or
  `Setup` (client) until its switch finishes, so the armed room resumes at
  the armed initial time; a fallback to the unarmed lobby drops the hold.
  The host stages at `max(session time, armed initial time)`, the successor
  at `C + (session time - Resume)`; the client's own `Resume` rebase is
  `RecoveringPlayer`'s.

`migration::LobbyOffer::prepare` captures a lobby checkpoint and a signed
descriptor for an explicitly selected backup. Its caller must first freeze
ordinary game/client updates and drain ordered loopback effects. The offer
exposes public metadata and an initial hash; authority bytes remain private.
Runtime keys come from currently live, Signet-approved connections. The
selected backup calls `consent_lobby_offer` only after explicit UI consent.
`LobbyOffer::accept` verifies that exact consent, the unchanged native state
and current admission before producing `ConsentedLobby`.

`ConsentedLobby::into_activation` owns the admitted QUIC pair connection.
`LobbyActivation` checks actual UTC approval and live keys before releasing
image frames. On the receiving device, `LobbyStandby` owns its admitted
listener through fresh native validation; `ArmedLobbyStandby` retains the
same checks through readiness publication. Errors stop setup. All human
capability ACKs and canonical restore equivalence are still required.

The resulting `PrimaryBootstrap` and `StandbyBootstrap` retain the actual
channel, transport and deferred receive queue for the normal runner. The
standby also retains early roster-control events. No frame counter, journal or
buffer is recreated at this handoff. The app must continue ordinary credential
expiry checks and consume retained ingress before advancing gameplay.

The focused target runs actual QUIC consent/restore, expiry, disconnect and
buffered-retry checks:

```sh
RUST_TEST_THREADS=1 cargo test --locked -p race-session --test migration -- --nocapture
```

Its prepared fixture has two humans, six native bots and no host player seat.
Human capability ACKs are supplied by the fixture's synthetic runtime keys;
the fixture explicitly freezes native updates. These checks do not exercise
the app lobby flow or constitute process-kill, phone or recovery-timing evidence;
the separate subprocess kill trial (see "Dedicated test host and benchmark")
does the process kill and recovery timing on localhost.

## Wire protocol (`proto.rs`, `PROTO_VERSION = 12`)

Version 12: the Shared Cake takes the others' good power-ups and stacks them, and the
Rose sends a rose seller. `Item::Rose` is the last variant (wire index 15; items travel
as serde enum tags, so a sixteenth needs no packing change). The full own racer carries
`queue: Vec<ItemRoll>` (at most 8, bounded on decode; 1 B empty, 2 B per entry) and its
`Effects` lose `cake_boost` and `cake_mine` and gain `rose: f32`, `rose_side: i8`,
`note: ItemNote` and `note_t: f32` (+5 B, +6 after a cake). `Effects.feather` is host-only
(`serde(skip)`) and rides in the migration checkpoint's `HostOnly` beside the trump
masks. In `Effects::pack` the freed cake-boost byte now carries the rose sale timer,
so remote racers stay at eleven bytes. `PlayState.roses` (at most 8 `RoseSeller`s,
33 B each, bounded and validated on decode) is droppable: `Snapshot::budget` drops the
farthest seller, egg or cactus first, since who is being pestered also travels in that
racer's own `effects.rose`. Clients do not predict the cake or the seller: both come
with the snapshot, like a bump. The seller's shove does not need the seller: it is
`Racer::integrate`'s, from the own racer's `effects.rose` and `rose_side`, so it is
predicted. Sellers are interpolated like the frogs, matched by `owner`; one pestering
the client's own racer is drawn beside its predicted position. Battles roll neither, so the arenas' worst case only
grows by the effects (about 1,023 B of 1,024).

Version 11 marks armed host-migration rooms: remote humans reach an armed room
over session-bound player links carrying certified outputs (see
`docs/host-migration.md`). The plain message set is unchanged from version 10.

Version 10 gives five built-in tracks real hills, and custom tracks may have
them too (track format 2), so heights, grades and jump arcs differ. Version 9
makes drift a proportional power-slide, lets cake lock the buttons, and makes
jumps bigger with a glide; clients predict all of it.

Version 8 (unreleased, so it was not bumped again) restores the coin top-speed bonus
(`coin_bonus`, which clients predict) and carries the 0.4.9 gameplay changes, all
backward-compatible additions at the end of their enums:

- Drift feel: `Racer.drift` (full own racer only) gains six `u8` fields (`blend_q`,
  `carry_q`, `over_ticks`, `pending`, `pending_ticks`, `pending_charge_q`) and `Racer`
  gains `boost_tier: u8`: +7 bytes in the own racer, none for remote racers
  (`RacerNet` is unchanged). Everything is whole numbers so worlds serialise exactly.
- Knock-offs: `HitCause::Pepe` is the last `HitCause` variant, and `Event::CoinsTaken
  { from, to, count }` the last `Event` variant (a rug pull moved every coin of `from`
  to `to`; the app flies them over about 0.8 s). `Event::Hit` for rugs, Pepe and the
  printer now means the rider was unseated. `Effects.knock` (`KnockKind`) is one byte
  in the full own racer (+1 byte). For remote racers it costs nothing: the last byte
  of `Effects::pack` is the trump timer (0..=99 deciseconds) or, when no rocket is
  running, `100 + KnockKind code`, so the same 11 bytes show the cause. `Frog.paused`
  is `serde(skip)` host-only bookkeeping (zero bytes).
- Item button timer: `Effects` (full own racer only) gains two bools, `cake_mine` and
  `scramble_mine` (version 12 replaces `cake_mine`), so a client can tell its own Shared
  Cake / Egg Scrambler from one it merely suffers
 (+2 bytes; nothing for remote racers). Everything else the own item
  button needs is already in the own racer (`item`, `item_uses`, the effect timers,
  `shield_t`, `magnet_t`, `mega_t`) and `PlayState::scramble`; the app counts it down
  with `Racer::active_item`. The host rejects an item press while its racer has a
  timed item running (`World::use_item`; no event, nothing consumed). The client does
  not predict an item use: the button follows the snapshot (about one round trip).
- The snapshot worst case is the battle arenas' fixed part (eight racers with every
  effect, full wallets, widest battle counters, no eggs): about 1,009 B of 1,024 (Mempool Plaza; 1,004 B on Dust Bowl)
  after these changes (1,000 B before; the structural change is +3 B, the rest is
  the simulated racer state the test samples). Ordinary tracks fill to the cap by
  dropping the farthest eggs and cacti.

Version 7 adds the Laser Trump item. `Item` and `HitCause` each gain a last
variant (`Item::LaserTrump` is wire index 14), `Effects::pack` grows to eleven
bytes (the last carries the trump timer in deciseconds, so remote racers show
the rocket), and `RacerNet` is one byte larger. The host's per-rocket victim
bitmask (`Racer::trump_hit`) is `serde(skip)`: it never leaves the host. Older game protocols
are refused. Clients predict the rocket for their own racer: `Racer::integrate`
autosteers and speeds it up from the snapshot's exact `effects.trump`, while
hits, the wind-down and the exit boost's start are host-only world steps.

Version 6 adds player-deployed rug positions/owners and exact remount protection
to full-own state. Recovery now requires Jump, printer aim is forward only,
bash has been removed, and races wait for every connected human finisher.
Every participant must use this game protocol; Fray transport remains v6.

Version 5 added the money printer, guided dollar bills, dismounted jockey offsets
and at most two rug states, plus the shared Le Mans start rule. Full-own effects stay exact for prediction; remote
presentation is packed into ten bytes. Snapshot budgeting still retains nearby
projectiles within the existing 1,024-byte cap. Older game protocols are refused.

Version 4 adds current-race wallets (`u32`), effect timers, shared frog/scrambler
state, bounded egg bits and visual attire. Version 3 added custom hashes in `LobbyView` and `RaceStart`, `TrackData` and
`TrackReady`. A peer with any other version is refused at `Hello`. Version 2
added battles and arena tracks.

bincode 1.3 with varint ints, a 4 KiB decode limit (garbage never allocates or
panics).

| Dir | Kind | Message |
|---|---|---|
| C->H | reliable | `Hello{proto,name,character}`, `SetCharacter`, `SetAttire([u8;3])`, `SetReady`, `TrackReady{hash}`, `RematchReady`, `Leave`, `Ping{id,client_t}` |
| C->H | lossy 60 Hz | `Inputs{race,seq,inputs[<=5 x 4 B]}` |
| H->C | reliable | `Welcome{proto,seat}`, `Lobby(LobbyView)` (incl. `battle` and `custom`), `TrackData{hash,idx,total,bytes<=900}`, `RaceStart{config,custom,start_host_ms,you,race}`, `Events(Vec<Event>)` (chunked under 1024 B), `Results(ResultsView)`, `Rematch(Countdown{ends_at_host_ms}\|Cancelled)`, `Pong`, `Closed(reason)` |
| H->C | lossy 30 Hz | `Snapshot{race,tick,ack_seq,flags,own: Racer,others:[RacerNet<=7],projectiles:[ProjNet],hazards:[HazNet],loose_coins,targets_alive:bitvec<=64B,play:PlayState,coins_alive:bitvec,battle:Option<BattleNet>}` |

`race` (1 byte, wraps) lets both sides drop stragglers of the previous race after
a rematch. Rematch: on the results screen everyone ready starts a 3 s
`Countdown`; an un-ready sends `Cancelled`; a departure leaves it running when
everyone left is ready (and starts it if the leaver was the one not ready); at
zero a new race (new seed, same lobby settings) begins. The results are `ResultsView { track, laps,
rows: [ResultRow{racer,name,character,place,bot,time,best_lap,coins,points(GP)}] }`.
`Snapshot::budget` keeps a snapshot at or under 1024 B: dropped coins go first,
then the eggs/cacti farthest from the recipient; own racer, the 7 others, the
coin/target bitsets, shared effects and battle state are never dropped. Tests
cover eight racers with maximum wallet values, all effects (Laser Trump included), three frogs, 512 eggs
and the widest battle counters, then budget the remaining entities below 1024 B.
Frog lists reject more than three entries before allocation; egg bitsets reject
more than 64 bytes. The full own racer carries exact effect timers for replay;
remote racers use seven decisecond timers and three purely visual attire bytes.
Names are cut to 12 characters (worst
case: all reliable messages stay under 1024 B, tested).

## Tests

`cargo test -p race-session` (loopback hub, virtual clock, per-client clock skew):
lobby/seats/3rd joiner takes a bot seat/9th refused/version check; seat ids
never wrap into the bot range over 300 joins; bot ids equal in lobby and race; a full 3-lap
race with 2 clients + 5 bots at 150 ms and 5 % loss (finishes, results and every
client's standings equal the host's); a battle on Dust Bowl with 2 clients + 4
bots at 150 ms and 5 % loss (ends, results follow the battle ranking, every
client's results, standings, feathers, hits and out order equal the host's);
battle snapshot worst case; prediction error against the host at
matching ticks (100 ms constant input: 0.0000 m; 100 ms + jitter + 5 % loss
weaving: 0.0000 m; start boost: 0.0000 m; among 7 bumping bots: worst 2.5 m,
mean 0.02 m, never a snap); starvation repeat held; a held jump across a
starved tick fires once; a press that only arrives late fires once (and a stray
duplicate later does not); your own start boost sounds at GO (150 ms link) and
once; a hostile host (bad `you`, poisoned `own`, out-of-range event indices)
cannot crash the client; easing durations; silent seat; disconnect mid-race to
bot and completion; rematch with cancel and a second race; host `to_lobby`;
snapshot worst case (every track) and event chunking; local race and
training; the host's own player through the mux; clock sync across skew.

## Things in race-core that were missing or awkward

- The doc says everything is re-exported at the crate root; `TICK_HZ`, `DT`,
  `wrap_angle`, `lerp`, `flat`, `forward`, `mix64` are only in `race_core::math`.
- A human leaving mid-race is done by setting `world.cfg.racers[i].kind =
  RacerKind::Bot(level)`; it works (kind is read every step) but is not in the doc.
- `RacerNet` carries no item kind, no `finished_at`, no `progress`/`s`;
  `apply_net` does not fill `s/lateral/progress/track_idx`, so the session
  projects them. After `RaceOver` the world keeps running, `finish()` still
  hands out `place` numbers and `standings()` keeps changing for unfinished
  racers: the host freezes `ResultsView` at the end tick and the client freezes
  the view's places to it.
- Magnet-moved coins (`CoinSpot.pos`) are not networked (only `alive`).

## Not done / open

- The grand-prix points accumulate nowhere: `ResultRow.points` is per race; the
  app owns the multi-race table.
- Sub-tick interpolation of your own racer (60 Hz steps on a 120 Hz screen).
- Hurdles (Training only) and bumps are not predicted (snapshot corrections).
- A client whose RTT changes a lot mid-race re-times its lead only at the next
  clock round (2.5 s); a burst of jitter beyond the 2-tick buffer shows up as a
  few starved ticks and a small correction.
- When your own racer finishes, the view stops predicting it and draws it like
  the others, 100 ms behind: a small visible step back at the line.
- Two presses of one button (press, release, press) that both fall inside a
  run of starved ticks reach the host as one press.
- Event indices are range-checked, but the host is otherwise trusted for game
  state (positions, places, results); only crashes are guarded against.

## Fray authority adapter

`race_session::authority::RaceAuthority` implements the synchronous
`fray_session::authority::Authority` contract over the same host rules.
`AuthorityConfig` builds the host from explicit settings and the supplied seed.
It does not call `HostConfig::default`, sample a game seed or own a socket,
signer, private key or clock. Its checkpoint contains the complete canonical
game image, config binding, completed tick and stable-player/logical-peer/RTT
mapping. Authority hashing uses Fray's authority domain; native migration keeps
its existing image encoding and hash domain.

The default features retain the native API, authority adapter and headless
binary. A guest selects `default-features = false, features = ["authority"]`;
that graph omits QUIC, Signet, signing and real-clock loopback services. A native
consumer that disables defaults only to omit the headless signal handler must
select `features = ["native"]` explicitly.

The trusted shell authenticates and admits at most eight players before
`Joined`, assigns session-stable player IDs and supplies `PeerInfo` metadata.
Only host-owned metadata may claim a local seat or RTT. Connection-local peer
IDs do not enter the authority. The shell supplies controller actions through
`Control`; a remote player's payload never becomes a controller event. Keep
passes, owner controls, signing and migration services remain shell-owned.

For each 60 Hz step, deliver ordered events with `Time::at_tick(n, 60)`, then
call `tick` with that same time. Buffer all returned commands for the step,
validate them against host-owned membership and bounds, then publish in order.
An error discards tentative commands and ends that instance. It does not promise
state rollback. Disconnects produce a subsequent ordered `Left`. Checkpoint only
after a completed tick; restore into a fresh instance using the same config.
The Rust adapter enforces its semantic/output bounds; component lifting and
platform isolation remain the runtime's responsibility.
