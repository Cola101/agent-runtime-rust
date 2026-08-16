#!/bin/sh

# Real JSONL MCP stdio fixture. It deliberately starts a grandchild so tests can
# prove the Runtime reaps a process tree rather than only its direct child.
if [ -n "${MCP_GRANDCHILD_PID_FILE:-}" ]; then
  (
    trap '' TERM
    while :; do
      sleep 1
    done
  ) </dev/null >/dev/null 2>&1 &
  echo "$!" > "$MCP_GRANDCHILD_PID_FILE"
fi
if [ -n "${MCP_GRANDCHILD_PID_LOG:-}" ]; then
  (
    trap '' TERM
    while :; do
      sleep 1
    done
  ) </dev/null >/dev/null 2>&1 &
  echo "$!" >> "$MCP_GRANDCHILD_PID_LOG"
fi
if [ -n "${MCP_START_MARKER:-}" ]; then
  printf 'started\n' >> "$MCP_START_MARKER"
fi

while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*)
      if [ -n "${MCP_FAIL_INITIALIZE_ATTEMPTS:-}" ] && [ -n "${MCP_START_MARKER:-}" ]; then
        start_count=$(wc -l < "$MCP_START_MARKER" | tr -d ' ')
        if [ "$start_count" -le "$MCP_FAIL_INITIALIZE_ATTEMPTS" ]; then
          exit 71
        fi
      fi
      if [ "${MCP_STALL_INITIALIZE:-}" = "1" ]; then
        sleep 30
        continue
      fi
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      capabilities_json=${MCP_SERVER_CAPABILITIES_JSON:-'{"tools":{}}'}
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":${capabilities_json},\"serverInfo\":{\"name\":\"stdio-test\",\"version\":\"1\"}}}"
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"ping"'*)
      if [ -n "${MCP_PING_MARKER:-}" ]; then
        printf 'ping\n' >> "$MCP_PING_MARKER"
      fi
      if [ "${MCP_STALL_PING:-}" = "1" ]; then
        sleep 30
        continue
      fi
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{}}"
      ;;
    *'"method":"resources/list"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resources\":[{\"uri\":\"kb://local/runbook\",\"name\":\"runbook\",\"mimeType\":\"text/markdown\",\"size\":5}],\"nextCursor\":\"resource-page-2\"}}"
      ;;
    *'"method":"resources/read"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"contents\":[{\"uri\":\"kb://local/runbook\",\"text\":\"local\"},{\"uri\":\"blob://local/a\",\"blob\":\"AAEC\"}]}}"
      ;;
    *'"method":"resources/templates/list"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resourceTemplates\":[{\"uriTemplate\":\"kb://local/{name}\",\"name\":\"knowledge\",\"mimeType\":\"text/markdown\"}],\"nextCursor\":\"template-page-2\"}}"
      ;;
    *'"method":"prompts/list"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"prompts\":[{\"name\":\"summarize\",\"description\":\"Summarize\",\"arguments\":[{\"name\":\"tone\",\"required\":false}]}],\"nextCursor\":\"prompt-page-2\"}}"
      ;;
    *'"method":"prompts/get"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"description\":\"resolved\",\"messages\":[{\"role\":\"user\",\"content\":{\"type\":\"text\",\"text\":\"Summarize this\"}}]}}"
      ;;
    *'"method":"tools/list"'*)
      if [ "${MCP_STALL_LIST:-}" = "1" ]; then
        sleep 30
        continue
      fi
      if [ -n "${MCP_LIST_MARKER:-}" ]; then
        printf 'listed\n' >> "$MCP_LIST_MARKER"
      fi
      if [ "${MCP_REVERSE_REQUEST_AT:-call}" = "list" ] && [ -n "${MCP_REVERSE_REQUEST_METHOD:-}" ]; then
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"reverse-stdio-1\",\"method\":\"${MCP_REVERSE_REQUEST_METHOD}\",\"params\":{}}"
        if IFS= read -r reverse_response; then
          if [ -n "${MCP_REVERSE_RESPONSE_MARKER:-}" ]; then
            printf '%s\n' "$reverse_response" > "$MCP_REVERSE_RESPONSE_MARKER"
          fi
        fi
      fi
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"tools\":[{\"name\":\"search\",\"description\":\"Return local runtime evidence\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}},\"required\":[\"query\"]}}]}}"
      if [ -n "${MCP_EXIT_AFTER_LIST_ATTEMPTS:-}" ] && [ -n "${MCP_START_MARKER:-}" ]; then
        start_count=$(wc -l < "$MCP_START_MARKER" | tr -d ' ')
        if [ "$start_count" -le "$MCP_EXIT_AFTER_LIST_ATTEMPTS" ]; then
          exit 72
        fi
      fi
      ;;
    *'"method":"tools/call"'*)
      id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
      progress_token=$(printf '%s' "$request" | sed -E 's/.*"progressToken":"([^"]+)".*/\1/')
      if [ -n "${MCP_CALL_MARKER:-}" ]; then
        printf 'called\n' >> "$MCP_CALL_MARKER"
      fi
      if [ "${MCP_REPORT_PROGRESS:-}" = "1" ]; then
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progressToken\":\"${progress_token}\",\"progress\":1,\"total\":2,\"message\":\"stdio work started\"}}"
      fi
      if [ "${MCP_STALL_CALL:-}" = "1" ]; then
        while IFS= read -r cancellation; do
          case "$cancellation" in
            *'"method":"notifications/cancelled"'*'"requestId":'"${id}"*)
              if [ -n "${MCP_CANCEL_MARKER:-}" ]; then
                printf 'cancelled\n' >> "$MCP_CANCEL_MARKER"
              fi
              exit 0
              ;;
          esac
        done
        exit 0
      fi
      if [ -n "${MCP_CALL_DELAY_SECONDS:-}" ]; then
        sleep "$MCP_CALL_DELAY_SECONDS"
      fi
      if [ "${MCP_REVERSE_REQUEST_AT:-call}" = "call" ] && [ -n "${MCP_REVERSE_REQUEST_METHOD:-}" ]; then
        printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"reverse-stdio-1\",\"method\":\"${MCP_REVERSE_REQUEST_METHOD}\",\"params\":{}}"
        if IFS= read -r reverse_response; then
          if [ -n "${MCP_REVERSE_RESPONSE_MARKER:-}" ]; then
            printf '%s\n' "$reverse_response" > "$MCP_REVERSE_RESPONSE_MARKER"
          fi
        fi
      fi
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"local mcp evidence\"}],\"isError\":false}}"
      ;;
  esac
done
