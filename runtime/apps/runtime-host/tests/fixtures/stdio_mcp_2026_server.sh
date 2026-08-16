#!/bin/sh

# Stateless MCP 2026-07-28 JSONL fixture. Each process accepts either the
# initial Tool round or a continuation so host replacement is part of the test.
while IFS= read -r request; do
  if [ -n "${MCP_REQUEST_LOG:-}" ]; then
    printf '%s\n' "$request" >> "$MCP_REQUEST_LOG"
  fi
  id=$(printf '%s' "$request" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$request" in
    *'"method":"server/discover"'*)
      case "$request" in
        *'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;;
        *) exit 81 ;;
      esac
      case "$request" in
        *'"elicitation"'*) ;;
        *) exit 86 ;;
      esac
      if [ "${MCP_READ_SURFACES:-}" = "1" ]; then
        capabilities_json='{"resources":{},"prompts":{}}'
      else
        capabilities_json='{"tools":{}}'
      fi
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":${capabilities_json}}}"
      ;;
    *'"method":"resources/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"resources\":[{\"uri\":\"kb://modern-stdio/runbook\",\"name\":\"runbook\"}],\"nextCursor\":\"modern-resource-2\"}}"
      ;;
    *'"method":"resources/read"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"contents\":[{\"uri\":\"kb://modern-stdio/runbook\",\"text\":\"modern stdio\"}]}}"
      ;;
    *'"method":"resources/templates/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"resourceTemplates\":[{\"uriTemplate\":\"kb://modern-stdio/{name}\",\"name\":\"knowledge\"}]}}"
      ;;
    *'"method":"prompts/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"prompts\":[{\"name\":\"summarize\"}]}}"
      ;;
    *'"method":"prompts/get"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"messages\":[{\"role\":\"user\",\"content\":{\"type\":\"text\",\"text\":\"modern stdio prompt\"}}]}}"
      ;;
    *'"method":"tools/list"'*)
      case "$request" in
        *'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;;
        *) exit 82 ;;
      esac
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"tools\":[{\"name\":\"confirm_search\",\"description\":\"Search after confirmation\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}},\"required\":[\"query\"]}}]}}"
      ;;
    *'"method":"tools/call"'*'"inputResponses"'*)
      case "$request" in
        *'"requestState":"stdio-state"'*) ;;
        *) exit 83 ;;
      esac
      case "$request" in
        *'"action":"accept"'*'"confirmed":true'*) ;;
        *) exit 87 ;;
      esac
      if [ -n "${MCP_CALL_MARKER:-}" ]; then
        printf 'continued\n' >> "$MCP_CALL_MARKER"
      fi
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"complete\",\"content\":[{\"type\":\"text\",\"text\":\"modern stdio approved\"}],\"isError\":false}}"
      ;;
    *'"method":"tools/call"'*)
      case "$request" in
        *'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;;
        *) exit 84 ;;
      esac
      if [ -n "${MCP_CALL_MARKER:-}" ]; then
        printf 'started\n' >> "$MCP_CALL_MARKER"
      fi
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"resultType\":\"input_required\",\"requestState\":\"stdio-state\",\"inputRequests\":{\"approval\":{\"method\":\"elicitation/create\",\"params\":{\"mode\":\"form\",\"message\":\"Approve stdio search\",\"requestedSchema\":{\"type\":\"object\",\"properties\":{\"confirmed\":{\"type\":\"boolean\"}},\"required\":[\"confirmed\"]}}}}}}"
      ;;
    *) exit 85 ;;
  esac
done
