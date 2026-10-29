# Stampede Keep in Docker

The Keep is a Stampede host with no player of its own. It stays open, and phones
join it with a six-letter room code from Multiplayer, Join with code.

## Build and run

    ./build-image.sh          # builds the program here, then the image stampede-keep:k0
    docker compose up -d      # starts it
    docker compose logs -f    # watch it; "ready" appears once a relay has been reached
    docker compose down       # stops it cleanly

## Find the code

Open `http://<box>:8780` from any device on the home network, or run
`curl -s http://127.0.0.1:8780/status.json` on the box. The page shows the code,
what the room is doing and who is in. "New code" replaces the code and sends
everyone out of the room.

The status page shows the room code to anyone who can reach it. Keep port 8780
on the home network; never forward it on the router.

## Invites: letting a friend in without the code

The Keep keeps its identity and its invites in a Docker volume (`stampede-keep-data`,
mounted at `/var/lib/stampede-keep/data`, shared by both compose files). Invite links
name the Keep by its key, so the key has to survive restarts and image updates: do not
delete the volume. Deleting it cancels every invite and gives the Keep a new key.
Back it up like any key file (`identity.key`, `persona.key`, `invites.json`, `owner.json`, `owner-state.json`).

On the status page, under Invites:

- **New guest pass** makes a link that lasts 24 hours. A guest pass works for the first
  person who uses it, until it expires: after that it is theirs, they can come back with
  it, and nobody else can use it. The page shows it as "unused" or "used by <name>".
- **More invite options** opens a form: a **personal invite** is for one person's npub
  (or hex key), good for 1 to 365 days (30 by default) and as many visits as they like;
  nobody else can use it. A label is optional on both.
- The link looks like `stampede://keep/<Keep key>/<secret>`. The page shows it
  after you make it, in a box to copy (open it again and you see the same link, not a new
  invite; a used guest pass is no longer shown there); `curl -s http://127.0.0.1:8780/status.json` lists the links of the
  live ones. The link holds no address, so a changed port or router does not break it.
- **Revoke** cancels an invite at once and disconnects the players it let in.

If `invites.json` or a key file in the volume cannot be read, the Keep still starts and
room-code play works, but invites are off and the page shows a red line saying which file
and why. The file is never overwritten: fix or move it and restart.

The status page has no login: it is for the home network only (never forward port 8780).
Anyone on your home network who opens it can see live invite links.
A Keep that hands out invites on a network you do not trust needs a different design.
The room code keeps working alongside the invites.

## Making the Keep yours, and inviting from My Signet

A Keep with a data volume (the default compose files) can be claimed once by one
My Signet persona; after that the owner makes and cancels invites in My Signet and the
status page makes none. Until the Keep is claimed it shows a pairing link:

- **In the Keep's log**, once at each start: `docker logs stampede-keep 2>&1 | grep "Make this your Keep"`.
  This works under both compose files. (It is the one line the log carries that is
  secret: the link holds a one-time code, so do not paste the log anywhere.)
- On the status page, only when you open it on the Keep's own machine
  (`http://127.0.0.1:8780`). Under the Proton compose the page sees requests from
  Docker's network, not from the machine, so use the log there.

In My Signet: Settings, My Keep, Add a Keep, paste the link. The first valid claim
makes that persona the owner and pairing closes; the code is new at each start. The
Keep listens for the owner's messages on the relays named in the link (the `--relay`
list, at most three), so it needs them reachable.

Invites the page made before the claim stay valid until they expire. The page lists
invites read-only and keeps New code.

**Owner personas.** The claimer is the first owner persona. From My Signet the owner can
list more of their personas on the Keep (each linking needs a signed update from an
active owner persona and an acceptance from the persona being added), and any active
owner persona can make and cancel invites. This is kept in `owner.json`, with a
revision number and the claim's scope (All of you or Only one persona, for display).
An acceptance that arrives before the update that lists its persona is held in memory
only (7 days, at most 32), is not answered and is not written to disk: My Signet sends it
again after the Keep restarts. When the Keep starts it reads the last few days of
messages from the relays, owner updates, acceptances and claims first and invites after
them, so a persona removed in that time cannot get an invite in. Removing a
persona ends its rights at once; the invites it made stay and belong to the Keep. A
Keep claimed by the earlier version has its `owner.json` rewritten in the new form
the first time it starts (the claimer stays an owner persona, revision 0); nothing
else about it changes. **Reset is the same as before: delete `owner.json`.**

