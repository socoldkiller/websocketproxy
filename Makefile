SHELL := /usr/bin/env bash

CARGO ?= cargo
PACKAGE_NAME ?= websockproxy-relay
DIST_DIR ?= dist
TARGET_TRIPLE ?= $(shell rustc -vV | awk '/^host: / { print $$2 }')
RELEASE_DIR ?= target/$(TARGET_TRIPLE)/release
RELEASE_BINARY := $(RELEASE_DIR)/$(PACKAGE_NAME)
RELEASE_ARTIFACT := $(DIST_DIR)/$(PACKAGE_NAME)
TARGET_TRIPLE_ENV := $(subst -,_,$(TARGET_TRIPLE))
OPENRC_DIR ?= openrc
OPENRC_SERVICE_NAME ?= websockproxy-relay
OPENRC_BIN_DIR ?= /usr/local/bin
OPENRC_INITD_DIR ?= /etc/init.d
OPENRC_CONFD_DIR ?= /etc/conf.d

ifeq ($(TARGET_TRIPLE),x86_64-unknown-linux-musl)
RELEASE_CC_ENV := CC_$(TARGET_TRIPLE_ENV)=clang CFLAGS_$(TARGET_TRIPLE_ENV)=--target=$(TARGET_TRIPLE)
endif

.PHONY: help fmt test build build-release release install-openrc clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  make build          Build debug binary' \
		'  make build-release  Build optimized binary' \
		'  make test           Run tests' \
		'  make fmt            Check formatting' \
		'  make release        Build and copy release binary into dist/' \
		'  make install-openrc Install binary and OpenRC service files' \
		'  make clean          Remove build artifacts'

fmt:
	$(CARGO) fmt --check

test:
	$(CARGO) test --locked

build:
	$(CARGO) build --locked

build-release:
	$(RELEASE_CC_ENV) $(CARGO) build --release --locked --target $(TARGET_TRIPLE)

release: build-release
	rm -rf $(DIST_DIR)
	mkdir -p $(DIST_DIR)
	install -m 755 $(RELEASE_BINARY) $(RELEASE_ARTIFACT)
	printf 'release binary: %s\n' $(RELEASE_ARTIFACT)

install-openrc: release
	install -Dm 755 $(RELEASE_ARTIFACT) $(DESTDIR)$(OPENRC_BIN_DIR)/$(OPENRC_SERVICE_NAME)
	install -Dm 755 $(OPENRC_DIR)/$(OPENRC_SERVICE_NAME).initd $(DESTDIR)$(OPENRC_INITD_DIR)/$(OPENRC_SERVICE_NAME)
	install -Dm 644 $(OPENRC_DIR)/$(OPENRC_SERVICE_NAME).confd $(DESTDIR)$(OPENRC_CONFD_DIR)/$(OPENRC_SERVICE_NAME)

clean:
	$(CARGO) clean
	rm -rf $(DIST_DIR)
