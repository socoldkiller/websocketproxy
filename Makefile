SHELL := /usr/bin/env bash

CARGO ?= cargo
PACKAGE_NAME ?= websockproxy-relay
DIST_DIR ?= dist
TARGET_TRIPLE ?= $(shell rustc -vV | awk '/^host: / { print $$2 }')
RELEASE_DIR ?= target/$(TARGET_TRIPLE)/release
RELEASE_BINARY := $(RELEASE_DIR)/$(PACKAGE_NAME)
RELEASE_ARTIFACT := $(DIST_DIR)/$(PACKAGE_NAME)

.PHONY: help fmt test build build-release release clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  make build          Build debug binary' \
		'  make build-release  Build optimized binary' \
		'  make test           Run tests' \
		'  make fmt            Check formatting' \
		'  make release        Build and copy release binary into dist/' \
		'  make clean          Remove build artifacts'

fmt:
	$(CARGO) fmt --check

test:
	$(CARGO) test --locked

build:
	$(CARGO) build --locked

build-release:
	$(CARGO) build --release --locked --target $(TARGET_TRIPLE)

release: build-release
	rm -rf $(DIST_DIR)
	mkdir -p $(DIST_DIR)
	install -m 755 $(RELEASE_BINARY) $(RELEASE_ARTIFACT)
	printf 'release binary: %s\n' $(RELEASE_ARTIFACT)

clean:
	$(CARGO) clean
	rm -rf $(DIST_DIR)
