# pa-computer-use

The host side of the bundled `computer-use` skill: the allowlist gate, permission reports, observation, diffing,
input, capture and the secure-field rules, behind the `computer_use.*` kernel host requests. The skill's Python
package (`skills/computer-use`) becomes a thin client of these requests (wired in a later commit).

This commit lands the platform-independent session logic, the X11 backend, and their tests (ported from the
skill's `tests/`). The macOS and Wayland backends, the pa-core wiring and the full README follow.
