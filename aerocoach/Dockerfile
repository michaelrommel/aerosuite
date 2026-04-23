FROM rust:alpine AS builder
COPY aerocoach /workspace/aerocoach
COPY aeroproto /workspace/aeroproto
COPY aerocore /workspace/aerocore
COPY Cargo.toml /workspace/Cargo.toml

# Create dummy folders/manifests for OTHER members listed in root Cargo.toml
RUN mkdir -p \
	/workspace/aeroftp/src \
	/workspace/aerogym/src \
	/workspace/aeroplug/src \
	/workspace/aeropulse/src \
	/workspace/aeroscale/src \
	/workspace/aeroslot/src && \
    echo -e '[package]\nname = "aeroftp"\nversion = "0.1.0"' > /workspace/aeroftp/Cargo.toml && \
    echo -e '[package]\nname = "aerogym"\nversion = "0.1.0"' > /workspace/aerogym/Cargo.toml && \
    echo -e '[package]\nname = "aeroplug"\nversion = "0.1.0"' > /workspace/aeroplug/Cargo.toml && \
    echo -e '[package]\nname = "aeropulse"\nversion = "0.1.0"' > /workspace/aeropulse/Cargo.toml && \
    echo -e '[package]\nname = "aeroscale"\nversion = "0.1.0"' > /workspace/aeroscale/Cargo.toml && \
    echo -e '[package]\nname = "aeroslot"\nversion = "0.1.0"' > /workspace/aeroslot/Cargo.toml && \
	touch /workspace/aeroftp/src/lib.rs && \
	touch /workspace/aerogym/src/lib.rs && \
	touch /workspace/aeroplug/src/lib.rs && \
	touch /workspace/aeropulse/src/lib.rs && \
	touch /workspace/aeroscale/src/lib.rs && \
	touch /workspace/aeroslot/src/lib.rs

WORKDIR /workspace
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static protoc
RUN cargo build --release -p aerocoach

FROM alpine
WORKDIR /aerocoach
COPY --from=builder --chown=aerocoach:aerocoach /workspace/target/release/aerocoach .
COPY aerocoach/scripts/run.sh .
COPY aerocoach/scripts/test_plan.json .
RUN apk add --no-cache curl tcpdump procps 
RUN adduser -D aerocoach
RUN chown aerocoach:aerocoach .
USER aerocoach
CMD [ "./run.sh" ]

