TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf

.PHONY: check test build-all clean

check:
	cargo check

test:
	cargo test

build-all:
	@for t in $(TARGETS); do \
		echo "==> $$t"; \
		cargo zigbuild --release --target $$t || exit 1; \
	done
	@for t in $(TARGETS); do \
		file target/$$t/release/pingcheck; \
	done

clean:
	cargo clean
