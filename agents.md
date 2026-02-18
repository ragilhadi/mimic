# 🤖 Agents Documentation

## 📌 Project Overview

**Mimic** is a lightweight, high-performance HTTP mock server built with Rust and Axum. It provides a simple way to mock HTTP endpoints by defining responses in JSON files. The server is designed for:

- Frontend/API development (mocking backend endpoints)
- API prototyping and testing
- Simulating third-party APIs
- CI/CD pipeline testing

**Key Features:**
- Ultra lightweight (1.66 MiB memory usage)
- Blazing fast (<10ms response time, <1s startup)
- File-based configuration (JSON mock files)
- Advanced request matching (query params, headers, body)
- Docker-ready with pre-built images

---

## 🤖 Agent Definitions

In the context of Mimic, "agents" refer to the logical components that work together to handle HTTP requests and return mocked responses. Each agent has a specific responsibility in the request processing pipeline.

### 1. **Loader Agent**

**Purpose:** Initialize and load mock configurations from the file system.

**Responsibilities:**
- Scan the `/app/mocks/` directory for JSON mock files
- Parse each JSON file into `MockConfig` structures
- Build a HashMap keyed by "METHOD:PATH" for fast lookup
- Validate mock configurations during startup
- Report the number of successfully loaded mocks

**Inputs:**
- Directory path: `/app/mocks/` (configurable via environment)
- JSON files containing mock definitions

**Outputs:**
- `HashMap<String, MockConfig>`: In-memory map of all mocks
- Startup logs showing loaded mock count
- Error logs for invalid or unparsable mock files

**Implementation:** `src/loader.rs`

**Configuration Example:**
```json
{
  "method": "GET",
  "path": "/users",
  "status": 200,
  "response": {"users": []}
}
```

---

### 2. **Router Agent**

**Purpose:** Route incoming HTTP requests to the appropriate handler.

**Responsibilities:**
- Set up HTTP routes and middleware stack
- Handle the `/health` endpoint for health checks
- Route all other requests to the catch-all handler
- Apply CORS middleware and request logging
- Manage the server lifecycle (startup, shutdown)

**Inputs:**
- Incoming HTTP requests on configured port (default: 8080)
- Loaded mock configurations from Loader Agent
- Environment variables: `PORT`, `RUST_LOG`

**Outputs:**
- HTTP responses (status code + JSON body)
- Access logs via tracing framework
- Health check responses with mock count

**Implementation:** `src/main.rs`

**Endpoints:**
- `GET /health` → Health check with mock statistics
- `/* (catch-all)` → Mock response handler

---

### 3. **Matcher Agent**

**Purpose:** Match incoming requests against available mocks using advanced pattern matching.

**Responsibilities:**
- Extract request context (method, path, query params, headers, body)
- Score each potential mock match based on multiple criteria
- Apply exact matching for method and path
- Apply pattern matching for query parameters
- Apply pattern matching for HTTP headers
- Apply content matching for request body (JSON, text, form)
- Select the highest-scoring mock
- Return 404 if no mock matches

**Inputs:**
- `RequestContext`: Parsed request data
  - HTTP method
  - URL path
  - Query parameters
  - HTTP headers
  - Request body (optional)
- Available mocks from Loader Agent

**Outputs:**
- Best matching `MockConfig` (if found)
- Match score for debugging
- `None` if no match found

**Implementation:** `src/matcher.rs`

**Scoring System:**
- Base score: 1000 (method + path match)
- Query params: +100 per matched param
- Headers: +50 per matched header
- Body match: +500

**Matching Capabilities:**
- **Query Parameters:** Exact, regex, "any value", strict mode
- **Headers:** Exact, prefix, contains, regex, forbidden headers
- **Body:** JSON (exact/partial), text (exact/contains/regex), form fields

---

### 4. **Handler Agent**

**Purpose:** Process HTTP requests and generate mock responses.

**Responsibilities:**
- Parse incoming HTTP requests
- Extract method, path, query parameters, headers, and body
- Build a `RequestContext` structure
- Delegate to Matcher Agent for mock selection
- Return the configured response (status + JSON body)
- Handle body consumption based on `consume_body` flag
- Generate 404 responses for unmatched requests
- Log all request/response details

**Inputs:**
- Raw HTTP request
- Loaded mocks from Loader Agent

