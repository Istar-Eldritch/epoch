# Contributing to Epoch

Thank you for your interest in contributing to Epoch! This document provides guidelines to help you get started.

## Getting Started

1. **Fork and clone** the repository
2. **Install Rust** using [rustup](https://rustup.rs/) (edition 2024 required)
3. **Build the project:** `cargo build`
4. **Run tests:** `cargo test`

## Code Style

- **Format code** with `cargo fmt` before committing
- **Run clippy** with `cargo clippy -- -D warnings` to catch common issues
- **Document public APIs** with rustdoc comments
- **Avoid `unwrap()`/`expect()`** in library code (tests are fine)

## Commit Convention

This project follows [Conventional Commits](https://www.conventionalcommits.org/). All commits should follow this format:

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

### Types

| Type       | Description                                      |
|------------|--------------------------------------------------|
| `feat`     | A new feature                                    |
| `fix`      | A bug fix                                        |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `test`     | Adding or updating tests                         |
| `docs`     | Documentation only changes                       |
| `perf`     | Performance improvements                         |
| `chore`    | Maintenance tasks (deps, CI, tooling)            |

### Scopes

Use the workspace crate name or domain concept as scope:

- **Crates:** `core`, `derive`, `mem`, `pg`
- **Concepts:** `aggregate`, `projection`, `saga`, `event-store`, `event-bus`, `command`

### Breaking Changes

For breaking changes, add `!` after the type/scope or include `BREAKING CHANGE:` in the footer:

```
feat(core)!: redesign Event trait interface

BREAKING CHANGE: Event trait now requires `event_type()` method
```

### Examples

```
feat(saga): add compensation support for failed steps
fix(pg): handle connection timeout during event persistence
refactor(mem): simplify event-bus channel handling
test(core): add property-based tests for aggregate versioning
docs(readme): add quick start guide
perf(pg): batch event inserts for improved throughput
chore(deps): update sqlx to 0.8
```

## Pull Request Process

1. **Create a feature branch** from `main`
2. **Make your changes** following the code style guidelines
3. **Write/update tests** for your changes
4. **Ensure all tests pass:** `cargo test`
5. **Ensure no clippy warnings:** `cargo clippy -- -D warnings`
6. **Format your code:** `cargo fmt`
7. **Write clear commit messages** following the convention above
8. **Open a pull request** with a clear description of the changes

## Testing Guidelines

- Write **unit tests** for new functionality
- Ensure tests are **independent** and can run in parallel
- Tests should be **idempotent** (pass when run multiple times)
- Focus on **behavior**, not implementation details

## Questions?

If you have questions or need help, feel free to open an issue for discussion.
