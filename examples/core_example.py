from newt_agent.core import Router, Tier

router = Router()
print(router.classify("rename foo to bar"))  # Output: Tier.Fast