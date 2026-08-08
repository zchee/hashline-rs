#!/bin/sh
# PreToolUse hook: route text reads to the hashline MCP server while keeping
# binary formats (images, PDFs) and notebooks on the built-in Read tool.
#
# Input: the PreToolUse JSON on stdin. Output: a deny decision on stdout, or
# nothing (exit 0) to let the normal permission flow decide. Requires jq.
#
# See docs/claude-code.md in hashline-rs for the full replacement recipe.

file_path=$(jq -r '.tool_input.file_path // empty')

case "$file_path" in
  '' | *.png | *.PNG | *.jpg | *.JPG | *.jpeg | *.JPEG | *.gif | *.GIF | \
  *.webp | *.WEBP | *.bmp | *.BMP | *.ico | *.ICO | *.pdf | *.PDF | *.ipynb)
    # No path, a binary format, or a notebook: the built-in Read renders
    # these and hashline's strict-UTF-8 contract deliberately does not.
    exit 0
    ;;
  *)
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: "Text files are read through the hashline server in this project: call mcp__hashline__read with {\"path\": \"...\"} to get the snapshot and LINE@BYTE positions that mcp__hashline__edit and mcp__hashline__write require. For binary files, Bash tools like xxd remain available."
      }
    }'
    ;;
esac
