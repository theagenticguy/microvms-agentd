# A MicroVM image that carries Claude Code and Codex CLI alongside agentd.
#
# The FROM must stay `public.ecr.aws/amazonlinux/amazonlinux:2023-minimal`:
# microvms-core pairs that ref with the managed `al2023-1` base and refuses a
# Dockerfile whose FROM disagrees with the create call's baseImageArn.
#
# The agentd COPY/CMD lines mirror the client's default Dockerfile
# (microvms-core/src/control/artifact.rs::default_dockerfile) — when you pass
# --dockerfile, you are replacing that default, so you carry its daemon lines
# yourself.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal

COPY agentd /agentd
RUN chmod 0755 /agentd

# Node 22 runs both CLIs; python3 and the rest are what a coding agent
# expects of a working shell.
RUN dnf install -y nodejs22 npm python3 git tar gzip which findutils procps-ng \
    && dnf clean all
RUN npm install -g @anthropic-ai/claude-code @openai/codex \
    && npm cache clean --force

RUN mkdir -p /workspace
WORKDIR /workspace

ENV AGENTD_PORT=9000
ENV AGENTD_LOG=info
EXPOSE 9000
ENTRYPOINT []
CMD ["/agentd"]
