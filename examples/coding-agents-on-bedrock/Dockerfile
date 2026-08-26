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

# A non-root user for the agents to run as. Appended to /etc/passwd and
# /etc/group directly because `useradd` is not in the minimal base and pulling
# shadow-utils in for one line is a bigger image for no more correctness.
#
# The uid is not decoration. Claude Code's --dangerously-skip-permissions
# refuses to run as root, and the refusal is nearly silent: the agent then
# denies its own Bash/Grep/WebFetch calls and returns a confident report over
# zero tool calls. Measured: the same task as uid 0 made no shell calls at
# all; as uid 1000 it made 147. run.sh passes `--user 1000` per exec — the
# daemon itself stays root, demotion is per-command.
RUN echo "agent:x:1000:1000::/workspace:/bin/bash" >> /etc/passwd \
    && echo "agent:x:1000:" >> /etc/group

# Owned by the agent uid, because WORKDIR here is where every exec lands and
# a root-owned one fails a demoted workload at its first write (the trap
# docs/EMBEDDING.md warns about).
RUN mkdir -p /workspace && chown 1000:1000 /workspace
WORKDIR /workspace

ENV AGENTD_PORT=9000
ENV AGENTD_LOG=info
EXPOSE 9000
ENTRYPOINT []
CMD ["/agentd"]
