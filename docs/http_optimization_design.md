# HTTP I/O Optimization Design

## Overview
The current Newt-Agent code base performs synchronous HTTP requests for two critical paths:

1. **Endpoint discovery** – `LocalOllamaBackend::discover` issues a GET request to `/api/tags` for each candidate endpoint, with a 500 ms timeout. In the worst case dozens of probes are issued before a usable backend is found, causing several seconds of latency before any inference can start.

2. **Inference request handling** – `try_complete` performs a blocking POST request (`/api/chat` for Ollama or a similar endpoint for vLLM) for every user request. Under high load or high‑latency network conditions this dominates overall execution time.

The goal of this design is to **reduce the latency introduced by HTTP I/O** while preserving correctness and keeping the binary small and fast.

## Goals
- **Fast backend discovery** – cut the time to locate a usable Ollama/vLLM endpoint from seconds to < 200 ms in typical cases.
- **Low‑latency inference** – make the request/response cycle for `try_complete` non‑blocking where possible and reuse connections.
- **Minimal runtime impact** – changes should be incremental, backward compatible, and not introduce new heavy dependencies.

## Current Issues
| Issue | Symptom | Root Cause |
|------|----------|------------|
| Synchronous discovery probes | `discover` blocks for seconds | Serial GET requests, each with its own TCP handshake and TLS negotiation |
| Blocking inference POST | `try_complete` stalls during network I/O | Uses blocking `reqwest` (or `curl`) with a single connection per request |
| No connection reuse | Each request opens a fresh socket | Default `reqwest` builder does not enable connection pooling |
| No concurrency in discovery | Sequential probing of candidates | Loop iterates over a `Vec<&str>` without parallelism |

## Design Principles
1. **Cache discovery results** – after the first successful discovery, reuse the endpoint for the lifetime of the process (or until a timeout). This eliminates repeated probes.
2. **Parallel discovery** – probe multiple candidates concurrently (e.g., using `tokio::spawn` or a thread pool) with a bounded concurrency to avoid overwhelming the network.
3. **Async non‑blocking HTTP client** – switch to an async client (`tokio::reqwest` or `hyper`) that supports connection pooling, HTTP keep‑alive, and non‑blocking I/O.
4. **Circuit‑breaker & timeout tuning** – shorten the per‑request timeout (e.g., 1 s) and add a circuit‑breaker to avoid hanging on unreachable endpoints.
5. **Reuse inference connections** – keep a persistent connection pool for the inference endpoint; reuse the same TCP connection for multiple `try_complete` calls.

## Implementation Steps

1. **Introduce an async runtime**  
   - Add `tokio = { version = "1", features = ["full"] }` to `newt-agent`'s dependencies (only for the server binary; CLI can stay sync).  
   - Wrap the discovery and inference logic in async functions.

2. **Create a discovery cache**  
   - `struct BackendCache { endpoint: Option<String>, last_checked: Instant, ttl: Duration }`  
   - `async fn get_backend(&mut self) -> Result<&str, DiscoveryError>`  
     - Return cached endpoint if fresh (TTL configurable, e.g., 5 min).  
     - Otherwise launch a bounded parallel discovery task.

3. **Parallel discovery**  
   - Use `tokio::try_join_all` (or `futures::stream::FuturesUnordered`) to issue GET `/api/tags` requests concurrently.  
   - Limit concurrency with a semaphore (e.g., max 4 simultaneous probes).  
   - Apply per‑request timeout (1 s) and a global deadline (2 s).

4. **Switch inference to async**  
   - Replace the current blocking `try_complete` implementation with an async version that:
     - Acquires a connection from a `tokio::Client` with connection pooling enabled.  
     - Sends the request and awaits the response.  
     - Handles errors with retry/back‑off if desired.

5. **Connection pooling**  
   - Configure the async client with `pool.max_conn = 20` (or appropriate for the expected load).  
   - Reuse the same client instance for the lifetime of the inference service.

6. **Graceful fallback**  
   - If discovery fails after the cache TTL expires, fall back to a static “localhost:11434” endpoint (the default Ollama address) to avoid total denial.

7. **Observability**  
   - Add metrics (e.g., Prometheus or simple counters) for:
     - `discovery_duration_ms`
     - `backend_cache_hits`
     - `inference_latency_ms`
   - Log at `info!` level when a new backend is discovered; `debug!` for each probe.

## Trade‑offs

| Aspect | Benefit | Cost / Risk |
|--------|---------|--------------|
| Async runtime | Eliminates thread‑per‑request bottleneck | Introduces async complexity; requires careful handling of `Send`/`Sync` bounds |
| Parallel discovery | Reduces discovery latency dramatically | Higher CPU/network usage during the probe window; need to bound concurrency |
| Caching | Avoids unnecessary probes | Stale backend if the process runs for a long time and the endpoint changes (mitigated by TTL) |
| Connection pooling | Reuses sockets, lower per‑request latency | Slightly higher memory usage; must ensure proper cleanup on shutdown |

## Monitoring & Validation

1. **Unit tests** – mock the HTTP client to verify that:
   - Discovery returns cached endpoint after first call.
   - Parallel probes are limited and respect timeouts.
2. **Integration tests** – spin up a local Ollama server (Docker) and measure:
   - End‑to‑end latency before vs. after changes.
   - Correctness of inference results.
3. **Benchmark suite** – add a `criterion` benchmark that measures:
   - `discover` time with a list of 10 dummy endpoints.
   - `try_complete` latency under varying concurrent request counts.
4. **CI** – ensure `just check` and `just test` remain green; add a performance gate (e.g., discovery < 200 ms on a typical CI environment).

## Summary

By **caching discovery results**, **parallelizing probes**, and **moving to an async, connection‑pooled HTTP client**, we can dramatically cut the slowest parts of Newt-Agent’s runtime. The changes are incremental, keep the binary fast, and provide observability for future tuning.