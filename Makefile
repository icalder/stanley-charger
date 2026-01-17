# --- Configuration ---
BINARY_NAME=stanley-charger
TARGET=armv7-unknown-linux-gnueabihf
REMOTE_HOST=root@einstein
REMOTE_PATH=$(BINARY_NAME)
LOCAL_BIN=target/$(TARGET)/release/$(BINARY_NAME)

.DEFAULT_GOAL := deploy

.PHONY: build patch deploy all

# 1. Build the release binary for ARMv7
build:
	cargo build --target $(TARGET) --release

# 2. Fix the interpreter for the Cerbo GX (Venus OS)
patch: build
	patchelf --set-interpreter /lib/ld-linux-armhf.so.3 $(LOCAL_BIN)

# 3. Copy to the Cerbo
deploy: patch
	scp $(LOCAL_BIN) $(REMOTE_HOST):$(REMOTE_PATH)
	@echo "Successfully deployed to $(REMOTE_HOST):$(REMOTE_PATH)"

# Shortcut for everything
all: deploy