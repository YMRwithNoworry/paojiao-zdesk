# Repository Guidelines

## Project Structure & Module Organization
This Rust workspace contains six crates under `crates/`: `zsdesk-core` holds domain models, `zsdesk-platform` defines platform abstractions, `zsdesk-windows` implements Win32/Shell integration, `zsdesk-ui` provides native UI, `zsdesk-plugin` hosts WASM plugins, and `zsdesk-app` coordinates the application. `docs/` contains user and developer guides, `native/` holds supporting native components, `packaging/` stores release metadata and installer assets, and `scripts/` contains build and test automation. Builds require the pinned ZSUI checkout beside this repository at `../zsui`; see `packaging/zsui-revision.txt`.

## Build, Test, and Development Commands
Use Windows x64 with the Rust MSVC toolchain, Visual Studio C++ tools, and the Windows SDK. Run commands from the repository root.

- `cargo test --workspace --release --locked` runs the workspace test suite used by CI.
- `cargo build --release --locked -p zsdesk-app` builds the default application edition.
- `cargo fmt --all -- --check` checks Rust formatting; `cargo fmt --all` applies it.
- Feature-specific editions can be checked with `cargo check --release --locked -p zsdesk-app --no-default-features --features themed` or `desktop-box`.

## Coding Style & Naming Conventions
Follow Rust 2021 conventions and let `rustfmt` format code with four-space indentation. Use `PascalCase` for types and traits, and `snake_case` for functions, modules, and local variables. Keep platform-specific code in the appropriate crate and gate Windows-only APIs with `cfg(windows)`.

## Testing Guidelines
Add focused unit tests near the code they cover, using existing `*_tests.rs` modules where appropriate. Keep shared integration fixtures under `crates/zsdesk-app/tests/`. Run the workspace test command before submitting; no repository-wide coverage threshold is configured.

## Commit & Pull Request Guidelines
Recent history uses Conventional Commit prefixes such as `feat:`, `fix:`, `perf:`, and `chore:`. Keep commit subjects concise and action-oriented. Pull requests should describe user-visible behavior, link an issue when relevant, report test commands and results, and include screenshots for UI changes.

## Agent-Specific Instructions
Run repository tasks with `niubash`. After completing work, push changes to the GitHub remote; create a remote repository if none exists.