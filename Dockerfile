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
COPY --from=builder /borhan/build/borhan /usr/local/bin/borhan
VOLUME /var/lib/borhan
EXPOSE 1995

# `serve` without a server.toml binds 127.0.0.1, which nothing outside the
# container can reach, so the first start writes one listening on every
# interface — with BORHAN_TOKEN as its token when that is set. It is written
# once: after that the file in the volume is the configuration, the same as on a
# host, and BORHAN_TOKEN is not read again. `init` runs on every start, as the
# systemd unit's ExecStartPre does: it creates the storage the first time and
# rebuilds each memory's index after, so an upgraded image never serves an index
# the new normalizer disagrees with.
CMD ["/bin/sh", "-c", "borhan init && { [ -f \"$BORHAN_HOME/server.toml\" ] || borhan init server --listen 0.0.0.0:1995 ${BORHAN_TOKEN:+--token \"$BORHAN_TOKEN\"}; } && exec borhan --info serve"]