**A relay that refuses or drops.** If a relay refuses part of that read (a rate limit,
say) or drops part way, the Keep holds what it got from that relay and reads its window
again later, waiting longer each time (15 s, up to 10 min). What the other relays send
meanwhile is applied. Until every relay's window has been read the Keep does not save
how far it has read, so a restart reads from the old point. A relay that stays down, or
keeps refusing, holds that point back for as long as it does: the next start reads
further back than it needs to, which costs a longer read and loses nothing.

**A relay that wants you to log in.** Some relays answer a read of the Keep's messages
with `auth-required` and a NIP-42 challenge. The Keep answers it with an event signed by
its own key (the relay's address and the challenge, nothing else), logs
`authenticated to <relay>`, and asks again at once the read the relay had closed. A relay
that refuses the answer is treated as any relay that refuses a read (above).

The page shows the link on the Keep's own machine only because a request from there
comes from the loopback address. Anything that makes other people's requests arrive from
the Keep's own network would show them the link too: a reverse proxy on the box, and,
under the Proton compose (where the Keep lives in gluetun's network), gluetun's HTTP
proxy or Shadowsocks server (`HTTPPROXY`, `SHADOWSOCKS`). Leave those off, or use the
log to pair.

**To reset the owner** (a lost phone, a wrong persona), in this order:

1. If you still have the old owner's My Signet, cancel the invites you do not want to
   keep there.
2. Stop the Keep and delete `owner.json` in the volume (for example
   `docker run --rm -v stampede-keep-data:/d alpine rm /d/owner.json`), then start it
   again. `owner-state.json` (the cancelled invites it remembers and the position in the
   relay stream) can stay.
3. **Before pairing again**, open the status page on the Keep's machine and cancel any
   of the old owner's invites that are left. Invites the old owner made are in
   `invites.json` and stay valid for up to a year; the page can cancel them only while
   the Keep has no owner. Once someone claims the Keep again, nobody can cancel them.
4. Pair with the new link.

If `owner.json` or `owner-state.json` cannot be read, the Keep still starts and
room-code play works, but it counts as claimed (the page makes no invites) and the page
says why. A file that cannot be read is never overwritten. While they are damaged nobody can cancel
an invite either, the owner's messages are not read and the page is read-only, so
repair or restore the file first.

## Reaching it from outside the house

By default the Keep announces the box's home-network address plus whatever
address STUN and the router (UPnP) can find. For a fixed route:

1. On the router, forward UDP 47800 to this box.
2. Add `--advertise <home public IP>:47800` to the `command:` list in
   `docker-compose.yml`. Leave `--advertise-only` off for this route: with it, the
   Keep announces nothing else, and a phone on the same Wi-Fi then has to reach the
   home public address from inside the house, which many routers can't do.
3. `docker compose up -d` again.

For this route the box must not send its traffic through a VPN. Replies leave by
the box's default route, so with a VPN up they go out from the wrong address and
phones outside the house never get an answer. Phones on the same Wi-Fi are
unaffected.

The room uses a throwaway test identity (`--test-identity`), so it is not a
production Keep.

## Route 2: hide your home address with Proton VPN

**Untested.** This route was written from the gluetun and Proton documentation and
has not been run against Proton yet. Treat the first run as a trial.

Here the Keep sits behind Proton VPN. Players see Proton's address and a port
Proton gives you, never your home connection. It needs a paid Proton plan, a P2P
server and Proton's NAT-PMP port forwarding. A container called gluetun holds the
VPN and the Keep shares its network. Use `docker-compose.proton.yml` instead of
`docker-compose.yml`, not alongside it.

1. Build the image as above (`./build-image.sh`).
2. At account.protonvpn.com open Downloads, then WireGuard configuration. Pick a
   P2P server near you, tick "NAT-PMP (Port Forwarding)" under "Select VPN
   options", create the config and copy the `PrivateKey` value.
3. `cp proton.env.example proton.env`, paste the key in, and set
   `FIREWALL_OUTBOUND_SUBNETS` to your home network if it is not `192.168.x.x`.
   `proton.env` is git-ignored; it is a secret.
4. `docker compose -f docker-compose.proton.yml up -d`, then
   `docker compose -f docker-compose.proton.yml logs -f`. Look for
   `Proton forwarded port ...` followed by the Keep's own "ready".
5. Open `http://<box>:8780` from the home network for the room code, as before.
   Port 8780 is published on the box for the home network only; never forward it
   on the router.

What players see: only Proton's exit address and port. The Keep announces nothing
else (`--advertise-only`), so your home address, STUN and UPnP never come into it.

Trade-offs:

- Every packet takes an extra hop through Proton, so expect somewhat more latency.
  Pick a server close to the players.
- Proton chooses the port and may change it whenever the VPN reconnects. When that
  happens the Keep restarts on the new port, which gives the room a **new code**
  and sends anyone in the room out. Check the status page for the current code.
- A restart takes up to about 15 seconds to notice a new port.
- If the VPN container itself is restarted (not just reconnected) and Proton happens
  to hand back the same port and address, the Keep does not notice and is left
  without a network. Run `docker compose -f docker-compose.proton.yml restart keep`.
- If the status page cannot be reached from the home network, `FIREWALL_INPUT_PORTS`
  and `FIREWALL_OUTBOUND_SUBNETS` are the settings to look at first.

How it works: gluetun writes the forwarded port and the VPN's public IP to files.
`proton-entrypoint.sh` waits for both, starts the Keep with `--quic-port <port>
--advertise <ip>:<port> --advertise-only`, checks the files every 15 seconds, and
exits when either changes so Docker restarts it. The files live in a memory-only
volume that `docker compose down` empties, so a port left over from an earlier run
is never mistaken for a new one.
The image has no curl or wget, which is why the files are read rather than
gluetun's control API (the API also needs authentication now).

## Route 3: the Keep and the Proton tunnel in one image

For hosts that run one container per app and cannot share a network between two (the
Archipelago node's app manifests are like that), `Dockerfile.proton` puts gluetun and the
Keep in one image. `ingle-entrypoint.sh` starts gluetun, waits for Proton's forwarded port
and exit address, then starts the Keep as an unprivileged user on that port, announcing that
address (and the home-network address, below). gluetun's firewall is the kill switch: with
the tunnel down, the Keep's traffic goes nowhere, never out by the host's route. The Keep holds
no capabilities, so it cannot change the firewall.

Build: `build-image-proton.sh [IMAGE:TAG]`. Run it with `NET_ADMIN`, `SETUID`, `SETGID`
(and no other capabilities), `/dev/net/tun`, a writable root filesystem (gluetun rewrites
`/etc/resolv.conf`), the data volume, and the key in `WIREGUARD_PRIVATE_KEY` (a secret from
the host, never baked in). `SERVER_COUNTRIES` and the other gluetun settings are plain
environment. Arguments after the image name are the Keep's own flags; do not give
`--quic-port` or `--advertise`, the entrypoint adds them, and give `--status` as two
arguments with port 8780 (the firewall rules and the published port are for 8780). Publish
the status page (`8780/tcp`) and, for the home Wi-Fi path, `47800/udp`. The entrypoint lets the
container's own subnet through the firewall so the status page can answer devices on it.

Home Wi-Fi path: set `INGLE_HOME_ADVERTISE=<the host's home-network address>:47800` (the
node's manifest fills it in from the host's own address; it must be a private IPv4 address
with port 47800). The Keep announces it first and Proton's address second, and a joiner dials
the announced addresses in that order, 150 ms apart: a phone on the home Wi-Fi connects
directly, a phone elsewhere loses 150 ms and connects through Proton. Inside the container
the entrypoint redirects UDP 47800 arriving on the container's own interface from the local
subnet to the Keep's real port (Proton's forwarded port); the Keep's replies leave by that
interface. The Keep user's firewall lets out only replies to a flow that came in that way
(and replies to a status-page connection), never a new connection, so the Keep still cannot
reach the home network by itself, over IPv4 or IPv6. If the redirect cannot be installed, only
Proton's address is announced.
Privacy: a joiner who holds a room code or an invite learns the host's private home address.
That address means nothing outside the home network; the public home address is never given.

If Proton gives a new port or exit address, the entrypoint restarts the Keep on it (at most
once per 30 seconds): a new room code, players sent out, invites and the owner claim kept.
A new pair is taken only when it has held for two polls in a row and, if the tunnel's files
had gone away, only when both are newer than that moment, so a new port is never announced
with the old exit address. It logs that the address changed, never the address or the key.
Every few seconds it also checks that the Keep user's firewall rules are loaded and that both
firewalls still drop by default, and stops the Keep and the container if not.

<!--
Sources for Route 2 (gluetun v3.41.3, image qmcgaw/gluetun:v3.41.3; checked 2026-10-21
against the gluetun wiki at github.com/qdm12/gluetun-wiki). Lines relied on:

Proton provider page (setup/providers/protonvpn.md):
  "`VPN_TYPE=wireguard`"
  "`WIREGUARD_PRIVATE_KEY` is your 32 byte key in base64 format. The private key can be
   obtained by generating a Wireguard configuration file and copy the displayed
   `PrivateKey` value. Note this value works for all ProtonVPN servers."
  "`SERVER_COUNTRIES`: Comma separated list of countries" / "`SERVER_CITIES`: ..."
  "`PORT_FORWARD_ONLY`: Filter only port-forwarding enabled (aka *p2p*) servers by
   setting it to `on`."
  "`VPN_PORT_FORWARDING`: defaults to `off` and can be set to `on` to enable port
   forwarding on the VPN server."
  "For Wireguard only, when generating a configuration file, under `Select VPN
   options`, enable `NAT-PMP (Port Forwarding)`"
  "ProtonVPN will forward a random port."

Port forwarding options (setup/options/port-forwarding.md):
  "`VPN_PORT_FORWARDING_PROVIDER` | Current provider in use | `private internet access`,
   `perfect privacy`, `privatevpn`, `protonvpn`"
  "`VPN_PORT_FORWARDING_STATUS_FILE` | `/tmp/gluetun/forwarded_port` | File path to use
   for writing the forwarded port obtained."
  "`VPN_PORT_FORWARDING_UP_COMMAND` / `VPN_PORT_FORWARDING_DOWN_COMMAND` | Shell command"
  (not used here; {{PORTS}} would be "a comma separated list of the ports that have been
   forwarded, for example `1234,5678`")

Port forwarding guide (setup/advanced/vpn-port-forwarding.md):
  "The forwarded port can be accessed: through the control server; through the file
   written at `/tmp/gluetun/forwarded_port` (will be deprecated in v4.0.0 release); by
   running a user specified command upon port forwarding starting"

Other options (setup/options/others.md):
  "`PUBLICIP_FILE` | `/tmp/gluetun/ip` | Filepath to store the public IP address assigned.
   This will be removed in the `v4` program, instead you might want to use the control
   server"

Firewall (setup/options/firewall.md):
  "`FIREWALL_INPUT_PORTS` | i.e. `1000,8000` | Comma separated list of ports to allow
   through the default interface."
  "`FIREWALL_OUTBOUND_SUBNETS` | ... | Comma separated subnets that Gluetun and the
   containers sharing its network stack are allowed to access."
  "Make sure that `FIREWALL_OUTBOUND_SUBNETS` does **not** overlap with the VPN tunnel
   address range (for example a `10.x.x.x/` Wireguard network)."

Control server (setup/advanced/control-server.md), not used:
  "All routes are now private by default and **require** authentication to be setup"
  "HTTP GET to `/v1/portforward` to obtain the port forwarded such as `{"port":5914}`"
  "HTTP GET to `/v1/publicip/ip`" -> `{"public_ip":"58.98.64.104"}`

Connecting a container (setup/connect-a-container-to-gluetun.md):
  "Add `network_mode: "service:gluetun"` to your second container so that it uses the
   `gluetun` network stack."
Port mapping (setup/port-mapping.md): published ports go on the gluetun service.

Proton facts given by the owner: paid plan, P2P servers only, NAT-PMP, port assigned by
Proton, mapping lifetime 60 s renewed about every 45 s, new port after reconnect.
Image facts (inspected locally): gluetun v3.41.3 has a built-in healthcheck
(`/gluetun-entrypoint healthcheck`), which `depends_on: service_healthy` uses.
-->
