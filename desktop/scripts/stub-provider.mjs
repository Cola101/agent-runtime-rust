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

/// Picks a reply from the prompt, so a developer can drive the client into a
/// specific lifecycle state on purpose instead of waiting for one to happen.
function script(prompt, tools) {
  const asked = prompt.toLowerCase();
  const has = (name) => tools.some((tool) => tool?.function?.name === name);

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
    const parts = script(prompt, Array.isArray(body.tools) ? body.tools : []);
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
