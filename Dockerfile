ARG DOCKER_REGISTRY
ARG DOCKER_ALPINE_VERSION=latest
FROM ${DOCKER_REGISTRY}rust:alpine${DOCKER_ALPINE_VERSION} AS builder

# gcc and musl-dev because SQLite and zstd are compiled from C by their build
# scripts; make because the build goes through the Makefile like every other one.
RUN apk add --no-cache make gcc musl-dev

WORKDIR /borhan

COPY Cargo.toml Cargo.lock Makefile ./
COPY src src
RUN make release && mv build/borhan-* build/borhan

# The entrypoint. Written here rather than as a CMD line so that the setup it
# does is a script somebody can read — and run, with `docker exec borhan-start
# --help` — instead of a quoted shell one-liner in image metadata.
#
# With no arguments it sets the container up and serves. With arguments it is
# borhan: `docker run borhan memory list` runs that command and exits, against
# the same store, without starting a server.
RUN printf '%s\n' \
	'#!/bin/sh' \
	'set -e' \
	'' \
	'# Before anything else, so that a command passed to docker run finds a' \
	'# store even on a fresh volume. Creates it the first time; on the starts' \
	'# after it rebuilds the index of every memory, so an upgraded image never' \
	'# serves an index that the normalizer in it disagrees with.' \
	'#' \
	'# Onto stderr, where the logs are: stdout belongs to the command below, and' \
	'# a caller reading `memory list --json` gets JSON and nothing else.' \
	'borhan init storage >&2' \
	'' \
	'# Anything after the image name is a borhan command, not a server:' \
	'#     docker run --rm -v borhan:/var/lib/borhan IMAGE memory list' \
	'if [ "$#" -gt 0 ]; then' \
	'	exec borhan "$@"' \
	'fi' \
	'' \
	'# serve with no server.toml binds 127.0.0.1, which nothing outside the' \
	'# container can reach. So the first start writes one that listens on every' \
	'# interface, on BORHAN_PORT, with BORHAN_TOKEN as its token when that is' \
	'# set. Written once: after that the file in the volume is the configuration,' \
	'# exactly as on a host, and neither variable is read again.' \
	'if [ ! -f "$BORHAN_HOME/server.toml" ]; then' \
	'	borhan init server --listen "0.0.0.0:${BORHAN_PORT:-1995}" ${BORHAN_TOKEN:+--token "$BORHAN_TOKEN"}' \
	'fi' \
	'' \
	'exec borhan --info serve' \
	> build/borhan-start \
	&& chmod 0755 build/borhan-start \
	&& sh -n build/borhan-start


FROM ${DOCKER_REGISTRY}alpine:${DOCKER_ALPINE_VERSION}
ARG BORHAN_VERSION
LABEL "org.opencontainers.image.authors"="pouriya.jahanbakhsh@gmail.com"
LABEL "org.opencontainers.image.title"="borhan"
LABEL "org.opencontainers.image.description"="Keyword memory for AI agents over MCP, CLI and HTTP, for Persian and English text"
LABEL "org.opencontainers.image.url"="https://github.com/pouriya/borhan"
LABEL "org.opencontainers.image.source"="https://github.com/pouriya/borhan"
LABEL "org.opencontainers.image.version"="${BORHAN_VERSION}"
LABEL "org.opencontainers.image.licenses"="MIT"

# Every borhan command reads its home from here, so `docker exec <container>
# borhan memory list` finds the same store the server is serving.
ENV BORHAN_HOME="/var/lib/borhan"

# The port the first start binds, and what EXPOSE advertises. Only read on that
# first start: after it, the port lives in server.toml in the volume, the same
# as on a host. Change it later by editing that file, not this variable.
ENV BORHAN_PORT="1995"

COPY --from=builder /borhan/build/borhan /usr/local/bin/borhan
COPY --from=builder /borhan/build/borhan-start /usr/local/bin/borhan-start
VOLUME /var/lib/borhan
EXPOSE ${BORHAN_PORT}

# No arguments: set the container up and serve. Arguments: run them as a borhan
# command against the same store and exit. `cat $(command -v borhan-start)` says
# what the first case does.
ENTRYPOINT ["borhan-start"]
