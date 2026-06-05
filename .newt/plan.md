# Testing Improvement Plan

## Goals
- Add comprehensive unit and integration tests for core Rust CLI (`newt-cli`) and Python library (`newt-agent-py`).
- Implement CI pipeline (GitHub Actions) to run linting, type checking, tests, and coverage on every PR.
- Enforce a minimum test coverage threshold (e.g., 80%) and fail builds on regression.

## Tasks
1. Write unit tests for key functions in `newt-core`, `newt-cli`, and `newt-agent-py`.
2. Add integration tests that exercise end‑to‑end workflows (e.g., `newt --help`, file edit operations).
3. Configure CI workflow:
   - Set up Rust cargo test and Python pytest.
   - Generate coverage reports (e.g., `tarpaulin` for Rust, `coverage` for Python).
   - Upload artifacts and enforce coverage thresholds.
4. Document testing guidelines in `README.md` and `docs/testing.md`.
5. Add a `Makefile` target `test` to run local tests quickly.

## Success Criteria
- All new code has corresponding tests.
- CI runs automatically on PRs and reports pass/fail with coverage metrics.
- Code coverage for critical modules ≥ 80%.