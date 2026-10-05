# ghcr.io/italoag/cuma-agent-node — a starting image for coding agents in a
# container or microVM sandbox (docker, microsandbox, arcbox, kubernetes,
# opensandbox): Node.js for npx-distributed ACP agents, git, and the tools
# agents expect. Agents themselves are not baked in — `npx -y <agent>` fetches
# the version the configuration names — and neither is any credential: those
# are forwarded by name at launch.
ARG BASE=public.ecr.aws/docker/library/node:22-bookworm-slim
FROM ${BASE}

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash ca-certificates curl git less openssh-client procps python3 ripgrep tar unzip \
 && rm -rf /var/lib/apt/lists/*

ENV NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false
