"""pi exits 0 when the endpoint is unreachable (Gecko's #2320 review): its log
must read as an inference failure, and PiLocal must raise instead of letting
Harbor grade the untouched workspace as a model failure.
The fixture is a real pi 0.85.1 run against a dead port, retries included.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest discover scripts/eval/harbor/tests
"""

import json
import unittest
from pathlib import Path

from harbor.agents.installed.base import NonZeroAgentExitCodeError

from pi_local import raise_on_inference_failure
from pi_log import inference_failure

FIXTURE = Path(__file__).parent / "fixtures" / "pi-endpoint-unreachable.jsonl"


def assistant_end(reason):
    return json.dumps({"type": "message_end", "message": {"role": "assistant", "stopReason": reason}})


class InferenceFailure(unittest.TestCase):
    def test_unreachable_endpoint_is_a_failure(self):
        failure = inference_failure(FIXTURE.read_text().splitlines())
        self.assertIn("auto_retry_end success=false", failure)

    def test_final_error_or_abort_or_silence_is_a_failure(self):
        self.assertIn("stopReason=error", inference_failure([assistant_end("error")]))
        self.assertIn("stopReason=aborted", inference_failure([assistant_end("stop"), assistant_end("aborted")]))
        self.assertEqual(inference_failure(['{"type":"agent_start"}']), "no assistant message")

    def test_a_retry_that_recovers_is_not(self):
        self.assertIsNone(inference_failure([assistant_end("error"), assistant_end("toolUse"), assistant_end("stop")]))

    def test_pi_local_raises_on_the_recorded_failure(self):
        with self.assertRaises(NonZeroAgentExitCodeError):
            raise_on_inference_failure(FIXTURE)
        with self.assertRaises(NonZeroAgentExitCodeError):
            raise_on_inference_failure(FIXTURE.with_name("absent.jsonl"))


if __name__ == "__main__":
    unittest.main()
