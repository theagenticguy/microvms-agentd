# VEX: the versioned vulnerability-override surface

`microvms-agentd.openvex.json` is an [OpenVEX](https://github.com/openvex/spec)
document, and it is the one place a **dependency vulnerability** finding gets
accepted or refuted. It is versioned, reviewed in PRs like any other change, and
consumed by two of the three scanners in CI's `sbom` job:

- **grype** reads it via `--vex`. Passing a document makes grype treat
  `not_affected` and `fixed` statements as ignore rules automatically.
- **trivy** reads it via the `TRIVY_VEX` environment variable on the action
  step (trivy's local-VEX-file support is marked experimental; if an upgrade
  changes the interface, the CI validation step still gates the document's
  shape and this README is the pointer to re-wire).
- **osv-scanner** does not consume VEX documents; its overrides stay in
  `osv-scanner.toml` at the repo root. Any vulnerability suppressed there must
  also carry a statement here, so this document remains the complete record.

What VEX deliberately does **not** cover:

- **Misconfiguration and secret findings** (trivy's other two scanners). VEX
  speaks only about vulnerabilities in components, so those acceptances live in
  `.trivyignore.yaml` with a written reason each.
- **CodeQL / Scorecard alerts**. Those are dismissed in the GitHub UI with a
  recorded reason; they have no VEX identity to match on.

## Adding a statement

1. Append to `statements` with:
   - `vulnerability.name`: the advisory ID as the scanner reports it
     (RUSTSEC-…, GHSA-…, CVE-…). Add known aliases if scanners disagree.
   - `products[].@id`: the package URL, e.g. `pkg:cargo/rkyv@0.7.46`. A purl
     without a version matches all versions — pin the version so a bump forces
     a re-evaluation rather than inheriting the old verdict.
   - `status`: `not_affected` | `affected` | `fixed` | `under_investigation`.
   - For `not_affected`: a `justification` from the OpenVEX label set, plus an
     `impact_statement` saying *why in this codebase*, including the command
     that proves it and the condition under which to re-check.
2. Bump `version` and refresh `timestamp` (`date -u +%Y-%m-%dT%H:%M:%SZ`).
3. CI validates the document's shape in the `sbom` job before any scanner runs.

The rule carried over from `.trivyignore.yaml`: an ignore without a reason is a
finding someone silenced; an ignore with one is a decision someone made.
