# LightCraft shared-library service (experimental)

This companion service is the server-side storage foundation for the browser shared-library PoC. The Web app can opt into it with `?remote=<service-base-url>`; without that parameter it continues to use OPFS or IndexedDB.

## Local development

Set a private token (at least 32 characters), then run the service directly with Cargo:

```sh
export LIGHTCRAFT_API_TOKEN='replace-with-a-private-32-character-or-longer-token'
export LIGHTCRAFT_DATA_DIR="$PWD/target/nas-library-poc"
export LIGHTCRAFT_ALLOWED_ORIGIN='http://127.0.0.1:8080'
cargo run -p lightcraft-library-server
```

It listens on `127.0.0.1:8090`. The data directory is separate from the application source and is retained across process restarts. `GET /healthz` is the only unauthenticated route.

The initial API is deliberately small:

- `POST /v1/lease` with `Authorization: Bearer <token>` acquires the single-editor lease.
- `POST /v1/lease/renew` and `DELETE /v1/lease` also require `X-LightCraft-Lease: <lease-id>`.
- `POST /v1/lease/handoff`, the current editor's `GET /v1/lease/handoff/pending`, its `POST /v1/lease/handoff/<id>/ack`, and the requester's `GET /v1/lease/handoff/<id>` coordinate a saved handoff. `POST /v1/lease/takeover` is the explicit timeout fallback.
- `GET /v1/files/<path>`, `PUT /v1/files/<path>`, `PATCH /v1/files/<path>` and `DELETE /v1/files/<path>` require both headers.
- `HEAD /v1/files/<path>` checks whether a file exists without downloading it.
- `PATCH` is restricted to `library/catalog.log` and requires `X-LightCraft-Offset`; stale offsets are rejected.
- `GET /v1/list?path=<directory>` lists direct children.

Writes are serialized, whole-file replacements use a temporary file and rename, originals are checked against LightCraft's content hash, and expired leases are rejected on every file operation. A lease is held in service memory and expires after 300 seconds by default; the browser renews it every 15 seconds on a timer independent of UI rendering. This longer window also tolerates background-tab timer throttling. Handoff requests are held in memory too: the active tab polls, blocks edits, flushes pending writes and acknowledges before the server transfers the lease. If it does not answer within 30 seconds, the requester is offered a warned forced takeover. A service restart invalidates the old lease and any pending request.

## Docker / Synology

The container is for deployment validation, not ordinary code iteration. Build from the workspace root with:

```sh
docker compose -f apps/lightcraft-library-server/compose.yaml build
```

Copy `.env.example` to `.env` beside `compose.yaml`, then set `LIGHTCRAFT_DATA_PATH` to a new, dedicated directory on the NAS, `LIGHTCRAFT_API_TOKEN` to a unique secret, and `LIGHTCRAFT_ALLOWED_ORIGIN` to the exact browser origin serving the Web app (scheme, host and optional port; no path). The compose file targets `linux/amd64`, matching the DS218+'s 64-bit Intel CPU, even when the image is built on an ARM Mac. The NAS should build it natively when possible. It binds the API port to host loopback so it is not directly exposed to the LAN; configure DSM's HTTPS reverse proxy to reach it. Do not expose this experimental service directly to the internet.

The same Compose project serves the Web frontend in an Nginx container. Build the static bundle on the development computer with `cargo xtask web`, then copy the complete `<target>/web-site/` directory to `LIGHTCRAFT_WEB_SITE_PATH` on the NAS (the default example is `/volume1/docker/lightcraft-web-site`). Nginx mounts it read-only, binds to NAS loopback port 8080, serves WebAssembly with the `application/wasm` MIME type, and uses the generated gzip sidecars. This avoids compiling Rust/WASM on the DS218+. Configure a second DSM HTTPS reverse-proxy hostname for the frontend, forwarding to `http://127.0.0.1:8080`; keep the API hostname forwarding to port 8090. Set `LIGHTCRAFT_ALLOWED_ORIGIN` to the exact HTTPS origin of the frontend.

From the repository root on the NAS, build and start it with:

```sh
docker compose --env-file apps/lightcraft-library-server/.env -f apps/lightcraft-library-server/compose.yaml build
docker compose --env-file apps/lightcraft-library-server/.env -f apps/lightcraft-library-server/compose.yaml up -d
```

The mounted catalog directory must be writable by the server's unprivileged user (UID 10001). The web-site directory only needs to be readable by the Nginx container. In DSM, grant the required permissions to both paths before starting the services.

The persistent bind mount is separate from the container image. Rebuilding or replacing the image does not replace the data directory. Use only disposable catalog and image data until the browser client, lease-loss handling, backup strategy, and NAS behavior have been exercised.

For the local development pairing, run `cargo xtask web --serve`, then open `http://127.0.0.1:8080/?remote=http://127.0.0.1:8090`. The page prompts for the service token. For the NAS-hosted setup, open the frontend's HTTPS hostname with `?remote=https://<API-hostname>`. Remote rendering starts one Web Worker by default (`&workers=2` to compare two, `&workers=0` to disable); workers share the page's lease. A closed or crashed browser releases the lease explicitly when possible; the 300-second expiry is the fallback.

This is an early PoC service, not a production backup or multi-user service. Use a disposable data directory until the browser flow and DSM deployment have been exercised.
