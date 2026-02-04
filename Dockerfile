FROM rust:alpine AS builder
COPY libunftp /workspace/libunftp
COPY unftp-sbe-opendal /workspace/unftp-sbe-opendal
COPY aeroftp /workspace/aeroftp
WORKDIR /workspace/aeroftp
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static
RUN cargo build --release
RUN apk add libcap-setcap
RUN setcap CAP_NET_BIND_SERVICE=+eip /workspace/aeroftp/target/release/aeroftp

FROM alpine
WORKDIR /aeroftp
COPY --from=builder --chown=aeroftp:aeroftp /workspace/aeroftp/target/release/aeroftp .
COPY --from=builder --chown=aeroftp:aeroftp /workspace/aeroftp/target/release/ecs_upload .
# COPY --from=builder --chown=aeroftp:aeroftp /workspace/aeroftp/target/release/s3_upload .
# RUN apk add libcap-getcap
RUN apk add libcap-utils
RUN apk add procps-ng
RUN apk add coreutils
RUN apk add jq
COPY aeroftp/scripts/track_mem.sh ./scripts/
COPY aeroftp/scripts/track_conn.sh ./scripts/
COPY aeroftp/scripts/track_cpu.sh ./scripts/
COPY aeroftp/scripts/stats.sh ./scripts/
COPY <<-EOT /aeroftp/credentials.json
[
	{ "username": "test", "password": "secret" }
]
EOT
RUN adduser -D aeroftp
USER aeroftp
CMD [ "./aeroftp" ]

