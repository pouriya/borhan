TARGET ?= $(shell rustc -vV 2>/dev/null | awk '$$1 == "host:"{print $$2}')
BUILD_DIR=$(CURDIR)/build
CARGO_TARGET_DIR ?= $(CURDIR)/target
VERSION=$(shell cat Cargo.toml | awk 'BEGIN{FS="[ \"]"}$$1 == "version"{print $$4;exit}')
BINARY_NAME := $(if $(findstring windows,$(TARGET)),borhan.exe,borhan)
RELEASE_FILENAME_POSTFIX := $(if $(findstring windows,$(TARGET)),.exe,)
CMD=${BUILD_DIR}/borhan-${VERSION}-${TARGET}${RELEASE_FILENAME_POSTFIX}
DEV_CMD=${BUILD_DIR}/borhan-${VERSION}-${TARGET}-dev${RELEASE_FILENAME_POSTFIX}

# A corpus to scan into a real storage, so that the splitter, the vector index
# and search get exercised on something bigger than a hand-typed sentence.
# Override the four variables to point at any repository of Markdown files:
#
#     make seed SEED_REPO=https://github.com/rust-lang/reference.git \
#               SEED_PATH=src SEED_NAME=reference
#
SEED_REPO ?= https://github.com/rust-lang/rfcs.git
SEED_PATH ?= text
SEED_NAME ?= rfcs
SEED_DIR=$(CURDIR)/seed/${SEED_NAME}

# Documents to scan. The whole of rust-lang/rfcs is ~3 minutes; the default
# slice is well past the 1024 rows where the vector index gets built, which is
# the interesting threshold. `make seed SEED_LIMIT=99999` for all of it.
SEED_LIMIT ?= 200

# The seed writes into the working tree, never into ~/.borhan: BORHAN_HOME is
# what every borhan command reads to find its storage, so setting it here keeps
# a scan of somebody else's documents out of the developer's own store.
SEED_HOME=$(CURDIR)/home

# Where `make systemd-install` puts things, and what it writes into the served
# user's server.toml. SERVER_PORT is the one knob worth overriding:
#
#     sudo make systemd-install SERVER_PORT=8080
#
SERVICE_FILE=borhan.service
SYSTEMD_DIR ?= /etc/systemd/system
INSTALL_DIR ?= /usr/local/bin
SERVER_PORT ?= 7777
SERVER_LISTEN ?= 127.0.0.1:${SERVER_PORT}

# Same name main.rs appends to a user's home when --home is absent.
BORHAN_HOME_DIRECTORY=.borhan


all: dev clippy test check-style


release: ${BUILD_DIR}
	cargo build --release --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/release/$(BINARY_NAME) ${CMD}
	@ ls -sh ${BUILD_DIR}/borhan-*


dev: ${BUILD_DIR}
	cargo build --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/debug/$(BINARY_NAME) ${DEV_CMD}
	@ ls -sh ${BUILD_DIR}/borhan-*dev*


start-dev: dev
	${DEV_CMD} --home ${SEED_HOME} --debug serve


# Install `borhan serve` as a systemd unit, from ${SERVICE_FILE} in the repo
# root. Run it with sudo: the unit lands in ${SYSTEMD_DIR} and the binary in
# ${INSTALL_DIR}, and both of those are root's.
#
# The service does not run as root. It runs as the user who typed sudo and
# serves that user's own ~/${BORHAN_HOME_DIRECTORY}, which is the point of the
# whole target: the CLI probes the listen address in `<home>/server.toml` and
# talks HTTP when something answers there, so a `borhan memory search` typed in
# that user's shell goes through this unit only if the unit and the shell agree
# on one home directory. Hence the templating -- ${SERVICE_FILE} carries
# @USER@, @GROUP@ and @HOME@, and the sed below fills them from the passwd
# entry of $${SUDO_USER}.
#
# The build is the one step that runs back as that user, through a login shell.
# Two reasons, both about what sudo does to an environment: cargo lives in
# ~/.cargo/bin, which is not on sudo's secure_path, and a `cargo build` run as
# root leaves a root-owned target/ that the next ordinary `make dev` cannot
# write.
# The path `release` writes, printed in whatever environment is asking. Used by
# `systemd-install`, which cannot work it out on its own: ${CMD} is named after
# ${TARGET}, ${TARGET} comes from `rustc -vV`, and rustc lives in ~/.cargo/bin,
# which is not on sudo's PATH. So the root-side make asks the user's login
# shell -- the same one that just ran the build -- what the file is called.
print-cmd:
	@ echo ${CMD}


