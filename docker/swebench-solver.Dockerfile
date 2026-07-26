FROM rustlang/rust:nightly-bookworm@sha256:1bb02816a99b185e05389d95b6a7f203ebd88a027203047ddf6dc775904b82b9 AS builder

WORKDIR /src
COPY . .
RUN cargo build --locked --release -p astrcode-cli --features dev-mode

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

ENV DEBIAN_FRONTEND=noninteractive \
    ASTRCODE_TEST_HOME=/state \
    GIT_CONFIG_GLOBAL=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_TERMINAL_PROMPT=0

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        jq \
        python-is-python3 \
        python3-pip \
        python3-venv \
        ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 astrcode \
    && install -d -o astrcode -g astrcode /state /work

ENV TMPDIR=/work

COPY --from=builder /src/target/release/astrcode /usr/local/bin/astrcode

USER astrcode
WORKDIR /work
ENTRYPOINT ["astrcode"]
CMD ["--help"]
