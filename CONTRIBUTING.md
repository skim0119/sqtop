# Contributing

Bug reports, documentation fixes, and code changes are welcome. For a bug report, include the sqtop version (`sqtop --version`), operating system, terminal, Slurm version if known, and steps to reproduce. Remove usernames, hostnames, job details, and other cluster data before sharing logs or screenshots.

## Development

Install a Rust toolchain and OpenSSH. Build and run from the repository:

```sh
cargo build
cargo run -- frontera
```

A live cluster is useful for testing SSH and Slurm integration. Unit tests run locally and do not require cluster access:

```sh
cargo fmt --check
cargo test
```

Use `cargo fmt` to format changes. For UI changes, check a narrow terminal and a wider one. For SSH or parser changes, describe the Slurm version and connection setup used for any live testing. Keep remote work read-only and avoid adding frequent controller queries.

## Submitting changes

Describe the problem, the change, and how you tested it. Keep changes focused and include a test when it helps verify behavior. Contributions are provided under the project's [MIT license](LICENSE).
