# Security policy

Rottweiler is pre-v1 security-sensitive software. The current source branch and
Homebrew HEAD formula are intended for development and evaluation; no stable
release is supported until the protected release gates pass.

## Reporting a vulnerability

Do not open a public issue for suspected vulnerabilities, credential exposure,
sandbox escapes, update-signature failures, session-data disclosure, or plugin
capability bypasses.

Use GitHub's private vulnerability reporting flow:

<https://github.com/Boredphilosopher96/Rottweiler/security/advisories/new>

Include the affected commit, platform, configuration, reproduction steps,
security boundary crossed, and whether credentials or persisted session data
may have been exposed. Remove real secrets and use canary values where possible.

The maintainer will acknowledge a complete report within three business days,
coordinate validation and remediation privately, and publish an advisory after
a fix is available. Please do not disclose the issue publicly before that
coordination completes.

## Scope

High-priority boundaries include:

- command and plugin sandbox escape;
- permission, trust, or driver-lease bypass;
- credential, OAuth, session-log, replay, or export disclosure;
- signed-update rollback, substitution, or threshold bypass;
- SSRF or private-network access outside an explicit policy grant;
- extension or MCP capability escalation;
- cross-workspace write or checkpoint corruption.

The detailed threat model and acceptance contracts are in
[`docs/05-SECURITY.md`](docs/05-SECURITY.md).
