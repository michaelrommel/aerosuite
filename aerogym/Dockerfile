FROM rust:alpine AS builder
COPY . /workspace/aerostress
WORKDIR /workspace/aerostress
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static
RUN cargo build --release

FROM alpine
WORKDIR /aerostress
COPY --from=builder --chown=aerostress:aerostress /workspace/aerostress/target/release/aerostress .
COPY scripts/run.sh .
RUN adduser -D aerostress
RUN chown aerostress:aerostress .
USER aerostress
CMD [ "./run.sh" ]

