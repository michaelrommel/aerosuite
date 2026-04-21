FROM rust:alpine AS builder
COPY . /workspace/aerogym
WORKDIR /workspace/aerogym
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static
RUN cargo build --release

FROM alpine
WORKDIR /aerogym
COPY --from=builder --chown=aerogym:aerogym /workspace/aerogym/target/release/aerogym .
COPY scripts/run.sh .
RUN adduser -D aerogym
RUN chown aerogym:aerogym .
USER aerogym
CMD [ "./run.sh" ]

