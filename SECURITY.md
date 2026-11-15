# Security policy

Tarloupe opens archives that may come from anywhere, so bugs in parsing,
decompression, path handling or the Windows mount can matter for security.

## Supported versions

Only the latest commit on `main` (and, once releases exist, the latest
release) gets security fixes.

## Reporting a vulnerability

Please do **not** open a public issue. Report it privately through
GitHub: go to the repository's **Security** tab and choose
**Report a vulnerability**.

Useful things to include:

- what an attacker can do (for example read or write files outside the
  archive, crash or hang the mount, exhaust memory or disk),
- a small archive or script that reproduces it,
- the Tarloupe commit or version and your OS.

You should get a reply within a week. Once a fix is out, the report will be
published as a GitHub security advisory, with credit if you want it.

## Examples of what counts

- Path traversal (`../`, absolute paths, symlinks, Windows drive letters or
  device names) that escapes the archive view.
- Malformed archives that cause memory unsafety, unbounded memory or disk
  use, or an endless loop.
- A tampered or stale index file that makes Tarloupe return data from the
  wrong offset or another file.
