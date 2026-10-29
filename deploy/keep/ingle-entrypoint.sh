#!/bin/sh
# Entrypoint of the one-image Keep + Proton tunnel (Dockerfile.proton). POSIX sh only.
#
# 1. starts gluetun (the tunnel and its firewall, which is the kill switch),
# 2. waits until gluetun has written Proton's forwarded port and exit address,
# 3. starts the Keep as the unprivileged keep user on that port, announcing only
#    "<exit address>:<port>" (the arguments given to this script are the Keep's own flags;
#    --quic-port and --advertise are added here and any given are dropped),
# 4. polls the two files; if either changes (Proton gave a new port after a reconnect) it
#    restarts the Keep on the new address, at most once per MIN_RESTART_SECONDS. A restart
#    gives the room a new code; invites and the owner claim live in the data dir and stay.
#    A new pair is taken only once it has held for two polls in a row and, if the files had
#    gone away, only when both are newer than the moment they went (never a new port paired
#    with the old address).
# 5. home Wi-Fi path (optional): INGLE_HOME_ADVERTISE=<home address>:47800 (the node fills it
#    in) is announced as a second address. UDP arriving on the container's own interface from
#    the local subnet on 47800 is redirected to the Keep's port, and the Keep's replies leave
#    by that interface (the firewall allows the Keep user only replies to such flows, never a
#    new connection). If the redirect cannot be installed the address is not announced.
# 6. every poll, checks that the Keep-user firewall rules are still loaded and that both
#    firewalls still default to DROP; if not, the Keep is killed and the container exits.
#
# Logs never carry the key or the addresses. If gluetun exits, so does the container (the
# host's service manager restarts it). If the tunnel is only down, the Keep keeps its old
# address but has no network: the firewall leaves it nowhere to go but the tunnel.
set -eu

STATE_DIR=${GLUETUN_STATE_DIR:-/tmp/gluetun}
PORT_FILE=${PORT_FILE:-$STATE_DIR/forwarded_port}
IP_FILE=${IP_FILE:-$STATE_DIR/ip}
KEEP_BIN=${KEEP_BIN:-/usr/local/bin/stampede-headless}
GLUETUN_BIN=${GLUETUN_BIN:-/usr/local/bin/gluetun}
KEEP_UID=${KEEP_UID:-999}
KEEP_GID=${KEEP_GID:-999}
POLL_SECONDS=${POLL_SECONDS:-5}
MIN_RESTART_SECONDS=${MIN_RESTART_SECONDS:-30}
STALE_SECONDS=${STALE_SECONDS:-90}
STOP_GRACE_SECONDS=${STOP_GRACE_SECONDS:-10}

log() { printf '%s\n' "ingle-entrypoint: $*" >&2; }
die() { log "$*"; exit 1; }

[ -n "${WIREGUARD_PRIVATE_KEY:-}" ] || die "WIREGUARD_PRIVATE_KEY is not set (the node's secret_env should supply it); not starting"

# The key may carry a newline from the secrets file; a base64 key has no whitespace.
WIREGUARD_PRIVATE_KEY=$(printf '%s' "$WIREGUARD_PRIVATE_KEY" | tr -d ' \t\r\n')
export WIREGUARD_PRIVATE_KEY

# First line of a file with blanks stripped; empty if the file is missing or unreadable.
read_first() {
    [ -r "$1" ] || return 0
    head -n 1 "$1" 2>/dev/null | tr -d ' \t\r' || true
}

valid_port() {
    case "$1" in '' | *[!0-9]*) return 1 ;; esac
    [ "${#1}" -le 5 ] && [ "$1" -ge 1 ] && [ "$1" -le 65535 ]
}

# Dotted-quad IPv4 only (what Proton's WireGuard exits give).
valid_ip() {
    case "$1" in '' | *[!0-9.]*) return 1 ;; esac
    _old_ifs=$IFS
    IFS=.
    # shellcheck disable=SC2086
    set -- $1
    IFS=$_old_ifs
    [ "$#" -eq 4 ] || return 1
    for _o in "$@"; do
        case "$_o" in '' | *[!0-9]* | 0?*) return 1 ;; esac
        [ "${#_o}" -le 3 ] && [ "$_o" -le 255 ] || return 1
    done
}

# A forwarded port the Keep may use: never the status page's port (it would put the page, with
# the room code and live invite links, on the internet).
port_ok() {
    valid_port "$1" && [ "$1" != 8780 ]
}

