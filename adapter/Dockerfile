# The ASK surface (aria-ask) — hand-written Rust, no codegen.
# Build context: the repo's adapter/ directory (a self-contained workspace).
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin aria-ask

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/aria-ask /usr/local/bin/aria-ask

# The default config is baked in (overridable via ARIA_CONFIG / a mounted
# volume). Secrets never live in the image: ARIA_BRAIN_TOKEN & co. come from
# the pod's secret env.
COPY config/aria.example.toml /etc/aria/aria-ask.toml
ENV ARIA_CONFIG=/etc/aria/aria-ask.toml

EXPOSE 8090
ENTRYPOINT ["aria-ask"]
CMD ["serve"]
