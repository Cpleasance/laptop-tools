# Makefile for laptop-tools

BIN_DIR ?= $(HOME)/.local/bin
SYSTEMD_USER_DIR ?= $(HOME)/.config/systemd/user

.PHONY: all build test check install status check-config stress-test clean

all: build

build:
	cargo build --release

test:
	cargo test --workspace

check:
	cargo check --workspace

install: build
	install -d $(BIN_DIR)
	install -d $(SYSTEMD_USER_DIR)
	install -m755 target/release/powerwatch $(BIN_DIR)/powerwatch
	install -m755 target/release/resumecheck $(BIN_DIR)/resumecheck
	install -m644 powerwatch/powerwatch.service $(SYSTEMD_USER_DIR)/powerwatch.service
	install -m644 resumecheck/resumecheck.service $(SYSTEMD_USER_DIR)/resumecheck.service
	systemctl --user daemon-reload || true
	@echo "Installed binaries to $(BIN_DIR) and service units to $(SYSTEMD_USER_DIR)"

status: build
	@echo "=== powerwatch status ==="
	@target/release/powerwatch status || true
	@echo ""
	@echo "=== resumecheck status ==="
	@target/release/resumecheck status || true

check-config: build
	@target/release/powerwatch check-config
	@target/release/resumecheck check-config

stress-test: build
	@bash tests/stress_test.sh

clean:
	cargo clean
