XDG_CONFIG_HOME ?= $(HOME)/.config
CONFIG_DIR := $(XDG_CONFIG_HOME)/pr-bot
CACHE_DIR := $(HOME)/.cache/pr-bot

.PHONY: install

install:
	mkdir -p $(CONFIG_DIR) $(CACHE_DIR)
	@if [ ! -f $(CONFIG_DIR)/config.toml ]; then \
		cp config.example.toml $(CONFIG_DIR)/config.toml; \
		echo "Installed config.toml to $(CONFIG_DIR)"; \
	else \
		echo "config.toml already exists at $(CONFIG_DIR), skipping"; \
	fi
	cp -r workflows $(CONFIG_DIR)/
	@echo "Done. Edit $(CONFIG_DIR)/config.toml to set bot_username and authorized_user."
