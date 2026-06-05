# Testing Improvements Plan

## Goal
Implement comprehensive testing improvements for the Newt-Agent codebase.

## Tasks
1. Add integration tests for core router classification functionality.
2. Create a dedicated test utilities module under `tests/common/src`.
3. Add a GitHub Actions CI workflow to run tests on every push.
4. Update existing test suites with additional coverage cases.
5. Ensure all new code adheres to formatting and linting standards.

## Files to Modify/Add
- `tests/integration/router_test.rs` - integration tests for router classification.
- `.github/workflows/ci.yml` - CI pipeline configuration.
- `tests/common/src/lib.rs` - enhance shared test helpers if needed.
- Existing test files as needed for coverage.

## Success Criteria
- All new tests pass locally.
- CI pipeline runs tests and reports success/failure.
- Code coverage improves with new tests.
- No linting or formatting violations introduced.