# Unreal project UScanner test fixtures

This folder keeps reusable request templates for testing `UScanner` against a
local Unreal project.

Committed files live under `templates/` and use placeholders instead of local
absolute paths. Generate runnable local files with:

```powershell
cd ucore_test
.\make-local.ps1 -ProjectRoot "D:\UnrealProjects\YourProject"
```

Generated files are written to `local/` and are ignored by git.

Run the server with the generated registry:

```powershell
cd ucore_test
.\start-server.ps1
```

Run lifecycle requests from another terminal:

```powershell
cd ucore_test
.\run-lifecycle.ps1
```

Run a query:

```powershell
cd ucore_test
.\run-query.ps1 query_get_modules
```

Run all query fixtures and write results under `out/`:

```powershell
cd ucore_test
.\run-all-queries.ps1
```
