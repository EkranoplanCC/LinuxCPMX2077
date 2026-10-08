# Working on CPMX2077

## Keep the user docs current

`docs/getting-started.md` (startup tutorial) and `docs/user-guide.md`
(reference for every tab, button and behaviour) describe what the app does
for users. Every functional change must update them in the same commit or
PR:

- New or changed buttons, tabs, settings, labels, install rules, sources,
  storage paths or safety checks: update the matching section of
  `docs/user-guide.md`.
- Anything that changes the first-run steps (game detection, Nexus sign-in,
  frameworks, Linux setup fixes, first install): update
  `docs/getting-started.md`.
- Renamed UI text: search both files for the old name.
- Keep the README's feature list in step when a feature is added or removed.

Write for players, not developers: what they see and click, in plain words,
using the exact button labels from `ui/`. Pure refactors, tests and CI
changes need no docs update.

## Checks before pushing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

CI uses the latest stable Rust, so run `rustup update stable` first.
