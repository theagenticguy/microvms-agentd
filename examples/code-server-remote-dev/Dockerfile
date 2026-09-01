# A MicroVM image that carries code-server alongside agentd, for browser-based
# remote dev over `microvm port-forward`.
#
# The FROM must stay `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal`:
# microvms-core pairs that ref with the managed `al2023-1` base and refuses a
# Dockerfile whose FROM disagrees with the create call's baseImageArn. A
# `@sha256:` digest suffix on that same ref is accepted, and this file pins the
# same digest as examples/coding-agents-on-bedrock (see that Dockerfile for the
# full reasoning; bump both together when the tag moves).
#
# The agentd COPY/CMD lines mirror the client's default Dockerfile
# (microvms-core/src/control/artifact.rs::default_dockerfile) — when you pass
# --dockerfile, you are replacing that default, so you carry its daemon lines
# yourself.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal@sha256:c439fb4994ea7ca529233d6256446d3f8b7b4efb58956073e015303a170011de

COPY agentd /agentd
RUN chmod 0755 /agentd

# What a dev box needs beside the IDE. code-server bundles its own Node, so
# nothing heavier is required; add your project's toolchain here. Unpinned
# deliberately, matching coding-agents-on-bedrock: these are base utilities
# tracking the AL2023 repos, and the base image digest pin above is what
# makes the build reproducible.
# hadolint ignore=DL3041
RUN dnf install -y git tar gzip which findutils procps-ng \
    && dnf clean all

# code-server, from the release RPM (self-contained; dnf resolves its
# dependencies). `microvm build` passes no --build-arg, so bump the default
# here to change versions — the image name is keyed to this file's content
# hash, so an edited version builds fresh under a new name.
ARG CODE_SERVER_VERSION=4.135.0
RUN dnf install -y \
    "https://github.com/coder/code-server/releases/download/v${CODE_SERVER_VERSION}/code-server-${CODE_SERVER_VERSION}-arm64.rpm" \
    && dnf clean all

# A non-root user for the IDE to run as, same pattern and same reasoning as
# examples/coding-agents-on-bedrock: appended to /etc/passwd directly because
# `useradd` is not in the minimal base. run.sh passes `--user 1000` on the
# code-server exec; the daemon itself stays root, demotion is per-command.
RUN echo "dev:x:1000:1000::/workspace:/bin/bash" >> /etc/passwd \
    && echo "dev:x:1000:" >> /etc/group

# Owned by the dev uid: WORKDIR is where every exec lands, code-server serves
# it as the project folder, and HOME points here so settings and extensions
# land somewhere uid 1000 can write.
RUN mkdir -p /workspace && chown 1000:1000 /workspace
WORKDIR /workspace

ENV AGENTD_PORT=9000
ENV AGENTD_LOG=info
EXPOSE 9000
# Documentation only: reachability is governed by the endpoint proxy's
# port-scoped auth tokens (docs/PLATFORM.md, "Endpoint authentication"),
# which `microvm port-forward` mints per hop — not by EXPOSE.
EXPOSE 8080
ENTRYPOINT []
CMD ["/agentd"]
