IMAGE_TAG ?= ecal-mcp:e2e
BUILDER_TAG ?= ecal-mcp:builder

.PHONY: build image builder-image test e2e e2e-realnet shell clean

build:
	cargo build --release --bins

image:
	docker build -t $(IMAGE_TAG) .

# Build just the Dockerfile's `builder` stage so we can run `cargo test`
# against the same Ubuntu + eCAL toolchain CI / e2e use, even on hosts that
# don't have eCAL installed.
builder-image:
	docker build --target builder -t $(BUILDER_TAG) .

# Pure-logic unit tests (parse_min_level / level_priority / similar_names /
# intersect_active_transports). Runs inside the builder image so it works
# regardless of whether eCAL is on the host.
test: builder-image
	docker run --rm -v $(CURDIR):/src -w /src $(BUILDER_TAG) cargo test --bin ecal-mcp

e2e:
	ECAL_MCP_IMAGE=$(IMAGE_TAG) python3 tests/e2e.py

# Cross-container ("realnet") e2e: four containers (ecal-pub, ecal-sub,
# ecal-svc, ecal-mcp) on a user-defined Docker
# bridge with eCAL in network mode + TCP transport. Validates code paths the
# single-container e2e structurally cannot reach (cross-host transport
# selection, real protobuf descriptors via the bundled person sample, and the
# cross-host shm_transport_domain finding in ecal_diagnose_topic).
e2e-realnet: image
	ECAL_MCP_IMAGE=$(IMAGE_TAG) python3 tests/e2e_realnet.py

# Open an interactive shell inside the runtime image (useful for debugging).
shell:
	docker run --rm -it --shm-size=256m --entrypoint /bin/bash $(IMAGE_TAG)

clean:
	cargo clean
	-docker image rm $(IMAGE_TAG)
