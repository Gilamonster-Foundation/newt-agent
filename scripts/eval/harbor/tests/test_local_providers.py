"""The injected provider config must parse, select the local provider, and
refuse to run without an endpoint (a silent empty base URL would send pi or
codex to its hosted default and measure a different model).
Run: PYTHONPATH=scripts/eval/harbor python -m unittest discover scripts/eval/harbor/tests
"""

import json
import tomllib
import unittest

from codex_local import provider_toml
from pi_local import models_json

URL = "http://inference.invalid:8080/v1"


class LocalProviders(unittest.TestCase):
    def test_codex_selects_local_provider(self):
        # Harbor appends `openai_base_url` / `[mcp_servers.*]` after this block;
        # a trailing top-level key must still parse as top-level.
        doc = tomllib.loads(provider_toml(URL, "131072") + '\n[mcp_servers.x]\nurl = "u"\n')
        self.assertEqual(doc["model_provider"], "local")
        self.assertEqual(doc["model_context_window"], 131072)
        self.assertEqual(doc["model_providers"]["local"]["base_url"], URL)

    def test_pi_provider_carries_model_and_window(self):
        local = json.loads(models_json(URL, "qwen3-coder_30b", "131072"))["providers"]["local"]
        self.assertEqual(local["baseUrl"], URL)
        self.assertEqual(local["models"], [{"id": "qwen3-coder_30b", "contextWindow": 131072}])

    def test_missing_window_refuses(self):
        # Unset, pi assumes 128000 and codex its own default, not the served ctx-size.
        with self.assertRaises(ValueError):
            provider_toml(URL, " ")
        with self.assertRaises(ValueError):
            models_json(URL, "m", "")

    def test_missing_endpoint_refuses(self):
        with self.assertRaises(ValueError):
            provider_toml("", "1")
        with self.assertRaises(ValueError):
            models_json("", "m", "1")


if __name__ == "__main__":
    unittest.main()
