# wasmux
#
# `make` alone builds and tests everything that needs no external toolchain.
# The image targets need clang 20, binaryen and wabt, and only matter when the guest programs
# or the compiled-in backend have to be rebuilt.

CARGO ?= cargo
WASM_TARGET := wasm32-wasip2

.PHONY: all
all: build test ## build and test

.PHONY: build
build: ## build the library and the CLI
	$(CARGO) build --workspace

.PHONY: test
test: ## the whole suite: unit, corpus, isolation, doc tests
	$(CARGO) test --workspace

.PHONY: bench
bench: ## measure, on the interpreter backend
	$(CARGO) run --release -p wasmux-cli --bin wasmux-bench

.PHONY: check
check: ## what CI checks: formatting, lints, docs, tests
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) doc --no-deps -p wasmux
	$(CARGO) test --workspace

.PHONY: wasm
wasm: ## build the CLI as a component, compiled-in backend only
	rustup target add $(WASM_TARGET) 2>/dev/null || true
	$(CARGO) build --release --target $(WASM_TARGET) -p wasmux-cli \
		--no-default-features --features aot

.PHONY: test-wasm
test-wasm: wasm ## run the 60 corpus cases against the compiled-in backend
	@command -v wasmtime >/dev/null || { echo "wasmtime is not installed"; exit 1; }
	wasmtime run -W exceptions=y target/$(WASM_TARGET)/release/wasmux-corpus.wasm

.PHONY: bench-wasm
bench-wasm: wasm ## benchmark the compiled-in backend, under wasmtime
	@command -v wasmtime >/dev/null || { echo "wasmtime is not installed"; exit 1; }
	wasmtime run -W exceptions=y target/$(WASM_TARGET)/release/wasmux-bench.wasm

.PHONY: images
images: ## rebuild bin/*.wasm from source (needs clang 20, binaryen, wabt)
	./toolchain/build-images.sh

.PHONY: archive
archive: ## rebuild bin/libwasmux-images.a, the compiled-in programs (needs wasi-sdk, wabt)
	./toolchain/build-archive.sh

.PHONY: manifest
manifest: ## refresh the checksums in bin/MANIFEST.toml
	@cd bin && sha256sum *.wasm *.commands > SHA256SUMS && echo "wrote bin/SHA256SUMS"

.PHONY: clean
clean:
	$(CARGO) clean

.PHONY: help
help: ## list these targets
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
