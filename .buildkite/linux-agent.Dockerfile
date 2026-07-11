FROM buildkite/hosted-agent-base:latest@sha256:db770041c55b13a92ddb8365dc601a0141add0459dfd1d804f3e28926d4770da

ENV DEBIAN_FRONTEND=noninteractive
ENV MISE_EXPERIMENTAL=true
ENV MISE_YES=true

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    bash \
    build-essential \
    ca-certificates \
    curl \
    git \
    libssl-dev \
    parallel \
    pkg-config \
    xz-utils \
  && rm -rf /var/lib/apt/lists/*

RUN curl https://mise.run | sh
# Pre-bake the repo's pinned toolchain (rust-toolchain.toml) with the
# llvm-tools-preview the PGO flow needs, plus the 1.95 msrv floor, so
# agent jobs don't rustup-download at job time.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile default --default-toolchain stable \
  && /root/.cargo/bin/rustup toolchain install nightly-2026-07-04 --profile default -c llvm-tools-preview \
  && /root/.cargo/bin/rustup toolchain install 1.95.0 --profile default \
  && /root/.cargo/bin/rustup component add rustfmt clippy --toolchain stable

ENV PATH="/root/.cargo/bin:/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
