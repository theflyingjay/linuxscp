# Security Policy

## Supported Versions

LinuxSCP is under active development. Security updates are currently provided only for the latest released version.

| Version | Supported |
| ------- | --------- |
| Latest release | Yes |
| Older releases | No |
| Development builds | No |

Users should upgrade to the latest release before reporting an issue that may already have been resolved.

## Reporting a Vulnerability

Please do not report suspected security vulnerabilities through public GitHub issues, discussions, or pull requests.

Use GitHub's private vulnerability reporting feature:

1. Open the LinuxSCP repository.
2. Select **Security**.
3. Select **Report a vulnerability**.
4. Provide as much detail as possible.

Please include:

- The affected LinuxSCP version
- Linux distribution and version
- Steps required to reproduce the issue
- The expected and observed behavior
- The potential security impact
- Relevant logs, screenshots, or proof-of-concept code
- Whether the issue is already being actively exploited

Do not include real passwords, private keys, access tokens, server addresses, or other sensitive information.

## Security-Relevant Issues

Examples include:

- Exposure or improper storage of passwords or private keys
- SSH host-key verification failures or bypasses
- Command or argument injection
- Path traversal
- Unauthorized local or remote file access
- Unsafe handling of symbolic links
- File-permission vulnerabilities
- Malicious server responses causing code execution or data exposure
- Vulnerabilities that allow unintended file overwrites
- Dependency vulnerabilities that directly affect LinuxSCP

Ordinary bugs, crashes without a security impact, and feature requests should be submitted through the public issue tracker.

## Disclosure Process

We will acknowledge reports as soon as reasonably possible, investigate their impact, and coordinate remediation and disclosure with the reporter.

Please allow time for a fix to be developed and released before publicly disclosing the vulnerability. When appropriate, reporters will be credited in the published security advisory unless they request anonymity.

## Security Advisories

Confirmed vulnerabilities will be documented through GitHub Security Advisories when appropriate. Users should watch the repository and keep LinuxSCP updated to receive security fixes.

## Scope

This policy covers the LinuxSCP application and code published through the official LinuxSCP repository. It does not cover third-party operating systems, SSH servers, unofficial packages, modified builds, or unrelated dependencies unless LinuxSCP’s use of them creates the vulnerability.
