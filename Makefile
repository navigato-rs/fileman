PREFIX ?= $(HOME)/.local
BINDIR = $(PREFIX)/bin
DATADIR = $(PREFIX)/share
APPDIR = $(DATADIR)/applications
ICONDIR = $(DATADIR)/icons/hicolor/scalable/apps
UNAME_S := $(shell uname -s)
# Launchpad looks in ~/Applications or /Applications, not PREFIX.
ifeq ($(filter $(HOME) $(HOME)/%,$(PREFIX)),)
MACAPPDIR ?= /Applications
else
MACAPPDIR ?= $(HOME)/Applications
endif

.PHONY: build install uninstall

build:
	cargo build --release --locked --bin fileman

install: build
	# GNU install -D creates parent directories; BSD/macOS install does not,
	# and fails with a temp name like INS@xxxxx in the missing directory.
	mkdir -p "$(BINDIR)"
	install -m 755 target/release/fileman "$(BINDIR)/fileman"
ifeq ($(UNAME_S),Darwin)
	sh scripts/macos-app.sh target/release/fileman "$(MACAPPDIR)/FileMan.app"
	@echo "Installed $(BINDIR)/fileman and $(MACAPPDIR)/FileMan.app"
else
	mkdir -p "$(ICONDIR)" "$(APPDIR)"
	install -m 644 etc/fileman.svg "$(ICONDIR)/fileman.svg"
	sed 's|Exec=fileman|Exec=$(BINDIR)/fileman|' etc/fileman.desktop \
		> "$(APPDIR)/fileman.desktop"
	chmod 644 "$(APPDIR)/fileman.desktop"
	-update-desktop-database "$(APPDIR)" >/dev/null 2>&1
	-gtk-update-icon-cache -f -t "$(DATADIR)/icons/hicolor" >/dev/null 2>&1
	@echo "Installed to $(PREFIX). Make sure $(BINDIR) is in your PATH."
endif

uninstall:
	rm -f "$(BINDIR)/fileman"
ifeq ($(UNAME_S),Darwin)
	rm -rf "$(MACAPPDIR)/FileMan.app"
else
	rm -f "$(APPDIR)/fileman.desktop"
	rm -f "$(ICONDIR)/fileman.svg"
endif
