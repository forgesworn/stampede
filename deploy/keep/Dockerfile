# The Stampede Keep: a headless host that stays open and is joined by its room code.
# Invite links name the Keep by its key, which is kept in the data volume.
# The binary is built on the host first (see build-image.sh); nothing is compiled here.
FROM ubuntu:24.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /var/lib/stampede-keep --shell /usr/sbin/nologin keep

# The Keep's identity and invites live here; keep it on a volume so they survive
# an image update. Made here so a new volume starts owned by the keep user.
RUN mkdir -p /var/lib/stampede-keep/data \
 && chown keep:keep /var/lib/stampede-keep/data \
 && chmod 700 /var/lib/stampede-keep/data
VOLUME /var/lib/stampede-keep/data

COPY --chown=root:root stampede-headless /usr/local/bin/stampede-headless

USER keep
WORKDIR /var/lib/stampede-keep
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/stampede-headless"]
# With no command it prints its usage; docker-compose.yml supplies the real flags.
