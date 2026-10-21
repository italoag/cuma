# ghcr.io/italoag/cuma-sandbox-test — what CUMA's live sandbox tests run in.
#
# The test agent (crates/cuma-sandbox/tests/fixtures/acp_agent.sh) needs only
# sh and sed; copied workspaces travel as tar archives (GNU tar, for long
# names); the OpenSandbox stdio tunnel needs node. Bases come from Google's
# mirror of Docker's official images (public.ecr.aws refuses anonymous pulls
# from shared runner addresses once a data limit is reached), pinned.
ARG BASE=mirror.gcr.io/library/node:22-alpine3.22
FROM ${BASE}

RUN apk add --no-cache bash tar
