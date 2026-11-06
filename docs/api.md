# API gateway

An HTTP server in front of `claude -p`. It speaks the **OpenAI chat completions** protocol, so existing SDKs and tools work by changing `base_url`. It also has a **native runs API** that streams tool steps.

The backend is Claude Code on your machine with your own login, so usage comes from your Claude subscription. No Anthropic API key is involved. Every request is a full Claude Code agent run with tool access in the profile's workspace (see [security.md](security.md)).

## Enable it

```bash
cc-gateway setup --api --profile api     # new API-only profile; prints key "default" once
# or, for an existing profile: set [api] enabled = true
cc-gateway profile edit <profile>
cc-gateway run <profile>                  # or: cc-gateway service install <profile>
```

```toml
[api]
enabled = true
bind = "127.0.0.1:8787"
max_concurrent = 4
```

The server listens on `http://127.0.0.1:8787` by default. If you bind to anything other than a loopback address, it logs and prints a warning at startup. Changes to `bind` and `max_concurrent` take effect on restart.

## Keys

```bash
cc-gateway key create <profile> <name>    # prints ccg_… once
cc-gateway key list <profile>
cc-gateway key revoke <profile> <name>
```

- Keys look like `ccg_` followed by 43 base62 characters. Only the sha256 hash, the name and the creation time are stored in `config.toml` (mode 0600).
- Send the key as `Authorization: Bearer ccg_…` (the scheme is case-insensitive). The key is checked before the request body is read.
- The server re-reads keys on every request, so creating or revoking a key works without a restart.
- Each key has its own memory namespace, `users/key-<name>/` (MEMORY.md and USER.md). All keys share the profile's `SOUL.md`. Key names use letters, digits, `-` and `_`.

## Endpoints

| Method | Path | Auth | |
|--------|------|------|-|
| GET | `/health` | no | `{"ok": true, "version": "0.1.0"}` |
| GET | `/v1/models` | yes | `opus`, `sonnet`, `haiku`, plus the profile's `default_model` if it's a full id |
| POST | `/v1/chat/completions` | yes | OpenAI chat completions, streaming or not |
| POST | `/v1/runs` | yes | Native run: `{prompt, session_id?, model?, stream?}` |
| DELETE | `/v1/runs/{id}` | yes | Stop a run started by the same key |

`model` takes any value `claude --model` accepts: `opus`, `sonnet`, `haiku` or a full model id. If you leave it out, the profile's `default_model` is used.

## Chat completions

### curl

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CCG_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "sonnet",
    "messages": [
      {"role": "system", "content": "Answer in one sentence."},
      {"role": "user", "content": "What is in the current directory?"}
    ]
  }'
```

### Python (openai)

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="ccg_...")

resp = client.chat.completions.create(
    model="sonnet",
    messages=[{"role": "user", "content": "Write a haiku about Rust."}],
)
print(resp.choices[0].message.content)

# streaming
stream = client.chat.completions.create(
    model="haiku",
    messages=[{"role": "user", "content": "Count to five."}],
    stream=True,
)
for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="", flush=True)
```

### JavaScript / TypeScript (openai)

```ts
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "http://127.0.0.1:8787/v1", apiKey: process.env.CCG_KEY });

const resp = await client.chat.completions.create({
  model: "sonnet",
  messages: [{ role: "user", content: "Hello!" }],
});
console.log(resp.choices[0].message.content);

const stream = await client.chat.completions.create({
  model: "sonnet",
  messages: [{ role: "user", content: "Tell me a short story." }],
  stream: true,
});
for await (const chunk of stream) process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
```

### How requests are mapped

- `system` and `developer` messages are appended to the system prompt, after SOUL.md and the key's USER.md and MEMORY.md.
- **Stateless by default**, as in the OpenAI API. A single user message is sent as-is. A longer conversation is rendered into the prompt as a transcript (`User: … / Assistant: …`) ending with "Reply to the last User message."
- Supported content parts: `text`, and `image_url` with a base64 `data:` URL (`data:image/png;base64,…`, strict base64). Images are saved to `workspace/inbox/key-<name>/`, referenced by path, and deleted when the run ends. With `X-CC-Session`, only images in the last user message are used. `http(s)://` image URLs are rejected with 400; the server never fetches remote URLs.
- Parameters such as `temperature`, `max_tokens` and `tools` are ignored, not rejected.

### Response

```json
{
  "id": "chatcmpl-3f2a…",
  "object": "chat.completion",
  "created": 1793938580,
  "model": "sonnet",
  "choices": [{"index": 0, "message": {"role": "assistant", "content": "…"}, "finish_reason": "stop"}],
  "usage": {"prompt_tokens": 45706, "completion_tokens": 8, "total_tokens": 45714, "total_cost_usd": 0.0074}
}
```

