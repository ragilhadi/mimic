# 🤖 Agents Documentation

## 📌 Project Overview

**Mimic** is a lightweight HTTP mock server built with Rust and Axum. It mocks HTTP endpoints using JSON configuration files for development, testing, and API prototyping.

**Key Features:** Ultra lightweight (1.66 MiB), fast (<10ms response), file-based config, advanced request matching, Docker-ready.

---

## 🤖 Agent Definitions

"Agents" are logical components in the HTTP request processing pipeline:

### 1. **Loader Agent** (`src/loader.rs`)
- **Purpose:** Load mock configurations from JSON files
- **Inputs:** `/app/mocks/` directory with JSON mock files
- **Outputs:** `HashMap<String, MockConfig>` for O(1) lookup
- **Responsibilities:** Scan, parse, validate mock files; build method:path index

### 2. **Router Agent** (`src/main.rs`)
- **Purpose:** Route HTTP requests to handlers
- **Inputs:** HTTP requests on port 8080, `PORT` and `RUST_LOG` env vars
- **Outputs:** HTTP responses, access logs, health checks
- **Responsibilities:** Setup routes (`/health`, catch-all), middleware, lifecycle management

### 3. **Matcher Agent** (`src/matcher.rs`)
- **Purpose:** Find best matching mock using pattern matching
- **Inputs:** `RequestContext` (method, path, query, headers, body), available mocks
- **Outputs:** Best matching `MockConfig` or `None`
- **Responsibilities:** Score each mock, match on query params/headers/body
- **Scoring:** Base 1000 (method+path) + 100/query + 50/header + 500 (body)

### 4. **Handler Agent** (`src/handler.rs`)
- **Purpose:** Process requests and generate responses
- **Inputs:** Raw HTTP request, loaded mocks
- **Outputs:** HTTP response (status, JSON body, headers), logs
- **Responsibilities:** Parse request → build context → call Matcher → return response/404

### 5. **Importer Agent** (`src/openapi.rs`)
- **Purpose:** Generate mock files from an OpenAPI 3.x spec
- **Inputs:** `mimic import-openapi <spec.yaml|spec.json> [--out <dir>] [--status <code>] [--force] [--brace-params]`
- **Outputs:** One `MockConfig` JSON file per operation + response status
- **Responsibilities:** Parse JSON/YAML, resolve `$ref`, pick examples or build schema stubs, translate `{id}` path templates, refuse to clobber a non-empty output directory without `--force`
- **Note:** Runs instead of the server (checked in `main` before the async runtime starts) and exits when done

### 6. **Logger Agent** (`src/main.rs`)
- **Purpose:** Observability and debugging
- **Inputs:** `RUST_LOG` env var, application events
- **Outputs:** Structured logs to stdout
- **Responsibilities:** Log startup, requests, responses, errors (trace/debug/info/warn/error)

---

## 🔄 Agent Interaction Flow

### Startup Phase
1. **Logger Agent** → Initialize logging
2. **Loader Agent** → Scan `/app/mocks/`, parse JSON, build HashMap
3. **Router Agent** → Start HTTP server on port 8080

### Request Phase
```
HTTP Request → Router Agent → Handler Agent → Matcher Agent
                   ↓              ↓              ↓
                Logger         Logger         Logger
                                              ↓
                               Match Found or 404
                                              ↓
                                       HTTP Response
```

**Step-by-step:**
1. Router receives request
2. If `/health` → return health check (skip to 6)
3. Handler parses request into `RequestContext`
4. Matcher finds best matching mock (highest score)
5. Handler returns configured response or 404
6. Logger logs request/response
7. HTTP response sent to client

---

## 📋 Rules & Constraints

### Coding Rules
- **Type Safety:** Use Rust types, Serde, `Result<T, E>`, no unsafe code
- **Performance:** async/await, minimize allocations, HashMap O(1) lookup, default `consume_body: false`
- **Code Style:** rustfmt, zero clippy warnings, document public APIs
- **Testing:** 35+ unit tests, ~90% coverage, test edge cases

### Error Handling
- **Loader:** Log parse errors, continue loading other files, never panic
- **Handler:** Return 404 for unmatched requests, log errors with context
- **Matcher:** Return `None` if no match, handle missing fields gracefully
- **Router:** Graceful shutdown on SIGTERM, health check always succeeds

### Security
- Validate JSON at load time
- Read-only volume mounts for mocks
- No request data persistence
- Never log auth tokens
- Regular `cargo audit`

### Performance Targets
- Memory: <2 MiB at idle
- Response time: <10ms
- Startup time: <1s

---

## 📚 Usage Guide

### Creating Mocks

Create JSON files in `mocks/` directory:
```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": {"users": []}
}
```

Restart server: `docker compose restart mimic`

### Advanced Matching

See [ADVANCED_MATCHING.md](ADVANCED_MATCHING.md) for:
- Query parameter matching (exact, regex, any value)
- Header matching (prefix, contains, regex)
- Body matching (JSON, text, form)

### Extending Agents

**Add new matcher types:**
1. Update `types.rs` with new matcher variant
2. Implement matching logic in `matcher.rs`
3. Add scoring in `find_matching_mock`
4. Add unit tests

**Add new routes:**
```rust
// src/main.rs
let app = Router::new()
    .route("/health", get(health_check))
    .route("/metrics", get(metrics_handler)) // new
    .fallback(handle_request);
```

### Development

```bash
make dev          # Run dev server
make test         # Run tests
make lint         # Run clippy
make fmt          # Format code
```

---

## 📝 Assumptions

1. **Agent Definition:** "Agents" are logical components (Rust modules/functions), not autonomous processes
2. **Conceptual Model:** Framework for understanding system architecture
3. **Deployment:** Assumes Docker-based deployment as primary use case
4. **User Knowledge:** Basic HTTP, JSON, Docker, and CLI familiarity

---

**Last Updated:** 2026-02-18 | **Version:** 1.0.0 | **Maintained by:** Mimic Contributors