systemd-install:
	@ test -d ${SYSTEMD_DIR} || { \
		echo "No ${SYSTEMD_DIR}: nothing here runs systemd, so there is no unit to install."; \
		exit 1; }
	@ command -v systemctl >/dev/null 2>&1 || { \
		echo "No systemctl on PATH: nothing here runs systemd."; exit 1; }
	@ test "`id -u`" = "0" || { \
		echo "systemd-install writes ${SYSTEMD_DIR} and ${INSTALL_DIR}."; \
		echo "Run: sudo $(MAKE) systemd-install"; exit 1; }
	@ test -n "$${SUDO_USER}" || { \
		echo "Run this with sudo from your own login rather than as root: the unit runs"; \
		echo "as $${SUDO_USER} and serves that user's ~/${BORHAN_HOME_DIRECTORY}, and there is no such user here."; \
		exit 1; }
	@ set -e; \
	user="$${SUDO_USER}"; \
	group=`id -gn "$$user"`; \
	home=`getent passwd "$$user" | cut -d: -f6`; \
	test -n "$$home" || { echo "No home directory in the passwd entry of $$user."; exit 1; }; \
	borhan_home="$$home/${BORHAN_HOME_DIRECTORY}"; \
	echo "==> building as $$user"; \
	sudo -u "$$user" -H sh -lc "cd ${CURDIR} && exec $(MAKE) --no-print-directory release"; \
	cmd=`sudo -u "$$user" -H sh -lc "cd ${CURDIR} && exec $(MAKE) --no-print-directory print-cmd"`; \
	test -x "$$cmd" || { echo "make release left no binary at \"$$cmd\"."; exit 1; }; \
	echo "==> installing $$cmd at ${INSTALL_DIR}/borhan"; \
	systemctl stop ${SERVICE_FILE} >/dev/null 2>&1 || true; \
	install -d -m 0755 ${INSTALL_DIR}; \
	install -m 0755 "$$cmd" ${INSTALL_DIR}/borhan; \
	sudo -u "$$user" mkdir -p "$$borhan_home"; \
	if [ -f "$$borhan_home/server.toml" ]; then \
		echo "==> keeping $$borhan_home/server.toml, which already says where to listen"; \
	else \
		echo "==> writing $$borhan_home/server.toml listening at ${SERVER_LISTEN}"; \
		sudo -u "$$user" ${INSTALL_DIR}/borhan --home "$$borhan_home" \
			init server --listen ${SERVER_LISTEN}; \
	fi; \
	echo "==> writing ${SYSTEMD_DIR}/${SERVICE_FILE} for $$user, home $$borhan_home"; \
	sed -e "s|@USER@|$$user|g" -e "s|@GROUP@|$$group|g" -e "s|@HOME@|$$borhan_home|g" \
		${CURDIR}/${SERVICE_FILE} > ${SYSTEMD_DIR}/${SERVICE_FILE}; \
	chmod 0644 ${SYSTEMD_DIR}/${SERVICE_FILE}; \
	echo "==> systemctl daemon-reload"; \
	systemctl daemon-reload; \
	echo "==> systemctl enable --now ${SERVICE_FILE}"; \
	systemctl enable --now ${SERVICE_FILE}; \
	systemctl --no-pager --full status ${SERVICE_FILE} || true


clippy:
	cargo clippy --all-targets --no-deps -- -D warnings


check-style:
	cargo fmt --check --verbose


fmt:
	cargo fmt


lint: clippy check-style


# Fetch the corpus, scan it into ${SEED_HOME}, then search it.
seed: seed-scan seed-test


seed-fetch: ${SEED_DIR}

${SEED_DIR}:
	./scripts/fetch-seed.sh ${SEED_REPO} ${SEED_DIR} ${SEED_PATH}


