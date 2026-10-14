## Summary

<!-- What does this change and why? Link related issues (e.g. "Fixes #12"). -->

## Checklist

- [ ] `cargo test --workspace --locked` passes
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` passes
- [ ] New or changed backend logic is covered by `FakeRunner` tests
- [ ] No test runs system-changing commands or writes to system paths
- [ ] Docs updated (README, `docs/platforms.md`, `docs/design.md`, `CHANGELOG.md` under `[Unreleased]`) if behaviour, flags or backend commands changed
