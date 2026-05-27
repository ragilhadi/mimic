---
title: Environment Variables
description: Full reference for the environment variables Mimic recognizes.
---

Mimic is configured entirely through environment variables. There are only two.

## `PORT`

The TCP port Mimic listens on.

- **Type:** integer
- **Default:** `8080`
- **Example:** `PORT=9000`

When you change `PORT`, you also need to update the Docker port mapping. For example, to run Mimic on port 9000:

```bash
docker run -d \
  --name mimic \
  -p 9000:9000 \
  -e PORT=9000 \
  -v $(pwd)/mocks:/app/mocks:ro \
  ragilhadi/mimic:latest
```

## `RUST_LOG`

Controls log verbosity.

- **Type:** string
- **Default:** `info`
- **Allowed values:** `trace`, `debug`, `info`, `warn`, `error`

| Level   | When to use |
|---------|-------------|
| `error` | Production. Logs only real errors. |
| `warn`  | Production with a bit more context. |
| `info`  | Default. Logs requests and major events. |
| `debug` | Local development and rule debugging. Shows match scoring and reload events. |
| `trace` | Deep debugging. Very verbose. |

`debug` is particularly useful when figuring out why a specific mock is or isn't being selected. See [Match Priority](/matching/priority/) for what those logs look like.

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
