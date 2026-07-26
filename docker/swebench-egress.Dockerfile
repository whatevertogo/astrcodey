FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates python3 tinyproxy \
    && rm -rf /var/lib/apt/lists/*

COPY docker/swebench-egress-tinyproxy.conf /etc/tinyproxy/tinyproxy.conf
COPY docker/swebench-egress-filter /etc/tinyproxy/swebench-egress-filter
COPY docker/swebench-provider-gateway.py /usr/local/bin/swebench-provider-gateway.py
COPY docker/swebench-egress-entrypoint /usr/local/bin/swebench-egress-entrypoint
COPY docker/swebench-control-relay.py /usr/local/bin/swebench-control-relay.py
RUN chmod 0555 \
    /usr/local/bin/swebench-control-relay.py \
    /usr/local/bin/swebench-egress-entrypoint \
    /usr/local/bin/swebench-provider-gateway.py

USER tinyproxy
EXPOSE 8080 8888
ENTRYPOINT ["/usr/local/bin/swebench-egress-entrypoint"]
