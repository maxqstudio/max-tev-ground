# ADR 0003: Roadmap source and acceptance evidence

- Status: Accepted for RG-R00
- Scope: Phase progression and local acceptance

## Decision

ROADMAP.yaml is authoritative. ROADMAP.md is generated from it. The phase manifest and implementation state must agree with the active phase and deliverables. The governance validator fails on missing or invalid sources, multiple active phases, out-of-order phases, missing evidence for any accepted phase, scope drift, generated-document drift, or automatic unlocking of a later phase.

The acceptance runner is non-interactive, records an exact clean Git commit and tree SHA, and never launches the desktop app. Owner UI review remains a separate acceptance state.

## Reason

Roadmap drift and interactive desktop automation can otherwise create false completion claims. The Owner remains the only authority for accepting RG-R00 and authorizing RG-R01.

## Consequences

- RG-R00 remains active until Owner acceptance.
- RG-R01 stays locked even after local automated tests pass.
- If no official remote is configured, CI remains NOT_CONFIGURED; no remote is invented.
- Automated tests, desktop compilation, Owner UI acceptance, and trading-edge evidence remain distinct.