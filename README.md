# sqtop

A read-only Slurm schedule explorer over SSH.

I'm building sqtop to maintain my sanity while waiting for Slurm. I'd like to stop staring at the queue, wondering whether my job will start before the next meeting, and go prepare the update. Or forget about the job entirely until the meeting reminds me.

Refreshing the queue hasn't made it move faster so far. I have tried.

## Install

With a Rust toolchain installed, run from the source directory:

```sh
cargo install --locked --path .
```

Make sure Cargo's bin directory (normally `~/.cargo/bin`) is on your PATH. To update an existing installation, add `--force`.

## Run

```sh
sqtop user@host
sqtop --help
```

The host can be an SSH alias or `user@host`. This increment validates and prints the destination; remote collection comes next.

Use the same SSH alias or `user@host` you would pass to `ssh`. sqtop uses your OpenSSH configuration and authentication. No sqtop configuration file is needed.

## Development

```sh
cargo fmt --all --check
cargo test --locked --all-targets
```
