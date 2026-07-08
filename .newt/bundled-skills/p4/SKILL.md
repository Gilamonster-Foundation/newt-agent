# Perforce (p4)

Execute `p4` commands for Perforce Helix Core operations. Use when the workspace uses Perforce instead of Git.

## Prerequisites

- `p4` client installed and configured
- Environment variables: `P4PORT`, `P4USER`, `P4CLIENT`, `P4PASSWD` (or use `p4 login`)

## Common Operations

### Workspace & Client

```bash
# View current client
p4 client -o
p4 client -i < /tmp/client_spec  # Edit and submit new spec

# Sync workspace
p4 sync //depot/main/...@have
p4 sync //depot/main/...#head

# Revert changes
p4 revert //depot/main/file.cpp
p4 revert //depot/main/...

# Resolve conflicts
p4 resolve -ay  # Auto-resolve all
p4 diff -du      # Diff unresolved files
```

### Changes & Depots

```bash
# View changelists
p4 changes -s submitted //depot/main/...@1000,2000
p4 changes -s pending -u $USER
p4 describe 123456

# Submit (commit)
p4 submit -d "Fix bug: description"
p4 submit -t /tmp/submit_template

# Open files for edit
p4 edit //depot/main/file.cpp
p4 add //depot/main/new_file.txt
p4 delete //depot/main/old_file.txt
```

### Branches & Views

```bash
# List branches
p4 branches | head -50
p4 branch -o my-branch

# Create/modify branch spec
p4 branch -i < /tmp/branch_spec  # Interactive edit
p4 submit //my-branch/...        # Submit branch changes

# View branch mapping
p4 describe 123456 | grep -A 20 "branches:"
```

### Diff & Log

```bash
# File diff (unresolved)
p4 diff -du //depot/main/file.cpp

# Changelist diff
p4 diff -du //depot/main/...@1000,2000

# File log (history)
p4 filelog //depot/main/file.cpp | head -30

# Show revision content
p4 print -q //depot/main/file.cpp@123 > /tmp/file.cpp.v123
```

### Stream Depots (modern Perforce)

```bash
# View stream
p4 streams | grep -A 5 my-stream
p4 stream -o my-stream

# Switch workspace to stream
p4 client -s my-stream

# Sync and integrate
p4 sync
p4 integrate //main/... //my-stream/...
p4 resolve
p4 submit
```

### Tags & Labels

```bash
# List labels
p4 labels -o //depot/main/... | head -50

# Create label
p4 labels -o my-label
# Edit spec, then:
p4 label -i < /tmp/label_spec

# Apply label to files
p4 labelfiles my-label //depot/main/file.cpp
```

### Search & Help

```bash
# Search depot
p4 fstat //depot/main/... | grep "//"
p4 changes -u $USER -s submitted //depot/main/...

# Open help
p4 help commands
p4 help submit
```

## Tips

- Use `p4 diff -du` for unified diff output (compatible with git/diff tools)
- Environment: `P4CONFIG` to specify client config file per project
- For large depots, use `@time` or `@date` instead of revision numbers
- Always `p4 sync` before working; never edit outside the workspace root
