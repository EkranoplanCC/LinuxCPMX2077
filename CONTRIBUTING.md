# Contributing

- Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`
  before pushing.
- **Update the user docs with every functional change.** If your change adds,
  removes or changes something a user sees or does, update
  [`docs/user-guide.md`](docs/user-guide.md), and
  [`docs/getting-started.md`](docs/getting-started.md) when first-run steps
  change, in the same PR. Use the exact button labels from `ui/`. See
  [`CLAUDE.md`](CLAUDE.md) for the full rule.
