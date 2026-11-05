# sqtop

A read-only Slurm schedule explorer over SSH.

I'm building sqtop to maintain my sanity while waiting for Slurm. I'd like to stop staring at the queue, wondering whether my job will start before the next meeting, and go prepare the update. Or forget about the job entirely until the meeting reminds me.

Refreshing the queue hasn't made it move faster so far. I have tried.

## Development

```sh
cargo run
cargo fmt --all --check
cargo test --locked --all-targets
```
