PREFIX ?= /usr
DESTDIR ?=
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications

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

.PHONY: all build test check install uninstall clean

all: build

build:
	cargo build --release

test:
	cargo test

check:
	cargo clippy --all-targets

install: build
	install -Dm755 $(BIN) $(DESTDIR)$(BINDIR)/rarc
	install -Dm644 data/rarc.desktop $(DESTDIR)$(APPDIR)/rarc.desktop
	install -Dm755 data/rarc.tap $(DESTDIR)$(PLUGINDIR)/rarc.tap
	@if command -v update-desktop-database >/dev/null 2>&1 && [ -z "$(DESTDIR)" ]; then \
		update-desktop-database -q $(APPDIR) || true; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/rarc
	rm -f $(DESTDIR)$(APPDIR)/rarc.desktop
	rm -f $(DESTDIR)$(PLUGINDIR)/rarc.tap

clean:
	cargo clean