# RFC 1918 dotted quad (the node's home-network address).
private_v4() {
    valid_ip "$1" || return 1
    case "$1" in
        10.* | 192.168.*) return 0 ;;
        172.*)
            _s=${1#172.}
            _s=${_s%%.*}
            [ "$_s" -ge 16 ] && [ "$_s" -le 31 ]
            ;;
        *) return 1 ;;
    esac
}

# One second. A plain foreground sleep: the shell runs a TERM trap once it returns (at most a
# second late). A backgrounded sleep with `wait $!` was seen to block for good in dash.
tick() {
    sleep 1 || true
}

# --- the Keep's flags: drop what this script owns, make --status listen on every address --
# (the status page is published to the host's LAN; inside the container it must not bind a
# host address that does not exist here).
skip=0
prev=
n=$#
while [ "$n" -gt 0 ]; do
    a=$1
    shift
    n=$((n - 1))
    if [ "$skip" -eq 1 ]; then
        skip=0
        continue
    fi
    case "$a" in
        --quic-port | --advertise)
            skip=1
            continue
            ;;
        --quic-port=* | --advertise=*)
            continue
            ;;
        --status=*)
            die "give --status and its value as two arguments (--status 0.0.0.0:8780); not starting"
            ;;
    esac
    if [ "$prev" = "--status" ]; then
        # The firewall rules and the published port are for 8780 only.
        [ "${a##*:}" = 8780 ] || die "--status must use port 8780 (the firewall rules and the published port are for it); not starting"
        a="0.0.0.0:8780"
    fi
    prev=$a
    set -- "$@" "$a"
done

# --- the network as it is before the tunnel: let the firewall answer the LAN ---
# gluetun's firewall would send replies to the status page's callers into the tunnel unless
# their subnet is listed. Taken from the container's own default interface, not hard-coded.
# (gluetun then also lets everything addressed to the container on that subnet in, which is
# what carries the home Wi-Fi game traffic after the redirect below.)
HOME_DEV=$(ip -4 route show default 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit }}')
HOME_SUBNETS=
if [ -n "$HOME_DEV" ]; then
    HOME_SUBNETS=$(ip -4 route show dev "$HOME_DEV" scope link 2>/dev/null | awk '$1 ~ /\// { print $1 }' | sort -u)
fi
if [ -z "${FIREWALL_OUTBOUND_SUBNETS:-}" ]; then
    if [ -n "$HOME_SUBNETS" ]; then
        FIREWALL_OUTBOUND_SUBNETS=$(printf '%s\n' "$HOME_SUBNETS" | paste -sd, -)
        export FIREWALL_OUTBOUND_SUBNETS
        log "the local subnet will be allowed through the firewall for the status page"
    else
        log "no local subnet found; the status page will only answer from inside the tunnel"
    fi
fi

