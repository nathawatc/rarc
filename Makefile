PREFIX ?= /usr
DESTDIR ?=
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications
LOCALEDIR ?= $(PREFIX)/share/locale
LINGUAS := $(shell grep -v '^\#' po/LINGUAS 2>/dev/null)

ifeq ($(wildcard /usr/lib/xfce4/thunar-archive-plugin/.),)
  ifneq ($(wildcard /usr/libexec/thunar-archive-plugin/.),)
    PLUGINDIR ?= $(PREFIX)/libexec/thunar-archive-plugin
  else
    PLUGINDIR ?= $(PREFIX)/lib/xfce4/thunar-archive-plugin
  endif
else
  PLUGINDIR ?= $(PREFIX)/lib/xfce4/thunar-archive-plugin
endif

BIN = target/release/rarc

.PHONY: all build locales pot test check install uninstall clean

all: build

build: locales
	RARC_LOCALEDIR=$(LOCALEDIR) cargo build --release

locales:
	@for lang in $(LINGUAS); do \
		mkdir -p target/locale/$$lang/LC_MESSAGES; \
		msgfmt -o target/locale/$$lang/LC_MESSAGES/rarc.mo po/$$lang.po; \
	done

pot:
	xgettext --language=Rust --from-code=UTF-8 --keyword=tr:1 --keyword=trn:1,2 \
		--files-from=po/POTFILES --package-name=rarc --package-version=0.1.0 -o po/rarc.pot

test:
	cargo test

check:
	cargo clippy --all-targets

install: build
	install -Dm755 $(BIN) $(DESTDIR)$(BINDIR)/rarc
	install -Dm644 data/rarc.desktop $(DESTDIR)$(APPDIR)/rarc.desktop
	install -Dm755 data/rarc.tap $(DESTDIR)$(PLUGINDIR)/rarc.tap
	@for lang in $(LINGUAS); do \
		install -Dm644 target/locale/$$lang/LC_MESSAGES/rarc.mo \
			$(DESTDIR)$(LOCALEDIR)/$$lang/LC_MESSAGES/rarc.mo; \
	done
	@if command -v update-desktop-database >/dev/null 2>&1 && [ -z "$(DESTDIR)" ]; then \
		update-desktop-database -q $(APPDIR) || true; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/rarc
	rm -f $(DESTDIR)$(APPDIR)/rarc.desktop
	rm -f $(DESTDIR)$(PLUGINDIR)/rarc.tap
	@for lang in $(LINGUAS); do \
		rm -f $(DESTDIR)$(LOCALEDIR)/$$lang/LC_MESSAGES/rarc.mo; \
	done

clean:
	cargo clean