# Built with `release`, not `dev`: the debug build is several times slower at
# tokenizing and indexing, which turns a few hundred documents from a coffee
# into an afternoon.
#
# One document is one message, named by its path under ${SEED_PATH} with the
# slashes turned into dashes -- so `--message` stays unique, which `memory add`
# requires, and the whole corpus reads as one long conversation the cursor can
# walk. The path and not the filename because a corpus laid out in directories
# has an `index.md` in every one of them, and every one would collide.
#
# `find` and not a glob for the same reason: rust-lang/rfcs is one flat
# directory, and almost nothing else is.
#
# `--` before the text because the text is a positional argument and a Markdown
# file that opens with a list item starts with `- `, which clap would otherwise
# read as a flag.
seed-scan: release seed-fetch
	@ rm -rf ${SEED_HOME}
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet init storage
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory create ${SEED_NAME} \
		--languages en \
		--description "Rust RFCs from ${SEED_REPO} under ${SEED_PATH}/, scanned by make seed as a local memory corpus"
	@ start=`date +%s`; count=0; \
	for file in `find ${SEED_DIR}/${SEED_PATH} -name '*.md' | sort | head -n ${SEED_LIMIT}`; do \
		name=`echo $$file | sed -e 's|^${SEED_DIR}/${SEED_PATH}/||' \
			-e 's|\.md$$||' -e 's|/|-|g'`; \
		BORHAN_HOME=${SEED_HOME} ${CMD} memory add ${SEED_NAME} \
			--session ${SEED_NAME} --message $$name \
			--role assistant --author ${SEED_NAME} \
			-- "`cat $$file`" >/dev/null 2>&1 || { \
				echo "FAIL: $$file"; exit 1; }; \
		count=$$((count + 1)); \
		if [ $$((count % 25)) -eq 0 ]; then printf '  %s documents\n' $$count; fi; \
	done; \
	echo "scanned $$count documents in $$((`date +%s` - start))s into ${SEED_HOME}"


# What the scan produced, then three searches, then the top hit read back with
# the messages around it. Every command goes through the same BORHAN_HOME the
# scan wrote to.
#
# Each search is one query of ideas rather than a sentence: words inside one
# pair of parentheses are alternatives that compete for one slot, and separate
# parts are separate things being asked about, which is what coverage scores.
# The query is one argument, so it is always quoted — the shell would otherwise
# take the parentheses.
seed-test:
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory list
	@ echo; echo "? borrowing a value mutably"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		'(borrow borrowed borrowing) (mutable mutably mut)' --limit 5
	@ echo; echo "? associated types on a trait"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		'(trait traits) associated (type types)' --limit 5
	@ echo; echo "? a deprecation warning from the compiler"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		'+(deprecated deprecation) (warning warn lint)' --limit 5
	@ echo; echo "? what a word looks like in this memory before searching for it"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory lexicon ${SEED_NAME} \
		borrow Borrowing lifetimes rustc UNRESOLVED_QUESTIONS
	@ echo; echo "? the top hit, and the messages around it"
	@ top=`BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		'(borrow borrowing) (mutable mutably)' --limit 1 \
		2>/dev/null | head -n 1 | awk '{print $$3}'`; \
	BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory cursor ${SEED_NAME} $$top \
		--before 0 --after 0 | head -n 20


# Drops the scanned storage but keeps the fetched corpus, so a re-scan does not
# re-clone. `make purge` removes both.
seed-clean:
	@ rm -rf ${SEED_HOME}


test:
	cargo test --target ${TARGET}


docs:
	cargo doc --no-deps


open-docs:
	cargo doc --no-deps --open


clean:
	@ cargo clean


dist-clean: clean
	@ rm -rf ${BUILD_DIR}


# Also drops the seed corpus; `make seed` fetches it again.
purge: dist-clean seed-clean
	@ rm -rf $(CURDIR)/seed


${BUILD_DIR}:
	@ mkdir -p ${BUILD_DIR}


.PHONY: all release dev start-dev print-cmd systemd-install clippy check-style fmt lint test docs open-docs clean dist-clean purge seed seed-fetch seed-scan seed-test seed-clean
