# Release tooling

There is no CI release pipeline. These scripts build and check a release
payload locally; the Makefile targets drive them:

- `make release-dry-run` / `make continuous-dry-run`: release build,
  `split_debug.py` (Linux), `bundle_catalog.py`, `assemble_artifacts.py`,
  `verify_decoders.py` and `verify_release.py` for the host target.
- `make package`: the same staged layout through `scripts/package_release.py`.
- `make catalog-assets-gates`, `make fold-gates`, `make legacy-upgrade-gates`:
  the scripts' own contract tests.

`assemble_source.py` builds the source tarball the Homebrew formula
(`packaging/homebrew/`) builds from. `test_legacy_upgrade.py` runs old
installers against a payload by hand (see its usage text).
