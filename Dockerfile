# This Dockerfile is for the `docker:generic` builder.
# See https://docs.testground.ai/builder-library/docker-generic for details about the builder.
FROM rust:1.61-bullseye as builder
WORKDIR /usr/src/test-plan

# `prost-build`, a dependency of `libp2p-gossipsub`, requires cmake.
# There is a discussion for removing cmake from their dependency.
# https://github.com/tokio-rs/prost/pull/620
RUN apt-get update && apt-get install -y cmake

# Cache dependencies between test runs,
# See https://blog.mgattozzi.dev/caching-rust-docker-builds/
# And https://github.com/rust-lang/cargo/issues/2644
RUN mkdir -p ./plan/src/
# This is a placeholder main function to build only the dependencies.
RUN echo "fn main() {}" > ./plan/src/main.rs
COPY ./plan/Cargo.lock ./plan/
COPY ./plan/Cargo.toml ./plan/
RUN cd ./plan/ && cargo build --release

COPY . .

# Note: In `docker:generic` builder, the root of the docker build context is one directory higher than this test plan.
# See https://docs.testground.ai/builder-library/docker-generic#usage
RUN cd ./plan/ && cargo build --release && cargo install --path .

FROM debian:bullseye-slim
COPY --from=builder /usr/local/cargo/bin/gossipsub-testground /usr/local/bin/gossipsub-testground

ENTRYPOINT ["gossipsub-testground"]