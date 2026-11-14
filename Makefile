# Simple installer for LinuxSCP. For distro packaging you can override
# PREFIX and DESTDIR as usual.
PREFIX ?= /usr/local
DESTDIR ?=
BINDIR = $(DESTDIR)$(PREFIX)/bin
DATADIR = $(DESTDIR)$(PREFIX)/share
LOCALEDIR = $(DATADIR)/locale
APP_ID = io.github.theflyingjay.LinuxSCP
# Translation domain; must match `i18n::DOMAIN` in the sources.
DOMAIN = linuxscp
LANGS = $(patsubst po/%.po,%,$(wildcard po/*.po))

.PHONY: all build release install uninstall test deb flatpak-sources clean mo pot check-po

all: release

build:
	cargo build

release:
	cargo build --release

# Compile the .po catalogs to .mo (needs gettext's msgfmt).
mo: $(patsubst po/%.po,po/%.mo,$(wildcard po/*.po))

po/%.mo: po/%.po
	msgfmt --check -o $@ $<

# Refresh the template after adding or changing tr!/ngettext/plural text.
pot:
	./scripts/i18n-extract.py

# Translations must keep their message id's {} placeholders; msgfmt cannot
# see those.
check-po:
	./scripts/check-po.py

test:
	cargo test

install: release mo
	install -Dm755 target/release/linuxscp $(BINDIR)/linuxscp
	install -Dm755 target/release/linuxscp-askpass $(BINDIR)/linuxscp-askpass
	install -Dm644 data/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).metainfo.xml $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	install -Dm644 data/icons/hicolor/256x256/apps/$(APP_ID).png \
		$(DATADIR)/icons/hicolor/256x256/apps/$(APP_ID).png
	install -Dm644 data/sounds/success.mp3 $(DATADIR)/linuxscp/sounds/success.mp3
	for lang in $(LANGS); do \
		install -Dm644 po/$$lang.mo $(LOCALEDIR)/$$lang/LC_MESSAGES/$(DOMAIN).mo; \
	done
	@echo "Installed LinuxSCP to $(PREFIX)"

deb:
	./scripts/build-deb.sh

# The Flatpak builds offline from the crates pinned in this file; rerun after
# any dependency change (CI fails if it drifts from Cargo.lock).
flatpak-sources:
	./scripts/flatpak-cargo-sources.py

uninstall:
	rm -f $(BINDIR)/linuxscp $(BINDIR)/linuxscp-askpass
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	rm -f $(DATADIR)/icons/hicolor/256x256/apps/$(APP_ID).png
	rm -f $(DATADIR)/linuxscp/sounds/success.mp3

clean:
	cargo clean
