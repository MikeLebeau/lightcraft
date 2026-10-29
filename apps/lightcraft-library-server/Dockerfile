FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p lightcraft-library-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin lightcraft \
    && mkdir /data \
    && chown lightcraft:lightcraft /data
COPY --from=build /src/target/release/lightcraft-library-server /usr/local/bin/lightcraft-library-server
USER 10001:10001
ENV LIGHTCRAFT_BIND=0.0.0.0:8090 \
    LIGHTCRAFT_DATA_DIR=/data \
    LIGHTCRAFT_ALLOWED_ORIGIN=http://127.0.0.1:8080
EXPOSE 8090
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/lightcraft-library-server"]
