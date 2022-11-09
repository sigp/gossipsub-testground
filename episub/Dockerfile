# based on laurentsenta/shared-deps-testground-example

FROM rust:1.65-bullseye as builder
WORKDIR /usr/src/test-plan

RUN apt-get update

# Accept the PLAN_PATH parameters that is passed by testground.
ARG PLAN_PATH

# == (optional) docker layer caching with rust ==
# Note: I'm leaving this for later reference, but rust
# is not a good candidate for docker layer caching:
# creating an empty `main.rs` hack below is enough to fake support with simple projects,
# but with more complex workspaces, it's not enough.
RUN mkdir -p ./plan/${PLAN_PATH}/src/

# This is a placeholder main function to build only the dependencies.
RUN echo "fn main() { println!(\"If you see this message, you may want to clean up the target directory or the Docker build cache.\") }" > ./plan/${PLAN_PATH}/src/main.rs

# Copy the current test's Cargo.toml
COPY ./plan/${PLAN_PATH}/Cargo.toml ./plan/${PLAN_PATH}/

# Copy the other required packages Cargo.toml or the build will fail.
# All the workspace members need to go here
COPY ./plan/utils/gen_topology ./plan/utils/gen_topology
COPY ./plan/utils/gen_topology_files ./plan/utils/gen_topology_files
COPY ./plan/smoke ./plan/smoke

# Copy the root Cargo.toml
COPY ./plan/Cargo.toml ./plan/

# This will build a "simple" main, used to cache dependencies between runs.

RUN apt-get update && apt-get -y upgrade && apt-get install -y protobuf-compiler
RUN cd ./plan/${PLAN_PATH} && cargo build --release -p episub

# == end of docker layer caching with rust ==

COPY . .

# required when you use build caching.
RUN touch ./plan/${PLAN_PATH}/src/main.rs
RUN cd ./plan/${PLAN_PATH}/ && cargo build --release -p episub

FROM debian:bullseye-slim
COPY --from=builder /usr/src/test-plan/plan/target/release/episub /usr/local/bin/episub

ENTRYPOINT ["episub"]
