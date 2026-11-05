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

## Homebrew releases

Publishing a stable `vX.Y.Z` GitHub release runs the Homebrew workflow. It updates
`Formula/sqtop.rb` in `skim0119/homebrew-tap` and pushes a commit to its `main` branch.
Prereleases are skipped.

One-time setup: create a fine-grained personal access token restricted to
`skim0119/homebrew-tap`, with **Contents: read and write**. Save it in sqtop's
repository Actions secrets as `HOMEBREW_TAP_TOKEN`. The built-in `GITHUB_TOKEN`
cannot write to the separate tap repository. The tap must allow this token to
push to `main`.

To retry an update after setting or renewing the secret, run **Update Homebrew
tap** from GitHub Actions and enter the published tag. The workflow can also
update the existing `v0.1.0` release this way. Repeating an update is harmless;
older releases cannot replace newer formula versions. This workflow updates the
source formula, not Homebrew bottles.
