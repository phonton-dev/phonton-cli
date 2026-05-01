# Security Policy

Phonton CLI is a local-first developer tool. It reads source code, stores local state, calls model providers configured by the user, and may execute local verification commands. Treat it with the same care as any tool that can inspect and modify a repository.

## Supported Versions

| Version | Supported |
|---|---|
| 0.1.x | Private-alpha security fixes only |

## Reporting A Vulnerability

Do not open a public issue for secrets exposure, command execution bypasses, sandbox escapes, or provider credential handling bugs.

Report privately by emailing the maintainer address listed on the project profile, or by opening a private GitHub security advisory if available.

Please include:

- affected commit or version;
- OS and shell;
- exact command or workflow;
- impact;
- reproduction steps;
- whether credentials, local files, or network calls are involved.

## Security Expectations

Phonton should:

- never print full API keys;
- keep config and store files local by default;
- mark risky execution paths clearly;
- avoid sending broad repo context when task-specific context is enough;
- fail closed when provider configuration is invalid.

## Current Limitations

The sandbox and trust model are still pre-1.0. Do not run Phonton on repositories you do not trust, and review commands before using it on sensitive production systems.
