# Simple installer for LinuxSCP. For distro packaging you can override
# PREFIX and DESTDIR as usual.
PREFIX ?= /usr/local
DESTDIR ?=
BINDIR = $(DESTDIR)$(PREFIX)/bin
DATADIR = $(DESTDIR)$(PREFIX)/share
APP_ID = io.github.theflyingjay.LinuxSCP

.PHONY: all build release install uninstall test deb clean

all: release

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

install: release
	install -Dm755 target/release/linuxscp $(BINDIR)/linuxscp
	install -Dm755 target/release/linuxscp-askpass $(BINDIR)/linuxscp-askpass
	install -Dm644 data/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/hicolor/256x256/apps/$(APP_ID).png \
		$(DATADIR)/icons/hicolor/256x256/apps/$(APP_ID).png
	install -Dm644 data/sounds/success.mp3 $(DATADIR)/linuxscp/sounds/success.mp3
	@echo "Installed LinuxSCP to $(PREFIX)"

deb:
	./scripts/build-deb.sh

uninstall:
	rm -f $(BINDIR)/linuxscp $(BINDIR)/linuxscp-askpass
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	rm -f $(DATADIR)/icons/hicolor/256x256/apps/$(APP_ID).png
	rm -f $(DATADIR)/linuxscp/sounds/success.mp3

clean:
	cargo clean
