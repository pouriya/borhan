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
	${DEV_CMD} --debug


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


# Also drops the vendored model; `make model` re-downloads it.
purge: dist-clean
	@ rm -rf $(CURDIR)/models


${BUILD_DIR}:
	@ mkdir -p ${BUILD_DIR}


.PHONY: all release dev start-dev model clippy check-style fmt check-arrow lint test docs open-docs clean dist-clean purge
