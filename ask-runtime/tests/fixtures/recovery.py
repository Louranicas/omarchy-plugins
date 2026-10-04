"""Local synthetic ACP adapter: cancellation/replacement transport tests only."""
import json
import os
import signal
import sys

receipt, mode = sys.argv[1:]
signal.alarm(10)

def record(value):
    with open(receipt, "a", encoding="utf-8") as out:
        out.write(json.dumps(value) + "\n")

def emit(value):
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)

def permission():
    return dict(sessionId="reused-session", toolCall=dict(title="Synthetic action"),
                options=[dict(optionId="allow", kind="allow_once")])

def terminate(_signum, _frame):
    record(dict(signal="term"))

if mode == "unresponsive":
    signal.signal(signal.SIGTERM, terminate)
record(dict(pid=os.getpid()))
prompt = None
for line in sys.stdin:
    message = json.loads(line)
    record(dict(received=message))
    method = message.get("method")
    if method == "initialize":
        emit(dict(id=message["id"], result=dict(protocolVersion=1,
             agentCapabilities=dict(sessionCapabilities=dict(close={})))))
    elif method == "session/new":
        emit(dict(id=message["id"], result=dict(sessionId="reused-session")))
    elif method == "session/prompt":
        prompt = message["id"]
        emit(dict(id=77, method="session/request_permission", params=permission()))
    elif method == "session/cancel" and mode != "unresponsive":
        emit(dict(method="session/update", params=dict(sessionId="reused-session",
             update=dict(sessionUpdate="agent_message_chunk",
                         content=dict(type="text", text="cancelled late text")))))
        emit(dict(id=78, method="session/request_permission", params=permission()))
        emit(dict(id=prompt, result=dict(stopReason="cancelled")))
    elif method == "session/close" and mode != "unresponsive":
        emit(dict(id=message["id"], result={}))
        break
    elif method is None and message.get("id") == 77:
        outcome = message.get("result", {}).get("outcome", {})
        if outcome.get("outcome") == "selected":
            emit(dict(id=prompt, result=dict(stopReason="end_turn")))
