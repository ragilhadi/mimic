# Makefile for Mimic - Axum Web Server Project
# This file provides convenient shortcuts for common development tasks

.PHONY: help dev build release test test-verbose test-coverage test-all clean fmt lint check docker-build docker-run docker-stop docker-compose-up docker-compose-down install audit watch

# Default target - show help
help:
	@echo "Mimic Project - Available Commands"
	@echo "==================================="
	@echo ""
	@echo "Development:"
	@echo "  make dev          - Run development server with debug logging"
	@echo "  make watch        - Auto-rebuild and run on file changes (requires cargo-watch)"
	@echo "  make check        - Quick compile check without building"
	@echo ""
	@echo "Building:"
	@echo "  make build        - Build debug binary"
	@echo "  make release      - Build optimized release binary"
	@echo "  make install      - Install binary to system"
	@echo ""
	@echo "Testing:"
	@echo "  make test         - Run all tests"
	@echo "  make test-verbose - Run tests with output"
	@echo "  make test-coverage- Generate test coverage report"
	@echo "  make test-all     - Run all tests including coverage"
	@echo ""
	@echo "Code Quality:"
	@echo "  make fmt          - Format code with rustfmt"
	@echo "  make lint         - Run clippy linter"
	@echo "  make audit        - Security audit of dependencies"
	@echo "  make ci           - Run all CI checks (fmt + lint + test)"
	@echo ""
	@echo "Docker:"
	@echo "  make docker-build       - Build Docker image"
	@echo "  make docker-run         - Run application in Docker container"
	@echo "  make docker-stop        - Stop running Docker container"
	@echo "  make docker-compose-up  - Start with docker-compose (uses .env)"
	@echo "  make docker-compose-down- Stop docker-compose services"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean        - Remove build artifacts"
	@echo "  make clean-all    - Deep clean including dependencies"

# Development
dev:
	@echo "Starting development server..."
	RUST_LOG=debug cargo run

watch:
	@echo "Starting file watcher..."
	@command -v cargo-watch >/dev/null 2>&1 || { echo "cargo-watch not installed. Run: cargo install cargo-watch"; exit 1; }
	cargo watch -x run

check:
	@echo "Running quick compile check..."
	cargo check

# Building
build:
	@echo "Building debug binary..."
	cargo build

release:
	@echo "Building release binary (optimized)..."
	cargo build --release
	@echo "Binary location: target/release/mimic"

install: release
	@echo "Installing binary to system..."
	cargo install --path .

# Testing
test:
	@echo "Running tests..."
	cargo test

test-verbose:
	@echo "Running tests with output..."
	cargo test -- --nocapture --test-threads=1

test-coverage:
	@echo "Generating test coverage report..."
	@command -v cargo-tarpaulin >/dev/null 2>&1 || { echo "cargo-tarpaulin not installed. Run: cargo install cargo-tarpaulin"; exit 1; }
	cargo tarpaulin --out Html --output-dir ./coverage
	@echo "Coverage report generated in ./coverage/index.html"

test-all: test test-coverage
	@echo "All tests completed with coverage!"

# Code Quality
fmt:
	@echo "Formatting code..."
	cargo fmt

lint:
	@echo "Running clippy linter..."
	cargo clippy -- -D warnings

audit:
	@echo "Auditing dependencies for security vulnerabilities..."
	@command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit not installed. Run: cargo install cargo-audit"; exit 1; }
	cargo audit

ci: fmt lint test
	@echo "All CI checks passed!"

ci-local: check fmt lint test
	@echo "Local CI checks passed! Ready to commit."

# Docker
docker-build:
	@echo "Building Docker image..."
	docker build -t mimic:latest .
	@echo "Image built: mimic:latest"

docker-compose-up:
	@echo "Starting with docker-compose..."
	docker compose up -d
	@echo "Container running. Check with: docker-compose ps"

docker-compose-down:
	@echo "Stopping docker-compose services..."
	docker compose down

docker-run: docker-build
	@echo "Running Docker container..."
	docker run -d --name mimic-container -p 8080:8080 mimic:latest
	@echo "Container running on http://localhost:8080"
	@echo "Stop with: make docker-stop"

docker-stop:
	@echo "Stopping Docker container..."
	docker stop mimic-container || true
	docker rm mimic-container || true

# Cleanup
clean:
	@echo "Cleaning build artifacts..."
	cargo clean

clean-all: clean
	@echo "Deep cleaning (including dependencies cache)..."
	rm -rf target/
	rm -rf Cargo.lock