`prompt_tokens` counts input plus cache tokens, as reported by Claude Code; it is 0 if Claude doesn't report them. `total_cost_usd` is an extension field: the cost Claude Code computes for the run, which comes out of your subscription limits.

### Streaming

With `"stream": true` the response is Server-Sent Events:

```
data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],…}
data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Paris"},"finish_reason":null}],…}
data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{…},…}
data: [DONE]
```

Text arrives one Claude text block at a time, not token by token. When a run has several text blocks (for example before and after tool use), they are separated by a blank line. If the run fails after streaming has started, the error is sent as `data: {"error": {…}}` before `[DONE]`. Closing the connection stops the run.

## Sessions

To keep a real Claude Code session instead of resending the conversation:

1. Every response for a run that started includes the header `X-CC-Session: <session id>` (streaming responses too) and `X-CC-Run: <run id>`.
2. Send `X-CC-Session: <session id>` on the next request. The server resumes that session (`--resume`) and sends **only the last user message** from `messages`.

```bash
curl -i http://127.0.0.1:8787/v1/chat/completions -H "Authorization: Bearer $CCG_KEY" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Remember the number 42."}]}'
# → X-CC-Session: 4df4e193-…

curl http://127.0.0.1:8787/v1/chat/completions -H "Authorization: Bearer $CCG_KEY" \
  -H "X-CC-Session: 4df4e193-…" -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"What number?"}]}'
```

```python
raw = client.chat.completions.with_raw_response.create(model="sonnet", messages=[{"role": "user", "content": "Remember 42."}])
session = raw.headers["x-cc-session"]
client.chat.completions.create(
    model="sonnet",
    messages=[{"role": "user", "content": "What number?"}],
    extra_headers={"X-CC-Session": session},
)
```

Requests on the same session run one at a time. Requests without a session run in parallel, up to `max_concurrent`.

A session belongs to the key that created it. Resuming a session id that this key didn't create (unknown, another key's, or a Telegram chat's) returns 404 `session_not_found`.

## Native runs API

```bash
curl http://127.0.0.1:8787/v1/runs -H "Authorization: Bearer $CCG_KEY" \
  -H "Content-Type: application/json" \
  -d '{"prompt": "List the files here and summarize them", "model": "sonnet"}'
```

Body: `prompt` (required), `session_id` (optional, resumes that session), `model` (optional), `stream` (optional, default false).

Non-streaming response (also returns the `X-CC-Run` and `X-CC-Session` headers; `session_id` must be one this key created):

```json
{"run_id": "9c1e…", "text": "…", "session_id": "4df4…", "cost_usd": 0.012, "duration_ms": 5400, "is_error": false}
```

With `"stream": true` you get SSE with named events (`init` is always first; the response also has `X-CC-Run` and `X-CC-Session` headers):

| Event | Data |
|-------|------|
| `init` | `{"run_id", "session_id"}` |
| `tool` | `{"name": "Bash", "input": "ls -la"}` (short input) |
| `text` | `{"text": "…"}` |
| `result` | same object as the non-streaming response |
| `error` | `{"type", "message"}` |

```
event: init
data: {"run_id":"9c1e…","session_id":"4df4…"}

event: tool
data: {"input":"ls","name":"Bash"}

event: result
data: {"cost_usd":0.01,"duration_ms":5400,"is_error":false,"run_id":"9c1e…","session_id":"4df4…","text":"…"}
```

Stop a run (only the key that started it can stop it). This kills claude's whole process group:

```bash
curl -X DELETE http://127.0.0.1:8787/v1/runs/9c1e… -H "Authorization: Bearer $CCG_KEY"
# {"run_id":"9c1e…","stopped":true}
```

## Errors

Errors use the OpenAI shape:

```json
{"error": {"message": "invalid or revoked API key", "type": "authentication_error", "code": null}}
```

| Status | `type` | When |
|--------|--------|------|
| 400 | `invalid_request_error` | Bad JSON, no user message, remote `image_url`, bad `X-CC-Session` |
| 401 | `authentication_error` | Missing, unknown or revoked key |
| 404 | `session_not_found` / `not_found` | Session not created by this key / unknown endpoint or no such run for this key |
| 405 | `invalid_request_error` | Wrong HTTP method for the endpoint |
| 409 | `cancelled` | The run was stopped |
| 413 | `invalid_request_error` | Body larger than 20 MB |
| 429 | `rate_limit_error` | More than `max_concurrent` requests queued or running |
| 500 | `server_error` | claude couldn't run (see `cc-gateway doctor`) |
| 502 | `api_error` | claude returned an error |
| 504 | `timeout` | Run exceeded `run_timeout_secs` |
