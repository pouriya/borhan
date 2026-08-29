TARGET ?= $(shell rustc -vV | awk '$$1 == "host:"{print $$2}')
BUILD_DIR=$(CURDIR)/build
VERSION=$(shell cat Cargo.toml | awk 'BEGIN{FS="[ \"]"}$$1 == "version"{print $$4;exit}')
BINARY_NAME := $(if $(findstring windows,$(TARGET)),borhan.exe,borhan)
RELEASE_FILENAME_POSTFIX := $(if $(findstring windows,$(TARGET)),.exe,)
CMD=${BUILD_DIR}/borhan-${VERSION}-${TARGET}${RELEASE_FILENAME_POSTFIX}
DEV_CMD=${BUILD_DIR}/borhan-${VERSION}-${TARGET}-dev${RELEASE_FILENAME_POSTFIX}

# A model directory used to exercise the directory-backed loader. The *default*
# model is not this one -- it lives in src/embedding/ and is committed, because
# include_bytes! bakes it into the binary. Not fetched by `all`: it is a 31 MB
# download that only has to happen once, so it hangs off a file target.
MODEL_NAME=potion-base-8M
MODEL_DIR=$(CURDIR)/models/${MODEL_NAME}
MODEL_FILE=${MODEL_DIR}/model.safetensors

# The one dependency constraint that silently produces nonsense when broken.
ARROW_MAJOR=58

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


all: dev clippy test check-style check-arrow


release: ${BUILD_DIR}
	cargo build --release --target ${TARGET}
	@ cp ./target/${TARGET}/release/$(BINARY_NAME) ${CMD}
	@ ls -sh ${BUILD_DIR}/borhan-*


dev: ${BUILD_DIR}
	cargo build --target ${TARGET}
	@ cp ./target/${TARGET}/debug/$(BINARY_NAME) ${DEV_CMD}
	@ ls -sh ${BUILD_DIR}/borhan-*dev*


start-dev: dev
	${DEV_CMD} --debug serve


# Downloads the model only if it is not already on disk.
model: ${MODEL_FILE}

${MODEL_FILE}:
	./scripts/fetch-model.sh


clippy:
	cargo clippy --all-targets --no-deps -- -D warnings


check-style:
	cargo fmt --check --verbose


fmt:
	cargo fmt


# lancedb pins arrow ^58. A second arrow major in the tree compiles right up
# until two `arrow_schema::Schema` types fail to unify, and the error does not
# mention versions -- so fail loudly here instead.
check-arrow:
	@ versions=`grep -A1 '^name = "arrow"$$' Cargo.lock | grep '^version' | sed 's/version = //;s/"//g' | sort -u`; \
	count=`echo "$$versions" | grep -c .`; \
	if [ "$$count" -ne 1 ]; then \
		echo "FAIL: expected exactly one arrow version in Cargo.lock, found $$count:"; \
		echo "$$versions" | sed 's/^/  /'; \
		echo "  run 'cargo tree -i arrow' to find who pulled the second one"; \
		exit 1; \
	fi; \
	case "$$versions" in ${ARROW_MAJOR}.*) ;; *) \
		echo "FAIL: arrow is $$versions but lancedb pins ^${ARROW_MAJOR}"; \
		exit 1;; \
	esac; \
	echo "arrow $$versions (single version, matches lancedb's ^${ARROW_MAJOR} pin)"


lint: clippy check-style check-arrow


# Fetch the corpus, scan it into ${SEED_HOME}, then search it.
seed: seed-scan seed-test


seed-fetch: ${SEED_DIR}

${SEED_DIR}:
	./scripts/fetch-seed.sh ${SEED_REPO} ${SEED_DIR} ${SEED_PATH}


# Built with `release`, not `dev`: a debug build spends 1.6s of every invocation
# loading the model against 0.17s, which turns a few hundred documents from a
# coffee into an afternoon.
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
		--description "${SEED_REPO} ${SEED_PATH}/, scanned by make seed"
	@ start=`date +%s`; count=0; \
	for file in `find ${SEED_DIR}/${SEED_PATH} -name '*.md' | sort | head -n ${SEED_LIMIT}`; do \
		name=`echo $$file | sed -e 's|^${SEED_DIR}/${SEED_PATH}/||' \
			-e 's|\.md$$||' -e 's|/|-|g'`; \
		BORHAN_HOME=${SEED_HOME} ${CMD} memory add ${SEED_NAME} \
			--session ${SEED_NAME} --message $$name \
			--role assistant --role-name ${SEED_NAME} \
			-- "`cat $$file`" >/dev/null 2>&1 || { \
				echo "FAIL: $$file"; exit 1; }; \
		count=$$((count + 1)); \
		if [ $$((count % 25)) -eq 0 ]; then printf '  %s documents\n' $$count; fi; \
	done; \
	echo "scanned $$count documents in $$((`date +%s` - start))s into ${SEED_HOME}"


# What the scan produced, then a search, then the top hit read back in full.
# Every command here goes through the same BORHAN_HOME the scan wrote to.
seed-test:
	@ BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory list
	@ for query in \
		"how do I borrow a value mutably" \
		"what happens when a trait has an associated type" \
		"the compiler should emit a deprecation warning"; \
	do \
		echo; echo "? $$query"; \
		BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
			--type sentence --limit 5 -- "$$query"; \
	done
	@ echo; echo "? the top hit, read back in full"
	@ top=`BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory search ${SEED_NAME} \
		--type sentence --limit 1 -- "how do I borrow a value mutably" \
		2>/dev/null | awk '{print $$3}'`; \
	BORHAN_HOME=${SEED_HOME} ${CMD} --quiet memory get ${SEED_NAME} --json $$top


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


# Also drops the vendored model and the seed corpus; `make model` and
# `make seed` fetch them again.
purge: dist-clean seed-clean
	@ rm -rf $(CURDIR)/models $(CURDIR)/seed


${BUILD_DIR}:
	@ mkdir -p ${BUILD_DIR}


.PHONY: all release dev start-dev model clippy check-style fmt check-arrow lint test docs open-docs clean dist-clean purge seed seed-fetch seed-scan seed-test seed-clean
