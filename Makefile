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
	install -m755 target/release/thresholdctl $(BIN_DIR)/thresholdctl
	install -m755 target/release/thermalwatch $(BIN_DIR)/thermalwatch
	install -m755 target/release/dockwatch $(BIN_DIR)/dockwatch
	install -m755 target/release/audiopop-fix $(BIN_DIR)/audiopop-fix
	install -m644 powerwatch/powerwatch.service $(SYSTEMD_USER_DIR)/powerwatch.service
	install -m644 resumecheck/resumecheck.service $(SYSTEMD_USER_DIR)/resumecheck.service
	systemctl --user daemon-reload || true
	@echo "Installed binaries to $(BIN_DIR) and service units to $(SYSTEMD_USER_DIR) - all 6 tools active"

status: build
	@echo "=== powerwatch status ==="
	@target/release/powerwatch status || true
	@echo ""
	@echo "=== resumecheck status ==="
	@target/release/resumecheck status || true
	@echo ""
	@echo "=== thresholdctl status ==="
	@target/release/thresholdctl status || true
	@echo ""
	@echo "=== thermalwatch status ==="
	@target/release/thermalwatch status || true
	@echo ""
	@echo "=== dockwatch status ==="
	@target/release/dockwatch status || true
	@echo ""
	@echo "=== audiopop-fix status ==="
	@target/release/audiopop-fix status || true

check-config: build
	@target/release/powerwatch check-config
	@target/release/resumecheck check-config

stress-test: build
	@bash tests/stress_test.sh

clean:
	cargo clean
