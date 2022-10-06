# This Dockerfile is for the `docker:generic` builder.
# See https://docs.testground.ai/builder-library/docker-generic for details about the builder.
FROM rust:1.62-bullseye as builder
WORKDIR /usr/src/test-plan

# * `prost-build`, a dependency of `libp2p-gossipsub`, requires cmake.
#   There is a discussion for removing cmake from their dependency.
#   https://github.com/tokio-rs/prost/pull/620
# * Since `prost-build` v0.11, `protoc` is required.
#   https://github.com/tokio-rs/prost/releases/tag/v0.11.0
RUN apt-get update && apt-get install -y cmake && apt-get install -y protobuf-compiler

# Cache dependencies between test runs,
# See https://blog.mgattozzi.dev/caching-rust-docker-builds/
# And https://github.com/rust-lang/cargo/issues/2644
RUN mkdir -p ./plan/src/
# This is a placeholder main function to build only the dependencies.
RUN echo "fn main() { println!(\"If you see this message, you may want to clean up the target directory or the Docker build cache.\") }" > ./plan/src/main.rs
COPY ./plan/Cargo.toml ./plan/
RUN cd ./plan/ && cargo build --release

COPY . .

# This is in order to make sure `main.rs`s mtime timestamp is updated to avoid the dummy `main`
# remaining in the release binary.
# https://github.com/rust-lang/cargo/issues/9598
RUN touch ./plan/src/main.rs

# Note: In `docker:generic` builder, the root of the docker build context is one directory higher
# than this test plan.
# See https://docs.testground.ai/builder-library/docker-generic#usage
RUN cd ./plan/ && cargo build --release

FROM debian:bullseye-slim
COPY --from=builder /usr/src/test-plan/plan/target/release/emulation /usr/local/bin/emulation

# Configure Logging
# ENV RUST_LOG=libp2p_gossipsub=debug

ENTRYPOINT ["emulation"]