# --- the home-network address to announce (from the node's manifest), if usable ---
HOME_ADV=
HOME_ON=0
if [ -n "${INGLE_HOME_ADVERTISE:-}" ]; then
    _h=${INGLE_HOME_ADVERTISE%:*}
    _p=${INGLE_HOME_ADVERTISE##*:}
    if [ "$_p" = 47800 ] && private_v4 "$_h" && [ -n "$HOME_DEV" ] && [ -n "$HOME_SUBNETS" ]; then
        HOME_ADV=$INGLE_HOME_ADVERTISE
        HOME_ON=1
    else
        log "INGLE_HOME_ADVERTISE is not a private IPv4 address with port 47800, or no local subnet was found; the home-network path is off"
    fi
fi
unset INGLE_HOME_ADVERTISE

mkdir -p "$STATE_DIR"

# --- gluetun ---
"$GLUETUN_BIN" &
gluetun_pid=$!
# Nothing started from here on needs the key.
unset WIREGUARD_PRIVATE_KEY
keep_pid=

# This script runs as root with only the few capabilities the host grants (no CAP_KILL), so it
# cannot signal a process of another user. Signals to the Keep are sent by a short-lived
# process running as the keep user; liveness is read from /proc, where a zombie counts as dead.
as_keep() {
    env -i PATH="$PATH" setpriv --reuid="$KEEP_UID" --regid="$KEEP_GID" --clear-groups --inh-caps=-all --no-new-privs -- "$@"
}

signal_keep() {
    as_keep sh -c 'kill -s "$1" "$2"' sh "$1" "$keep_pid" 2>/dev/null || true
}

keep_alive() {
    [ -n "$keep_pid" ] || return 1
    _st=$(sed 's/^.*) //; s/ .*//' "/proc/$keep_pid/stat" 2>/dev/null) || return 1
    [ -n "$_st" ] && [ "$_st" != Z ] && [ "$_st" != X ]
}

stop_keep() {
    [ -n "$keep_pid" ] || return 0
    signal_keep TERM
    _n=0
    while keep_alive && [ "$_n" -lt "$STOP_GRACE_SECONDS" ]; do
        tick
        _n=$((_n + 1))
    done
    if keep_alive; then signal_keep KILL; fi
    wait "$keep_pid" 2>/dev/null || true
    keep_pid=
}

stop_all() {
    stop_keep
    kill -TERM "$gluetun_pid" 2>/dev/null || true
    wait "$gluetun_pid" 2>/dev/null || true
}
trap 'log "stopping"; stop_all; exit 0' TERM INT

# The home-network address is announced first: a joiner dials the announced addresses in order,
# 150 ms apart, so a phone on the home Wi-Fi gets the direct path at once and a phone elsewhere
# loses 150 ms before it dials the tunnel's address.
# The Keep gets a bare environment: no WireGuard key, no STAMPEDE_ADVERTISE, no capabilities
# (setpriv drops to the keep user; a non-root process without file capabilities holds none).
start_keep() {
    env -i PATH="$PATH" HOME=/var/lib/stampede-keep \
        setpriv --reuid="$KEEP_UID" --regid="$KEEP_GID" --clear-groups --inh-caps=-all --no-new-privs -- \
        "$KEEP_BIN" "$@" --quic-port "$PORT" ${HOME_ADV:+--advertise "$HOME_ADV"} --advertise "$IP:$PORT" &
    keep_pid=$!
    last_start=$(date +%s)
}

gluetun_alive() { kill -0 "$gluetun_pid" 2>/dev/null; }

# --- the Keep-user firewall rules and the default-DROP policies ---
# The rules (Dockerfile.proton, /iptables/post-rules.txt) are the only layer that stops the Keep
# reaching the local network. gluetun skips that file silently if it is missing, so check, for
# both IP versions: the jump from OUTPUT into the Keep's chain, the chain's final DROP, and the
# default DROP on output.
rules_loaded() {
    for _fw in iptables ip6tables; do
        "$_fw" -t mangle -C OUTPUT -m owner --uid-owner "$KEEP_UID" -j KEEPUSER 2>/dev/null || { _why="$_fw has no jump into the Keep's chain"; return 1; }
        [ "$("$_fw" -t mangle -S KEEPUSER 2>/dev/null | tail -n 1)" = "-A KEEPUSER -j DROP" ] || { _why="$_fw's Keep chain has no final DROP"; return 1; }
        "$_fw" -S OUTPUT 2>/dev/null | grep -qx -- '-P OUTPUT DROP' || { _why="$_fw does not default to DROP on output"; return 1; }
    done
}

# --- the home Wi-Fi path: UDP 47800 from the local subnet to the Keep's port ---
# nat PREROUTING sends it to the Keep's port (the chain holds one rule and is rewritten when the
# port changes). gluetun's firewall already lets everything addressed to the container on the
# local subnet in, and the Keep user's replies to such flows are let out by the Keep chain
# (reply direction of a flow first seen coming in on 47800); it cannot start one.
home_jump() {
    # $1: -C, -A or -D. One jump per local subnet.
    for _net in $HOME_SUBNETS; do
        iptables -t nat "$1" PREROUTING -i "$HOME_DEV" -s "$_net" -p udp --dport 47800 -j KEEPHOME 2>/dev/null || return 1
    done
}
home_install() {
    # $1: the Keep's port.
    iptables -t nat -N KEEPHOME 2>/dev/null || true
    iptables -t nat -F KEEPHOME && iptables -t nat -A KEEPHOME -p udp -j DNAT --to-destination ":$1" || return 1
    home_jump -C || home_jump -A || return 1
}
home_ok() {
    # $1: the Keep's port.
    home_jump -C && [ "$(iptables -t nat -S KEEPHOME 2>/dev/null | tail -n 1)" = "-A KEEPHOME -p udp -j DNAT --to-destination :$1" ]
}

# --- wait for the tunnel's forwarded port and exit address ---
PORT=
IP=
_n=0
while :; do
    gluetun_alive || { wait "$gluetun_pid" 2>/dev/null || true; die "gluetun exited before the tunnel was ready"; }
    PORT=$(read_first "$PORT_FILE")
    IP=$(read_first "$IP_FILE")
    if valid_port "$PORT" && valid_ip "$IP"; then
        break
    fi
    if [ $((_n % 30)) -eq 0 ]; then
        log "waiting for the tunnel and its forwarded port"
    fi
    _n=$((_n + 1))
    tick
done

# The Keep must not take the status page's own port from the tunnel.
port_ok "$PORT" || { stop_all; die "Proton forwarded the status page's port; not starting the Keep"; }
# Refuse to start the Keep unless its firewall rules are loaded and both firewalls default to DROP
# (a failed ip6tables would leave IPv6 open; a missing rules file would leave the local network open).
_why=
rules_loaded || { stop_all; die "$_why; not starting the Keep"; }
if [ "$HOME_ON" -eq 1 ]; then
    if home_install "$PORT"; then
        log "the home-network path is on (UDP 47800 from the local subnet reaches the Keep)"
    else
        log "could not install the home-network redirect; announcing only the tunnel address"
        HOME_ON=0
        HOME_ADV=
    fi
fi
log "tunnel up; starting the Keep on the forwarded port"
start_keep "$@"

ticks=0
pending=0
t_gone=0        # when the port or address files were first seen missing (0: not missing)
cand_port=
cand_ip=
cand_n=0        # polls in a row the candidate pair has been seen unchanged
home_warned=0
while :; do
    tick
    if ! gluetun_alive; then
        status=0
        wait "$gluetun_pid" 2>/dev/null || status=$?
        signal_keep KILL
        die "gluetun exited (status ${status})"
    fi
    now=$(date +%s)
    if [ -n "$keep_pid" ] && ! keep_alive; then
        status=0
        wait "$keep_pid" 2>/dev/null || status=$?
        keep_pid=
        log "the Keep exited (status ${status}); restarting it"
        pending=1
    fi
    ticks=$((ticks + 1))
    if [ "$ticks" -ge "$POLL_SECONDS" ]; then
        ticks=0
        # The firewall rules must still be there; if not, nothing of the Keep may stay up.
        _why=
        rules_loaded || { signal_keep KILL; stop_all; die "$_why; stopping"; }
        if [ "$HOME_ON" -eq 1 ] && ! home_ok "$PORT"; then
            # gluetun can rewrite its firewall on a reconnect; put the redirect back.
            if home_install "$PORT"; then
                log "the home-network redirect was missing and has been put back"
            elif [ "$home_warned" -eq 0 ]; then
                home_warned=1
                log "the home-network redirect is missing and could not be put back"
            fi
        fi
        new_port=$(read_first "$PORT_FILE")
        new_ip=$(read_first "$IP_FILE")
        if port_ok "$new_port" && valid_ip "$new_ip"; then
            if [ "$new_port" = "$PORT" ] && [ "$new_ip" = "$IP" ]; then
                # Back to (or still on) the address the Keep has.
                cand_n=0
                t_gone=0
            else
                if [ "$new_port" = "$cand_port" ] && [ "$new_ip" = "$cand_ip" ]; then
                    cand_n=$((cand_n + 1))
                else
                    cand_port=$new_port
                    cand_ip=$new_ip
                    cand_n=1
                fi
                fresh=1
                if [ "$t_gone" -gt 0 ] && [ $((now - t_gone)) -lt "$STALE_SECONDS" ]; then
                    # The files went away (tunnel down): a file not rewritten since is stale, say
                    # the old address next to a new port. Both must be newer than the moment they went.
                    _mp=$(stat -c %Y "$PORT_FILE" 2>/dev/null || echo 0)
                    _mi=$(stat -c %Y "$IP_FILE" 2>/dev/null || echo 0)
                    if [ "$_mp" -lt "$t_gone" ] || [ "$_mi" -lt "$t_gone" ]; then fresh=0; fi
                fi
                if [ "$cand_n" -ge 2 ] && [ "$fresh" -eq 1 ]; then
                    log "the forwarded port or exit address changed; the Keep will restart (new room code)"
                    PORT=$new_port
                    IP=$new_ip
                    pending=1
                    cand_n=0
                    t_gone=0
                fi
            fi
        else
            # Tunnel probably down or reconnecting; keep the old address until there is a new one.
            cand_n=0
            if [ "$t_gone" -eq 0 ]; then
                t_gone=$now
                log "forwarded port or exit address not available right now; keeping the old one"
            fi
        fi
    fi
    if [ "$pending" -eq 1 ] && [ $((now - last_start)) -ge "$MIN_RESTART_SECONDS" ]; then
        pending=0
        stop_keep
        if [ "$HOME_ON" -eq 1 ] && ! home_ok "$PORT"; then
            home_install "$PORT" || log "could not point the home-network redirect at the new port"
        fi
        start_keep "$@"
        log "the Keep restarted"
    fi
done
