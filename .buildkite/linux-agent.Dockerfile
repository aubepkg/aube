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

# -f + explicit installer file: a piped `curl | sh` with no -f would let a
# network-level curl failure produce an empty script and a silently
# mise-less image; verifying the binary afterwards fails the layer loudly.
RUN set -eux; \
  curl --proto '=https' --tlsv1.2 -fsSL https://mise.run -o /tmp/mise-install.sh; \
  sh /tmp/mise-install.sh; \
  rm /tmp/mise-install.sh; \
  /root/.local/bin/mise --version
# Pre-bake the repo's toolchains with MINIMAL profiles + only the components
# jobs use (the default profile ships rust-docs, ~600 MB per toolchain):
#   - nightly-2026-07-04: the rust-toolchain.toml pin (rustfmt/clippy for
#     lint jobs, llvm-tools-preview for the PGO flow)
#   - 1.95.0: the msrv floor cargo-msrv verifies against
#   - stable: rustup's default for anything outside the repo checkout
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable \
  && /root/.cargo/bin/rustup toolchain install nightly-2026-07-04 --profile minimal \
      -c rustfmt -c clippy -c llvm-tools-preview \
  && /root/.cargo/bin/rustup toolchain install 1.95.0 --profile minimal \
  && rm -rf /root/.rustup/downloads /root/.rustup/tmp

# Pre-bake Socket Firewall (free tier — keyless; jobs holding a Socket key
# select enterprise at job time) plus its package-manager shims, pinned +
# integrity-checked against external-tools.json (sfw-free 1.13.1, sha512
# hex below = that file's SRI decoded).
COPY sfw-shim-template.sh /tmp/sfw-shim-template.sh
RUN set -eux; \
  arch="$(uname -m)"; \
  case "$arch" in \
    x86_64)  asset=sfw-free-linux-x86_64; sha=c1a2ebb0f1b66bb10ebf45eebd70d0646802678313b4e7d987c4e619b33a827d81e8d87a1c8fb5e636a829d012f7081d4f222a4eea94f8ef8db5561dfa4b8568 ;; \
    aarch64) asset=sfw-free-linux-arm64;  sha=158458479d922fe28a165756e288183949d31f9392aafb57c331857968b45b6c7eef929970f886cd605b84c9a1d1fe50b6060b57ee2a75112098a08111ac8db4 ;; \
    *) echo "unsupported arch $arch" >&2; exit 1 ;; \
  esac; \
  mkdir -p /root/.local/share/aube/dev-tools/rack/sfw-free/1.13.1 /root/.local/share/aube/dev-tools/bin; \
  curl -fsSL -o /root/.local/share/aube/dev-tools/rack/sfw-free/1.13.1/sfw \
    "https://github.com/SocketDev/sfw-free/releases/download/v1.13.1/$asset"; \
  echo "$sha  /root/.local/share/aube/dev-tools/rack/sfw-free/1.13.1/sfw" | sha512sum -c -; \
  chmod 0755 /root/.local/share/aube/dev-tools/rack/sfw-free/1.13.1/sfw; \
  ln -sf /root/.local/share/aube/dev-tools/rack/sfw-free/1.13.1/sfw /root/.local/share/aube/dev-tools/bin/sfw; \
  for cmd in npm yarn pnpm pip pip3 uv cargo; do \
    sentinel="SFW_SHIM_ACTIVE_$(printf '%s' "$cmd" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')"; \
    sed -e "s/__CMD__/$cmd/g" -e "s/__SENTINEL__/$sentinel/g" \
      /tmp/sfw-shim-template.sh > "/root/.local/share/aube/dev-tools/bin/$cmd"; \
    chmod 0755 "/root/.local/share/aube/dev-tools/bin/$cmd"; \
  done; \
  rm /tmp/sfw-shim-template.sh

ENV PATH="/root/.local/share/aube/dev-tools/bin:/root/.cargo/bin:/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
