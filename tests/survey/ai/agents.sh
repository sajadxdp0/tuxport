# AI coding CLIs (docs/rounds/103-ai-cli-survey.md): Claude Code, Codex and Gemini CLI without
# accounts; the mock server below poses as the three vendors' model APIs and asks the CLIs, in a
# fixed few steps, to call tools (run commands, write, read and edit files, grep, glob), printing
# each step's tool result; then the two vendors' bubblewrap sandboxes, and the interactive UIs in
# tmux (plus opencode, GitHub Copilot CLI, Crush and Qwen Code, for versions and UIs only). Runs in
# the Alpine and Debian stores and in native containers, installing missing packages with apk or
# apt; npm needs the network. Each item is preceded by "== name"
export LANG=C.UTF-8 HOME=/root PATH=/usr/local/bin:$PATH SHELL=/bin/sh
need="nodejs npm python3 git ripgrep tmux bubblewrap socat curl bash"
if command -v apk >/dev/null; then
    apk add -q $need 2>&1 | tail -2
else
    apt-get update -q >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends $need \
        ca-certificates procps 2>&1 | tail -1
fi
echo "node $(node --version), npm $(npm --version)"
W=/tmp/ai
cat > /tmp/mock_llm.py <<'PY'
"""Mocks the three vendors' model APIs, so AI coding CLIs can complete a round of conversation
with tools without accounts: Anthropic Messages (Claude Code), OpenAI Responses (Codex), Gemini
(Gemini CLI).

The main conversation (requests carrying the tool that runs commands) asks for the tool calls in
STEPS in turn, and answers "all steps done" once all tool results are back; other requests
(making up a title, judging the topic and the like) get "ok". Each tool result is appended to LOG,
which the survey prints to show each tool's result in Tuxport. Each request's path and tool names
are recorded too.

    python3 mock_llm.py port log workdir
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG, WORK = int(sys.argv[1]), sys.argv[2], sys.argv[3]

# (action, argument): shell is the same for all three; file actions are converted to each vendor's
# tool and parameter names
STEPS = [
    ("shell", f"echo shell-ok; expr 6 '*' 7; uname -sr; cd {WORK} && pwd"),
    ("write", (f"{WORK}/hello.py", "print('hi from python')\n")),
    ("read", f"{WORK}/hello.py"),
    ("edit", (f"{WORK}/hello.py", "hi from", "edited by")),
    ("shell", f"python3 {WORK}/hello.py"),
    ("grep", ("edited", WORK)),
    ("glob", ("**/*.py", WORK)),
    ("shell", "sleep 1 & wait; echo background-ok; git --version"),
]


def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")


def anthropic_call(step):
    kind, a = step
    return {
        "shell": lambda: ("Bash", {"command": a, "description": "run"}),
        "write": lambda: ("Write", {"file_path": a[0], "content": a[1]}),
        "read": lambda: ("Read", {"file_path": a}),
        "edit": lambda: ("Edit", {"file_path": a[0], "old_string": a[1], "new_string": a[2]}),
        "grep": lambda: ("Grep", {"pattern": a[0], "path": a[1], "output_mode": "content"}),
        "glob": lambda: ("Glob", {"pattern": a[0], "path": a[1]}),
    }[kind]()


def codex_call(step, tools):
    """Codex only offers shell and apply_patch: file actions become commands."""
    kind, a = step
    cmd = {
        "shell": lambda: a,
        "write": lambda: f"cat > {a[0]} <<'EOF'\n{a[1]}EOF\necho wrote",
        "read": lambda: f"cat {a}",
        "edit": None,
        "grep": lambda: f"rg -n {a[0]} {a[1]}",
        "glob": lambda: f"rg --files -g '{a[0]}' {a[1]}",
    }[kind]
    if cmd is None:
        path, old, new = a
        if "apply_patch" in tools:
            patch = (f"*** Begin Patch\n*** Update File: {path}\n@@\n-print('{old} python')\n"
                     f"+print('{new} python')\n*** End Patch\n")
            if tools["apply_patch"].get("type") == "custom":
                return "apply_patch", patch, True
            return "apply_patch", json.dumps({"input": patch}), False
        cmd = lambda: f"sed -i 's/{old}/{new}/' {path} && echo edited"
    cmd = cmd()
    for name in ("exec_command", "shell", "local_shell", "shell_command"):
        if name in tools:
            params = tools[name].get("parameters", {}).get("properties", {})
            if "cmd" in params:
                args = {"cmd": cmd}
            elif params.get("command", {}).get("type") == "string":
                args = {"command": cmd}
            else:
                args = {"command": ["bash", "-lc", cmd]}
            if "workdir" in params:
                args["workdir"] = WORK
            return name, json.dumps(args), False
    raise KeyError(f"no shell tool in {list(tools)}")


def gemini_call(step, decls):
    """Fills in by the function and parameter names in the declaration: versions differ in
    parameter names (absolute_path, file_path…).
    """
    kind, a = step
    names = {
        "shell": ["run_shell_command"],
        "write": ["write_file"],
        "read": ["read_file"],
        "edit": ["replace", "edit"],
        "grep": ["grep_search", "search_file_content", "grep"],
        "glob": ["glob"],
    }[kind]
    name = next((n for n in names if n in decls), None)
    if name is None:
        name, kind, a = "run_shell_command", "shell", {"read": f"cat {a}"}.get(kind, str(a))
    props = decls[name]
    pick = lambda *cands: next((c for c in cands if c in props), cands[0])
    if kind == "shell":
        args = {pick("command"): a}
    elif kind == "write":
        args = {pick("file_path", "absolute_path", "path"): a[0], pick("content"): a[1]}
    elif kind == "read":
        args = {pick("absolute_path", "file_path", "path"): a}
    elif kind == "edit":
        args = {pick("file_path", "absolute_path"): a[0], pick("old_string"): a[1],
                pick("new_string"): a[2]}
        if "instruction" in props:
            args["instruction"] = "edit the greeting"
    else:
        args = {pick("pattern"): a[0], pick("path", "dir_path"): a[1]}
    return name, args


def fake_json(schema):
    """Builds a valid value from a JSON schema (Gemini's type names are upper case): other
    requests (model routing's classification) need it.
    """
    ty = str(schema.get("type", "object")).lower()
    if schema.get("enum"):
        return schema["enum"][0]
    if ty == "object":
        return {k: fake_json(v) for k, v in schema.get("properties", {}).items()}
    if ty == "array":
        return [fake_json(schema.get("items", {}))]
    if ty in ("integer", "number"):
        return schema.get("minimum", 1)
    if ty == "boolean":
        return False
    return "ok"


def sse(events, named=True):
    out = ""
    for e in events:
        if named:
            out += f"event: {e['type']}\n"
        out += f"data: {json.dumps(e)}\n\n"
    return out.encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        log(f"GET {self.path}")
        if self.path.split("?")[0].endswith("/models"):
            return self.reply(200, b'{"data": [], "models": [], "object": "list"}')
        self.reply(404, b'{"error": {"type": "not_found_error", "message": "mock"}}')

    def do_HEAD(self):
        self.reply(200, b"")

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        path = self.path.split("?")[0]
        if path.endswith("/v1/messages"):
            return self.anthropic(body)
        if path.endswith("/responses"):
            return self.openai_responses(body)
        if ":streamGenerateContent" in path or ":generateContent" in path:
            return self.gemini(body, ":stream" in path)
        if path.endswith("count_tokens") or path.endswith(":countTokens"):
            return self.reply(200, b'{"input_tokens": 10, "totalTokens": 10}')
        log(f"POST {self.path} (unknown)")
        self.reply(404, b'{"error": {"type": "not_found_error", "message": "mock"}}')

    def step(self, api, done, results, tools):
        if done and results:
            log(f"{api} result {done}: {results[-1][:400]!r}")
        if done < len(STEPS):
            log(f"{api} step {done + 1}: {STEPS[done][0]}")
            return STEPS[done]
        return None

    # ---- Anthropic Messages ----
    def anthropic(self, body):
        tools = [t.get("name") for t in body.get("tools", [])]
        blocks = [b for m in body.get("messages", []) if isinstance(m.get("content"), list)
                  for b in m["content"] if b.get("type") == "tool_result"]

        def text(b):
            c = b.get("content")
            s = " ".join(x.get("text", "") for x in c if isinstance(x, dict)) if isinstance(c, list) else str(c)
            return ("ERROR " if b.get("is_error") else "") + s

        main = "Bash" in tools
        step = self.step("anthropic", len(blocks), [text(b) for b in blocks], tools) if main else None
        if main and not blocks:
            log(f"anthropic tools: {tools}")
        model = body.get("model", "claude")
        if step:
            name, args = anthropic_call(step)
            block = {"type": "tool_use", "id": f"toolu_{len(blocks) + 1:04d}", "name": name, "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(args)}
            stop, full = "tool_use", dict(block, input=args)
        else:
            t = "all steps done" if main else "ok"
            block, delta = {"type": "text", "text": ""}, {"type": "text_delta", "text": t}
            stop, full = "end_turn", {"type": "text", "text": t}
        usage = {"input_tokens": 10, "output_tokens": 5}
        msg = {"id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [],
               "stop_reason": None, "stop_sequence": None, "usage": usage}
        if not body.get("stream"):
            return self.reply(200, json.dumps(dict(msg, content=[full], stop_reason=stop)).encode())
        self.reply(200, sse([
            {"type": "message_start", "message": msg},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
             "usage": {"output_tokens": 5}},
            {"type": "message_stop"},
        ]), "text/event-stream")

    # ---- OpenAI Responses ----
    def openai_responses(self, body):
        tools = {t.get("name") or t.get("type"): t for t in body.get("tools", [])}
        outputs = [i for i in body.get("input", []) if isinstance(i, dict)
                   and i.get("type") in ("function_call_output", "custom_tool_call_output")]
        texts = [str(o.get("output"))[:400] for o in outputs]
        main = any(n in tools for n in ("shell", "exec_command", "local_shell", "shell_command"))
        if main and not outputs:
            log(f"codex tools: {list(tools)}")
        step = self.step("codex", len(outputs), texts, tools) if main else None
        n = len(outputs) + 1
        if step:
            name, args, custom = codex_call(step, tools)
            if custom:
                item = {"type": "custom_tool_call", "id": f"ctc_{n}", "call_id": f"call_{n}",
                        "name": name, "input": args, "status": "completed"}
            else:
                item = {"type": "function_call", "id": f"fc_{n}", "call_id": f"call_{n}",
                        "name": name, "arguments": args, "status": "completed"}
        else:
            t = "all steps done" if main else "ok"
            item = {"type": "message", "id": f"msg_{n}", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": t, "annotations": []}]}
        resp = {"id": f"resp_{n}", "object": "response", "status": "completed", "output": [item],
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
                          "total_tokens": 15}}
        if not body.get("stream"):
            return self.reply(200, json.dumps(resp).encode())
        self.reply(200, sse([
            {"type": "response.created", "response": dict(resp, status="in_progress", output=[])},
            {"type": "response.output_item.added", "output_index": 0, "item": item},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": resp},
        ]), "text/event-stream")

    # ---- Gemini ----
    def gemini(self, body, stream):
        decls = {}
        for t in body.get("tools", []):
            for d in t.get("functionDeclarations", []):
                schema = d.get("parameters") or d.get("parametersJsonSchema") or {}
                decls[d["name"]] = schema.get("properties", {})
        parts = [p for c in body.get("contents", []) for p in c.get("parts", [])
                 if "functionResponse" in p]
        texts = [json.dumps(p["functionResponse"].get("response"))[:400] for p in parts]
        main = "run_shell_command" in decls
        if main and not parts:
            log(f"gemini tools: {sorted(decls)}")
        step = self.step("gemini", len(parts), texts, decls) if main else None
        config = body.get("generationConfig", {})
        schema = config.get("responseSchema") or config.get("responseJsonSchema")
        if step:
            name, args = gemini_call(step, decls)
            part = {"functionCall": {"name": name, "args": args}}
        elif not main and schema:
            part = {"text": json.dumps(fake_json(schema))}
        else:
            part = {"text": "all steps done" if main else "ok"}
        resp = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP",
                                "index": 0}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
                "modelVersion": "gemini-mock"}
        if not stream:
            return self.reply(200, json.dumps(resp).encode())
        self.reply(200, sse([resp], named=False), "text/event-stream")


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
PY
python3 /tmp/mock_llm.py 18080 /tmp/mock.log $W &
mock=$!
M=http://127.0.0.1:18080

# Install one at a time: installing all seven at once with every one hitting the cache makes npm
# unpack several big packages at the same time, peaking above the app's memory limit on the
# development VM (2 GB)
for p in @anthropic-ai/claude-code @openai/codex @google/gemini-cli opencode-ai @github/copilot \
    @charmland/crush @qwen-code/qwen-code; do
    s=$(date +%s)
    npm install -g --no-audit --no-fund --loglevel=error $p >/tmp/npm.out 2>&1
    echo "npm install $p: $? ($(($(date +%s) - s))s)"
    tail -n 2 /tmp/npm.out
done
echo "memory peak $(($(cat /sys/fs/cgroup/memory.peak) >> 20)) MiB"
for c in claude codex gemini opencode copilot crush qwen; do
    echo "$c: $($c --version 2>&1 | head -n 1)"
done

# Run one item: empty the work directory and the mock server's log; print the exit code, the time,
# the last lines of the output and the log
case_() {
    name=$1
    shift
    rm -rf $W && mkdir -p $W && cd $W && : >/tmp/mock.log
    echo "== $name"
    s=$(date +%s)
    timeout 300 "$@" </dev/null >/tmp/case.out 2>&1
    echo "exit $? ($(($(date +%s) - s))s)"
    tail -n 4 /tmp/case.out
    cut -c1-300 /tmp/mock.log
    cd /
}

export ANTHROPIC_BASE_URL=$M ANTHROPIC_API_KEY=sk-ant-mock CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
case_ claude claude -p "do the steps" --allowedTools "Bash Read Write Edit Grep Glob"

mkdir -p $HOME/.codex
cat >$HOME/.codex/config.toml <<EOF
model = "gpt-mock"
model_provider = "mock"
[model_providers.mock]
name = "mock"
base_url = "$M/v1"
wire_api = "responses"
env_key = "MOCK_API_KEY"
EOF
export MOCK_API_KEY=mock
case_ codex codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox "do the steps"

# The auth method must be chosen beforehand; --yolo has no effect when the current directory is not
# trusted
mkdir -p $HOME/.gemini
echo '{"security": {"auth": {"selectedType": "gemini-api-key"}, "folderTrust": {"enabled": false}}}' \
    >$HOME/.gemini/settings.json
export GOOGLE_GEMINI_BASE_URL=$M GEMINI_API_KEY=mock
case_ gemini gemini -p "do the steps" --yolo

# Sandboxes: both vendors use bubblewrap. Native docker containers need extra permissions to create
# namespaces (SURVEY_DOCKER_OPTS, docs/rounds/123-bwrap.md)
case_ codex-sandbox codex exec --skip-git-repo-check -s workspace-write "do the steps"
echo "== codex sandbox"
codex sandbox -- sh -c 'echo inside' 2>&1 | tail -n 3
echo "== bwrap"
bwrap --ro-bind / / --dev /dev --proc /proc true 2>&1 | tail -n 2
mkdir -p $HOME/.claude
echo '{"sandbox": {"enabled": true}}' >$HOME/.claude/settings.json
case_ claude-sandbox claude -p "do the steps" --allowedTools "Bash Read Write Edit Grep Glob"
rm $HOME/.claude/settings.json

# Interactive UIs: only one open at a time (several at once exceed the app's memory limit in
# Tuxport); wait a while in tmux and take a screenshot, then see how much memory all processes used
# together (cgroup's memory.current: the app's usage in Tuxport, the container's natively). Codex's
# background service cannot read process start times on Alpine (natively too), so use --no-daemon
T="tmux -S /tmp/tmux-ai"
mkdir -p $W
for c in claude "codex --no-daemon" gemini opencode copilot crush qwen; do
    $T kill-server 2>/dev/null
    sleep 2
    base=$(($(cat /sys/fs/cgroup/memory.current) >> 20))
    $T new-session -d -s s -x 110 -y 32 -c $W -e TERM=xterm-256color "$c"
    $T set -g remain-on-exit on
    sleep 20
    used=$(($(cat /sys/fs/cgroup/memory.current) >> 20))
    echo "== tui ${c%% *}: $($T display -p -t s:0 '#{pane_current_command} dead=#{pane_dead}'), memory $base -> $used MiB"
    $T capture-pane -p -t s:0 | grep -v '^ *$' | head -n 10 | cut -c1-110
done
$T kill-server
kill $mock
