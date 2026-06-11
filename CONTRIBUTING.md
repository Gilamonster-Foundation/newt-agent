# Contributing to Newt-Agent

## How to contribute

1. **Fork & clone**  
   - Fork the repository on GitHub.  
   - Clone your fork to your local machine: `git clone https://github.com/<your‑username>/newt-agent.git`  
   - Enter the project directory: `cd newt-agent`

2. **Create a new branch**  
   - Always work on a dedicated branch for your feature/bug‑fix:  
     ```bash
     git checkout -b <descriptive‑branch‑name>
     ```

3. **Set up the development environment**  
   - **Python**: create a virtual environment and install the project in editable mode:  
     ```bash
     python -m venv .venv
     source .venv/bin/activate
     pip install -e .
     ```
   - **Rust CLI**: install the `newt` binary (and any other needed tools) via Cargo:  
     ```bash
     cargo install --path newt-cli
     ```

4. **Coding standards**  
   - **Rust**: use `cargo fmt` and `cargo clippy` before committing.  
   - **Python**: run `black .` and `ruff .` (or `flake8`) to format and lint.  
   - Keep code readable, add doc‑comments for public items, and avoid noisy `println!` statements – use the `tracing` crate for structured logs.

5. **Testing**  
   - Run the test suite locally:  
     ```bash
     # Rust tests
     cargo test
     # Python tests
     pytest
     ```  
   - Ensure all tests pass and coverage remains high (≥ 80 %).  

6. **Plan files**  
   - For any new feature or substantial change, create a plan file in the `.newt/` directory, keyed by a conversation‑ID or descriptive name, e.g.:  
     ```bash
     just plan my‑new‑feature
     ```  
   - The plan should outline the problem, proposed solution, and any relevant decisions.

7. **Commit & push**  
   - After making changes, stage them: `git add <files>`  
   - Write clear, atomic commit messages (e.g., “Add CONTRIBUTING.md with workflow guide”).  
   - Push your branch: `git push origin <branch‑name>`.

8. **Open a Pull Request**  
   - In GitHub, click “New pull request” against the `main` branch.  
   - Fill in the PR template, reference any related issues, and request review.

## Git hooks (optional)

If you want automatic linting on commit, add a pre‑commit hook using `pre-commit` (see the `.pre-commit-config.yaml` file).

Thank you for helping improve Newt-Agent! 🎉