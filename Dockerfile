# Stage 1: Build the Rust application
FROM rust:1.83 AS builder

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

# Expose port
EXPOSE 8033

# Set environment variables
ENV MOCKS_DIR=/app/mocks
ENV PORT=8033
ENV RUST_LOG=info

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD wget -q --spider http://localhost:8033/health || exit 1

# Run the application
ENTRYPOINT ["/usr/local/bin/mimic"]
