# Local discovery and Accio integration

## Explicit side effects

`GET /api/bridge/discover` triggers a bounded loopback scan; it does not run at startup. Four probes run concurrently, each with a 700 ms request/body deadline and 2 MiB JSON limit. The configured bridge port is excluded. The probes cover Ollama, LM Studio, vLLM, llama.cpp, KoboldCpp, LocalAI, Jan, Text Generation WebUI, and six alternative OpenAI-compatible ports. Product labels are port/response-shape hints, not authenticated identity. Local discovery never supplies a provider key.

`POST /api/bridge/discover/models` explicitly requests a model listing at the supplied endpoint or a saved provider group's endpoint. Saved provider credentials stay internal. Redirects are disabled, errors do not include URLs or credentials, and results are bounded to 2 MiB / 10 seconds. No inference request is made, but model-list endpoints belong to external providers when the caller selects one. Both operations bypass system HTTP proxies to keep loopback traffic local and credential destinations explicit.

Current discovery responses set `cached:false`; each request scans again. This is an intentional initial Rust implementation difference from Node's 10-second result cache, not a stub.

## Inspection

Accio inspection locates platform-standard install paths or the configured explicit executable path. It reads bounded edition/update/install metadata, counts account directories (excluding guest), and reads only `gateway-cli.json` runtime connection metadata. It does not read account authorization files, send account requests, or return account identifiers. Runtime gateway URLs are restricted to loopback and reduced to an origin, excluding credential paths and queries. `signedIn` means an account directory exists, not independently verified authentication.

The detector uses `tasklist` on Windows and exact-name `pgrep` on Unix with a 1.5-second timeout. Commands use separate arguments, never a shell. Version and gateway fields remain null when unknown instead of inventing a version or assuming port 4097. Detection evidence is heuristic, not signature verification. `packageRegion` is not treated as the application's edition.

## Launch

Only an explicit launch API request starts Accio. Non-CN/unknown edition and an already-running process require `force:true` (or configured `allowNonCN` for the edition check). The executable is passed directly to `Command`, not interpreted by a shell. The child inherits the environment with `GATEWAY_BASE_URL` set to the active bridge address. It is not killed automatically and no existing Accio process is terminated. Arbitrary custom executable paths are an administrator capability: do not expose management routes without authentication.

## Verification boundary

Unit tests create disposable metadata directories and localhost HTTP fixtures. They test edition evidence, absent metadata, launch planning without spawning, response-shape parsing, model annotations and refusal to follow credential-bearing redirects. No installed Accio program, real account, actual inference service or billable model is invoked by these tests. Linux compilation and fixture tests do not establish Windows/macOS installation or launch compatibility; those require native acceptance tests.
