# newt-agent-py Example

Demonstrates basic usage of the Python library:

```python
from newt_agent.core import Router

router = Router()
print(router.classify("rename foo to bar"))  # Tier.Fast
```