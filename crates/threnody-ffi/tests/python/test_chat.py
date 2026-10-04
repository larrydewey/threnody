"""Two Threnody nodes driven entirely through the generated Python bindings."""
import sys
import tempfile

from threnody_ffi import NodeEvent, ThrenodyNode


def wait(node, kind, timeout_s=20):
    for _ in range(timeout_s * 10):
        e = node.next_event(100)
        if e is not None and isinstance(e, kind):
            return e
    raise AssertionError(f"no {kind.__name__} event")


with tempfile.TemporaryDirectory() as d:
    alice = ThrenodyNode.open(f"{d}/alice", None)
    bob = ThrenodyNode.open(f"{d}/bob", "correct horse")
    addr = bob.listen("127.0.0.1:0")
    bob_fp = alice.connect(bob.invite_link(addr))
    assert bob_fp == bob.device_fingerprint()
    wait(bob, NodeEvent.CONNECTED)
    # Bob wants Alice's messages; otherwise they'd arrive as requests.
    bob.accept_contact(alice.device_fingerprint())

    assert alice.send_text(bob_fp, "hello from Python") == 1
    msg = wait(bob, NodeEvent.MESSAGE)
    assert msg.text == "hello from Python", msg
    assert msg.peer == alice.device_fingerprint()

    bob.send_text(alice.device_fingerprint(), "hi back")
    assert wait(alice, NodeEvent.MESSAGE).text == "hi back"

    alice.set_approval(bob_fp, True)
    bob.set_approval(alice.device_fingerprint(), True)
    assert wait(alice, NodeEvent.APPROVAL_CHANGED).mutual
    [c] = [c for c in alice.contacts() if c.fingerprint == bob_fp]
    assert c.mutually_approved and c.connected
    assert alice.safety_number(bob_fp) == bob.safety_number(alice.device_fingerprint())

    alice.shutdown()
    bob.shutdown()
    print("python bindings: ok")
    sys.exit(0)
