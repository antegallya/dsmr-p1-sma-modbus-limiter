FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs \
    && cargo build --release \
    && cp target/release/dsmr-sma-limiter /build/dsmr-sma-limiter

FROM scratch
COPY --from=builder /build/dsmr-sma-limiter /dsmr-sma-limiter

ENV DRY_RUN=true
ENV LOG_LEVEL=info
ENV HEALTH_PORT=8080

EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/dsmr-sma-limiter", "--healthcheck"]

ENTRYPOINT ["/dsmr-sma-limiter"]
