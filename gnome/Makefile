# Install Workboard for GNOME for the current user, as in a toolbox:
# `make -C gnome install`. PREFIX=/usr/local installs it system-wide.
APP_ID = io.github.jmarrero_bot.Workboard
PREFIX ?= $(HOME)/.local

.PHONY: build install uninstall
build:
	cargo build --release --locked -p workboard-gnome
install: build
	install -Dm755 ../target/release/workboard-gnome $(PREFIX)/bin/workboard-gnome
	install -Dm644 data/$(APP_ID).desktop $(PREFIX)/share/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(PREFIX)/share/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/hicolor/scalable/apps/$(APP_ID).svg $(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg $(PREFIX)/share/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
uninstall:
	rm -f $(PREFIX)/bin/workboard-gnome $(PREFIX)/share/applications/$(APP_ID).desktop $(PREFIX)/share/metainfo/$(APP_ID).metainfo.xml
	rm -f $(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg $(PREFIX)/share/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
