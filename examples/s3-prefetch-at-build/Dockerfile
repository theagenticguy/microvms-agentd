# A MicroVM image that prefetches S3 content while the image builds, so every
# VM launched from it starts with the data already on disk — see this
# example's README for the mechanism and its measured grounding.
#
# The FROM must stay `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal`:
# microvms-core pairs that ref with the managed `al2023-1` base and refuses a
# Dockerfile whose FROM disagrees with the create call's baseImageArn. A
# `@sha256:` digest suffix on that same ref is accepted, and this file pins
# the same digest as examples/coding-agents-on-bedrock (see that Dockerfile
# for the full reasoning; bump both together when the tag moves).
FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal@sha256:c439fb4994ea7ca529233d6256446d3f8b7b4efb58956073e015303a170011de

COPY agentd /agentd
RUN chmod 0755 /agentd

# AWS CLI v2 runs the prefetch; AL2023 packages it as awscli-2. Unpinned
# deliberately, matching coding-agents-on-bedrock: the base image digest pin
# above is what makes the build reproducible.
# hadolint ignore=DL3041
RUN dnf install -y awscli-2 \
    && dnf clean all

# run.sh rewrites these two lines with your values before building. They are
# ENV rather than ARG because they are read at app start (in the snapshot
# VM), not during the docker build — and `microvm build` passes no
# --build-arg either way. Baking them in is what keys the image to its
# content: a different URI is a different Dockerfile hash, a different image
# name, and a fresh build.
ENV PREFETCH_URI=""
ENV PREFETCH_NO_SIGN="0"

# The start wrapper is materialized here with printf because it cannot be
# COPY'd: the build context `microvm build` uploads carries exactly two
# members — this Dockerfile and the agentd binary
# (microvms-core/src/control/artifact.rs) — so there is no file beside them
# to copy. Issue #74 tracks widening that context; when it lands, this
# becomes a COPY of a script shellcheck can see.
#
# The wrapper prefetches, then hands the process to agentd. It is
# deliberately fail-loud: `set -e` means a failed sync stops the script,
# agentd never starts, the build's ready hook times out, and the build FAILS
# with a named reason instead of shipping a snapshot that silently lacks the
# data. An empty PREFETCH_URI skips with a log line, so this file also
# builds as checked in.
# hadolint ignore=SC2016
RUN printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'if [ -n "${PREFETCH_URI:-}" ]; then' \
    '  echo "prefetch: ${PREFETCH_URI} -> /opt/prefetch"' \
    '  args=(s3 sync --no-progress "${PREFETCH_URI}" /opt/prefetch)' \
    '  if [ "${PREFETCH_NO_SIGN:-0}" = "1" ]; then args+=(--no-sign-request); fi' \
    '  aws "${args[@]}"' \
    '  chown -R 1000:1000 /opt/prefetch' \
    '  du -sh /opt/prefetch' \
    'else' \
    '  echo "prefetch: PREFETCH_URI is empty, skipping"' \
    'fi' \
    'exec /agentd' \
    > /start.sh && chmod 0755 /start.sh

# A non-root user, same pattern and same reasoning as
# examples/coding-agents-on-bedrock: appended to /etc/passwd directly because
# `useradd` is not in the minimal base. The prefetched tree is chown'd to it
# so demoted workloads can read what was fetched.
RUN echo "agent:x:1000:1000::/workspace:/bin/bash" >> /etc/passwd \
    && echo "agent:x:1000:" >> /etc/group

RUN mkdir -p /workspace && chown 1000:1000 /workspace
WORKDIR /workspace

ENV AGENTD_PORT=9000
ENV AGENTD_LOG=info
EXPOSE 9000
ENTRYPOINT []
CMD ["/start.sh"]