**Outputs:**
- HTTP response with:
  - Status code (from mock config)
  - JSON body (from mock config)
  - Appropriate headers (Content-Type, etc.)
- Tracing logs for debugging

**Implementation:** `src/handler.rs`

**Body Consumption:**
- `consume_body: false` (default) → Skip body reading for performance
- `consume_body: true` → Read body (required for file uploads, multipart/form-data)

---

### 5. **Logger Agent**

**Purpose:** Provide observability and debugging capabilities.

**Responsibilities:**
- Initialize structured logging with `tracing-subscriber`
- Log server startup and configuration
- Log each incoming request (method, path, query params)
- Log matched mock details
- Log response status codes
- Log errors and warnings
- Support configurable log levels via `RUST_LOG`

**Inputs:**
- `RUST_LOG` environment variable (trace, debug, info, warn, error)
- Application events and errors

**Outputs:**
- Formatted log messages to stdout
- Structured logs with context (timestamps, levels, targets)

**Implementation:** Initialized in `src/main.rs`, used throughout

**Log Levels:**
- `trace` - Very detailed debugging
- `debug` - Detailed debugging
- `info` - General information (recommended)
- `warn` - Warnings only
- `error` - Errors only

---

## 🔄 Agent Interaction Flow

The following diagram shows how agents coordinate to handle an HTTP request:

```
┌─────────────────────────────────────────────────────────────┐
│                      Server Startup                         │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼
        ┌──────────────────────────────────┐
        │      1. Logger Agent             │
        │   (Initialize Logging)           │
        └──────────────────────────────────┘
                            │
                            ▼
        ┌──────────────────────────────────┐
        │      2. Loader Agent             │
        │   (Load Mock Files)              │
        │   • Scan /app/mocks/             │
        │   • Parse JSON files             │
        │   • Build HashMap                │
        └──────────────────────────────────┘
                            │
                            ▼
        ┌──────────────────────────────────┐
        │      3. Router Agent             │
        │   (Start HTTP Server)            │
        │   • Bind to PORT (8080)          │
        │   • Setup routes                 │
        │   • Apply middleware             │
        └──────────────────────────────────┘
                            │
┌───────────────────────────┴─────────────────────────┐
│                Server Running - Ready               │
└─────────────────────────────────────────────────────┘


┌─────────────────────────────────────────────────────────────┐
│                   Request Processing                        │
└─────────────────────────────────────────────────────────────┘

    HTTP Request
         │
         ▼
┌──────────────────────────────────┐
│      Router Agent                │
│   (Route Request)                │
└──────────────────────────────────┘
         │
         ├─── /health ────────────────────┐
         │                                 │
         └─── /* (all other) ──┐          ▼
                                │    ┌─────────────────┐
                                │    │ Return Health   │
                                │    │ Check Response  │
                                │    └─────────────────┘
                                │
                                ▼
                ┌──────────────────────────────────┐
                │      Handler Agent               │
                │   (Process Request)              │
                │   • Extract method, path         │
                │   • Parse query params           │
                │   • Extract headers              │
                │   • Read body (if needed)        │
                │   • Build RequestContext         │
                └──────────────────────────────────┘
                                │
                                ▼
                ┌──────────────────────────────────┐
                │      Matcher Agent               │
                │   (Find Best Match)              │
                │   • Iterate all mocks            │
                │   • Score each match             │
                │   • Check method & path          │
                │   • Check query params           │
                │   • Check headers                │
                │   • Check body                   │
                │   • Select highest score         │
                └──────────────────────────────────┘
                                │
                ┌───────────────┴───────────────┐
                │                               │
                ▼                               ▼
        Match Found                      No Match
                │                               │
                ▼                               ▼
    ┌──────────────────────┐      ┌──────────────────────┐
    │ Return Mock Response │      │ Return 404 Response  │
    │ • Status from config │      │ • Error message      │
    │ • Body from config   │      │ • Request details    │
    └──────────────────────┘      └──────────────────────┘
                │                               │
                └───────────────┬───────────────┘
                                │
                                ▼
                ┌──────────────────────────────────┐
                │      Logger Agent                │
                │   (Log Response)                 │
                │   • Request details              │
                │   • Matched mock (if any)        │
                │   • Response status              │
                └──────────────────────────────────┘
                                │
                                ▼
                        HTTP Response
```

### Step-by-Step Workflow

