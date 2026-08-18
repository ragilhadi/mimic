# 🧩 Mimic - Lightweight HTTP Mock Server

[![Tests](https://github.com/ragilhadi/mimic/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/mimic/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/mimic?label=docker)](https://hub.docker.com/r/ragilhadi/mimic)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

**Mimic** is a fast, lightweight HTTP mock server built with Rust and Axum. Perfect for testing, development, and API prototyping. Define your mock responses in simple JSON files and let Mimic handle the rest.

## Features

- **Blazing Fast** - Built with Rust and Axum for maximum performance
- **Ultra Lightweight** - Only **1.66 MiB** memory usage
- **File-Based Configuration** - Define mocks in simple JSON files
- **Hot Reload** - Changes to mock files are reflected immediately
- **Docker Ready** - Pre-built images available on Docker Hub
- **Well Tested** - 60+ unit tests with high code coverage
- **Easy to Use** - Simple configuration, no complex setup
- **Configurable Body Consumption** - Control request body handling per endpoint
- **File Upload Support** - Handle multipart/form-data with `consume_body` option
- **Advanced Matching** - Match on path parameters, query params, headers, and request body
- **Path Parameters** - `/users/:id` and `/users/{id}` syntax, one mock covers every value
- **Dynamic Response Templating** - Echo path, query, header, and body fields back into responses with `{{path.x}}` syntax
- **File-Backed Responses** - Serve a PDF, PNG, CSV, or XML fixture from disk with `response_file`, bytes intact
- **Faker Data Generators** - Fresh random values on every call with `{{faker.uuid}}`, `{{faker.name}}`, `{{faker.int min=1 max=100}}`
- **OpenAPI Import** - Generate a whole mocks directory from an OpenAPI 3.x spec with `mimic import-openapi ./spec.yaml`
- **Scenario Tags** - Keep happy-path and error mocks side by side and switch between them with `MIMIC_ACTIVE_TAGS` or `POST /admin/scenario`
- **Built-in CORS** - `MIMIC_CORS=true` answers `OPTIONS` preflights automatically and adds the allow-origin header to every mock — no per-endpoint CORS files
- **Admin Dashboard** - Inspect loaded mocks, read *why* a request didn't match, and see the exact response served — at `/admin/dashboard`
- **Proxy / Record-and-Replay** - Forward unmatched requests to a real upstream and optionally save the response as a new mock with `MIMIC_PROXY_UPSTREAM` and `MIMIC_RECORD_UPSTREAM`

---

## Performance

Mimic is incredibly efficient and lightweight:

```
CONTAINER ID   NAME      CPU %     MEM USAGE / LIMIT    MEM %     NET I/O           BLOCK I/O   PIDS
b056ebec5099   mimic     0.02%     1.66MiB / 6.238GiB   0.03%     17.1kB / 34.6kB   0B / 0B     17
```

**Key Metrics:**
- **Memory Usage**: Only **1.66 MiB** (yes, megabytes!)
- **CPU Usage**: **0.02%** at idle
- **Startup Time**: < 1 second
- **Response Time**: < 10ms for most requests

Perfect for resource-constrained environments, CI/CD pipelines, and local development.

---

## 🚀 Quick Start

### Using Docker (Recommended)

Pull the latest image from Docker Hub:

```bash
docker pull ragilhadi/mimic:latest
```

Run with your mock files:

```bash
docker run -d \
  --name mimic \
  -p 8080:8080 \
  -v $(pwd)/mocks:/app/mocks:ro \
  ragilhadi/mimic:latest
```

Test it:

```bash
curl http://localhost:8080/health
```

### Using Docker Compose

1. Create a `docker-compose.yml`:

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

2. Start the service:

```bash
docker compose up -d
```

---

## 📝 Configuration

### Environment Variables

Create a `.env` file to customize your setup:

```bash
# Port configuration (default: 8080)
PORT=8080

# Logging level: trace, debug, info, warn, error (default: info)
RUST_LOG=info

# Directory (or single JSON file) mocks are read from.
# Default: /app/mocks when it exists (the Docker image), else ./mocks.
MIMIC_MOCKS_DIR=./mocks

# Maximum request body Mimic will buffer, in bytes (default: 10485760 = 10 MB)
MIMIC_MAX_BODY_SIZE=10485760

# How many requests the admin request log keeps (default: 1000, 0 = unbounded)
MIMIC_MAX_LOG_ENTRIES=1000

# How much of each request/response body the log stores, in bytes
# (default: 65536 = 64 KB, 0 = store whole bodies)
MIMIC_MAX_RECORDED_BODY=65536

# Scenario tags active at startup, comma-separated (default: unset = all
# mocks matchable). See "Tagged Mock Groups" below.
MIMIC_ACTIVE_TAGS=happy-path,smoke-test

# Where the health check is served (default: /health). Empty = don't serve it,
# freeing the path for a mock. See "Reserved endpoints" below.
MIMIC_HEALTH_PATH=/health

# Prefix the admin API is mounted under (default: /admin).
MIMIC_ADMIN_PREFIX=/admin

# Switch the admin API off entirely (default: false)
MIMIC_DISABLE_ADMIN=false

# Bearer token the admin API requires (default: unset = no authentication)
MIMIC_ADMIN_TOKEN=

# Body field names whose values are replaced with [REDACTED] in the request
# log, comma-separated. Empty stores bodies verbatim. See "What the request
# log keeps" below for the default list.
MIMIC_REDACT_BODY_FIELDS=password,token,secret,api_key

# Store no request/response bodies in the log at all (default: false)
MIMIC_DISABLE_BODY_LOG=false

# Largest file a mock may serve with `response_file`, in bytes
# (default: 10485760 = 10 MB, 0 = no limit). See "File-Backed Responses" below.
MIMIC_MAX_RESPONSE_FILE=10485760

# Built-in CORS (default: off — no response gains a header, OPTIONS still 404s).
# See "Built-in CORS" below.
MIMIC_CORS=false
MIMIC_CORS_ORIGINS=*
MIMIC_CORS_METHODS=GET,POST,PUT,PATCH,DELETE,OPTIONS
MIMIC_CORS_HEADERS=*
MIMIC_CORS_CREDENTIALS=false
MIMIC_CORS_MAX_AGE=600

# Proxy an unmatched request to a real upstream instead of 404ing (default:
# unset = unchanged 404 behavior). See "Proxy / Record-and-Replay" below.
MIMIC_PROXY_UPSTREAM=https://api.example.com

# Record proxied responses as new mock files, so the next identical request
# is replayed from disk (default: false)
MIMIC_RECORD_UPSTREAM=true

# How long to wait for the upstream before falling back to a 404, in
# milliseconds (default: 5000)
MIMIC_PROXY_TIMEOUT_MS=5000
```

**Log Levels**:
- `trace` - Very detailed debugging
- `debug` - Detailed debugging
- `info` - General information (recommended)
- `warn` - Warnings only
- `error` - Errors only

### Mock Files

Mimic reads mock definitions from JSON files, searching the directory it
resolves at startup and every subdirectory beneath it.

**Where it reads from**, in order:

1. `MIMIC_MOCKS_DIR`, if set — a directory or a single `.json` file. Used
   verbatim: a path that doesn't exist is reported as missing rather than
   silently replaced by a default.
2. `/app/mocks`, if it exists — the Docker image's mount point, so every
   `docker run -v $(pwd)/mocks:/app/mocks` command below resolves exactly there.
3. `./mocks`, relative to the working directory — what `cargo run`, `make dev`,
   an installed binary, or a release build picks up from a clone of this repo.

The directory Mimic actually resolved is logged at startup, and a run that
registers no mocks says which of the two reasons applies — the directory isn't
there, or it's there and holds no mock files:

```
INFO mimic: Configuration:
INFO mimic:   Mocks directory: ./mocks (default, relative to the working directory)
INFO mimic: Loaded 31 mock(s)
```

**Mock File Structure:**

```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": {
    "users": [
      {
        "id": 1,
        "name": "Alice Johnson",
        "email": "alice@example.com"
      }
    ]
  }
}
```

**Fields:**
- `method` - HTTP method (GET, POST, PUT, DELETE, PATCH, etc.)
- `path` - URL path (e.g., `/users`, `/api/v1/products`)
- `status` - HTTP status code (200, 201, 404, 500, etc.)
- `response` - JSON response body (can be object, array, or null)
- `response_file` - (Optional) Serve the body from a file next to the mock instead of `response`; see [File-Backed Responses](#-file-backed-responses-response_file)
- `template` - (Optional) Boolean enabling `{{...}}` templating inside a `response_file` body (default: `false`)
- `consume_body` - (Optional) Boolean to control request body consumption (default: `false`)
  - `true` - Consume request body (required for file uploads, multipart/form-data)
  - `false` - Skip body consumption (faster, default behavior)

### Reserved endpoints

Mimic answers a handful of routes itself, and they are matched **ahead of** the
mock set. A mock declaring one of these loads normally, is listed by
`GET /admin/mocks`, and then never serves a request:

| Reserved | Method(s) |
|---|---|
| `/health` | `GET` |
| `/admin/dashboard` | `GET` |
| `/admin/requests` | `GET`, `DELETE` |
| `/admin/mocks` | `GET` |
| `/admin/sequences` | `GET` |
| `/admin/sequences/reset` | `POST` |
| `/admin/scenario` | `GET`, `POST` |

Only these exact **method + path** pairs are reserved. `POST /health` and
`GET /admin/users` reach the mock set normally and can be mocked.

A collision is reported rather than left to be discovered:

- the loader warns at startup, and again whenever hot reload introduces one,
  naming the file — `mocks/health_down.json declares GET /health, which is
  reserved by Mimic's health check and will never be served`;
- `GET /admin/mocks` reports the mock with `"reachable": false` and an
  `unreachable_reason`, and the dashboard shows it as an **unreachable** badge —
  so a permanent `hits: 0` is distinguishable from "nothing has called it yet".

**If your API genuinely owns these paths**, take them back:

```bash
MIMIC_HEALTH_PATH= mimic                    # no health check; GET /health is yours
MIMIC_HEALTH_PATH=/_mimic/health mimic      # or move it out of the way
MIMIC_ADMIN_PREFIX=/_mimic mimic            # admin API at /_mimic/mocks, etc.
MIMIC_DISABLE_ADMIN=true mimic              # no admin API at all
```

Whatever is left reserved is logged at startup, and a freed route is a normal
mock path from that moment on.

### Hot Reload

Mimic rescans the mocks directory every 2 seconds and picks up changes without
a restart. **Failures are isolated per file**: if one file has invalid JSON —
an editor mid-save, a teammate's work-in-progress in a shared mocks directory —
every other file in that cycle still applies. A single typo can no longer block
unrelated mock changes from taking effect.

The broken file's own route is not dropped, either. It keeps serving its
last successfully-loaded response until the file parses again, so routes don't
flap in and out of existence while somebody is editing. Each failure is logged
with the file name and parse error, plus a summary of how many endpoints were
applied and how many were carried forward.

Deletions still take effect normally: on a clean cycle (no parse errors) the
mock set is replaced outright, so removing a file removes its route.

#### What happens to sequence state across a reload

Sequence positions and hit counts belong to **the mock's file**, not to its
position in the list of mocks sharing a `METHOD:path`. Across a reload:

- **Editing a file** — including editing its `sequence` — leaves that mock's
  position and hit count where they were. Reset explicitly with
  `POST /admin/sequences/reset` when you want a clean run.
- **Adding a mock** under a `METHOD:path` that already has one does not disturb
  the existing mock's counters, whichever order the two files sort in.
- **Deleting a file** drops its counters. They are not carried over to whatever
  is loaded next.
- **A file that stops parsing** keeps both its route and its counters, since
  its last-known-good response is still being served.

Mock files load in a fixed order — depth-first, alphabetical by full path — so
which of several mocks registered for the same `METHOD:path` wins a tie is a
function of their file names and nothing else. It does not vary between
filesystems, between a fresh clone and a rebuilt image, or after an unrelated
file is added to the directory.

---

## 📁 Mock Examples

### Example 1: GET Request

**File**: `mocks/get_users.json`

```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": {
    "users": [
      {
        "id": 1,
        "name": "Alice Johnson",
        "email": "alice@example.com",
        "role": "admin"
      },
      {
        "id": 2,
        "name": "Bob Smith",
        "email": "bob@example.com",
        "role": "user"
      }
    ],
    "total": 2
  }
}
```

**Usage:**
```bash
curl http://localhost:8080/users
```

### Example 2: POST Request

**File**: `mocks/post_login.json`

```json
{
  "method": "POST",
  "path": "/login",
  "status": 200,
  "response": {
    "success": true,
    "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
    "user": {
      "id": 1,
      "username": "admin",
      "email": "admin@example.com"
    },
    "expiresIn": 3600
  }
}
```

**Usage:**
```bash
curl -X POST http://localhost:8080/login
```

### Example 3: Error Response

**File**: `mocks/get_error.json`

```json
{
  "method": "GET",
  "path": "/error",
  "status": 500,
  "response": {
    "error": "Internal Server Error",
    "message": "Something went wrong",
    "code": "ERR_500"
  }
}
```

**Usage:**
```bash
curl http://localhost:8080/error
```

### Example 4: DELETE Request

**File**: `mocks/delete_user.json`

```json
{
  "method": "DELETE",
  "path": "/users/123",
  "status": 204,
  "response": null
}
```

**Usage:**
```bash
curl -X DELETE http://localhost:8080/users/123
```

### Example 5: File Upload with consume_body

**File**: `mocks/ocr_image.json`

```json
{
  "method": "POST",
  "path": "/ocr-image",
  "status": 200,
  "consume_body": true,
  "response": {
    "status": "SUCCESS",
    "text": "Extracted text from image",
    "confidence": 0.95,
    "detected_text": [
      {
        "text": "Hello World",
        "bbox": [10, 20, 100, 40]
      }
    ]
  }
}
```

**Usage:**
```bash
# Upload image file
curl -X POST http://localhost:8080/ocr-image \
  -F "image=@document.jpg" \
  -F "language=en"
```

**Note:** Set `consume_body: true` for endpoints that handle:
- File uploads (images, documents, etc.)
- Multipart/form-data requests
- Large request payloads

Without `consume_body: true`, clients may encounter "Broken Pipe" errors when sending large files.

---

## 🎯 Advanced Matching

Mimic supports advanced request matching beyond just HTTP method and path. You can match requests based on **path parameters**, **query parameters**, **headers**, and **request body** content.

### Path Parameters

A literal `path` only matches one exact URL, which gets impractical fast for REST-style resources — mocking `GET /users/1`, `GET /users/2`, `GET /users/3` would otherwise need a separate file per id. Use named path parameters instead, and a single mock covers every value in that segment.

Both `:id` (Express-style) and `{id}` (OpenAPI-style) syntax are supported:

**File**: `mocks/advanced/get_user_by_id.json`

```json
{
  "method": "GET",
  "path": "/users/:id",
  "status": 200,
  "response": {
    "id": "{{path.id}}",
    "name": "Mock User"
  }
}
```

```bash
curl http://localhost:8080/users/42
# { "id": "42", "name": "Mock User" }
```

Multiple parameters, and nested resources, work the same way:

```json
{
  "method": "DELETE",
  "path": "/orgs/{org}/repos/{repo}",
  "status": 204,
  "response": null
}
```

**Semantics:**
- Captured values are available for [response templating](#-dynamic-response-templating) as `{{path.id}}`.
- An **exact path always wins over a pattern** when both could match — e.g. if `/users/42` and `/users/:id` are both defined, a request for `/users/42` hits the exact mock and everything else falls through to the pattern.
- Exact-path lookups stay O(1); the pattern scan only runs when the exact lookup matched nothing, so mocks with no path parameters see no performance change. Each path template is compiled to a regex once per process and reused, never recompiled per request.
- A [sequence](#-stateful-response-sequences) on a pattern mock advances a single shared counter across every value of the parameter (e.g. `/items/1` and `/items/2` progress the same sequence for `/items/:id`), not one counter per resolved id.

### Query Parameter Matching

Match requests based on URL query string parameters:

**File**: `mocks/get_search.json`

```json
{
  "method": "GET",
  "path": "/search",
  "status": 200,
  "query_params": {
    "params": {
      "q": "test",
      "page": "1"
    },
    "strict": false
  },
  "response": {
    "results": [
      {"id": 1, "title": "Test Result 1"},
      {"id": 2, "title": "Test Result 2"}
    ],
    "query": "test",
    "page": 1
  }
}
```

**Usage:**
```bash
# Matches - exact params
curl "http://localhost:8080/search?q=test&page=1"

# Matches - extra params ignored (strict=false)
curl "http://localhost:8080/search?q=test&page=1&extra=value"

# Doesn't match - wrong value
curl "http://localhost:8080/search?q=wrong&page=1"  # Returns 404
```

**Advanced Query Patterns:**

```json
{
  "query_params": {
    "params": {
      "page": {"regex": "^[0-9]+$"},
      "limit": {"regex": "^(10|20|50|100)$"},
      "status": {"any": null}
    },
    "strict": false
  }
}
```

- **Exact match**: `"param": "value"`
- **Regex match**: `"param": {"regex": "^pattern$"}`
- **Any value**: `"param": {"any": null}` (param must exist)
- **Strict mode**: `"strict": true` (rejects extra params)

### Header Matching

Match requests based on HTTP headers:

**File**: `mocks/get_protected.json`

```json
{
  "method": "GET",
  "path": "/api/protected",
  "status": 200,
  "headers": {
    "required": {
      "authorization": {
        "prefix": "Bearer "
      }
    },
    "forbidden": [],
    "strict": false
  },
  "response": {
    "data": "This is protected content",
    "user": {
      "id": 1,
      "role": "admin"
    }
  }
}
```

**Usage:**
```bash
# Matches - valid Bearer token
curl -H "Authorization: Bearer my_token" http://localhost:8080/api/protected

# Doesn't match - missing header
curl http://localhost:8080/api/protected  # Returns 404

# Doesn't match - wrong prefix
curl -H "Authorization: Basic token" http://localhost:8080/api/protected  # Returns 404
```

**Advanced Header Patterns:**

```json
{
  "headers": {
    "required": {
      "authorization": "Bearer token123",
      "content-type": {"contains": "json"},
      "x-api-key": {"regex": "^[A-Za-z0-9]{32}$"},
      "accept": {"any": null}
    },
    "forbidden": ["x-debug", "x-internal"],
    "strict": false
  }
}
```

- **Exact match**: `"header": "value"`
- **Prefix match**: `"header": {"prefix": "Bearer "}`
- **Contains**: `"header": {"contains": "substring"}`
- **Regex match**: `"header": {"regex": "^pattern$"}`
- **Any value**: `"header": {"any": null}`
- **Forbidden headers**: Headers that must NOT be present
- **Case-insensitive**: Header names are case-insensitive (per HTTP spec)

### Request Body Matching

Match requests based on JSON, text, or form data in the request body:

#### JSON Body Matching

**File**: `mocks/post_login.json`

```json
{
  "method": "POST",
  "path": "/api/login",
  "status": 200,
  "headers": {
    "required": {
      "content-type": "application/json"
    }
  },
  "body": {
    "type": "json",
    "partial": {
      "username": "admin",
      "password": "secret123"
    }
  },
  "response": {
    "success": true,
    "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
    "user": {
      "id": 1,
      "username": "admin",
      "role": "admin"
    }
  }
}
```

**Usage:**
```bash
# Matches - exact credentials
curl -X POST http://localhost:8080/api/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"secret123"}'

# Matches - extra fields ignored (partial matching)
curl -X POST http://localhost:8080/api/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"secret123","remember_me":true}'

# Doesn't match - wrong password
curl -X POST http://localhost:8080/api/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"wrong"}'  # Returns 404
```

**JSON Body Options:**

```json
{
  "body": {
    "type": "json",
    "exact": {"key": "value"},      // Entire body must match exactly
    "partial": {"name": "Alice"},   // Specified fields must match
    "strict": false                 // If true with partial, reject extra fields
  }
}
```

#### Text Body Matching

```json
{
  "body": {
    "type": "text",
    "contains": "search term"
  }
}
```

**Text Body Options:**
- **Exact**: `"exact": "exact string"`
- **Contains**: `"contains": "substring"`
- **Regex**: `"regex": "^pattern$"`

#### Form Body Matching

```json
{
  "body": {
    "type": "form",
    "fields": {
      "username": "admin",
      "password": "secret"
    },
    "strict": false
  }
}
```

### Combined Matching

Combine all matching types for precise mock selection:

**File**: `mocks/post_search_combined.json`

```json
{
  "method": "POST",
  "path": "/api/search",
  "status": 200,
  "query_params": {
    "params": {
      "type": "user"
    }
  },
  "headers": {
    "required": {
      "authorization": {"prefix": "Bearer "},
      "content-type": "application/json"
    }
  },
  "body": {
    "type": "json",
    "partial": {
      "query": "Alice"
    }
  },
  "response": {
    "results": [
      {
        "id": 1,
        "name": "Alice Johnson",
        "email": "alice@example.com"
      }
    ],
    "total": 1
  }
}
```

**Usage:**
```bash
curl -X POST "http://localhost:8080/api/search?type=user" \
  -H "Authorization: Bearer my_token" \
  -H "Content-Type: application/json" \
  -d '{"query":"Alice","filters":{"active":true}}'
```

This mock only matches when **all** criteria are met:
1. ✅ Method is POST
2. ✅ Path is `/api/search`
3. ✅ Query param `type=user`
4. ✅ Authorization header starts with "Bearer "
5. ✅ Content-Type is application/json
6. ✅ Body contains `{"query": "Alice"}`

### Match Priority

When multiple mocks could match a request, Mimic uses a scoring system:

- **Base score**: Method + Path match (1000 points)
- **Query params**: +100 points per matched param
- **Headers**: +50 points per matched header
- **Body**: +500 points if body matches
- **Path pattern penalty**: -100 points for a `:id`/`{id}` match, so an exact path always outranks a pattern

The mock with the **highest score** wins. Equal scores are broken
deterministically — most literal path segments first (`/users/:id` beats
`/{resource}/:id`), then lowest `METHOD:path` key lexicographically, then
earliest position among mocks sharing that key. The winner never depends on
load order, so it stays the same across restarts and hot reloads. See
[ADVANCED_MATCHING.md](ADVANCED_MATCHING.md#match-priority) for details.

**Strict header mode** (`"strict": true`) ignores headers every HTTP client
sends by default — `accept`, `accept-encoding`, `connection`, `content-length`,
`host`, `user-agent` — so a plain `curl` request isn't rejected for headers you
never asked about.

---

## 📬 Custom Response Headers

Set arbitrary response headers per mock with `response_headers` — for CORS, redirects, non-JSON content types, cache control, rate-limit simulation, or auth challenges.

### CORS example

```json
{
  "method": "GET",
  "path": "/api/data",
  "status": 200,
  "response_headers": {
    "Access-Control-Allow-Origin": "*",
    "Access-Control-Allow-Methods": "GET, POST, OPTIONS"
  },
  "response": { "data": [1, 2, 3] }
}
```

> **Or just set `MIMIC_CORS=true`** and skip this entirely — see
> [Built-in CORS](#-built-in-cors) below. Per-mock headers are still the way to
> mock a *specific* CORS response (including a broken one); the env var is the
> way to stop repeating them on every endpoint.

### Non-JSON response (XML)

```json
{
  "method": "GET",
  "path": "/data.xml",
  "status": 200,
  "response_headers": {
    "Content-Type": "application/xml; charset=utf-8",
    "Cache-Control": "no-cache"
  },
  "response": "<users><user id=\"1\"/></users>"
}
```

### Created resource with Location

```json
{
  "method": "POST",
  "path": "/resources",
  "status": 201,
  "response_headers": {
    "Location": "/resources/99",
    "X-Request-Id": "abc-123"
  },
  "response": { "id": 99 }
}
```

### Semantics

- Header names are **case-insensitive** (`content-type` and `Content-Type` both work).
- `Content-Type: application/json` is added automatically **only when** your headers don't set a content type — mocks without `response_headers` behave exactly as before.
- When a **non-JSON** content type is set and `response` is a JSON string, the raw string is sent as the body — so XML/CSV/plain-text responses aren't JSON-quoted.
- Invalid header names or values are skipped with a warning; the response is still served.
- Headers apply to every response of the mock, including all [sequence](#-stateful-response-sequences) steps.

---

## 📎 File-Backed Responses (`response_file`)

Some bodies don't want to live inside a JSON file. A PDF export can't be
written as JSON at all; a 200 KB captured payload turns a mock into something
nobody can read; a SOAP envelope hand-escaped into a JSON string is impossible
to diff. `response_file` points a mock at a file next to it and serves that
file's exact bytes.

```json
{
  "method": "GET",
  "path": "/reports/:id/export",
  "status": 200,
  "response_file": "fixtures/report.csv",
  "response_headers": {
    "Content-Disposition": "attachment; filename=\"report.csv\""
  }
}
```

```bash
curl -i http://localhost:8080/reports/9/export
# HTTP/1.1 200 OK
# content-type: text/csv; charset=utf-8
# content-disposition: attachment; filename="report.csv"
#
# id,name,plan,seats,mrr
# 1,Acme Corp,enterprise,250,12500
```

Binary works the same way, byte for byte:

```json
{
  "method": "GET",
  "path": "/users/:id/avatar",
  "status": 200,
  "response_file": "fixtures/avatar.png"
}
```

### Where the file is looked up

The path is resolved **relative to the mock file's own directory**, not to the
working directory — so a mocks tree stays relocatable and a Docker volume mount
works unchanged:

```
mocks/
└── advanced/
    ├── get_report_export.json   → "response_file": "fixtures/report.csv"
    └── fixtures/
        ├── report.csv
        ├── invoice.xml
        └── avatar.png
```

A path that resolves outside the mocks root — `../../etc/passwd`, an absolute
path, or a symlink pointing out of the tree — is **refused at load time** with
an error naming the mock file, and that mock is not registered.

### Content type

The first of these wins:

1. `Content-Type` in the mock's own `response_headers`;
2. the file extension: `.json`, `.xml`, `.csv`, `.html`, `.txt`, `.png`,
   `.jpg`/`.jpeg`, `.pdf`, `.zip`;
3. `application/octet-stream`.

A `.json` fixture is served as a JSON body, not as a JSON-quoted string.

### Templating (opt-in)

Set `"template": true` to interpolate `{{path.*}}`, `{{query.*}}`,
`{{header.*}}`, `{{body.*}}`, and `{{faker.*}}` inside the file — the same
expressions a `response` supports:

```json
{
  "method": "POST",
  "path": "/soap/invoices/:id",
  "status": 200,
  "template": true,
  "response_file": "fixtures/invoice.xml"
}
```

```xml
<InvoiceId>{{path.id}}</InvoiceId>
<Currency>{{query.currency}}</Currency>
<CustomerRef>{{body.customer_ref}}</CustomerRef>
```

Templating is **off by default** and never runs on a binary content type, so a
PNG that happens to contain the bytes `{{` is still a PNG.

### Sequences

A [sequence](#-stateful-response-sequences) step takes the same two fields, so
a retry flow can end in a real file:

```json
{
  "method": "GET",
  "path": "/flaky-export",
  "status": 200,
  "sequence": [
    { "status": 503, "response": { "error": "unavailable" } },
    { "status": 200, "response_file": "fixtures/report.csv", "repeat": true }
  ]
}
```

### Semantics

- **`response` and `response_file` are mutually exclusive.** A mock setting both
  is a load error naming the file. Setting neither is unchanged behavior
  (`response: null`).
- **Files are read at load time** and re-read on every [hot reload](#hot-reload)
  cycle, so editing a fixture takes effect within ~2 s without touching the mock
  file. Request handling never touches the disk.
- **`MIMIC_MAX_RESPONSE_FILE`** (default 10 MB) caps one fixture. A file over the
  cap is reported and its mock is skipped rather than half-loaded; `0` removes
  the cap.
- **A `.json` fixture is not loaded as a mock.** The loader reads every `.json`
  file under the mocks directory, but a file some mock claims as its
  `response_file` is served, never registered.
- **The request log** stores a text body verbatim (truncated at
  `MIMIC_MAX_RECORDED_BODY`) and a binary body as a one-line descriptor —
  `<70 bytes of image/png from fixtures/avatar.png>` — so the dashboard stays
  readable.
- Everything else composes as usual: path parameters, matchers, `delay_ms`,
  tags, and CORS all work with a file-backed body.

---

## 🌐 Built-in CORS

A browser calling a Mimic-backed API from `http://localhost:3000` normally fails
twice: it sends `OPTIONS /users` first (which no mock answers), and every real
mock has to repeat `Access-Control-Allow-Origin` in its `response_headers`. For
an API with N endpoints that's up to 2N files maintained by hand.

One variable replaces all of them:

```bash
MIMIC_CORS=true
```

That's it — preflights are answered automatically and every mock response
carries the allow-origin header.

### Configuration

| Variable | Default | What it does |
|---|---|---|
| `MIMIC_CORS` | `false` | Master switch. Everything below is ignored while it's off. |
| `MIMIC_CORS_ORIGINS` | `*` | `*`, or a comma-separated allowlist like `http://localhost:3000,http://localhost:5173`. |
| `MIMIC_CORS_METHODS` | `GET,POST,PUT,PATCH,DELETE,OPTIONS` | Advertised as `Access-Control-Allow-Methods` on a preflight. |
| `MIMIC_CORS_HEADERS` | `*` | `*` reflects the request's `Access-Control-Request-Headers`; a list is sent verbatim. |
| `MIMIC_CORS_CREDENTIALS` | `false` | Sends `Access-Control-Allow-Credentials: true`. |
| `MIMIC_CORS_MAX_AGE` | `600` | `Access-Control-Max-Age`, in seconds. |

```bash
docker run -d -p 8080:8080 \
  -e MIMIC_CORS=true \
  -e MIMIC_CORS_ORIGINS=http://localhost:3000,http://localhost:5173 \
  -v ./mocks:/app/mocks:ro \
  ragilhadi/mimic:latest
```

### What it does

```bash
# No OPTIONS mock anywhere — Mimic answers the preflight itself
curl -i -X OPTIONS http://localhost:8080/users \
  -H 'Origin: http://localhost:3000' \
  -H 'Access-Control-Request-Method: POST' \
  -H 'Access-Control-Request-Headers: content-type'

HTTP/1.1 204 No Content
access-control-allow-origin: *
access-control-allow-methods: GET, POST, PUT, PATCH, DELETE, OPTIONS
access-control-allow-headers: content-type
access-control-max-age: 600
```

### Semantics

- **Off by default.** With `MIMIC_CORS` unset, responses are byte-identical to
  what they were before this existed — no new headers, and `OPTIONS` still 404s.
- **A mock's own header always wins.** If a mock sets `Access-Control-Allow-Origin`
  in its `response_headers`, the global config leaves it alone — so mocking a CORS
  *failure* stays possible.
- **Preflights are gap-filled, not hijacked.** An `OPTIONS` request that no mock
  matches, but whose path has a mock registered for the method in
  `Access-Control-Request-Method`, is answered `204`. An explicit `OPTIONS` mock
  matches first and wins. A preflight for a path with nothing behind it still
  `404`s.
- **Only real endpoints, in the current scenario.** A mock hidden by the active
  [scenario](#-tagged-mock-groups-scenarios) has no endpoint to preflight, and its
  preflight `404`s too.
- **A bare `OPTIONS` isn't a preflight.** Without `Access-Control-Request-Method`
  — curl, a health probe — the request behaves exactly as it always has.
- **Preflights are logged.** They appear in `/admin/requests` and the dashboard
  with `matched_mock: null` and the explanation *"answered as a CORS preflight"*,
  so nothing looks dropped.
- **The allowlist is honored.** A request from an origin outside
  `MIMIC_CORS_ORIGINS` gets *no* `Access-Control-Allow-Origin` header rather than
  a wrong one, which is what makes the browser's own error message correct.
  Responses that depend on the origin also carry `Vary: Origin`.
- **`*` with credentials.** `MIMIC_CORS_ORIGINS=*` plus
  `MIMIC_CORS_CREDENTIALS=true` is invalid per the CORS spec; Mimic reflects the
  request origin instead and warns once at startup.
- Matching is untouched: CORS headers are added to the response, never to the
  request the matcher sees.

---

## ⏱️ Response Delays (Latency Simulation)

Simulate slow endpoints to test loading states, timeout handling, retries, circuit breakers, and debounce behavior. Add `delay_ms` to any mock — the response is held back for that duration before being sent.

### Fixed delay

```json
{
  "method": "GET",
  "path": "/slow-endpoint",
  "status": 200,
  "delay_ms": 2000,
  "response": { "data": "finally here" }
}
```

### Random delay with jitter

```json
{
  "method": "GET",
  "path": "/flaky-endpoint",
  "status": 200,
  "delay_ms": { "min": 100, "max": 3000 },
  "response": { "data": "..." }
}
```

Each request samples a fresh uniform value between `min` and `max` (inclusive), so repeated calls see realistic variable latency.

### Semantics

- No `delay_ms` → zero overhead, responses are as fast as before.
- The delay is applied **after** matching and request recording, with no locks held — a slow mock never blocks other requests, the admin API, or hot reload.
- Works together with [sequences](#-stateful-response-sequences): a sequence step's own `delay_ms` takes precedence; steps without one inherit the mock-level delay.

---

## 🔁 Stateful Response Sequences

A mock can return **different responses on successive calls** by declaring a `sequence` array. This makes it possible to test retry logic, rate limiting, flaky services, and multi-step flows without a real server.

```json
{
  "method": "POST",
  "path": "/api/submit",
  "status": 200,
  "response": { "ok": true },
  "sequence": [
    { "status": 503, "response": { "error": "service unavailable, retry later" } },
    { "status": 429, "response": { "error": "rate limited" }, "delay_ms": 100 },
    { "status": 200, "response": { "ok": true }, "repeat": true }
  ]
}
```

### Step Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | number | ✅ | HTTP status code for this step |
| `response` | any JSON | ✅ | Response body for this step |
| `delay_ms` | number | ❌ | Delay (milliseconds) before returning this step's response |
| `repeat` | boolean | ❌ | If `true`, the sequence stops advancing at this step (default `false`) |

### Semantics

- Steps are consumed **in order**, one per request.
- A step with `"repeat": true` is returned for **all subsequent calls** — the sequence stops advancing there.
- If no step has `"repeat": true`, the **last step repeats** once the sequence is exhausted.
- An empty `sequence` array falls back to the top-level `status`/`response`.
- Counters are **thread-safe** and tracked per mock — two mocks sharing a path (differentiated by body/query/header matchers) advance independently.
- Counters survive hot reload of mock files; use the reset endpoint to start over.

### Testing Retry Logic

With the example above, a client with retry/backoff sees exactly what a recovering service would produce:

```bash
curl -X POST http://localhost:8080/api/submit   # 503 service unavailable
curl -X POST http://localhost:8080/api/submit   # 429 rate limited (after 100ms delay)
curl -X POST http://localhost:8080/api/submit   # 200 ok
curl -X POST http://localhost:8080/api/submit   # 200 ok (repeats forever)
```

### Resetting Sequences

Reset call counters so tests start from step 0 again:

```bash
# Reset all sequence counters
curl -X POST http://localhost:8080/admin/sequences/reset

# Reset only the counters for one path
curl -X POST "http://localhost:8080/admin/sequences/reset?path=/api/submit"
```

**Response:**
```json
{ "reset": 1 }
```

---

## 🏷️ Tagged Mock Groups (Scenarios)

One `mocks/` directory can hold **both** your happy-path mocks and your error
mocks. Tag them, and switch which set is live per CI job or per test run — no
file editing, no second directory, no restart.

```json
{
  "method": "POST",
  "path": "/checkout",
  "status": 500,
  "tags": ["error-scenario"],
  "response": { "error": "internal error" }
}
```

```bash
# mocks/checkout_ok.json   → "tags": ["happy-path"]
# mocks/checkout_500.json  → "tags": ["error-scenario"]

MIMIC_ACTIVE_TAGS=happy-path mimic     # only checkout_ok.json is matchable
```

### Rules

- **A mock with no `tags` is always matchable.** Existing mock files need zero
  changes, and a server started without `MIMIC_ACTIVE_TAGS` behaves exactly as
  it did before this feature existed.
- **`MIMIC_ACTIVE_TAGS` unset or empty means no filtering** — every mock,
  tagged or not, is matchable.
- A tagged mock is matchable **while at least one of its tags is active**.
- Tags are matched **exactly and case-sensitively**; whitespace around a tag in
  the comma-separated list is trimmed (`happy-path, smoke-test` is two tags).
- An inactive mock **404s as if it were not loaded**. Requests fall through to
  whatever else can serve the path — an untagged mock, or a `/users/:id`
  pattern route.
- Tag filtering **does not touch sequence counters**: they are keyed per mock,
  so switching scenarios and back resumes a sequence where it left off rather
  than restarting it.
- With **no filter active**, two mocks tagged for opposite scenarios on the
  same path are *both* matchable and one of them wins — so set
  `MIMIC_ACTIVE_TAGS` (or `POST /admin/scenario`) whenever you keep competing
  scenarios side by side.

### Switching at runtime

```bash
# What's active right now?
curl http://localhost:8080/admin/scenario
```

```json
{
  "active_tags": ["happy-path"],
  "filtering": true,
  "known_tags": ["error-scenario", "happy-path"],
  "matchable_mocks": 12,
  "total_mocks": 14
}
```

```bash
# Switch scenarios — takes effect on the next request, no restart
curl -X POST http://localhost:8080/admin/scenario \
  -d '{"tags": ["error-scenario"]}'

# Turn filtering off again: every mock becomes matchable
curl -X POST http://localhost:8080/admin/scenario -d '{"tags": []}'
```

`POST /admin/scenario` **replaces** the active set (it is not additive) and
returns the same body `GET` does. A tag entry may itself be a comma-separated
list, so `{"tags": ["a,b"]}` and `{"tags": ["a", "b"]}` are equivalent. A body
that isn't valid JSON is answered `400` and leaves the current scenario alone.

### Testing an error path

```bash
MIMIC_ACTIVE_TAGS=happy-path mimic &

curl -X POST http://localhost:8080/checkout    # 200 {"order_id": "..."}

curl -X POST http://localhost:8080/admin/scenario -d '{"tags": ["error-scenario"]}'

curl -X POST http://localhost:8080/checkout    # 500 {"error": "internal error"}
```

### Observability

- `/admin/mocks` reports each mock's `tags` and an `active` flag, so a mock
  that is loaded but currently filtered out is obvious.
- A 404 caused by an inactive mock records *"N mock(s) match `POST:/checkout`
  but are filtered out by inactive tags"* in the request log and the debug log.
  The 404 **response body is unchanged** — scenario configuration is never
  leaked to API clients.

---

## 🧬 Dynamic Response Templating

Mock responses don't have to be fully static. Use `{{ }}` double-brace syntax inside any string value in `response` (or a sequence step's `response`) to echo back data from the incoming request — no custom code required.

| Template | Source |
|---|---|
| `{{query.page}}` | URL query parameter `?page=2` |
| `{{header.x-request-id}}` | Request header value (case-insensitive) |
| `{{body.username}}` | Top-level JSON (or form) body field |
| `{{body.user.email}}` | Nested JSON body field (dot notation) |
| `{{path.id}}` | Named [path parameter](#path-parameters) `:id` or `{id}` |
| `{{faker.uuid}}` | [Random generated data](#faker-generators) — no request input needed |

> **Credential headers are never echoed.** `{{header.authorization}}`,
> `{{header.cookie}}` and `{{header.set-cookie}}` always render as an empty
> string — the same as a missing key — regardless of letter case. This matches
> the redaction already applied to the `/admin/requests` log, so a mock file
> can't reflect a live bearer token or session cookie into a response body
> (and from there into browser devtools, HAR exports, or CI logs).

### Example

```json
{
  "method": "POST",
  "path": "/users",
  "status": 201,
  "response": {
    "id": 99,
    "username": "{{body.username}}",
    "email": "{{body.email}}",
    "created_by": "{{header.x-actor}}",
    "self_url": "/users/99"
  }
}
```

```bash
curl -X POST http://localhost:8080/users \
  -H "X-Actor: admin" \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","email":"alice@example.com"}'
```

**Response:**
```json
{
  "id": 99,
  "username": "alice",
  "email": "alice@example.com",
  "created_by": "admin",
  "self_url": "/users/99"
}
```

### Faker Generators

The `faker` source doesn't read the request at all — it generates a fresh, plausible-looking value on every call, so one mock file can stand in for a whole fixture set.

| Template | Output |
|---|---|
| `{{faker.uuid}}` | RFC 4122 v4 UUID, e.g. `3fa85f64-5717-4562-b3fc-2c963f66afa6` |
| `{{faker.int}}` | Random integer in the default range `0..=1000000` |
| `{{faker.int min=1 max=100}}` | Random integer in `[1, 100]` |
| `{{faker.bool}}` | `true` or `false` |
| `{{faker.name}}` | Random name, e.g. `Priya Novak` |
| `{{faker.email}}` | Slugified random name at `example.com`, e.g. `priya.novak@example.com` |
| `{{faker.timestamp}}` | Current UTC time in RFC 3339, e.g. `2026-07-22T16:12:54.481+00:00` |

```json
{
  "method": "GET",
  "path": "/faker/user",
  "status": 200,
  "response": {
    "id": "{{faker.uuid}}",
    "name": "{{faker.name}}",
    "email": "{{faker.email}}",
    "age": "{{faker.int min=18 max=99}}",
    "verified": "{{faker.bool}}",
    "created_at": "{{faker.timestamp}}"
  }
}
```

```bash
curl http://localhost:8080/faker/user
```

**Response** (a different one every call):
```json
{
  "id": "9c0f1b6d-2f4e-4a1e-b0a2-6c1d7f3e88a4",
  "name": "Priya Novak",
  "email": "priya.novak@example.com",
  "age": "37",
  "verified": "true",
  "created_at": "2026-07-22T16:12:54.481+00:00"
}
```

Notes:

- Every occurrence resolves independently — two `{{faker.uuid}}` in one response produce two *different* UUIDs, and `{{faker.email}}` is not derived from a `{{faker.name}}` sitting next to it.
- Faker values are always rendered as JSON strings, since templates are interpolated into string values.
- Malformed arguments degrade to the generator's defaults instead of failing: `{{faker.int min=abc}}` and `{{faker.int min=100 max=1}}` both use the default `0..=1000000` range.
- An unknown generator (e.g. `{{faker.credit_card}}`) renders as an empty string, like any other unknown template.

### Semantics

- Templates are resolved after the mock/sequence step is chosen, so the interpolated value never affects matching itself — `{{faker.*}}` included, so a faker expression sitting in a matcher is treated as a literal string.
- An unknown source, a missing key, or an explicit JSON `null` all resolve to an empty string — malformed templates never panic and never leak into the response.
- Non-string body values (numbers, booleans, nested objects/arrays) render using their JSON text form, e.g. `{{body.age}}` for `{"age": 30}` produces `30`.
- A response with no `{{ }}` expressions is returned unchanged with no templating overhead.

---

## 📥 OpenAPI / Swagger Import

Already have an OpenAPI 3.x spec? Generate a whole `mocks/` directory from it in one command instead of hand-writing every file:

```bash
mimic import-openapi ./spec.yaml --out ./mocks/generated
```

JSON and YAML specs are both accepted. The importer writes one **live** mock file per operation — built from its primary response — plus an inert `.json.disabled` file for every other documented status. The output is plain `MockConfig` JSON, so it hot-reloads like any other mock and can be hand-edited afterwards.

The default output directory, `./mocks/generated`, sits inside the `./mocks`
the server falls back to locally — so `mimic import-openapi ./spec.yaml && mimic`
serves the generated mocks with nothing else to configure. When `--out` points
elsewhere, the importer prints the exact command to start the server against
it.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--out <dir>` | `./mocks/generated` | Output directory |
| `--status <code>` | `200` | Status treated as each operation's *primary* response; its file is the live one |
| `--force` | off | Write into a non-empty output directory, overwriting files of the same name |
| `--brace-params` | off | Emit path parameters as `{id}` instead of `:id` |
| `-h`, `--help` | — | Show usage |

### Example

Given this spec:

```yaml
openapi: 3.0.3
info: { title: Petstore, version: 1.0.0 }
paths:
  /users/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              example: { "id": 1, "name": "Alice" }
        '404':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Error'
components:
  schemas:
    Error:
      type: object
      properties:
        code: { type: integer }
        message: { type: string }
```

`mimic import-openapi spec.yaml --out ./mocks/generated` writes:

**`mocks/generated/get_users_id.json`** (the primary 200 response — no status suffix)

```json
{
  "method": "GET",
  "path": "/users/:id",
  "status": 200,
  "response": { "id": 1, "name": "Alice" },
  "consume_body": false
}
```

**`mocks/generated/get_users_id_404.json.disabled`** (an alternative response — inert until renamed)

```json
{
  "method": "GET",
  "path": "/users/:id",
  "status": 404,
  "response": { "code": 0, "message": "string" },
  "consume_body": false
}
```

Start the server and `GET /users/42` returns the 200 body immediately — path parameters are translated to Mimic's `:id` syntax, so generated mocks work with the matcher unedited.

To serve the 404 instead, swap which file is live:

```bash
mv mocks/generated/get_users_id.json mocks/generated/get_users_id_200.json.disabled
mv mocks/generated/get_users_id_404.json.disabled mocks/generated/get_users_id_404.json
```

Hot reload picks the change up within a couple of seconds — no restart needed.

### Where the response body comes from

For each response, the importer takes the first of these that the spec provides:

1. `content.<media type>.example`
2. The first entry in `content.<media type>.examples` (its `value`)
3. A stub built from `content.<media type>.schema`
4. `{}` — for body-less responses like `204`

JSON media types are preferred when a response offers several.

### Schema stubs

When there's no example, the schema is walked recursively and each field gets a type-appropriate placeholder:

| Schema | Stub |
|--------|------|
| `type: string` | `"string"` (or a format-appropriate value: `date`, `date-time`, `uuid`, `email`, `uri`) |
| `type: integer` / `number` | `0` / `0.0` |
| `type: boolean` | `false` |
| `type: object` | `{}` with every property stubbed recursively |
| `type: array` | one stubbed element from `items`, or `[]` if `items` is absent |
| `enum` / `default` / `example` | the spec's own value, which always wins over a stub |

`allOf` subschemas are merged; `oneOf` / `anyOf` use the first variant.

### Semantics

- **Multiple response codes become separate files, and only the primary one is live.** Alternatives share a path and method with the primary response and carry no matcher to tell them apart, so leaving them all enabled would make the served response depend on directory read order. They land as `.json.disabled` instead — rename to enable. They are alternative responses, not a call-order sequence, so they are also deliberately *not* folded into a single [`sequence`](#-stateful-response-sequences) mock.
- **The primary response** is `--status` if the operation declares it, otherwise its lowest declared `2xx`, otherwise its lowest declared status. So a `POST` documenting only `201` gets a live `201`, and an operation documenting only errors still gets a live mock rather than an all-disabled route.
- **OpenAPI's `default` key** maps to `--status` for its filename, but never outranks a declared success response — it usually documents an *error* shape, and serving that by default would be misleading. It becomes the live mock only when it's all an operation has.
- **`$ref` is resolved** for schemas, responses, path items, and named examples. Circular references collapse to `{}` at the point of recursion rather than looping forever; external (`./other.yaml#/...`) and unresolvable refs degrade to `{}` with a warning.
- **Existing files are protected.** A non-empty `--out` directory is refused unless you pass `--force`, so a re-import can't silently clobber mocks you edited by hand. With `--force`, every overwritten file is named in a warning.
- **Response keys** may be quoted (`'200'`) or bare (`200`), `default`, or wildcards (`4XX` → `400`).
- **Only OpenAPI 3.x is supported.** Swagger 2.0 specs are rejected with a clear message — convert first.
- A malformed spec produces an error and a non-zero exit code, never a panic.

---

## 🔌 Proxy / Record-and-Replay

Point Mimic at a real API once, and it grows a matching mock set for free. When `MIMIC_PROXY_UPSTREAM` is set, a request that matches no local mock is forwarded to that upstream instead of getting the usual `404 "mock not found"` — the real response is returned to the client. Add `MIMIC_RECORD_UPSTREAM=true` and that response is also saved as a new mock file, so the next identical request is served from disk with zero network calls.

```bash
MIMIC_PROXY_UPSTREAM=https://api.stripe.com MIMIC_RECORD_UPSTREAM=true mimic
```

```bash
# First call: no local mock exists, so Mimic forwards to api.stripe.com
# and returns the live response.
curl http://localhost:8080/v1/charges/ch_123

# A new file appears at mocks/_recorded/get_v1_charges_ch_123_1.json.
# The second identical call is served from that file — Stripe never sees it.
curl http://localhost:8080/v1/charges/ch_123
```

### Configuration

```bash
MIMIC_PROXY_UPSTREAM=https://api.example.com   # unset (default) = unchanged 404 behavior
MIMIC_RECORD_UPSTREAM=false                     # opt-in recording of proxied responses
MIMIC_PROXY_TIMEOUT_MS=5000                     # how long to wait for the upstream
```

### What gets recorded

A recorded file uses the exact same shape as any hand-written mock — [`method`, `path`, `status`, `response`](#-mock-examples), plus matchers built from the request that triggered the recording, so the *next* matching request is matched normally rather than through special-cased "recorded" logic:

- **`query_params`** — every query parameter, as an exact match.
- **`headers`** — every request header, **except**:
  - sensitive headers (`Authorization`, `Cookie`) — never written to disk, so a recorded mock can't become an accidental secrets store;
  - headers every mainstream client sends unconditionally (`User-Agent`, `Accept`, `Accept-Encoding`, `Host`, `Connection`, `Content-Length`) — capturing these would make the recording match only the exact tool that happened to trigger it.
- **`body`** — a JSON, text, or form matcher depending on the request's content type; no matcher at all for an empty body.
- **`response_headers`** — the upstream's response headers, again with `Set-Cookie` and friends left out.

Files land at `mocks/_recorded/<method>_<sanitized-path>_<n>.json` — e.g. `mocks/_recorded/get_v1_charges_ch_123_1.json` — fully readable and editable like any other mock. They pick up on the next [hot reload](#hot-reload), typically within a couple of seconds.

### Semantics

- **Recording is best-effort and never blocks the response.** The client gets the upstream's response immediately; the mock file is written in the background.
- **Only text-ish responses are recorded.** JSON, XML, plain text, JavaScript, and form-encoded bodies are recorded; binary responses (images, PDFs, arbitrary `application/octet-stream`) are still proxied to the client but never turned into a mock file — there's no good text representation for a `response` field.
- **Concurrent identical requests dedupe onto one file.** Several requests with the same method, path, query, (non-noise) headers, and body in flight at once produce exactly one recording, not a race of several.
- **A repeat of the same request doesn't re-record.** Once a given request shape has been recorded this run, later identical proxy calls (before the file has been hot-reloaded in, or if recording is on but nothing changed) are skipped rather than rewritten.
- **What Mimic reserves for itself is never proxied.** The health check and the admin API's own endpoints — see [Reserved endpoints](#reserved-endpoints) — are excluded even with no local mock behind them, so `MIMIC_PROXY_UPSTREAM` can't leak them onto the upstream. A path that merely starts with `/admin/` but isn't one Mimic answers (a typo, or an endpoint you're mocking) is an ordinary request and proxies like any other.
- **Self-referential upstreams are rejected at startup.** An upstream that resolves to Mimic's own listening address (e.g. `MIMIC_PROXY_UPSTREAM=http://localhost:8080` while Mimic itself listens on `8080`) disables proxying with a warning, instead of looping forever.
- **A slow or unreachable upstream falls back to the usual 404**, with an added `"upstream_error"` field explaining why — never an indefinitely hanging request. The wait is capped by `MIMIC_PROXY_TIMEOUT_MS`.

---

## 🖥️ Admin Dashboard

```
http://localhost:8080/admin/dashboard
```

A single dependency-free page — no build step, no CDN — for answering the two
questions that otherwise send you back to `RUST_LOG=debug`: *what is this
server configured to do?* and *why didn't my mock match?*

### Tabs

| Tab | What it shows |
|-----|---------------|
| **Requests** | Every recorded request. Expand a row for its headers, query params and body, **the response that was actually served** (status, headers, body), and a **Match** section explaining which mock won and why — or, for a 404, which mocks were in the running and what rejected each. |
| **Mocks** | Every loaded mock: method, path, status, which matchers it declares, delay, sequence length, hit count, and the file it came from. Expand a row for the full `MockConfig` JSON. |
| **Sequences** | Each stateful sequence's current step, with a per-path **Reset** button. |

The header bar reads `/health` for mocks loaded, uptime, port, and max body size.

### Match explanations

A matched request records the arithmetic behind its score:

```
matched mocks/get_users_id.json (score 1150: method+path 1000, headers +50, path pattern -100)
```

An unmatched one records the near-miss diagnosis instead — the mocks that
shared its `METHOD:path`, and the first matcher that turned each down:

```
2 candidate mock(s) for `GET:/users`, none matched:
  mocks/get_users.json — required header `x-api-key` was absent;
  mocks/get_users_admin.json — query param `role` was `viewer`, expected `admin`
```

When nothing is registered at all, the explanation says so — and points out the
wrong-verb case, which is what it usually is:

```
no mock is registered for `GET:/users` — `/users` is registered for POST, PUT
```

Explanations are produced by the matchers themselves (each `match_*` predicate
is defined as "no rejection reason"), so an explanation can never describe a
rule the server doesn't actually enforce.

### Filtering

| Filter | Behaviour |
|--------|-----------|
| Path | **Substring** match (`user` finds `/users` and `/users/active`) |
| Method | Exact, case-insensitive |
| Status | An exact code (`404`) or a class (`4xx`, `5xx`) |
| Unmatched only | Keeps just the requests no mock served |
| Search | Case-insensitive, over each request's body, headers and query params |

All of them are query parameters on `/admin/requests`, so they work from
`curl` too — the dashboard is a client of the same public API.

### Other behaviour

- **Auto-refresh appends** new rows rather than rebuilding the table, and
  **pauses while the pointer is over it** — new requests collect behind a
  "*N* new requests" banner instead of shifting the row you're reading.
- **Copy as curl** per row, and **Export log** for the whole filtered view.
- **Theme** follows `prefers-color-scheme`, with a manual toggle that sticks.
- **Redaction**: see [What the request log keeps](#what-the-request-log-keeps)
  — headers *and* body fields are scrubbed by default.
- **Bounded by default**: the log keeps the last `MIMIC_MAX_LOG_ENTRIES`
  requests (1000), and stored bodies are truncated past
  `MIMIC_MAX_RECORDED_BODY` (64 KB) with a `…[truncated]` marker — so a
  long-running server doesn't degrade the very UI meant to observe it.

### What the request log keeps

The log lives in memory and is served, unfiltered, by `GET /admin/requests` on
a server that binds `0.0.0.0`. Two of this README's own use cases — a shared
team mock server, and CI — put that port somewhere more than one person can
reach it. So credentials that pass through Mimic are scrubbed on the way *in*
to the log.

**Redacted by default:**

| Where | What | Configured by |
|---|---|---|
| Request headers, response headers, match explanations | `authorization`, `cookie`, `set-cookie` | — (fixed list) |
| Request bodies **and** response bodies | fields named `password`, `passwd`, `token`, `access_token`, `refresh_token`, `id_token`, `secret`, `client_secret`, `api_key`, `apikey`, `private_key`, `authorization` | `MIMIC_REDACT_BODY_FIELDS` |

Body redaction walks JSON — through nested objects and arrays — and
`application/x-www-form-urlencoded` fields. Field names match
**case-insensitively and exactly**: `token` scrubs `token`, not `tokenizer`,
which is why the default list spells out the common variants. A matching key
loses its whole value, object or array included. Anything with no field
structure (plain text, XML, binary) is stored as it came in.

**Not redacted:** query strings, paths, and the values of fields you haven't
named. `?api_key=...` in a URL is stored verbatim — put secrets in headers or
bodies.

**Binary response bodies** — a [`response_file`](#-file-backed-responses-response_file)
with a non-text content type — are stored as a descriptor
(`<70 bytes of image/png from fixtures/avatar.png>`) rather than as bytes, so
they never blow past `MIMIC_MAX_RECORDED_BODY` or arrive in the dashboard as a
wall of replacement characters.

**Redaction is a property of the log only.** The body sent to the client, the
body matching runs against, and the values `{{body.*}}` interpolates are all
untouched.

```bash
MIMIC_REDACT_BODY_FIELDS=password,cvv,pin mimic   # replace the default list
MIMIC_REDACT_BODY_FIELDS= mimic                   # store bodies verbatim
MIMIC_DISABLE_BODY_LOG=true mimic                 # store no bodies at all
```

### Protecting the admin API

`MIMIC_ADMIN_TOKEN` puts the admin endpoints behind a bearer token. Unset —
the default — leaves them open exactly as they have always been.

```bash
MIMIC_ADMIN_TOKEN=s3cret mimic
```

```bash
curl -H 'Authorization: Bearer s3cret' http://localhost:8080/admin/requests
```

Without the header, those endpoints answer `401 {"error": "unauthorized"}`.
Two things stay open on purpose: `/health`, because liveness probes call it
and carry no credentials, and any path under the admin prefix that Mimic
doesn't itself answer (`GET /admin/users`), because that's an ordinary mock.

Note that `/admin/dashboard` is guarded too, so a browser won't load it while a
token is set — use the token with `curl`, move the admin API somewhere private
with `MIMIC_ADMIN_PREFIX`, or leave the token unset on a machine only you can
reach.

---

## 🎯 Use Cases

### 1. **Frontend Development**
Mock backend APIs while building your frontend:

```bash
# Start Mimic with your API mocks, with CORS on for your dev server
docker run -d -p 8080:8080 \
  -e MIMIC_CORS=true \
  -e MIMIC_CORS_ORIGINS=http://localhost:3000 \
  -v ./mocks:/app/mocks:ro ragilhadi/mimic:latest

# Point your frontend to http://localhost:8080 — preflights are handled for you
```

### 2. **API Prototyping**
Quickly prototype API responses:

```bash
# Create mock files
echo '{"method":"GET","path":"/api/v1/products","status":200,"response":[]}' > mocks/products.json

# Start Mimic
docker compose up -d
```

### 3. **Third-Party API Simulation**
Simulate external APIs for testing:

```bash
# Mock Stripe API
# Mock GitHub API
# Mock any REST API
```

---

## 🛠️ Development

### Prerequisites

- Rust 1.70+ (for building from source)
- Docker (for containerized development)
- Make (optional, for convenience commands)

### Local Development

1. **Clone the repository:**

```bash
git clone https://github.com/ragilhadi/mimic.git
cd mimic
```

2. **Run locally:**

```bash
# Using Makefile
make dev

# Or using Cargo directly
cargo run
```

Either one serves the mocks in this repo's `./mocks` directory — no
configuration needed. To read a different directory, set `MIMIC_MOCKS_DIR`:

```bash
MIMIC_MOCKS_DIR=./fixtures/staging cargo run
```

3. **Run tests:**

```bash
# Run all tests
make test

# Run with verbose output
make test-verbose

# Generate coverage report
make test-coverage
```

### Available Make Commands

```bash
# Development
make dev          # Run development server with debug logging
make watch        # Auto-rebuild and run on file changes
make check        # Quick compile check

# Building
make build        # Build debug binary
make release      # Build optimized release binary

# Testing
make test         # Run all tests
make test-verbose # Run tests with output
make test-coverage# Generate test coverage report
make ci-local     # Run all CI checks locally

# Code Quality
make fmt          # Format code with rustfmt
make lint         # Run clippy linter
make audit        # Security audit of dependencies

# Docker
make docker-build       # Build Docker image
make docker-run         # Run in Docker container
make docker-compose-up  # Start with docker-compose
make docker-compose-down# Stop docker-compose services

# Cleanup
make clean        # Remove build artifacts
```

---

## 🔄 CI/CD

Mimic uses GitHub Actions for automated testing and deployment:

### Workflows

1. **Unit Tests** (`.github/workflows/unit-test.yml`)
   - Runs on every push to `main`
   - Runs on every pull request
   - Checks code formatting
   - Runs linter (clippy)
   - Executes all 35+ unit tests
   - Tests on multiple Rust versions (stable, beta, nightly)
   - Performs security audit
   - Generates coverage report

2. **Docker Build & Push** (`.github/workflows/docker-build-push.yml`)
   - Builds Docker image on push to `main`
   - Pushes to Docker Hub automatically
   - Multi-platform support (amd64, arm64)
   - Version tagging from `vars/version` file

### Quality Metrics

- ✅ **35+ Unit Tests** - Comprehensive test coverage
- ✅ **~90% Code Coverage** - High quality assurance
- ✅ **Zero Clippy Warnings** - Clean, idiomatic Rust code
- ✅ **Security Audited** - Dependencies checked for vulnerabilities

---

## 📦 Docker Hub

Pre-built images are available on Docker Hub:

**Repository**: [ragilhadi/mimic](https://hub.docker.com/r/ragilhadi/mimic)

### Available Tags

```bash
# Latest version
docker pull ragilhadi/mimic:latest

# Specific version
docker pull ragilhadi/mimic:v1.0.0
```

### Image Details

- **Base Image**: Debian Bookworm Slim
- **Size**: ~90 MB (compressed)
- **Platforms**: linux/amd64, linux/arm64
- **Runtime**: Minimal dependencies (ca-certificates, wget)

---

## 🔍 Health Check

Mimic includes a built-in health check endpoint:

```bash
curl http://localhost:8080/health
```

**Response:**
```json
{
  "status": "healthy",
  "mocks_loaded": 5,
  "mock_count": 7,
  "service": "mimic",
  "version": "1.14.0",
  "uptime_seconds": 4021,
  "port": 8080,
  "max_body_size": 10485760,
  "max_log_entries": 1000,
  "max_recorded_body": 65536,
  "requests_recorded": 138
}
```

`mocks_loaded` counts registered `METHOD:path` routes; `mock_count` counts mock
definitions, which is larger when several mocks share one route. The admin
dashboard reads this endpoint for its header summary bar.

Use this endpoint for:
- Docker health checks
- Kubernetes liveness/readiness probes
- Load balancer health checks
- Monitoring systems

---

## ⚙️ Request Body Consumption

Mimic supports configurable request body consumption per endpoint via the `consume_body` field.

### Default Behavior

**By default, `consume_body` is `false`** (fast performance):
- Mimic responds immediately without reading the request body
- Optimal for endpoints that don't need the body
- Best performance and lowest memory usage

The decision is made **per endpoint**, scoped to the mocks registered for the
request's method and path — a `body` matcher on some unrelated mock elsewhere
in your mocks directory has no effect on this one.

Mimic reads the body when any mock that could serve the request:

- sets `"consume_body": true`, **or**
- declares a `body` matcher (it can't match what it hasn't read), **or**
- interpolates `{{body.…}}` into its `response` or a sequence step's response

...or when **no mock is registered at all** for that method and path, so the
404 response and the request log can still show what the client sent.

Otherwise the body is left unread — which is exactly what `consume_body: false`
promises.

### When to Use consume_body: true

Set `consume_body: true` for endpoints that handle:

1. **File Uploads**
   ```json
   {
     "method": "POST",
     "path": "/upload-document",
     "status": 200,
     "consume_body": true,
     "response": {"uploaded": true}
   }
   ```

2. **Multipart/Form-Data**
   ```json
   {
     "method": "POST",
     "path": "/ocr-image",
     "status": 200,
     "consume_body": true,
     "response": {"text": "extracted text"}
   }
   ```

3. **Large Payloads**
   - Prevents "Broken Pipe" errors
   - Required when clients send large request bodies
   - Critical for image/document processing endpoints

### Performance Comparison

| consume_body | Speed | Memory | Use Case |
|--------------|-------|--------|----------|
| `false` (default) | ⚡ Fastest | Minimal | No body expected |
| `true` | Standard | Temporary spike | File uploads, large payloads |

**Example:**
```bash
# Works with consume_body: true
curl -X POST http://localhost:8080/ocr-image \
  -F "image=@large-document.jpg"

# Works with consume_body: false (default)
curl -X POST http://localhost:8080/trigger-job
```

### Maximum Body Size

Request bodies are capped at **10 MB** by default. The cap is enforced *while
the body streams in*, so an oversized request is turned away before Mimic
allocates memory for it — a client cannot drive the server's memory use past
the limit no matter how much it sends.

Over-limit requests get a `413 Payload Too Large`:

```json
{
  "error": "payload too large",
  "method": "POST",
  "path": "/upload",
  "max_body_size": 10485760
}
```

Raise or lower the cap with the `MIMIC_MAX_BODY_SIZE` environment variable
(in bytes):

```bash
MIMIC_MAX_BODY_SIZE=52428800 mimic   # 50 MB
```

An unset, unparsable, or zero value falls back to the 10 MB default. The
active limit is printed at startup.

---

## 📖 API Reference

### Endpoints

#### Health Check
```
GET /health
```

Returns server status, loaded mock counts, and the runtime configuration the
dashboard's summary bar displays. See [Health Check](#-health-check).

#### Admin

```
GET    /admin/dashboard         # Web dashboard (Requests / Mocks / Sequences)
GET    /admin/requests          # List recorded requests (see filters below)
DELETE /admin/requests          # Clear recorded requests
GET    /admin/mocks             # List every loaded mock, with matchers and hit counts
GET    /admin/sequences         # Current step of every in-progress sequence
POST   /admin/sequences/reset   # Reset sequence counters (optional: ?path=/api/submit)
GET    /admin/scenario          # Which scenario tags are currently active
POST   /admin/scenario          # Replace the active tag set (body: {"tags": [...]})
```

All admin endpoints are read-only apart from the ones that say otherwise, and
all return JSON — the dashboard is just one client of them.

##### `GET /admin/requests`

| Query param | Meaning |
|-------------|---------|
| `path` | Substring of the request path |
| `method` | Exact method, case-insensitive |
| `status` | Exact code (`404`) or class (`4xx`, `5xx`) |
| `unmatched_only` | `true`/`1`/`yes` — only requests no mock served |
| `search` | Case-insensitive text search over body, headers and query params |

```bash
# Every 4xx whose path mentions "user" and that nothing matched
curl "http://localhost:8080/admin/requests?path=user&status=4xx&unmatched_only=true"
```

Each record carries the request as before, plus — when there is something to
report — the response served and the match diagnosis:

```json
{
  "id": 12,
  "timestamp": "2026-08-07T10:15:04Z",
  "method": "GET",
  "path": "/users/42",
  "query_params": {},
  "headers": { "accept": "application/json", "authorization": "[REDACTED]" },
  "matched_mock": "GET:/users/42",
  "response_status": 200,
  "response_body": "{\"id\":42}",
  "response_headers": { "content-type": "application/json" },
  "match_score": 900,
  "path_params": { "id": "42" },
  "match_explanation": "matched mocks/get_users_id.json (score 900: method+path 1000, path pattern -100)"
}
```

Every field after `response_status` is additive and omitted when empty, so
existing consumers of this endpoint keep working unchanged.

##### `GET /admin/mocks`

```json
{
  "count": 12,
  "mocks": [
    {
      "key": "GET:/users/:id",
      "index": 0,
      "method": "GET",
      "path": "/users/:id",
      "status": 200,
      "source": "mocks/generated/get_users_id.json",
      "has_path_params": true,
      "matchers": { "query_params": false, "headers": true, "body": false },
      "delay_ms": null,
      "sequence_steps": null,
      "response_headers": 1,
      "consume_body": false,
      "hits": 4,
      "tags": [],
      "active": true,
      "config": { "method": "GET", "path": "/users/:id", "status": 200, "response": {} }
    }
  ]
}
```

`source` is the file the mock was loaded from, and is absent for mocks not read
from disk. `config` is the full `MockConfig`. `hits` counts requests served by
that specific mock, so two mocks sharing a path stay distinguishable. The
endpoint reads through the same lock hot reload writes to, so it always reflects
the mock set currently serving. `tags` and `active` describe the mock's
[scenario](#-tagged-mock-groups-scenarios) membership — `active: false` means
the mock is loaded but filtered out by the current scenario, and so unmatchable.

##### `GET /admin/sequences`

```json
{
  "count": 1,
  "sequences": [
    {
      "key": "POST:/submit#0",
      "method": "POST",
      "path": "/submit",
      "step": 2,
      "total": 3,
      "source": "mocks/post_submit.json"
    }
  ]
}
```

`step` is how many calls the sequence has served — the index of the step the
next request will get. A sequence appears only once it has served a request.

##### `POST /admin/sequences/reset`

Returns the number of counters that were reset:

```json
{ "reset": 2 }
```

##### `GET /admin/scenario`

```json
{
  "active_tags": ["happy-path"],
  "filtering": true,
  "known_tags": ["error-scenario", "happy-path"],
  "matchable_mocks": 12,
  "total_mocks": 14
}
```

`known_tags` is every tag declared by a loaded mock. `filtering` is `false`
(and `active_tags` empty) when no scenario filter is configured, i.e. every
mock is matchable.

##### `POST /admin/scenario`

Replaces the active tag set and returns the same body as the `GET`:

```bash
curl -X POST http://localhost:8080/admin/scenario -d '{"tags": ["error-scenario"]}'
```

The body is read as JSON regardless of `Content-Type`, so plain `curl -d` works.
`{"tags": []}` clears the filter; a malformed body is a `400` and leaves the
current scenario untouched. See
[Tagged Mock Groups](#-tagged-mock-groups-scenarios).

#### Mock Endpoints

All other endpoints are defined by your mock files. Mimic will:

1. Match the HTTP method and path
2. Return the configured status code
3. Return the configured response body

**If no mock matches** — `404 Not Found`. The body echoes what the server
actually received, so you can see why nothing matched:

```json
{
  "error": "mock not found",
  "method": "GET",
  "path": "/undefined",
  "query_params": {},
  "headers_received": ["host", "user-agent", "accept"]
}
```

`query_params` is the parsed query string and `headers_received` lists the
header names the request arrived with (names only — values are never echoed).
Both fields are always present. See
[ADVANCED_MATCHING.md](ADVANCED_MATCHING.md#debugging) for using this to debug
a mismatch.

**If the request body exceeds the size limit** — `413 Payload Too Large`. See
[Maximum Body Size](#maximum-body-size):

```json
{
  "error": "payload too large",
  "method": "POST",
  "path": "/upload",
  "max_body_size": 10485760
}
```

---

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

### Development Workflow

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run tests: `make test`
5. Run linter: `make lint`
6. Format code: `make fmt`
7. Submit a pull request

### Running Tests

```bash
# Run all tests
make test

# Run with coverage
make test-coverage

# Run all CI checks
make ci-local
```

---

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

---

## 🙏 Acknowledgments

Built with:
- [Rust](https://www.rust-lang.org/) - Systems programming language
- [Axum](https://github.com/tokio-rs/axum) - Web framework
- [Tokio](https://tokio.rs/) - Async runtime
- [Serde](https://serde.rs/) - Serialization framework

---

## 📞 Support

- **Issues**: [GitHub Issues](https://github.com/ragilhadi/mimic/issues)
- **Discussions**: [GitHub Discussions](https://github.com/ragilhadi/mimic/discussions)
- **Docker Hub**: [ragilhadi/mimic](https://hub.docker.com/r/ragilhadi/mimic)

---

## 🗺️ Roadmap

- [x] Request body matching
- [x] Query parameter matching
- [x] Header matching
- [x] Stateful response sequences (different response per call)
- [x] Response delays (mock-level `delay_ms`, fixed or random range, plus per sequence step)
- [x] Custom response headers (`response_headers` — CORS, redirects, non-JSON content types)
- [x] Admin UI (request history dashboard)
- [x] Hot reload for mock files
- [x] Dynamic response templating (`{{query.x}}`, `{{header.x}}`, `{{body.x}}`, `{{path.x}}`)
- [x] Path parameter matching (`:id`, `{id}` syntax)
- [x] Faker-style random data generators (`{{faker.uuid}}`, `{{faker.name}}`, `{{faker.int}}`, …)
- [x] Built-in CORS with automatic `OPTIONS` preflight handling (`MIMIC_CORS=true`)

---

**Made with ❤️ using Rust**