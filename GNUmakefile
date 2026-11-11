# Nuke built-in rules.
.SUFFIXES:

# This Makefile is a thin convenience wrapper -- scripts/*.sh are the
# actual source of truth for how each step works, and can be run directly.

.PHONY: all
all:
	./scripts/build-iso.sh

.PHONY: run
run: all
	./scripts/run-qemu.sh

.PHONY: docker
docker:
	./scripts/docker-shell.sh

.PHONY: format
format:
	./scripts/format.sh

.PHONY: format-check
format-check:
	./scripts/format.sh --check

.PHONY: clean
clean:
	$(MAKE) -C kernel clean
	rm -rf iso_root Anshi.iso

.PHONY: distclean
distclean: clean
