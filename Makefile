.PHONY: build debug test clean
build:
	cargo build --release -p codecraft-cli
	mkdir -p bin && cp target/release/codecraft bin/codecraft
debug:
	cargo build -p codecraft-cli
	mkdir -p bin && cp target/debug/codecraft bin/codecraft
test:
	cargo test --workspace
clean:
	cargo clean && rm -rf bin
