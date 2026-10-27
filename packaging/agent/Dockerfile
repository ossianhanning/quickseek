# quickseek-agent: one static binary on an empty image (a few MB).
#   docker build -f packaging/agent/Dockerfile -t quickseek-agent .
# Releases publish it as ghcr.io/ossianhanning/quickseek-agent for
# linux/amd64 and linux/arm64 (built on a runner of each architecture).

FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# The toolchain is this image's; rust-toolchain.toml would also fetch rustfmt and clippy.
ENV RUSTUP_TOOLCHAIN=1.99.1
RUN cargo build --release --locked -p quickseek-agent \
 && strip target/release/quickseek-agent \
 && mkdir -p /out/config \
 && cp target/release/quickseek-agent /out/

FROM scratch
COPY --from=build /out/ /
ENV QUICKSEEK_CONFIG=/config
EXPOSE 48623
VOLUME /config
ENTRYPOINT ["/quickseek-agent"]
