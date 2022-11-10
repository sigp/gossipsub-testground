# based on laurentsenta/shared-deps-testground-example

FROM rust:1.65-bullseye as chef
WORKDIR test-plan
RUN cargo install cargo-chef

FROM chef AS planner
COPY ./plan .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef as builder
RUN apt-get update && apt-get -y upgrade && apt-get install -y protobuf-compiler
COPY --from=planner /test-plan/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY . .

RUN cargo build --release -p episub

RUN find -maxdepth 2 -type d -ls 

FROM debian:bullseye-slim

COPY --from=builder /test-plan/target/release/episub /usr/local/bin/episub

ENV RUST_LOG=libp2p_gossipsub=debug,episub=debug

ENTRYPOINT ["episub"]
