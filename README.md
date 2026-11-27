# Mimic

[![Tests](https://github.com/ragilhadi/mimic/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/mimic/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/mimic?label=docker)](https://hub.docker.com/r/ragilhadi/mimic)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

**Mock HTTP APIs with JSON or YAML.** Mimic is a lightweight server for local development and integration tests. Edit mock files while it runs, test retries with response sequences, switch scenarios to simulate errors, and inspect requests in the dashboard.

**[Read the documentation](https://mimic.ragilhadi.com/)**

## Resource usage

A `docker stats --no-stream` snapshot from the local Compose playground.

```text
NAME                 CPU %    MEM USAGE / LIMIT
mimic-test-mimic-1    2.52%    9.004 MiB / 31.27 GiB
mimic-test-proxy-1    0.02%    2.254 MiB / 31.27 GiB
```

## Quick start

### 1. Create a mock

Save a request method and path, a status code, and a response body in `mocks/hello.json`.

```sh
mkdir -p mocks
cat > mocks/hello.json <<'JSON'
{
  "method": "GET",
  "path": "/hello",
  "status": 200,
  "response": {"message": "Hello from Mimic"}
}
JSON
```

Prefer YAML? Save the same mock as `mocks/hello.yaml` or `mocks/hello.yml`.

```yaml
method: GET
path: /hello
status: 200
response:
  message: Hello from Mimic
```

Use one file for `GET /hello`. You can mix JSON and YAML for different mocks in the same directory. Mimic reloads them when you save.

### 2. Start Mimic and call the mock

```sh
docker run --rm -d --name mimic \
  -p 127.0.0.1:8080:8080 \
  -v "$(pwd)/mocks:/app/mocks:ro" \
  ragilhadi/mimic:latest
```

```sh
curl http://localhost:8080/hello
```

Open the [local dashboard](http://localhost:8080/admin/dashboard) to inspect requests and loaded mocks. Stop the server with `docker stop mimic`.

## Documentation

- [Install and deploy](https://mimic.ragilhadi.com/start-here/installation/)
- [Define mocks and match requests](https://mimic.ragilhadi.com/matching/overview/)
- [Import an OpenAPI spec](https://mimic.ragilhadi.com/guides/openapi-import/)
- [Configure Mimic](https://mimic.ragilhadi.com/reference/environment/)
- [Use the admin API](https://mimic.ragilhadi.com/reference/admin-api/)

The documentation source lives on the [`docs` branch](https://github.com/ragilhadi/mimic/tree/docs).

## Development

Run `cargo run` to start Mimic from source and `cargo test` to run the test suite.

Mimic is licensed under [Apache 2.0](LICENSE).
