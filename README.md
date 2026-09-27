# MarkSync server

Sync server for [MarkSync](https://github.com/MarkSyncOrg/core), written in Rust. It is a
rewrite of the [xBrowserSync API](https://github.com/xbrowsersync/api) and keeps its REST
contract ([`openapi/xbrowsersync-api.yaml`](https://github.com/MarkSyncOrg/core/blob/main/openapi/xbrowsersync-api.yaml)
in MarkSync core) byte for byte. MarkSync clients and existing xBrowserSync clients can use
it without changes.

The server only stores ciphertext. Clients encrypt bookmarks before upload, and the server
never sees keys or plaintext.

## Features

- **Same API as xBrowserSync.** `/info`, `/bookmarks` and `/bookmarks/{id}[/lastUpdated|/version]`
  keep their status codes, `{ code, message }` error bodies and ISO timestamps. Routes are
  selected from `Accept-Version` the same way, including the legacy `~1.0.0` routes. The
  service also applies the same daily new-sync limit per IP, total sync cap, throttling,
  CORS and security headers.
- **Two storage backends:**
  - **SQLite** (default): one file, no database server to run.
  - **MongoDB**: uses the same collections and document shapes as xBrowserSync
    (`bookmarks` with `BinData(4)` UUID ids, and `newsynclogs`). It can serve the database
    of an existing xBrowserSync deployment, and both servers can run against it side by
    side.
- **Same settings as xBrowserSync.** It reads the xBrowserSync `settings.json` format, so
  an existing settings file keeps working.
- **Small image.** A static (musl) binary on a `scratch` base, about 14 MB. The binary has its own
  `healthcheck` command, optional native HTTPS (rustls) and graceful shutdown.
- **Consistent conflict detection.** Updates that pass `lastUpdated` are applied
  atomically, and every update gets a strictly increasing timestamp. Two writes in the
  same millisecond can no longer hide a conflict.

## Quick start

```sh
cargo run --release                      # http://127.0.0.1:8080, data in ./data/marksync.db
cargo run --release -- --config my-settings.json
```

To run it in production with automatic HTTPS (Caddy), the same setup as `xbrowsersync/api-docker`:

```sh
cd deploy
$EDITOR .env            # set API_HOSTNAME
$EDITOR settings.json   # optional service settings
docker compose up -d                                  # SQLite storage
docker compose -f docker-compose.mongo.yml up -d      # or MongoDB storage
```

To run it on a server without a reverse proxy, with automatic updates from GHCR through
[Watchtower](https://github.com/nicholas-fedor/watchtower), use `deploy/standalone/`:

```sh
cd deploy/standalone
echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-user> --password-stdin   # token with read:packages
docker compose up -d
```

In this setup Watchtower checks every container on the host every 5 minutes and cleans up
old images. The MarkSync client accepts plain `http` only for `localhost`. On a public
server, enable native HTTPS (`server.https` in `settings.json`, mounting the certificate),
or put the service behind something that terminates TLS.

To run the image on its own:

```sh
docker run -d -p 8080:8080 -v marksync-data:/data ghcr.io/marksyncorg/marksync-server:latest
```

Images for `linux/amd64` and `linux/arm64` are published to GitHub Container Registry on
every push to `main` (`latest`, `sha-<commit>`) and on `v*` tags (`1.2.3`, `1.2`). While the
repository is private, run `docker login ghcr.io` first, using a personal access token with
the `read:packages` scope. To build locally, use `docker build -t marksync/server .`.

## Configuration

Settings use the xBrowserSync JSON layout. They are layered in this order, and later
layers win:

1. Built-in defaults: [`config/settings.default.json`](config/settings.default.json).
2. The `MARKSYNC_SETTINGS_JSON` environment variable, a JSON object. The Docker image uses
   it for container defaults (listen on `0.0.0.0`, database in `/data`).
3. The settings file: `--config <path>`, then `$MARKSYNC_CONFIG`, then `config/settings.json`.

Objects are merged recursively and arrays are concatenated, like xBrowserSync's `deepmerge`.

| Setting | Default | Description |
| --- | --- | --- |
| `allowedOrigins` | `[]` | CORS allow-list. When empty, any origin is allowed. |
| `dailyNewSyncsLimit` | `3` | New syncs per client IP per day. `0` disables the limit. |
| `db.type` | `"sqlite"` | `"sqlite"` or `"mongodb"`. |
| `db.path` | `"data/marksync.db"` | SQLite file. `":memory:"` keeps the data in memory only. |
| `db.uri` | `""` | Full MongoDB connection string. When set, it overrides `db.host`, `db.port`, the credentials and the other connection fields. |
| `db.host`, `db.port`, `db.name`, `db.username`, `db.password`, `db.authSource`, `db.ssl`, `db.useSRV`, `db.connTimeout` | as in xBrowserSync | MongoDB connection. The credentials fall back to `XBROWSERSYNC_DB_USER` and `XBROWSERSYNC_DB_PWD`. |
| `location` | `""` | ISO 3166-1 alpha-2 country code shown by `/info`. |
| `log.stdout.enabled`, `log.stdout.level` | `true`, `info` | Console logging. |
| `log.file.*` | disabled | Rotating JSON log file (`rotationPeriod` `1h` or `1d`, `rotatedFilesToKeep`). |
| `maxSyncs` | `5242` | Total sync cap. At the cap, `/info` reports status `3`. `0` means no cap. |
| `maxSyncSize` | `512000` | Maximum request body, in bytes. `/info` also reports it to clients. |
| `server.host`, `server.port` | `127.0.0.1`, `8080` | Listen address. |
| `server.behindProxy` | `false` | Take the client IP from `X-Forwarded-For`. |
| `server.https.*` | disabled | Native TLS: `enabled`, `certPath`, `keyPath` (PEM). |
| `server.relativePath` | `"/"` | Path prefix for every route. |
| `status.online` | `true` | When `false`, sync routes answer `503` and `/info` reports status `2`. |
| `status.allowNewSyncs` | `true` | When `false`, new syncs are refused with `405`. |
| `status.message` | `""` | HTML message shown in `/info`. Script tags are stripped. |
| `syncExpiryDays` | `21` | Syncs not accessed for this many days are deleted. `0` keeps syncs forever. |
| `throttle.maxRequests`, `throttle.timeWindow` | `1000`, `300000` | Maximum requests per client IP in each window (in ms). `0` disables throttling. |

## Migrating from xBrowserSync

Point the server at the existing MongoDB database with `"db": { "type": "mongodb", ... }`,
keeping the `db` settings you already use. Existing sync IDs, timestamps and the daily
new-sync logs carry over as they are, and clients do not notice the switch.

Differences from the reference API, all intentional:

- CORS headers are also sent on errors raised before routing, such as an oversized or
  malformed body. xBrowserSync leaves them out, so browsers report a network error instead
  of `413`.
- `GET /bookmarks/{id}` on a sync that has never been uploaded returns `"bookmarks": ""`,
  as the contract requires. xBrowserSync omits the field. Clients handle both.
- When `allowedOrigins` is set, requests without an `Origin` header are still served,
  without CORS headers. This covers health checks and non-browser clients. xBrowserSync
  rejects them.
- The server binds only to `server.host`, where xBrowserSync binds every interface.
  The Docker image sets `0.0.0.0`.
- `GET /` serves a short static description of the API instead of the xBrowserSync docs
  site.
- File logging is off by default.

## Development

```sh
cargo test                                                   # unit + HTTP tests (SQLite)
MARKSYNC_TEST_MONGO_URI=mongodb://127.0.0.1:27017 cargo test --test api   # same suite on MongoDB
cargo clippy --all-targets -- -D warnings
```

MarkSync core also has a contract suite that runs its real client, including crypto,
against a live server:

```sh
MARKSYNC_SETTINGS_JSON='{"dailyNewSyncsLimit":0}' cargo run &
cd ../core && XBS_CONTRACT_URL=http://127.0.0.1:8080 pnpm test:contract
```

The code is organised as follows:

- `src/http.rs`: request handling, routing, CORS, throttling and headers.
- `src/service.rs`: API rules.
- `src/store/`: the SQLite and MongoDB backends.
- `src/version.rs`: `Accept-Version` matching.
- `src/config.rs`: settings.

## License

[MIT](LICENSE). This is an independent implementation written from scratch in Rust; it
contains no code from the GPL-3.0 xBrowserSync API, whose REST interface it implements.
