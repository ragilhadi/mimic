---
title: Docker Compose
description: Run Mimic alongside other services with Docker Compose.
---

For anything beyond a one-off `docker run`, Docker Compose is the recommended way to run Mimic. It keeps your configuration in one file, makes restarts easy, and plays well with multi-service projects.

## Minimal compose file

`docker-compose.yml`:

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    container_name: mimic
    ports:
      - "8080:8080"
    volumes:
      - ./mocks:/app/mocks:ro
    environment:
      - PORT=8080
      - RUST_LOG=info
    restart: unless-stopped
```

Start it:

```bash
docker compose up -d
```

Stop and remove it:

```bash
docker compose down
```

## With a health check

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    container_name: mimic
    ports:
      - "8080:8080"
    volumes:
      - ./mocks:/app/mocks:ro
    environment:
      - PORT=8080
      - RUST_LOG=info
    restart: unless-stopped
    healthcheck:
      # bash /dev/tcp probe -- bash ships in the image, wget does not
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/$${PORT:-8080} && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && head -1 <&3 | grep -q ' 200 ' || exit 1"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s
```

The image deliberately ships without `wget` or `curl` (it keeps the CVE surface small), so the probe uses bash's built-in `/dev/tcp`. The `$${PORT:-8080}` follows whatever `PORT` you set.

`start_period: 10s` gives Mimic time to scan the mocks folder before health checks begin failing.

## Alongside a frontend app

A common setup: your frontend dev server runs alongside Mimic so the frontend can hit Mimic as its API.

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    ports:
      - "8080:8080"
    volumes:
      - ./mocks:/app/mocks:ro
    environment:
      - RUST_LOG=info

  web:
    image: node:20-alpine
    working_dir: /app
    volumes:
      - ./web:/app
    ports:
      - "3000:3000"
    command: sh -c "npm install && npm run dev"
    environment:
      - API_BASE_URL=http://mimic:8080
    depends_on:
      - mimic
```

Inside the `web` container, the frontend can reach Mimic at `http://mimic:8080` (Docker Compose sets up DNS between services). From your host machine, both are reachable at `localhost:3000` and `localhost:8080`.

## Using a `.env` file

Compose automatically picks up a `.env` file in the same directory:

```bash
# .env
PORT=8080
RUST_LOG=debug
```

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    ports:
      - "${PORT}:${PORT}"
    volumes:
      - ./mocks:/app/mocks:ro
    environment:
      - PORT=${PORT}
      - RUST_LOG=${RUST_LOG}
    restart: unless-stopped
```

This is handy when running the same compose file across multiple environments (e.g. different ports on dev and CI).

## Common commands

```bash
# Start in background
docker compose up -d

# View logs
docker compose logs -f mimic

# Restart just Mimic
docker compose restart mimic

# Stop everything
docker compose down

# Stop and remove volumes (be careful)
docker compose down -v

# Pull newer images
docker compose pull

# Recreate after pulling
docker compose up -d --force-recreate
```

## Restricting network access

If the compose host is reachable beyond your own machine, bind the published port to localhost instead of every interface:

```yaml
services:
  mimic:
    image: ragilhadi/mimic:latest
    ports:
      - "127.0.0.1:8080:8080"
    volumes:
      - ./mocks:/app/mocks:ro
```

Leave `MIMIC_BIND_ADDRESS` at its default `0.0.0.0` inside the container — it's the `127.0.0.1:` prefix on the host-side port mapping that actually restricts reachability. See [Binding Address](/reference/environment/#binding-address) and, if the admin API is exposed, [Protecting the Admin API](/guides/admin-dashboard/#protecting-the-admin-api).

## Next steps

- For configuring log level and port: [Environment Variables](/reference/environment/).
- For the full mock file format: [Mock File Schema](/reference/mock-schema/).
