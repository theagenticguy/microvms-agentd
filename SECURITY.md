# Security

## Reporting a vulnerability

Report privately through **GitHub Security Advisories** on this repository
("Security" → "Report a vulnerability", or directly at
<https://github.com/theagenticguy/microvms-agentd/security/advisories/new>).
There is no security email address for this project. The advisory form is the
only private reporting channel.

Include the daemon version or commit, the region and API version if AWS behavior
is involved, and a reproduction. There is no funded response SLA. This is an
unpaid reference implementation, so no turnaround time is promised.

Do not open a public issue for a suspected vulnerability. Do open one for
anything in "Not vulnerabilities" below.

## Threat model

[`docs/TRUST.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/TRUST.md)
is the threat model. It describes what the daemon guarantees
when the workload is assumed hostile, and what it does not guarantee. Read it
before filing. `docs/PROTOCOL.md` states the enforced rules, and `model/` checks
the safety properties over every reachable state.

## In scope

- Reaching a bearer-authenticated control route without presenting the installed
  agent token. `/v1/health` and `/v1/schema` are deliberately open, because an
  orchestrator needs an unauthenticated liveness probe during the window before
  bootstrap (see `docs/TRUST.md`). Every other `/v1` route requires the token.
- Replacing or reading the installed agent token from inside the VM after
  bootstrap. Examples include a bootstrap race that lets a second caller win, or
  the token leaking into an exec'd child's environment.
- Escaping the extraction root during a tar upload: a member that writes outside
  the target directory, or a symlink or hard link that redirects a later member
  out of it.
- Crashing the daemon from an unauthenticated request. The daemon is the only
  channel into the VM, so a dead daemon means an unreachable VM with whatever
  work was in it.
- Making an unauthenticated caller allocate. Authorization is decided before any
  request body byte is read. A bypass of that ordering is a denial-of-service
  finding, because the VM baseline can be as small as 512 MiB.
- Anything in `docs/PROTOCOL.md` stated as enforced that is not.

## Not vulnerabilities

**A token holder running arbitrary code as root is the product.** `POST
/v1/exec/start` runs commands as root by design. Everything that follows from
that capability is intended:

- Reading, writing, or deleting any path in the VM via `/v1/exec` or the
  single-file `/v1/fs/file` routes. Those routes are deliberately not confined to
  a root, because a token holder can already reach every byte with one exec call.
  A confinement check there would not restrict anything in practice. "Path
  traversal via `/v1/exec`" is not a finding.
- Exhausting CPU, memory, or disk from an exec'd child. Resource bounds belong to
  the VM configuration.
- Escalating from a demoted exec user back to root. Demotion is a convenience,
  not a security boundary.

**Loopback source addresses.** The platform's own lifecycle hooks arrive from
`127.0.0.1`. At the socket level they are indistinguishable from an in-VM
process (measured; see `docs/PLATFORM.md`). Because of this, a source-address
rule on those routes would reject the platform's legitimate bootstrap and break
every launch. Reports proposing one will be closed with that measurement.

**The unenforced deployment invariant.** The daemon must be the container `CMD`,
and the harness must issue its first exec only after readiness. A base image that
starts its own background process before bootstrap breaks the trust boundary.
The daemon states this requirement but does not enforce it. `model/` runs that
misconfigured setup and reports the counterexample path, so the consequence of
breaking the invariant is a checked fact rather than a guess. Enforcement
belongs to whoever builds the image.

## Supply chain

The repo is source-only. Nothing is published to crates.io, PyPI, or npm, and
`publish = false` in the workspace manifest makes that a machine-enforced fact
rather than a convention. `mise run security` runs five checks with one exit:
semgrep over shipped source, betterleaks over the full git history, the SPDX
header gate over every tracked source file, `cargo deny check` against the
dependency-license policy in `deny.toml` (measured allowlist, yanked crates
denied, unknown registries denied), and actionlint over the workflows. CI runs
the same set, plus SBOM generation and three vulnerability scanners
(grype, trivy, osv-scanner); every accepted finding is recorded with its
reason in `.trivyignore.yaml` or `osv-scanner.toml`. Every workflow action is
SHA-pinned with its tag as a trailing comment, the downloaded scanner binaries
are version-pinned and checksummed, and `.github/dependabot.yml` watches cargo,
github-actions, and npm weekly.

## An open question

The `/run` lifecycle hook is **unauthenticated**. The platform presents no
credential when it calls the hook, so there is nothing for the daemon to verify,
and a source-address rule is ruled out by the measurement above. The only
available defense is that bootstrap succeeds exactly once. The first caller
installs the token, an identical replay returns 200, and a different token
returns 409 without changing state. The endpoint does not forward external
traffic until `/run` returns 200 (documented). That closes the race through the
endpoint, but it says nothing about a process already running inside the VM.

The one-shot property is therefore the only protection on this route. Defeating
it, by winning the race against the platform or by replacing an installed token,
is a real vulnerability, and we want the report. If you have a defense that does
not require a credential the platform does not have, open a public issue. That
proposal is a design discussion rather than a disclosure.
