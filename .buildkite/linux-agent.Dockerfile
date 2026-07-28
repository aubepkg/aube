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

# Download the installer before running it so network failures cannot yield an
# empty script and silently produce a mise-less image.
RUN set -eux; \
  curl --proto '=https' --tlsv1.2 -fsSL https://mise.run -o /tmp/mise-install.sh; \
  sh /tmp/mise-install.sh; \
  rm /tmp/mise-install.sh; \
  /root/.local/bin/mise --version
# Minimal-profile toolchains only (the default profile ships rust-docs,
# ~600 MB per toolchain): stable as the default, plus the MSRV floors the
# CI legs verify against (1.91 workspace rust-version; 1.93 leg). NO dated
# nightly is baked — the repo deliberately ships no rust-toolchain.toml
# (#1039), and the one nightly consumer (`cargo +nightly update` in
# scripts/soak/update-deps.mts) runs on dev machines, not this agent.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable \
  && /root/.cargo/bin/rustup toolchain install 1.91.0 1.93.0 --profile minimal \
  && /root/.cargo/bin/rustup component add rustfmt clippy --toolchain stable \
  && rm -rf /root/.rustup/downloads /root/.rustup/tmp

# Pre-bake Socket Firewall (free tier — keyless; jobs holding a Socket key
# select enterprise at job time) plus its package-manager shims, pinned +
# integrity-checked against external-tools.json (sfw-free 1.14.0, sha512
# hex below = that file's SRI decoded). scripts/soak/external-tools.mts
# --check asserts these embedded copies never drift from the manifest.
COPY sfw-shim-template.sh /tmp/sfw-shim-template.sh
RUN set -eux; \
  arch="$(uname -m)"; \
  case "$arch" in \
    x86_64)  asset=sfw-free-linux-x86_64; sha=85b108842381b948c50e93580d8149d63fc8c080226b1287b06690e1448e8b004ad72d69a2ea46948462aad0e4563e0ff71fbd2341a48b7c6856ae82da7c816f ;; \
    aarch64) asset=sfw-free-linux-arm64;  sha=098a790b829eca08576b4494b53e96afe5fce75857217d377234c52b7a80c94ff1f874245bb65a9c07ef5a13becc29aff0306b5c7556a38365bfa5673fabd75d ;; \
    *) echo "unsupported arch $arch" >&2; exit 1 ;; \
  esac; \
  mkdir -p /root/.local/share/aube/dev-tools/rack/sfw-free/1.14.0 /root/.local/share/aube/dev-tools/bin; \
  curl -fsSL -o /root/.local/share/aube/dev-tools/rack/sfw-free/1.14.0/sfw \
    "https://github.com/SocketDev/sfw-free/releases/download/v1.14.0/$asset"; \
  echo "$sha  /root/.local/share/aube/dev-tools/rack/sfw-free/1.14.0/sfw" | sha512sum -c -; \
  chmod 0755 /root/.local/share/aube/dev-tools/rack/sfw-free/1.14.0/sfw; \
  ln -sf /root/.local/share/aube/dev-tools/rack/sfw-free/1.14.0/sfw /root/.local/share/aube/dev-tools/bin/sfw; \
  for cmd in npm yarn pnpm pip pip3 uv cargo; do \
    sentinel="SFW_SHIM_ACTIVE_$(printf '%s' "$cmd" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')"; \
    sed -e "s/__CMD__/$cmd/g" -e "s/__SENTINEL__/$sentinel/g" \
      /tmp/sfw-shim-template.sh > "/root/.local/share/aube/dev-tools/bin/$cmd"; \
    chmod 0755 "/root/.local/share/aube/dev-tools/bin/$cmd"; \
  done; \
  rm /tmp/sfw-shim-template.sh

ENV PATH="/root/.local/share/aube/dev-tools/bin:/root/.cargo/bin:/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
