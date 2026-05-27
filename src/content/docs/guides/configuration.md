---
title: Configuration
description: Environment variables for tuning the Mimic server.
---

Mimic is configured entirely through environment variables. There is no configuration file — the mocks directory *is* the configuration, and everything about the server itself is set via env vars.

## Variables

| Variable   | Default | Description |
|------------|---------|-------------|
| `PORT`     | `8080`  | The TCP port Mimic listens on. |
| `RUST_LOG` | `info`  | Log verbosity. One of `trace`, `debug`, `info`, `warn`, `error`. |

## Setting variables with Docker

Pass `-e` flags on `docker run`:

```bash
docker run -d \
  --name mimic \
  -p 9000:9000 \
  -e PORT=9000 \
  -e RUST_LOG=debug \
  -v $(pwd)/mocks:/app/mocks:ro \
  ragilhadi/mimic:latest
```

## Setting variables with Docker Compose

Use the `environment` block in your `docker-compose.yml`:

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    ports:
      - "8080:8080"
    volumes:
      - ./mocks:/app/mocks:ro
    environment:
      PORT: 8080
      RUST_LOG: info
```

Or load them from a `.env` file in the same directory as your compose file:

```bash
# .env
PORT=8080
RUST_LOG=info
```

Docker Compose picks up the `.env` file automatically.

## Log levels

| Level    | Use when |
|----------|----------|
| `error`  | Production, when you only want to hear about real problems. |
| `warn`   | Production with a bit more context. |
| `info`   | The recommended default. Logs each request and significant events. |
| `debug`  | Local development. Shows mock matching decisions and reload events. |
| `trace`  | Deep debugging. Very verbose — use only when you're hunting a specific bug. |

Setting `RUST_LOG=debug` is the single most useful thing to do when something isn't working as you expect — particularly with [advanced matching](/matching/overview/), where you might want to see *why* a particular mock did or didn't match.
