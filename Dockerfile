FROM rust:alpine AS builder
COPY . /workspace/aeroscrape
WORKDIR /workspace/aeroscrape
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static
RUN cargo build --release

FROM alpine
WORKDIR /aeroscrape
RUN adduser -D aeroscrape
COPY --from=builder --chown=aeroscrape:aeroscrape /workspace/aeroscrape/target/release/aeroscrape .
COPY scripts/run.sh .
RUN chown aeroscrape:aeroscrape .
USER aeroscrape
CMD [ "./run.sh" ]

