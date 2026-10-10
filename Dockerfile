# Stage 1: Build the Rust application
FROM rust:slim AS builder

# Set working directory
WORKDIR /build

# Copy all source files
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY static ./static

# Build the application
RUN cargo build --release

# Stage 2: Create minimal runtime image
#
# Debian 13 + manual TLS trust bundle, deliberately package-free beyond the
# base set. Two choices keep the CVE surface as small as bookworm or the
# previous image allow:
# - wget is gone; `docker-compose.yml` healthchecks via bash /dev/tcp instead,
#   which removes wget and everything it drags in (openssl, libpsl, ...)
# - ca-certificates package is skipped (it Depends: openssl); the trust bundle
#   is copied straight from the builder, which keeps /etc/ssl/certs usable for
#   the proxy feature without installing the openssl packages scanners flag
#
# `upgrade` is still required: a build pulls the image's snapshot, so without
# it the runtime would miss Debian point-release security fixes published
# since the snapshot was pushed.
#
# Remaining Trivy/Hub findings have no fix published for the base distro
# (they are unfixed upstream, not missing updates); verify with:
#   trivy image --ignore-unfixed ragilhadi/mimic:<tag>
# An unfixed-only report of 0 is the bar a Debian-based image can reach.
FROM debian:trixie-slim

# Apply all security point-release updates
RUN apt-get update && \
    apt-get upgrade -y \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary and the TLS trust bundle from builder
COPY --from=builder /build/target/release/mimic /usr/local/bin/mimic
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Make binary executable
RUN chmod +x /usr/local/bin/mimic

# Set default environment variables (can be overridden in docker-compose)
ENV PORT=8080
ENV RUST_LOG=info
# Stated rather than inferred: outside Docker the server falls back to ./mocks,
# and the image's contract is the volume mount every `docker run -v
# ./mocks:/app/mocks` command in the README already uses.
ENV MIMIC_MOCKS_DIR=/app/mocks

# Run the application
ENTRYPOINT ["/usr/local/bin/mimic"]
