#!/usr/bin/env ruby
# frozen_string_literal: true
#
# Two-turn OpenAI-compatible provider that drives one `shell.exec` call whose
# command is a containment probe.
#
# Why the command is the probe: `sandbox-exec` receives its policy as argv, and
# that argv lives for milliseconds. Two earlier evidence files had to record
# "the live Worker's containment arguments were never captured" as unproven.
# A command running *inside* the container can answer the question directly.
#
# Every credential access is redirected to /dev/null and only a verdict string
# is printed, so no credential content can reach stdout, the event log, or any
# evidence file even if containment were broken.

require "json"
require "socket"

TOOL_NAME = "shell.exec"
TOOL_CALL_ID = "call_shell_probe_1"

# Only directories that exist can prove anything: a denied path that does not
# exist reports ENOENT, which is indistinguishable from a path that was never
# denied. Measured, not assumed.
CREDENTIAL_DIRECTORIES = [".ssh", ".aws", ".gnupg", ".config/gh"].freeze

def existing_credential_directories
  home = Dir.home
  CREDENTIAL_DIRECTORIES
    .map { |relative| File.join(home, relative) }
    .select { |path| File.directory?(path) }
end

def probe_command(credential_directories)
  checks = credential_directories.each_with_index.map do |path, index|
    <<~SH
      if ls #{path.inspect} >/dev/null 2>&1; then echo "cred#{index}_list=READABLE"; else echo "cred#{index}_list=DENIED"; fi
      if cat #{File.join(path, '*').inspect} >/dev/null 2>&1; then echo "cred#{index}_read=READABLE"; else echo "cred#{index}_read=DENIED"; fi
    SH
  end
  <<~SH
    echo "cwd=$(pwd)"
    echo "home=$HOME"
    echo "key=[${AGENT_RUNTIME_PROVIDER_API_KEY:-unset}]"
    #{checks.join}
    if echo escaped > /tmp/agent-shell-gate-escape 2>/dev/null; then echo tmp_write=ALLOWED; else echo tmp_write=DENIED; fi
    echo "workspace_write=$(echo ok > shell-gate-ran.txt && cat shell-gate-ran.txt)"
    echo "credentials_probed=#{credential_directories.length}"
    echo probe_done
  SH
end

def read_request(socket)
  headers = {}
  socket.gets or raise "empty request"
  while (line = socket.gets)
    break if line == "\r\n"

    name, _, value = line.partition(":")
    headers[name.strip.downcase] = value.strip
  end
  length = Integer(headers.fetch("content-length", "0"), 10)
  [headers, JSON.parse(length.positive? ? socket.read(length) : "{}")]
end

def http(payload)
  "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n" \
    "Content-Length: #{payload.bytesize}\r\nConnection: close\r\n\r\n#{payload}"
end

def turn_one(command)
  chunk = {
    "choices" => [{
      "index" => 0,
      "delta" => {
        "tool_calls" => [{
          "index" => 0,
          "id" => TOOL_CALL_ID,
          "function" => { "name" => TOOL_NAME, "arguments" => JSON.generate("command" => command) }
        }]
      },
      "finish_reason" => "tool_calls"
    }],
    "usage" => { "prompt_tokens" => 20, "completion_tokens" => 8 }
  }
  "data: #{JSON.generate(chunk)}\n\ndata: [DONE]\n\n"
end

def turn_two
  chunk = {
    "choices" => [{
      "index" => 0,
      "delta" => { "content" => "containment probe complete" },
      "finish_reason" => "stop"
    }],
    "usage" => { "prompt_tokens" => 40, "completion_tokens" => 5 }
  }
  "data: #{JSON.generate(chunk)}\n\ndata: [DONE]\n\n"
end

port = Integer(ENV.fetch("AGENT_RUNTIME_TEST_PROVIDER_PORT"), 10)
bearer = ENV.fetch("AGENT_RUNTIME_TEST_PROVIDER_API_KEY")
ready_file = ENV.fetch("AGENT_RUNTIME_TEST_PROVIDER_READY_FILE")
evidence_file = ENV.fetch("AGENT_RUNTIME_TEST_PROVIDER_EVIDENCE_FILE")

credential_directories = existing_credential_directories
command = probe_command(credential_directories)

server = TCPServer.new("127.0.0.1", port)
File.write(ready_file, "#{port}\n", mode: "w", perm: 0o600)
turns = []
begin
  [turn_one(command), turn_two].each_with_index do |payload, index|
    socket = server.accept
    begin
      headers, body = read_request(socket)
      raise "provider authorization failed" unless headers["authorization"] == "Bearer #{bearer}"

      tools = Array(body["tools"]).map { |tool| tool.dig("function", "name") }.compact
      tool_result = body["messages"].find { |message| message["role"] == "tool" }
      turns << {
        "turn" => index + 1,
        "advertised_tools" => tools,
        "shell_offered" => tools.include?(TOOL_NAME),
        "file_tools_leaked" => tools.any? { |tool| tool.start_with?("workspace.") },
        "tool_result" => tool_result && JSON.parse(tool_result.fetch("content"))
      }
      socket.write(http(payload))
    ensure
      socket.close
    end
  end
  File.write(
    evidence_file,
    JSON.pretty_generate("credentials_probed" => credential_directories.length, "turns" => turns),
    mode: "w",
    perm: 0o600
  )
ensure
  server.close
  File.unlink(ready_file) if File.exist?(ready_file)
end
