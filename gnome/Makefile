# Install Workboard for GNOME for the current user, as in a toolbox:
# `make -C gnome install`. PREFIX=/usr/local installs it system-wide.
#
# APP_ID names the desktop file, the metainfo and the icons, and the app is
# built with it as its D-Bus name. A build of your own takes its own, such
# as `make -C gnome install APP_ID=org.example.Workboard`; the build refuses
# one that isn't an application ID before anything is installed. The
# metainfo's developer ID is APP_ID without its last element unless
# DEVELOPER_ID says otherwise, and HOMEPAGE is the address the metainfo and
# the About dialog give, this repository's unless set.
SOURCE_ID = io.github.jmarrero_bot.Workboard
SOURCE_DEVELOPER = io.github.jmarrero_bot
SOURCE_HOMEPAGE = https://github.com/jmarrero-bot/work-tracker-poc
APP_ID ?= $(SOURCE_ID)
DEVELOPER_ID ?= $(basename $(APP_ID))
HOMEPAGE ?= $(SOURCE_HOMEPAGE)
PREFIX ?= $(HOME)/.local
# They go into sed expressions and a shell's quotes as they are.
UNSAFE = $(foreach c,| & ' ",$(findstring $(c),$(APP_ID)$(DEVELOPER_ID)$(HOMEPAGE)))
ifneq ($(strip $(UNSAFE))$(filter-out https://%,$(HOMEPAGE))$(if $(HOMEPAGE),,none),)
$(error HOMEPAGE must be one https:// address, and none of APP_ID, DEVELOPER_ID and HOMEPAGE may hold | & ' or ")
endif
# The files in data/ are named and written with SOURCE_ID, SOURCE_DEVELOPER
# and SOURCE_HOMEPAGE.
RENAME = sed -e 's/$(subst .,\.,$(SOURCE_ID))/$(APP_ID)/g' \
	-e 's|<developer id="$(SOURCE_DEVELOPER)">|<developer id="$(DEVELOPER_ID)">|' \
	-e 's|$(SOURCE_HOMEPAGE)|$(HOMEPAGE)|'

.PHONY: build install uninstall
build:
	WORKBOARD_APP_ID='$(APP_ID)' WORKBOARD_HOMEPAGE='$(HOMEPAGE)' cargo build --release --locked -p workboard-gnome
install: build
	install -Dm755 ../target/release/workboard-gnome $(PREFIX)/bin/workboard-gnome
	install -d $(PREFIX)/share/applications $(PREFIX)/share/metainfo
	$(RENAME) data/$(SOURCE_ID).desktop > $(PREFIX)/share/applications/$(APP_ID).desktop
	$(RENAME) data/$(SOURCE_ID).metainfo.xml > $(PREFIX)/share/metainfo/$(APP_ID).metainfo.xml
	chmod 644 $(PREFIX)/share/applications/$(APP_ID).desktop $(PREFIX)/share/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/hicolor/scalable/apps/$(SOURCE_ID).svg $(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/icons/hicolor/symbolic/apps/$(SOURCE_ID)-symbolic.svg $(PREFIX)/share/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
uninstall:
	rm -f $(PREFIX)/bin/workboard-gnome $(PREFIX)/share/applications/$(APP_ID).desktop $(PREFIX)/share/metainfo/$(APP_ID).metainfo.xml
	rm -f $(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg $(PREFIX)/share/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
