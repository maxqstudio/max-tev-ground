# Security and research boundaries

- This application is a local research foundation. It does not place, route, or manage live trades.
- The previous D:\MAX_JEV_BENCHMARK\JEV_TEV_EURUSD_H1_LAST2M research project is outside this repository and is not read, copied, or modified by RG-R00.
- The Tauri shell exposes one read-only governance snapshot command. No filesystem, process, shell, network, or credential plugin is enabled.
- SQLite receives experiment metadata only. It rejects overwriting an experiment ID, enforces UPDATE/DELETE immutability with database triggers, and verifies the stored manifest SHA-256 on every metadata read.
- The manifest digest detects accidental or uncoordinated edits; because it is stored beside the manifest, it is not a signature against someone who can rewrite the database file and digest together.
- GitHub Actions evidence contains only a source-SHA-bound acceptance summary and checksum manifest. A separate default-branch job signs the summary; build and test steps do not receive attestation write permissions.
- Model identity requires a digest and explicit authority. No model endpoint or secret is configured in RG-R00.
- Data fingerprints, timestamp availability, OOS exposure, and missing-data policy are explicit. No synthetic market data is generated.
- Keep credentials out of experiment manifests, logs, screenshots, and Git. Future model adapters require a separate threat review and scoped capabilities.
- Dependencies are exact-pinned and lockfiles are committed. Report security issues to the Owner before publishing the repository.
