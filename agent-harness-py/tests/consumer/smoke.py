"""Real CPython import grounds Rust's frame admission and byte-verification tests."""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

from _smart_harness_consumer import frame, harness


def refuses(action):
    try:
        action()
    except ValueError:
        return
    raise AssertionError("invalid evidence crossed the Python admission boundary")


def frame_roundtrip():
    material = b"source bytes"
    root = frame.root("operator_prompt", b"keep source", 1)
    root_id = frame.root_id(root)
    unit = frame.elide(material, 0, 6, root_id)
    unit_id = frame.unit_id(unit)
    assert frame.unit_id(json.dumps(json.loads(unit))) == unit_id
    canonical = frame.unit_canonical(unit)
    assert isinstance(canonical, bytes)
    assert frame.unit_id(frame.unit_from_canonical(canonical)) == unit_id
    assert frame.canonical_id(frame.unit_identity_canonical(unit)) == unit_id
    assert frame.canonical_id(canonical) != unit_id  # transfer includes lifecycle
    assert frame.verify_unit(unit, material) == b"source"
    refuses(lambda: frame.verify_unit(unit, b"tampered bytes"))
    refuses(lambda: frame.elide(material, 1, 1, root_id))
    refuses(lambda: frame.elide(material, 0, 99, root_id))
    refuses(lambda: frame.elide(material, 0, 6, "not a CID"))
    invalid = json.loads(unit)
    invalid["depth"] = 2
    refuses(lambda: frame.unit_id(json.dumps(invalid)))
    invalid = json.loads(unit)
    invalid["elided"] = json.loads(root)["content"]
    refuses(lambda: frame.verify_unit(json.dumps(invalid), material))


def harness_roundtrip():
    """Ground durable session replay in a fresh Python process and real files."""
    with tempfile.TemporaryDirectory() as directory:
        session = harness.Session('{"authority":"consumer-fixture"}', directory)
        body = {"messages": [{"role": "user", "content": "What is missing?"}]}
        request = json.loads(session.record_request(json.dumps(body), "openai"))
        assert json.loads(request["bytes"]) == body
        reply = session.record_reply(request["id"], b"Which source should I use?")
        assert session.pending_replies() == [reply]
        refuses(lambda: session.record_verdict(reply, "maybe"))
        session.record_verdict(reply, "question")
        assert session.pending_replies() == []
        refuses(lambda: session.record_verdict(reply, "answer"))
        assert json.loads(session.replay(request["id"])) == body
        pending = session.record_reply(request["id"], b"Checking the source now.")
        head = session.head()
        # A fresh interpreter verifies the stored run, then reconstructs the
        # original request and pending adjudication using no in-memory state.
        subprocess.run(
            [sys.executable, __file__, "restore", directory, head, request["id"], pending],
            check=True,
        )
        refuses(lambda: harness.Session.restore(directory, head, "another-consumer"))
        # Addressed evidence is checked when read: a real byte substitution
        # cannot turn into a valid request just because its filename is a CID.
        for source in Path(directory).iterdir():
            if source.is_file() and source.read_bytes() == request["bytes"].encode():
                source.write_bytes(b"substituted")
                break
        else:
            raise AssertionError("request evidence was not persisted")
        refuses(lambda: session.replay(request["id"]))
        refuses(lambda: harness.Session.restore(directory, head, "consumer-fixture"))


def projection_and_retrieval():
    session = harness.Session(json.dumps({
        "max_navigation_calls": 2, "max_slice_bytes": 8,
        "max_fetched_bytes": 40, "max_dereferences": 3,
    }))
    messages = [{"role": "system", "content": "rules"}, {"role": "user", "content": "task"}]
    assert json.loads(session.project(json.dumps(messages), 1000)) == messages
    refuses(lambda: session.project(json.dumps(messages), 1))
    refuses(lambda: session.project(json.dumps(messages), 1000))
    session.start_turn()
    request = json.loads(session.record_request(json.dumps({"messages": messages}), "openai"))
    reply = session.record_reply(request["id"], b"abcdefghijklmnopqrst")
    head = session.head()
    page = json.loads(session.re_read(reply, 0, 8))
    assert page["text"] == "abcdefgh" and page["next_offset"] == 8
    assert not page["complete"] and session.head() != head
    assert json.loads(session.re_read(reply, 8, 8))["text"] == "ijklmnop"
    refuses(lambda: session.re_read(reply, 16, 8))
    session.start_turn()
    page = json.loads(session.re_read(reply, 16, 8))
    assert page["text"] == "qrst" and page["complete"] and page["next_offset"] is None
    refuses(lambda: session.re_read(reply, 0, 0))
    refuses(lambda: session.re_read(request["id"], 0, 8))
    session.record_failure(reply, "auxiliary unavailable")
    assert session.pending_replies() == []


def event_admission():
    """Python can compose admitted parents; imports still verify derivation depth."""
    root = frame.root("operator_prompt", b"task", 1)
    body = {"root": frame.root_id(root), "origin": "model", "kind": "observation",
            "payload": json.loads(root)["content"], "seq": 1, "sources": [], "depth": 0}
    observation = frame.Event(json.dumps(body))
    assert frame.Event.from_canonical(observation.canonical()).id() == observation.id()
    assert frame.canonical_id(observation.canonical()) == observation.id()
    body.update(origin="harness", kind={"verdict": {"verdict": "question"}},
                seq=2, sources=[observation.id()], depth=1)
    verdict = frame.Event(json.dumps(body), [observation])
    assert verdict.parents() == [observation.id()]
    assert frame.Event.from_json(verdict.to_json(), [observation]).id() == verdict.id()
    refuses(lambda: frame.Event.from_json(verdict.to_json()))
    body.update(kind="intervention", seq=3, sources=[verdict.id()], depth=2)
    refuses(lambda: frame.Event(json.dumps(body), [verdict]))


