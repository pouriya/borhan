TARGET ?= $(shell rustc -vV | awk '$$1 == "host:"{print $$2}')
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
# Each search is concept groups rather than a sentence: words inside one group
# are alternatives that compete for one slot, and separate groups are separate
# things being asked about, which is what coverage scores. A group is one
# argument, so quoting matters only for the `!` that marks one required.
seed-test:
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory list
	@ echo; echo "? borrowing a value mutably"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		borrow,borrowed,borrowing mutable,mutably,mut --limit 5
	@ echo; echo "? associated types on a trait"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		trait,traits associated type,types --limit 5
	@ echo; echo "? a deprecation warning from the compiler"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		'!deprecated,deprecation' warning,warn,lint --limit 5
	@ echo; echo "? what a word looks like in this memory before searching for it"
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory lexicon ${SEED_NAME} \
		borrow Borrowing lifetimes rustc UNRESOLVED_QUESTIONS
	@ echo; echo "? the top hit, and the messages around it"
	@ top=`BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		borrow,borrowing mutable,mutably --limit 1 \
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


.PHONY: all release dev start-dev clippy check-style fmt lint test docs open-docs clean dist-clean purge seed seed-fetch seed-scan seed-test seed-clean
