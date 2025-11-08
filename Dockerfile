# Stage 1: Build the Rust application
FROM rust:slim AS builder

# Set working directory
WORKDIR /build

# Copy all source files
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build the application
RUN cargo build --release

# Stage 2: Create minimal runtime image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from builder
COPY --from=builder /build/target/release/mimic /usr/local/bin/mimic

# Make binary executable
RUN chmod +x /usr/local/bin/mimic

# Set default environment variables (can be overridden in docker-compose)
ENV PORT=8080
ENV RUST_LOG=info

# Run the application
ENTRYPOINT ["/usr/local/bin/mimic"]