**Startup Phase:**

1. **Logger Agent** initializes structured logging
2. **Loader Agent** scans and loads all mock files from `/app/mocks/`
3. **Router Agent** starts HTTP server on configured port

**Request Phase:**

1. **Router Agent** receives HTTP request
2. If path is `/health`, return health check response (skip to step 7)
3. **Handler Agent** parses request into `RequestContext`
4. **Matcher Agent** finds best matching mock using scoring algorithm
5. **Handler Agent** returns configured response or 404
6. **Logger Agent** logs request and response details
7. HTTP response sent to client

---

## 📋 Rules & Constraints

### Coding Rules

1. **Type Safety**
   - All agents use strongly-typed Rust structures
   - Leverage Serde for JSON serialization/deserialization
   - Use `Result<T, E>` for error handling
   - Avoid unsafe code

2. **Performance**
   - Use async/await for I/O operations
   - Minimize allocations in hot paths
   - Use `HashMap` for O(1) mock lookups
   - Default to `consume_body: false` for faster responses

3. **Code Style**
   - Follow Rust standard formatting (rustfmt)
   - Pass clippy lints with zero warnings
   - Use descriptive variable names
   - Document public APIs

4. **Testing**
   - Maintain 35+ unit tests
   - Target ~90% code coverage
   - Test edge cases and error paths
   - Use table-driven tests for matchers

### Error Handling

1. **Loader Agent**
   - Log errors for unparsable JSON files
   - Continue loading other files on error
   - Report total number of successfully loaded mocks
   - Never panic during loading

2. **Handler Agent**
   - Return 404 for unmatched requests
   - Include error details in response body
   - Log all errors with context
   - Never expose internal errors to clients

3. **Matcher Agent**
   - Return `None` if no match found
   - Handle missing fields gracefully
   - Validate regex patterns at startup
   - Log scoring details for debugging

4. **Router Agent**
   - Graceful shutdown on SIGTERM
   - Bind errors logged clearly
   - Health check always succeeds

### Logging Behavior

1. **Startup Logs**
   - Port number and bind address
   - Number of mocks loaded
   - Configuration summary

2. **Request Logs** (INFO level)
   - HTTP method and path
   - Query parameters
   - Matched mock (if any)
   - Response status code

3. **Debug Logs** (DEBUG level)
   - Request headers
   - Request body
   - Match scores
   - Detailed matcher evaluation

4. **Error Logs** (ERROR level)
   - File loading failures
   - JSON parsing errors
   - Server binding errors

### Security & Data Handling

1. **Input Validation**
   - Validate all JSON mock files at load time
   - Sanitize log output (avoid logging sensitive data)
   - Limit request body size to prevent DoS
   - Use read-only volume mounts for mock files

2. **Request Isolation**
   - Each request is handled independently
   - No shared state between requests
   - No request data persistence

3. **Container Security**
   - Run as non-root user in Docker
   - Minimal base image (Debian Slim)
   - Only required dependencies installed
   - Regular security audits with `cargo audit`

4. **Secrets Management**
   - Never log authorization tokens
   - Don't store credentials in mock files
   - Use environment variables for configuration

### Performance Constraints

1. **Memory Usage**
   - Target: <2 MiB at idle
   - All mocks loaded into memory at startup
   - No dynamic mock loading (for predictable performance)

2. **Response Time**
   - Target: <10ms for most requests
   - O(1) mock lookup by method:path
   - Lazy body consumption when not needed

3. **Startup Time**
   - Target: <1 second
   - Parallel mock file loading
   - Pre-compiled regex patterns

---

## 📚 Usage Guide

### For Developers

#### Creating New Mocks

1. **Create a JSON file** in the `mocks/` directory:

```bash
# Example: mocks/get_users.json
{
  "method": "GET",
  "path": "/api/users",
  "status": 200,
  "response": {
    "users": [
      {"id": 1, "name": "Alice"},
      {"id": 2, "name": "Bob"}
    ]
  }
}
```

2. **Restart the server** (or wait for hot reload if implemented):

```bash
docker compose restart mimic
```

3. **Test your mock**:

```bash
curl http://localhost:8080/api/users
```

#### Advanced Matching

