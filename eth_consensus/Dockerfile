# Get chef
FROM rust:1.77-bullseye as chef
WORKDIR test-plan
RUN cargo install cargo-chef

# Get chef to create a skeleton workspace
FROM chef AS planner
COPY ./plan .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef as builder
# Build dependencies
RUN apt-get update && apt-get -y upgrade && apt-get install -y protobuf-compiler
COPY --from=planner /test-plan/recipe.json ./recipe.json

# Cache the deps using the fake workspace
RUN cargo chef cook --release --recipe-path recipe.json

# Get the real code
COPY ./plan .

# Enjoy
RUN cargo build --release -p simulation

FROM debian:bullseye-slim

COPY --from=builder /test-plan/target/release/simulation /usr/local/bin/eth_consensus

#ENV RUST_LOG=libp2p_gossipsub=debug,simulation=debug
ENV RUST_LOG=simulation=info

ENTRYPOINT ["eth_consensus"]
