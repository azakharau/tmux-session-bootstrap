# tmux-session-bootstrap

`tmux-session-bootstrap` installs the `ts` command, a small helper for creating
and entering a predictable tmux workspace.

Each new session contains three windows:

1. `agent`
2. `vim`
3. `terminal`

Automatic window renaming is disabled so the layout stays stable. When invoked
inside an active tmux client, `ts` switches that client to the new session.
Outside tmux, it attaches to the new session. A partially created session is
removed if setup or attachment fails.

## Install

```sh
cargo install --path .
```

tmux must be installed and available on `PATH`.

## Usage

```sh
ts my-project
```

The command refuses to replace an existing session with the same name.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

MIT
