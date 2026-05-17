# Build libvproc_preload.so and test program
#
# Prerequisites: cargo (Rust), cc (C compiler)
#
# Usage:
#   make              # build everything
#   make test         # build and run tests
#   make clean

CC ?= cc
CFLAGS ?= -Wall -Wextra -O2
RUST_TARGET_DIR ?= ../target/release

.PHONY: all clean test e2e

all: libvproc_preload.so test_preload

# --- Rust runtime ---
$(RUST_TARGET_DIR)/libvproc.so:
	cd .. && cargo build --release

# --- C preload ---
libvproc_preload.so: preload.c
	$(CC) $(CFLAGS) -shared -fPIC -o $@ $< -ldl

# --- Test program ---
test_preload: test_preload.c
	$(CC) $(CFLAGS) -o $@ $<

# --- Test ---
test: libvproc_preload.so $(RUST_TARGET_DIR)/libvproc.so test_preload
	@echo ""
	@echo "=== Test 1: exit(42) should NOT kill the process ==="
	@echo "[test] Running with LD_PRELOAD..."
	@LD_PRELOAD=./libvproc_preload.so VPROC=1 ./test_preload; \
		echo "[test] Process survived! exit status: $$?"
	@echo ""
	@echo "=== Test 2: without VPROC=1, exit() works normally ==="
	@./test_preload; \
		echo "[test] Exit status (should be 42): $$?"

clean:
	rm -f libvproc_preload.so test_preload e2e_preload

# --- E2E test: coroutine exit() interception ---
# Build the example with -rdynamic so vproc_ffi_* symbols are visible to dlsym
e2e: libvproc_preload.so
	cd .. && RUSTFLAGS='-C link-args=-rdynamic' cargo build --example e2e_preload --release
	cp $(RUST_TARGET_DIR)/examples/e2e_preload .
	@echo ""
	LD_PRELOAD=./libvproc_preload.so VPROC=1 ./e2e_preload
