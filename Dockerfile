# 공식 multi-platform manifest를 고정해 같은 입력의 베이스를 보존한다.
FROM rust:1.96.0-slim-bookworm@sha256:4732ca96fd086cb9be682050c3f0176288eebaac2b80aa2bcefccfaf198e1950 AS build
WORKDIR /src
COPY README.md LICENSE-MIT LICENSE-APACHE ./
COPY engine ./engine
RUN cargo build --manifest-path engine/Cargo.toml --locked --release -p schemagraph-cli

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/engine/target/release/schemagraph /usr/local/bin/schemagraph
COPY LICENSE-MIT LICENSE-APACHE /usr/share/licenses/schemagraph/
WORKDIR /work
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/schemagraph"]
CMD ["--help"]
