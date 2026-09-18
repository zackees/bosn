# bosn

Let your agents use Docker all day without filling the disk.

Bosn is a daemon that owns the Docker development resources it creates: it
records them in a durable SQLite registry, bounds what they may consume, and
garbage-collects them safely. It also measures the Docker artifacts it does not
own and tells you, loudly, how to clear them.

```bash
cargo install bosn
bosn --version
bosn doctor
```

The same daemon ships to Python as the `bosn` wheel on PyPI, with a PyO3 client
binding (`import bosn`) and a stdio MCP server (`bosn mcp`).

This crate is published as one self-contained package; its modules (`core`,
`engine`, `generation`, `registry`, `setup`, `service`) are Bosn's internal
crates, amalgamated at release. See the
[repository](https://github.com/zackees/bosn) for documentation.

License: MIT