Use matchers for conditional responses:

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
      "password": "secret"
    }
  },
  "response": {
    "token": "xyz123"
  }
}
```

See [ADVANCED_MATCHING.md](ADVANCED_MATCHING.md) for detailed matcher syntax.

#### Debugging

Enable debug logging to see matcher scores:

```bash
# docker-compose.yml
environment:
  - RUST_LOG=debug
```

Check logs:
```bash
docker compose logs -f mimic
```

### Extending Agents

#### Adding New Matcher Types

1. **Update `types.rs`** to add new matcher variant:

```rust
pub enum BodyMatcher {
    Json { /* existing fields */ },
    Text { /* existing fields */ },
    Form { /* existing fields */ },
    Xml { /* new fields */ }, // Add this
}
```

2. **Implement matching logic** in `matcher.rs`:

```rust
fn matches_xml_body(/* params */) -> bool {
    // Implementation
}
```

3. **Add scoring** in `find_matching_mock`:

```rust
if matches_xml_body(...) {
    score += 500;
}
```

4. **Add tests** in `matcher.rs`:

```rust
#[test]
fn test_xml_body_matching() {
    // Test implementation
}
```

#### Adding New Routes

Update `main.rs` to add new endpoints:

```rust
let app = Router::new()
    .route("/health", get(health_check))
    .route("/metrics", get(metrics_handler)) // Add this
    .fallback(handle_request);
```

#### Adding Response Headers

Modify `handler.rs` to add custom headers:

```rust
let response = (
    StatusCode::from_u16(mock_config.status).unwrap(),
    [(header::CONTENT_TYPE, "application/json")],
    Json(mock_config.response.clone())
);
```

### Development Workflow

1. **Set up development environment**:

```bash
git clone https://github.com/ragilhadi/mimic.git
cd mimic
make dev
```

2. **Make changes** to agent code in `src/`

3. **Run tests**:

```bash
make test
```

4. **Run linter**:

```bash
make lint
```

5. **Format code**:

```bash
make fmt
```

6. **Build for production**:

```bash
make release
```

### Testing Agents

Each agent has dedicated unit tests:

- `loader.rs`: Tests for JSON parsing, file loading, error handling
- `matcher.rs`: Tests for all matching types, scoring algorithm
- `handler.rs`: Tests for request processing, response generation
- `main.rs`: Integration tests for routes and health checks

Run specific test:
```bash
cargo test test_loader_loads_valid_mocks
cargo test test_exact_path_matching
```

---

## 🎯 Best Practices

1. **Mock File Organization**
   - Use descriptive filenames: `get_users.json`, `post_login.json`
   - Group by feature: `mocks/auth/`, `mocks/users/`
   - One mock per file for clarity

2. **Response Design**
   - Keep responses realistic
   - Include all expected fields
   - Use appropriate HTTP status codes
   - Add error response mocks

3. **Matcher Optimization**
   - Use simple matchers when possible (exact match is fastest)
   - Avoid overly complex regex patterns
   - Use `strict: false` for flexible matching

4. **Performance**
   - Set `consume_body: false` unless needed
   - Minimize mock file count for faster startup
   - Use simple JSON structures in responses

---

## 🔗 References

- [README.md](README.md) - Full documentation
- [ADVANCED_MATCHING.md](ADVANCED_MATCHING.md) - Matcher syntax reference
- [Cargo.toml](Cargo.toml) - Dependencies and build config
- [Makefile](Makefile) - Development commands
- [Docker Hub](https://hub.docker.com/r/ragilhadi/mimic) - Pre-built images

---

## 📝 Assumptions

This document assumes the following:

1. **Agent Definition**: In the context of Mimic, "agents" are defined as logical components with distinct responsibilities, not autonomous AI entities or separate processes.

2. **Conceptual Model**: The agent model presented here is a conceptual framework to help understand the system architecture. In practice, these agents are implemented as Rust modules and functions.

3. **Future Evolution**: If Mimic adopts true autonomous agents (e.g., for dynamic mock generation, machine learning-based matching), this document should be updated accordingly.

4. **Deployment Context**: Assumes Docker-based deployment as the primary use case, though local development and other deployment methods are supported.

5. **User Knowledge**: Assumes basic familiarity with:
   - HTTP concepts (methods, status codes, headers)
   - JSON syntax
   - Docker and containerization
   - Command-line tools

---

**Last Updated:** 2026-02-18  
**Version:** 1.0.0  
**Maintained by:** Mimic Contributors
