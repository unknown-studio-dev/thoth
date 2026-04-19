# Security Policy

## Supported Versions

Thoth follows semantic versioning. Security fixes land on the current major version line.

| Version         | Supported |
| --------------- | --------- |
| 0.x (current)   | Yes       |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

If you believe you have found a security vulnerability, please report it privately using **GitHub Private Vulnerability Reporting**:

1. Open the [Security tab](https://github.com/unknown-studio-dev/thoth/security) of this repository.
2. Click **Advisories** → **Report a vulnerability**.
3. Fill in the form with the details below.

### What to include

- A descriptive summary of the vulnerability.
- Steps to reproduce (including proof-of-concept scripts or specific inputs).
- Affected version(s) and platform(s).
- Potential impact and severity.

### What to expect

- We aim to acknowledge receipt within 48 hours.
- We will triage the issue and keep you updated on progress toward a patch.
- Once resolved, we will publish a security advisory and credit you (if you wish).

## Scope

Security-relevant areas in Thoth include:

- **MCP server** (`thoth-mcp`) — handles untrusted input from LLM tool calls.
- **Markdown store** — parses user-editable files; injection through crafted headings/tags.
- **Gate binary** (`thoth-gate`) — enforcement decisions that block/allow tool calls.
- **Query pipeline** — prompt injection through `thoth_recall` query strings.
- **Domain adapters** (Notion/Asana/NotebookLM) — ingest external data with PII redaction.
