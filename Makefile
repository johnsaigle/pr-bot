.PHONY: install

XDG_CONFIG_HOME ?= $(HOME)/.config
CONFIG_DIR := $(XDG_CONFIG_HOME)/pr-bot

install:
	mkdir -p $(CONFIG_DIR)/workflows
	if [ ! -f $(CONFIG_DIR)/config.toml ]; then cp config.example.toml $(CONFIG_DIR)/config.toml; fi
	cp -r workflows/*.md $(CONFIG_DIR)/workflows/
