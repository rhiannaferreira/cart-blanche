# Build stage: a static binary tuned for the scorers (Intel Xeon, Cascade Lake or newer).
FROM rust:1.90-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN RUSTFLAGS="-C target-cpu=cascadelake -C target-feature=+crt-static" \
    cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && cp target/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions

# Run stage: only the binary. The scorer runs it as uid 1000 with no arguments.
FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
USER 1000:1000
ENTRYPOINT []
CMD ["/shopping-sessions"]
