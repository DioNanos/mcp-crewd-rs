#!/usr/bin/env python3
"""Minimal JSON-RPC driver for a fresh `crew mcp` stdio shim.

Use this when an already-mounted crew MCP shim has gone stale (e.g. the daemon
restarted and the shim shows `Broken pipe`): instead of relying on the dead
shim, this spawns a fresh `crew mcp` process, does the MCP initialize handshake,
and issues one or more tool calls, then exits.

Everything is parameterised — no paths are hardcoded. Point it at your runtime
dir, cell name and token file.

Examples:
  crew-mcp-driver.py --runtime-dir ~/.config/crewd/runtime \
    --cell coordinator --token-file ~/.config/crewd/coordinator.secret \
    list

  crew-mcp-driver.py ... spawn \
    --engine claude --profile max --cwd /path/to/project \
    --idem my-task-001 --task "Do the thing."

  crew-mcp-driver.py ... status <crewd_thread_id>
  crew-mcp-driver.py ... result <crewd_thread_id>
"""
import argparse, json, os, queue, subprocess, sys, threading


def rpc_session(crew_bin, runtime_dir, cell, token_file, calls, timeout=180):
    """Open one shim, initialize, run tools/call list; return result dicts."""
    p = subprocess.Popen(
        [crew_bin, "mcp", "--runtime-dir", runtime_dir,
         "--cell", cell, "--token-file", token_file],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1)
    outq = queue.Queue()
    threading.Thread(target=lambda: [outq.put(l) for l in p.stdout], daemon=True).start()

    def send(obj):
        p.stdin.write(json.dumps(obj) + "\n"); p.stdin.flush()

    def recv(want_id):
        while True:
            try:
                line = outq.get(timeout=timeout).strip()
            except queue.Empty:
                return None
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            if obj.get("id") == want_id:
                return obj

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "crew-mcp-driver", "version": "0"}}})
    if not recv(1):
        sys.stderr.write("initialize failed; stderr:\n" + p.stderr.read())
        sys.exit(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    results, nid = [], 10
    for name, args in calls:
        send({"jsonrpc": "2.0", "id": nid, "method": "tools/call",
              "params": {"name": name, "arguments": args}})
        results.append(recv(nid))
        nid += 1
    try:
        p.stdin.close(); p.terminate()
    except Exception:
        pass
    return results


def extract(res):
    if res is None:
        return {"_error": "no response"}
    if "error" in res:
        return {"_error": res["error"]}
    for c in res.get("result", {}).get("content", []):
        if c.get("type") == "text":
            try:
                return json.loads(c["text"])
            except Exception:
                return {"_text": c["text"]}
    return res.get("result", {})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--crew-bin", default="crew")
    ap.add_argument("--runtime-dir", required=True)
    ap.add_argument("--cell", required=True)
    ap.add_argument("--token-file", required=True)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    sp = sub.add_parser("spawn")
    sp.add_argument("--engine", default="claude")
    sp.add_argument("--profile")
    sp.add_argument("--cwd", required=True)
    sp.add_argument("--idem", required=True)
    sp.add_argument("--task", required=True)
    st = sub.add_parser("status"); st.add_argument("thread")
    rs = sub.add_parser("result"); rs.add_argument("thread")
    a = ap.parse_args()

    rt = os.path.expanduser(a.runtime_dir)
    tf = os.path.expanduser(a.token_file)
    common = (a.crew_bin, rt, a.cell, tf)

    if a.cmd == "list":
        calls = [("cell_list", {})]
    elif a.cmd == "spawn":
        args = {"engine": a.engine, "cwd": a.cwd, "task": a.task,
                "mode": "background", "idempotency_key": a.idem}
        if a.engine == "claude" and a.profile:
            args["profile"] = a.profile
        calls = [("cell_spawn", args)]
    elif a.cmd == "status":
        calls = [("cell_status", {"crewd_thread_id": a.thread})]
    elif a.cmd == "result":
        calls = [("cell_result", {"crewd_thread_id": a.thread})]

    [r] = rpc_session(*common, calls)
    print(json.dumps(extract(r), indent=2))


if __name__ == "__main__":
    main()
