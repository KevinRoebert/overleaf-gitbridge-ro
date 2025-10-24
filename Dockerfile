# ---------- Build stage ----------
FROM rust:1.86-bullseye AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch --locked

COPY src ./src
COPY templates ./templates

RUN cargo build --release --locked && \
    strip target/release/sharelatex-gitbridge-ro

# ---------- Runtime stage ----------
FROM debian:bookworm-slim

# install git + tini + ca-certificates
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        tini \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --home /srv/gitbridge --shell /usr/sbin/nologin gitbridge && \
    mkdir -p /sharelatex-data /data/git-bridge && \
    chown -R gitbridge:gitbridge /sharelatex-data /data/git-bridge

COPY --from=builder /app/target/release/sharelatex-gitbridge-ro /usr/local/bin/sharelatex-gitbridge-ro

ENV PORT=8022 \
    SHARELATEX_DATA_PATH=/sharelatex-data \
    PROJECTS_DIR=data/projects \
    GIT_ROOT=/data/git-bridge \
    READONLY_BRANCH=master \
    ADMIN_PASSWORD=""

EXPOSE 8022

USER gitbridge

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/sharelatex-gitbridge-ro"]
