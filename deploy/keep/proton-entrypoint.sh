#!/bin/sh
# Entrypoint for the Keep in docker-compose.proton.yml (Route 2). UNTESTED against Proton.
#
# The Keep shares gluetun's network, so it only knows its public face once gluetun
# has a Proton port forward. gluetun writes the forwarded port and the VPN's public
# IP to files; the compose file mounts that folder here, read-only. This script:
#   1. waits until both are present and valid,
#   2. starts the Keep on that port and announces only "<public IP>:<port>",
#   3. polls every POLL_SECONDS; if the port or IP changes (Proton gave a new one
#      after a reconnect) it stops the Keep and exits non-zero, and the compose
#      restart policy brings it back on the new port (which means a new room code).
# POSIX sh only.
set -eu

STATE_DIR=${GLUETUN_STATE_DIR:-/gluetun-state}
PORT_FILE=${PORT_FILE:-$STATE_DIR/forwarded_port}
IP_FILE=${IP_FILE:-$STATE_DIR/ip}
KEEP_BIN=${KEEP_BIN:-/usr/local/bin/stampede-headless}
KEEP_DATA_DIR=${KEEP_DATA_DIR:-/var/lib/stampede-keep/data}
STATUS_ADDR=${STATUS_ADDR:-0.0.0.0:8780}
POLL_SECONDS=${POLL_SECONDS:-15}

log() { printf '%s\n' "proton-entrypoint: $*" >&2; }

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
        case "$_o" in '' | *[!0-9]*) return 1 ;; esac
        [ "${#_o}" -le 3 ] && [ "$_o" -le 255 ] || return 1
    done
}

# Wait for a valid port and IP; sets PORT and IP.
wait_for_route() {
    _n=0
    while :; do
        PORT=$(read_first "$PORT_FILE")
        IP=$(read_first "$IP_FILE")
        if valid_port "$PORT" && valid_ip "$IP"; then
            return 0
        fi
        if [ $((_n % 4)) -eq 0 ]; then
            log "waiting for gluetun: port='${PORT}' ip='${IP}' (${PORT_FILE}, ${IP_FILE})"
        fi
        _n=$((_n + 1))
        sleep "$POLL_SECONDS"
    done
}

wait_for_route
log "Proton forwarded port ${PORT}, public IP ${IP}"

"$KEEP_BIN" --online --test-identity \
    --data-dir "$KEEP_DATA_DIR" \
    --quic-port "$PORT" \
    --advertise "$IP:$PORT" --advertise-only \
    --status "$STATUS_ADDR" \
    --seconds 0 &
child=$!

stop_child() {
    kill -TERM "$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
}
trap 'log "stopping"; stop_child; exit 0' TERM INT

ticks=0
unreadable=0
while kill -0 "$child" 2>/dev/null; do
    sleep 1 &
    wait $! || true
    ticks=$((ticks + 1))
    [ "$ticks" -ge "$POLL_SECONDS" ] || continue
    ticks=0
    new_port=$(read_first "$PORT_FILE")
    new_ip=$(read_first "$IP_FILE")
    if valid_port "$new_port" && valid_ip "$new_ip"; then
        unreadable=0
        if [ "$new_port" != "$PORT" ] || [ "$new_ip" != "$IP" ]; then
            log "route changed (${IP}:${PORT} -> ${new_ip}:${new_port}); restarting for a new room code"
            stop_child
            exit 75
        fi
    elif [ "$unreadable" -eq 0 ]; then
        # Tunnel probably down or reconnecting; keep serving until gluetun has a new port.
        unreadable=1
        log "forwarded port or IP not available right now; keeping ${IP}:${PORT}"
    fi
done

status=0
wait "$child" || status=$?
log "the Keep exited (status ${status})"
[ "$status" -ne 0 ] || status=1
exit "$status"