def host_composition():
    """Compose tool dispatch, exact retention, and independent auxiliary recording."""
    with tempfile.TemporaryDirectory() as directory:
        session = harness.Session('{"authority":"composition-fixture"}', directory)
        assert json.loads(session.config())["authority"] == "composition-fixture"
        assert session.run_id() and Path(session.checkpoint_path()).is_file()
        messages = [{"role":"user", "content":"read the file"}]
        session.record_messages(json.dumps(messages))
        request = json.loads(session.record_request(json.dumps({"messages":messages}), "openai"))
        reply = session.record_reply(request["id"], b"tool call")
        calls = [{"id":"call_1", "type":"function", "function":{"name":"read_file", "arguments":"{}"}}]
        session.record_tool_dispatch(reply, json.dumps(calls))
        retained = session.retain_tool_output("read_file", b"unabridged file contents")
        assert json.loads(session.re_read(retained, 0, 100))["text"] == "unabridged file contents"
        messages += [{"role":"assistant", "tool_calls":calls},
                     {"role":"tool", "tool_call_id":"call_1", "content":"unabridged file contents"}]
        session.record_messages(json.dumps(messages))
        assert json.loads(session.restored_messages()) == messages
        session.start_turn()
        catalog = json.loads(session.catalog(json.dumps(messages), 4096))
        navigation = session.record_navigation_request(json.dumps(catalog), "select source CIDs")
        selected = [candidate["cid"] for candidate in catalog["candidates"]]
        session.record_navigation_reply(navigation, json.dumps(selected))
        assert json.loads(session.project_selection(json.dumps(messages), selected, 4096)) == messages
        reply = session.record_reply(request["id"], b"Which file next?")
        session.record_model_message(reply, "Which file next?")
        model_message = session.last_message()
        assert model_message is not None
        adjudication = session.record_adjudication_request(reply, "classify the observation")
        session.record_adjudication_reply(adjudication, '"question"')
        session.record_verdict(reply, "question")
        session.record_outcome(reply, "await_operator", "HOST DECORATION: Which file next?")
        restored = harness.Session.restore_with_config(directory, session.head(), session.config())
        assert restored.run_id() == session.run_id()
        history = json.loads(restored.restored_messages())
        assert history[-1] == {"role":"assistant", "content":"Which file next?"}
        assert "HOST DECORATION" not in json.dumps(history)
        host_message = restored.record_host_message("Continue with the task.", model_message)
        history.append({"role":"user", "content":"Continue with the task."})
        restored.record_messages(json.dumps(history))
        assert json.loads(restored.restored_messages())[-1]["content"] == "Continue with the task."
        catalog = json.loads(restored.catalog(json.dumps(history), 4096))
        assert host_message not in [candidate["cid"] for candidate in catalog["candidates"]]
        changed = json.loads(session.config())
        changed["max_slice_bytes"] += 1
        refuses(lambda: harness.Session.restore_with_config(directory, session.head(), json.dumps(changed)))


def provider_rendering():
    """The foreign host keeps source roles when Anthropic coalesces wire messages."""
    session = harness.Session()
    messages = [{"role":"system", "content":"rules"}, {"role":"user", "content":"task"}]
    session.record_messages(json.dumps(messages))
    host = {"role":"user", "content":"host notice"}
    host_id = session.record_host_envelope(json.dumps(host), session.last_message())
    messages.append(host)
    session.record_messages(json.dumps(messages))
    request = json.loads(session.record_rendered_request('{"model":"fixture","max_tokens":32}', "anthropic", json.dumps(messages)))
    body = json.loads(request["bytes"])
    assert body["system"] == "rules" and len(body["messages"]) == 1
    assert body["messages"][0] == {"role":"user", "content":[
        {"type":"text", "text":"task"}, {"type":"text", "text":"host notice"}
    ]}
    assert session.replay(request["id"]) == request["bytes"].encode()
    catalog = json.loads(session.catalog(json.dumps(messages), 4096))
    assert host_id not in [candidate["cid"] for candidate in catalog["candidates"]]
    session.account_navigation_elapsed(120000)
    refuses(lambda: session.catalog(json.dumps(messages), 4096))
    native = {"role":"user", "content":[{"type":"text", "text":"protected operator content"}]}
    refuses(lambda: harness.Session().record_rendered_request('{}', "anthropic", json.dumps([native])))
    native_body = {"model":"fixture", "messages":[native]}
    native_request = json.loads(harness.Session().record_request(json.dumps(native_body), "anthropic"))
    assert json.loads(native_request["bytes"]) == native_body


if __name__ == "__main__":
    if len(sys.argv) > 1:
        _, _, directory, head, request, pending = sys.argv
        restored = harness.Session.restore(directory, head, "consumer-fixture")
        assert restored.head() != head
        assert restored.pending_replies() == [pending]
        assert json.loads(restored.replay(request))["messages"][0]["role"] == "user"
    else:
        frame_roundtrip()
        event_admission()
        harness_roundtrip()
        projection_and_retrieval()
        host_composition()
        provider_rendering()
