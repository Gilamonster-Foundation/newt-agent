---
name: jupyter-notebook
description: "Execute Jupyter notebooks (.ipynb) using nbconvert, capturing outputs and updating the notebook with execution results."
version: 1.0.0
license: Apache-2.0
when_to_use: Need to run a Jupyter notebook end-to-end and capture cell outputs, errors, and execution results. Useful for data science workflows, verifying notebooks run correctly, or executing notebooks as part of an automated pipeline.
caveats:
  exec:
    only:
      - "jupyter"
      - "nbconvert"
  fs_read:
    only:
      - "*.ipynb"
  fs_write:
    only:
      - "*.ipynb"
  net: { only: [] }
  max_calls: { at_most: 10 }
---

# Jupyter Notebook Execution Skill

This skill provides the ability to execute Jupyter notebooks (`.ipynb` files) using nbconvert, capturing all cell outputs, errors, and execution metadata. The notebook is updated in-place with execution results.

## Prerequisites

- **Jupyter** must be installed and available in PATH (`jupyter` command)
- **nbconvert** is typically included with Jupyter installation
- A Python kernel (typically `python3`) must be available

## Core Tool

The skill uses the `newt_agent.tools.execute_notebook` function which wraps `jupyter nbconvert --execute --inplace`.

## Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `notebook_path` | string | **required** | Path to the notebook file (.ipynb) |
| `working_dir` | string | notebook's parent directory | Working directory for execution |
| `timeout_seconds` | integer | 300 | Timeout for entire notebook execution |
| `save_outputs` | boolean | true | Whether to save executed notebook with outputs |
| `kernel_name` | string | "python3" | Kernel name to use for execution |

## Returns

A `JupyterExecuteResult` object containing:
- `success` (bool): Whether execution succeeded
- `notebook_path` (string): Path to the executed notebook
- `cells_executed` (int): Number of cells executed
- `cells_failed` (int): Number of cells that failed
- `execution_time_seconds` (float): Total execution time
- `error` (string, optional): Error message if execution failed
- `cell_outputs` (list of `CellOutputSummary`): Per-cell execution summary

## CellOutputSummary

Each cell output summary contains:
- `cell_index` (int): Index of the cell
- `cell_type` (string): "code", "markdown", or "raw"
- `success` (bool): Whether the cell executed successfully
- `output_count` (int): Number of outputs produced
- `error` (string, optional): Error message if cell failed

## Example Usage

```python
from newt_agent.tools import execute_notebook, JupyterExecuteParams

# Execute a notebook with default settings
params = JupyterExecuteParams(
    notebook_path="analysis.ipynb",
    timeout_seconds=60,
    save_outputs=True
)
result = execute_notebook(params)

if result.success:
    print(f"Executed {result.cells_executed} cells in {result.execution_time_seconds:.1f}s")
    for cell in result.cell_outputs:
        if not cell.success:
            print(f"Cell {cell.cell_index} failed: {cell.error}")
else:
    print(f"Execution failed: {result.error}")
```

## Workflow

1. **Prepare** the notebook file (`.ipynb`) with code cells ready to execute
2. **Call** `execute_notebook` with the notebook path and optional parameters
3. **Check** the result for success/failure and inspect cell outputs
4. **The notebook file is updated in-place** with execution outputs if `save_outputs=true`

## Tips

- For long-running notebooks, increase `timeout_seconds` (default 300s/5min)
- Use `working_dir` to control the execution context (e.g., for relative paths in the notebook)
- The `kernel_name` should match an installed Jupyter kernel (list with `jupyter kernelspec list`)
- If `save_outputs=false`, the notebook is executed but not updated with outputs

## Error Handling

Common errors:
- **Notebook not found**: Check the `notebook_path` is correct
- **Jupyter not installed**: Install with `pip install jupyter`
- **Kernel not found**: Install the required kernel or use a different `kernel_name`
- **Timeout**: Increase `timeout_seconds` or optimize slow cells
- **Cell errors**: Check `cell_outputs` for specific cell failures with tracebacks