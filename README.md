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
- **Faker Data Generators** - Fresh random values on every call with `{{faker.uuid}}`, `{{faker.name}}`, `{{faker.int min=1 max=100}}`
- **OpenAPI Import** - Generate a whole mocks directory from an OpenAPI 3.x spec with `mimic import-openapi ./spec.yaml`

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

# Maximum request body Mimic will buffer, in bytes (default: 10485760 = 10 MB)
MIMIC_MAX_BODY_SIZE=10485760
```

**Log Levels**:
- `trace` - Very detailed debugging
- `debug` - Detailed debugging
- `info` - General information (recommended)
- `warn` - Warnings only
- `error` - Errors only

### Mock Files

Mimic reads mock definitions from JSON files in the `/app/mocks` directory (or `./mocks` locally).

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
- `consume_body` - (Optional) Boolean to control request body consumption (default: `false`)
  - `true` - Consume request body (required for file uploads, multipart/form-data)
  - `false` - Skip body consumption (faster, default behavior)

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

JSON and YAML specs are both accepted. The importer writes one ordinary mock file per **operation + response status** — the output is plain `MockConfig` JSON, so it hot-reloads like any other mock and can be hand-edited afterwards.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--out <dir>` | `./mocks/generated` | Output directory |
| `--status <code>` | `200` | Status treated as each operation's *primary* response; its file gets no status suffix |
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

**`mocks/generated/get_users_id_404.json`** (an alternative response — suffixed)

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

- **Multiple response codes become separate files.** They are alternative responses, not a call-order sequence, so they are deliberately *not* folded into a single [`sequence`](#-stateful-response-sequences) mock — enable the ones you want by keeping or deleting files.
- **`$ref` is resolved** for schemas, responses, path items, and named examples. Circular references collapse to `{}` at the point of recursion rather than looping forever; external (`./other.yaml#/...`) and unresolvable refs degrade to `{}` with a warning.
- **Existing files are protected.** A non-empty `--out` directory is refused unless you pass `--force`, so a re-import can't silently clobber mocks you edited by hand. With `--force`, every overwritten file is named in a warning.
- **Response keys** may be quoted (`'200'`) or bare (`200`), `default`, or wildcards (`4XX` → `400`).
- **Only OpenAPI 3.x is supported.** Swagger 2.0 specs are rejected with a clear message — convert first.
- A malformed spec produces an error and a non-zero exit code, never a panic.

---

## 🎯 Use Cases

### 1. **Frontend Development**
Mock backend APIs while building your frontend:

```bash
# Start Mimic with your API mocks
docker run -d -p 8080:8080 -v ./mocks:/app/mocks:ro ragilhadi/mimic:latest

# Point your frontend to http://localhost:8080
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
  "service": "mimic"
}
```

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

Returns server status and number of loaded mocks.

**Response:**
```json
{
  "status": "healthy",
  "mocks_loaded": 5,
  "service": "mimic"
}
```

#### Admin

```
GET    /admin/dashboard         # Web dashboard for request history
GET    /admin/requests          # List recorded requests (filters: ?path=&method=&status=)
DELETE /admin/requests          # Clear recorded requests
POST   /admin/sequences/reset   # Reset sequence counters (optional: ?path=/api/submit)
```

`POST /admin/sequences/reset` returns the number of counters that were reset:

```json
{ "reset": 2 }
```

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

---

**Made with ❤️ using Rust**