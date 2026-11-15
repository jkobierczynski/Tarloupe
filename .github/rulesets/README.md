# Repository rulesets

These JSON files are GitHub rulesets for this repository. They are not
applied automatically: import them once under
**Settings → Rules → Rulesets → New ruleset → Import a ruleset**.

| File | What it protects |
|---|---|
| `protect-main.json` | The default branch: no deletion or force-push, changes only through a pull request (merge, squash and rebase merges all allowed), and the four CI jobs (`test` and `msrv` on Linux and Windows) must pass on an up-to-date branch. No approving review is required, so the solo maintainer can merge his own PRs. Repository admins can bypass only by merging a pull request, never by pushing directly. |
| `protect-tags.json` | Release tags `v*`: once created they cannot be moved or deleted by anyone except repository admins. |

If a CI job is renamed in `.github/workflows/ci.yml`, update the
`required_status_checks` list in `protect-main.json` and the ruleset on
GitHub, or pull requests will wait forever for a check that never runs.
