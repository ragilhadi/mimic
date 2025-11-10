# 🧩 Mimic - Lightweight HTTP Mock Server

[![Tests](https://github.com/ragilhadi/mimic/workflows/Unit%20Tests/badge.svg)](https://github.com/ragilhadi/mimic/actions)
[![Docker](https://img.shields.io/docker/v/ragilhadi/mimic?label=docker)](https://hub.docker.com/r/ragilhadi/mimic)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

**Mimic** is a fast, lightweight HTTP mock server built with Rust and Axum. Perfect for testing, development, and API prototyping. Define your mock responses in simple JSON files and let Mimic handle the rest.

## ✨ Features

- 🚀 **Blazing Fast** - Built with Rust and Axum for maximum performance
- 💾 **Ultra Lightweight** - Only **1.66 MiB** memory usage
- 📁 **File-Based Configuration** - Define mocks in simple JSON files
- 🔄 **Hot Reload** - Changes to mock files are reflected immediately
- 🐳 **Docker Ready** - Pre-built images available on Docker Hub
- ✅ **Well Tested** - 44+ unit tests with high code coverage
- 🔧 **Easy to Use** - Simple configuration, no complex setup
- ⚙️ **Configurable Body Consumption** - Control request body handling per endpoint
- 📤 **File Upload Support** - Handle multipart/form-data with `consume_body` option

---

## 📊 Performance

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

#### Mock Endpoints

All other endpoints are defined by your mock files. Mimic will:

1. Match the HTTP method and path
2. Return the configured status code
3. Return the configured response body

**If no mock matches:**
```json
{
  "error": "mock not found",
  "method": "GET",
  "path": "/undefined"
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

- [ ] WebSocket support
- [ ] Request body matching
- [ ] Query parameter matching
- [ ] Header matching
- [ ] Response delays
- [ ] Request logging
- [ ] Admin UI
- [ ] Hot reload for mock files

---

**Made with ❤️ using Rust**