# Mimic

[![Tests](https://github.com/ragilhadi/mimic/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/mimic/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/mimic?label=docker)](https://hub.docker.com/r/ragilhadi/mimic)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

Mimic is a lightweight HTTP mock server for development and testing. Define responses in JSON or YAML and edit them while the server is running.

**[Read the documentation](https://mimic.ragilhadi.com/)**

## Quick start

Create a mock and start Mimic with Docker:

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

docker run --rm -d --name mimic \
  -p 127.0.0.1:8080:8080 \
  -v "$(pwd)/mocks:/app/mocks:ro" \
  ragilhadi/mimic:latest
```

Call the mock with `curl http://localhost:8080/hello`. Open the [local dashboard](http://localhost:8080/admin/dashboard) to inspect requests and loaded mocks. Stop the server with `docker stop mimic`.

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
