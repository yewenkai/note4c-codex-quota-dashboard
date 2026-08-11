# Security and privacy

## Sensitive data that stays local

- Codex/OpenAI account tokens remain under the Mac user's `~/.codex` directory.
- The relay reads `codex-auth`'s registry but never uploads registry JSON or tokens.
- The SSH publishing key remains on the Mac.
- The HTTPS read-only password is stored in the server password file and NOTE4C NVS; it must not be committed.

## Data published to the server

The server stores a rendered PNG, a 30,000-byte BWRY frame, and a manifest. The rendered images contain the displayed account label, plan, quota, active-account marker, and update time. Configure `accountLabels` in the local sync configuration if real email addresses must not leave the Mac.

The manifest contains only revision, timestamp, frame path, SHA-256, size, format, width, and height.

## Credential separation

- Use a dedicated unprivileged `note4c-publisher` SSH user and dedicated Ed25519 key for Mac-to-server writes.
- Use a different, random Basic Auth password for device/browser read access.
- Never use Basic Auth over plain HTTP.
- Do not reuse a root password, cloud account password, Codex password, or SSH passphrase.

## Trust boundaries

- The Mac and `codex-auth` are trusted with Codex account credentials.
- The VPS is trusted with rendered quota information, but not Codex tokens.
- The NOTE4C LAN settings endpoint is intended for a trusted local network. It hides the stored password in responses but allows settings changes from the LAN while the endpoint is running.
- A stolen device may expose the read-only dashboard credential from its flash/NVS to a sufficiently capable attacker. Rotate the Basic Auth password if a device is lost.

## Safe failure behavior

The relay does not publish when a paid account refresh is missing, account count differs from three, cached usage is stale, rendering fails, or SSH upload fails. Firmware keeps the previous frame when HTTPS, authentication, manifest validation, frame length, SHA-256, or storage validation fails.

## Reporting

Open a GitHub security advisory for vulnerabilities. Before attaching logs or screenshots, remove public IPs, hostnames, email addresses, Wi-Fi details, credentials, tokens, and SSH fingerprints.
