---
title: Environment Variables
description: Full reference for the environment variables Mimic recognizes.
---

Mimic is configured entirely through environment variables. There is no configuration file — the mocks directory *is* the configuration, and everything about the server itself is set via env vars.

## Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8080` | The TCP port Mimic listens on. |
| `MIMIC_BIND_ADDRESS` | `0.0.0.0` | Network interface to bind. Set to `127.0.0.1` to restrict access to localhost. Both IPv4 and IPv6 literals are accepted (e.g. `::1`, `::`) — don't append a port. An unparsable value fails startup rather than silently falling back to `0.0.0.0`. Inside Docker, leave this at `0.0.0.0` — `127.0.0.1` there means "this container", not "this host". See [Binding Address](#binding-address) below. |
| `RUST_LOG` | `info` | Log verbosity: `trace`, `debug`, `info`, `warn`, `error`. |

### Binding address

By default Mimic listens on every network interface, which is convenient on a laptop but means the mock server, its request log, and its admin API are reachable from anywhere that can route to the machine. On a shared box or a machine with a public IP, `MIMIC_BIND_ADDRESS` restricts it:

```bash
MIMIC_BIND_ADDRESS=127.0.0.1 mimic   # only this machine can reach it
MIMIC_BIND_ADDRESS=::1 mimic         # IPv6 loopback
```

The bound address is always visible — it's printed in the startup log next to the port, and reported by `GET /health` as `bind_address`. See [What the Request Log Keeps](/guides/admin-dashboard/#what-the-request-log-keeps) for what's exposed if the admin API is reached.

## Mocks

| Variable | Default | Description |
|---|---|---|
| `MIMIC_MOCKS_DIR` | `/app/mocks` if it exists, else `./mocks` | Directory (or single `.json`/`.yaml`/`.yml` file) mocks are read from. Used verbatim when set — a path that doesn't exist is reported as missing rather than silently replaced. |
| `MIMIC_MAX_BODY_SIZE` | `10485760` (10 MB) | Maximum request body Mimic will buffer, in bytes. Requests over the limit get `413 Payload Too Large`. |
| `MIMIC_MAX_RESPONSE_FILE` | `10485760` (10 MB) | Largest file a mock may serve with [`response_file`](/dynamic-responses/response-file/), in bytes. `0` removes the cap. |
| `MIMIC_STRICT_TRAILING_SLASH` | `false` | Disable [trailing-slash tolerance](/matching/trailing-slash/) — `/things` and `/things/` become distinct paths again. |
| `MIMIC_STRICT_IGNORE_HEADERS` | unset | Comma-separated header names to add to [strict header mode](/matching/headers/#strict-modes-built-in-ignore-list)'s built-in ignore list. |

## Health & Admin

| Variable | Default | Description |
|---|---|---|
| `MIMIC_HEALTH_PATH` | `/health` | Where the health check is served. Empty disables it, freeing the path for a mock. |
| `MIMIC_ADMIN_PREFIX` | `/admin` | Prefix the admin API is mounted under. |
| `MIMIC_DISABLE_ADMIN` | `false` | Switch the admin API off entirely. |
| `MIMIC_ADMIN_TOKEN` | unset | Bearer token the admin API requires. Unset means no authentication. |

See [Reserved Endpoints](/reference/reserved-endpoints/) for the exact routes these control, and [Admin Dashboard](/guides/admin-dashboard/#protecting-the-admin-api) for how the token is checked.

## Request Log

| Variable | Default | Description |
|---|---|---|
| `MIMIC_MAX_LOG_ENTRIES` | `1000` | How many requests the admin request log keeps. `0` is unbounded. |
| `MIMIC_MAX_RECORDED_BODY` | `65536` (64 KB) | How much of each request/response body the log stores, in bytes. `0` stores whole bodies. |
| `MIMIC_REDACT_BODY_FIELDS` | `password,token,secret,api_key` (and variants — see below) | Body field names whose values are replaced with `[REDACTED]` in the request log, comma-separated. Empty stores bodies verbatim. |
| `MIMIC_DISABLE_BODY_LOG` | `false` | Store no request/response bodies in the log at all. |

Full defaults and behavior in [What the Request Log Keeps](/guides/admin-dashboard/#what-the-request-log-keeps).

## Scenario Tags

| Variable | Default | Description |
|---|---|---|
| `MIMIC_ACTIVE_TAGS` | unset | Scenario tags active at startup, comma-separated. Unset means every mock is matchable regardless of `tags`. |

See [Scenario Tags](/guides/scenarios/).

## CORS

| Variable | Default | Description |
|---|---|---|
| `MIMIC_CORS` | `false` | Master switch. Everything below is ignored while it's off. |
| `MIMIC_CORS_ORIGINS` | `*` | `*`, or a comma-separated allowlist. |
| `MIMIC_CORS_METHODS` | `GET,POST,PUT,PATCH,DELETE,OPTIONS` | Advertised as `Access-Control-Allow-Methods` on a preflight. |
| `MIMIC_CORS_HEADERS` | `*` | `*` reflects the request's `Access-Control-Request-Headers`; a list is sent verbatim. |
| `MIMIC_CORS_CREDENTIALS` | `false` | Sends `Access-Control-Allow-Credentials: true`. |
| `MIMIC_CORS_MAX_AGE` | `600` | `Access-Control-Max-Age`, in seconds. |

See [Built-in CORS](/guides/cors/).

## Proxy & Recording

| Variable | Default | Description |
|---|---|---|
| `MIMIC_PROXY_UPSTREAM` | unset | Forward unmatched requests to this upstream instead of 404ing. |
| `MIMIC_RECORD_UPSTREAM` | `false` | Record proxied responses as new mock files. |
| `MIMIC_PROXY_TIMEOUT_MS` | `5000` | How long to wait for the upstream before falling back to a 404. |
| `MIMIC_RECORD_MATCH_HEADERS` | unset | Extra request headers to pin as matchers on a recording, comma-separated. |

See [Proxy & Record-and-Replay](/guides/proxy/).

## Setting variables

### `docker run`

Pass each variable with `-e`:

```bash
docker run -d \
  --name mimic \
  -p 8080:8080 \
  -e PORT=8080 \
  -e RUST_LOG=info \
  -v $(pwd)/mocks:/app/mocks:ro \
  ragilhadi/mimic:latest
```

### `docker-compose.yml`

Use the `environment` block:

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    environment:
      PORT: 8080
      RUST_LOG: info
```

### `.env` file

Docker Compose automatically reads a `.env` file in the same directory as the compose file:

```bash
# .env
PORT=8080
RUST_LOG=info
```

Reference the variables in your compose file with `${VAR}`:

```yaml
services:
  mimic:
    environment:
      - PORT=${PORT}
      - RUST_LOG=${RUST_LOG}
    ports:
      - "${PORT}:${PORT}"
```

### Running from source

If you're running a locally built binary instead of the Docker image, export the variables in your shell:

```bash
export PORT=8080
export RUST_LOG=debug
cargo run --release
```
