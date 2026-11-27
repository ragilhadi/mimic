# Mimic

[![Tests](https://github.com/ragilhadi/mimic/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/mimic/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/mimic?label=docker)](https://hub.docker.com/r/ragilhadi/mimic)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

**Mock HTTP APIs with JSON or YAML.** Mimic is a lightweight mock server for local development and integration tests. Run it in Docker, keep mock definitions alongside your tests, and change responses without restarting the server.

[Quick start](#quick-start) · [Features](#features) · [Documentation](https://mimic.ragilhadi.com/)

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

```json
{"message":"Hello from Mimic"}
```

Stop the server with `docker stop mimic`.

## Features

- **Match requests** by method, path parameters, query parameters, headers, or body.
- **Test failure and recovery** with response delays, sequences, and switchable scenarios.
- **Verify requests** through the admin API, including call counts and explanations for unmatched requests.
- **Reuse API definitions** by importing OpenAPI specs or recording and replaying upstream responses.

For example, [this query matcher](mocks/advanced/get_query_without_search.json) uses `query_params.forbidden` to serve requests with no `q` parameter while still accepting parameters like `page`. An empty `?q=` still counts as present.

## Resource usage

A single `docker stats --no-stream mimic` sample with the quick-start mock loaded and the server idle.

```text
NAME      CPU %     MEM USAGE / LIMIT
mimic     0.00%     2.137MiB / 31.27GiB
```

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
