# UScanner

Rust backend for `UCore.nvim`.

`UScanner` owns the Unreal project indexer and query engine:

- `u_scanner` CLI bridge
- `u_core_server` TCP + MsgPack RPC server
- SQLite project / engine caches
- symbol, module, asset, config, include, and diagnostics queries

## Build

```powershell
cargo build --release --bin u_core_server --bin u_scanner
```

Built binaries land under `target/release/`.

## Local Test Fixtures

`ucore_test/` contains reusable request fixtures for testing against a real
Unreal project.

```powershell
cd ucore_test
.\make-local.ps1 -ProjectRoot "D:\UnrealProjects\YourProject"
.\start-server.ps1
```

From another terminal:

```powershell
cd ucore_test
.\run-lifecycle.ps1
.\run-query.ps1 query_get_modules
.\run-all-queries.ps1
```

## Repository Role

`UScanner` is intentionally not a Neovim plugin. `UCore.nvim` builds and runs
this backend, while local monorepo development can point directly at a sibling
`../UScanner` checkout.
