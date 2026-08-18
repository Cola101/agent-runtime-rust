// A stand-in model provider, on loopback, for developing the desktop client.
//
// The runtime is real in this setup and the model is not. That distinction has
// to survive all the way to the screen: a run driven by this server produces
// genuine events, genuine lifecycle boundaries and genuine approvals, but the
// text inside them was chosen by a switch statement. The launcher writes a
// marker file so the shell can say so on the status line rather than letting a
// scripted answer pass for a model's.
//
// No credentials, no network egress, no vendor. It speaks the OpenAI-compatible
// streaming shape the model gateway parses in `openai_compatible.rs`.
import http from "node:http";

const port = Number(process.argv[2] ?? 0);

function sse(parts) {
  return parts.map((part) => `data: ${JSON.stringify(part)}\n\n`).join("") + "data: [DONE]\n\n";
}

function textChunks(text) {
  // Deltas rather than one blob, so the client exercises the streaming path.
  const words = text.split(" ");
  return words.map((word, index) => ({
    choices: [{ index: 0, delta: { content: index === 0 ? word : ` ${word}` } }],
  }));
}

function done(reason) {
  return { choices: [{ index: 0, delta: {}, finish_reason: reason }] };
}

function usage(input, output) {
  return {
    choices: [{ index: 0, delta: {} }],
    usage: { prompt_tokens: input, completion_tokens: output },
  };
}

function callChunk(name, args) {
  return {
    choices: [{
      index: 0,
      delta: {
        tool_calls: [{
          index: 0,
          id: `stub-call-${name}`,
          function: { name, arguments: JSON.stringify(args) },
        }],
      },
    }],
  };
}

/// Where the last `process.*` result left the session.
///
/// The tools are cursor-driven: every call but `start` and `attach` takes the
/// `stdout_cursor` the previous result ended at, and passing a stale one is how
/// a real agent re-reads bytes it already has. Reading them back out of the
/// transcript is what makes this stub drive a session rather than replay a
/// fixed script at one.
function lastSession(messages) {
  const results = messages.filter((message) => message.role === "tool");
  for (let at = results.length - 1; at >= 0; at -= 1) {
    try {
      const body = JSON.parse(results[at].content);
      if (typeof body?.session_id === "string") {
        return {
          count: results.length,
          session_id: body.session_id,
          stdout_cursor: body.stdout_cursor ?? 0,
          stderr_cursor: body.stderr_cursor ?? 0,
        };
      }
    } catch {
      // A tool result that is not this shape belongs to another tool.
    }
  }
  return { count: results.length, session_id: null, stdout_cursor: 0, stderr_cursor: 0 };
}

/// One durable process session, driven the way an agent drives one.
///
/// Worth doing here rather than describing in a README: the process surface
/// cannot be looked at without a session in a durable log, and this is the only
/// way to get one on a machine with no vendor account.
function processScript(messages) {
  const at = lastSession(messages);
  const cursors = {
    session_id: at.session_id,
    stdout_cursor: at.stdout_cursor,
    stderr_cursor: at.stderr_cursor,
  };
  switch (at.count) {
    case 0:
      return [
        ...textChunks("Starting a session on a terminal."),
        callChunk("process.start", {
          initial_stdin: "echo hello-from-session\n",
          tty: true, cols: 100, rows: 30, yield_time_ms: 2000,
        }),
        usage(210, 30), done("tool_calls"),
      ];
    case 1:
      return [
        callChunk("process.write", { ...cursors, stdin: "date +%Y\n", yield_time_ms: 2000 }),
        usage(240, 26), done("tool_calls"),
      ];
    case 2:
      return [callChunk("process.poll", cursors), usage(260, 22), done("tool_calls")];
    // A bounded tail read. Small on purpose: when the session has written more
    // than this, the result starts past the cursor the polls reached, and the
    // client has to say that the bytes in between never entered the log.
    case 3:
      return [
        callChunk("process.attach", { session_id: at.session_id, max_bytes: 64 }),
        usage(280, 22), done("tool_calls"),
      ];
    case 4:
      return [callChunk("process.close", cursors), usage(300, 22), done("tool_calls")];
    default:
      return [
        ...textChunks(
          "Session closed. Everything above is what the process tools actually returned.",
        ),
        usage(320, 30), done("stop"),
      ];
  }
}

/// Picks a reply from the prompt, so a developer can drive the client into a
/// specific lifecycle state on purpose instead of waiting for one to happen.
function script(prompt, tools, messages) {
  const asked = prompt.toLowerCase();
  const has = (name) => tools.some((tool) => tool?.function?.name === name);

  if ((asked.includes("process") || asked.includes("进程") || asked.includes("会话"))
      && has("process.start")) {
    return processScript(messages);
  }

  if ((asked.includes("shell") || asked.includes("命令")) && has("shell.exec")) {
    return [
      ...textChunks("I need to run a command to answer that."),
      {
        choices: [{
          index: 0,
          delta: {
            tool_calls: [{
              index: 0,
              id: "stub-call-1",
              function: { name: "shell.exec", arguments: JSON.stringify({ command: "ls -la" }) },
            }],
          },
        }],
      },
      usage(180, 24),
      done("tool_calls"),
    ];
  }

  if ((asked.includes("read") || asked.includes("读")) && has("workspace.read_text")) {
    return [
      ...textChunks("Let me look at that file."),
      {
        choices: [{
          index: 0,
          delta: {
            tool_calls: [{
              index: 0,
              id: "stub-call-1",
              function: { name: "workspace.read_text", arguments: JSON.stringify({ path: "notes.txt" }) },
            }],
          },
        }],
      },
      usage(160, 20),
      done("tool_calls"),
    ];
  }

  return [
    ...textChunks(
      "This reply came from the local stub provider, not from a model. " +
      "The run around it is real: real event log, real lifecycle boundary, real cost accounting.",
    ),
    usage(120, 32),
    done("stop"),
  ];
}

const server = http.createServer((request, response) => {
  if (request.method !== "POST") {
    response.writeHead(405).end();
    return;
  }
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    let body = {};
    try {
      body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    } catch {
      response.writeHead(400).end();
      return;
    }
    const messages = Array.isArray(body.messages) ? body.messages : [];
    const last = [...messages].reverse().find((message) => message.role === "user");
    const prompt = typeof last?.content === "string" ? last.content : "";
    const parts = script(prompt, Array.isArray(body.tools) ? body.tools : [], messages);
    const payload = sse(parts);
    response.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "close",
      "content-length": Buffer.byteLength(payload),
    });
    response.end(payload);
  });
});

server.listen(port, "127.0.0.1", () => {
  const address = server.address();
  process.stdout.write(`${address.port}\n`);
});
