# Feedback / Bug Reports

Tervo has a built-in feedback path in **Settings → Help → Feedback**.

## What it does

1. The user writes a short description.
2. Tervo creates a bounded, redacted local bundle under the state directory:
   `feedback/tervo-feedback-<timestamp>.tar.gz`.
3. The bundle path is shown in the settings page.
4. If a feedback issue URL is configured, Tervo opens that URL with a short
   prefilled title/body; the user attaches the bundle manually.
5. If no URL is configured, Tervo opens the local bundle directory instead.

It never creates a merge request, never pushes code, and never uploads the
bundle by itself.

## Bundle contents

```text
feedback/
  report.md             user description (redacted) + version/commit/build
  env.json              selected environment facts (no env values dump)
  config.redacted.toml  config tail, redacted
  logs/                 newest log tails, redacted, bounded
```

Limits:

```text
max 8 log files
128 KiB per log/config tail
no terminal scrollback
no environment-variable values
no Vault/credentials/API keys
```

## Configuration

```toml
# ~/.config/tervo/config.toml
feedback_issue_url = "https://gitlab.example/group/project/-/issues/new"
```

Environment override:

```bash
TERVO_FEEDBACK_ISSUE_URL="https://github.com/owner/repo/issues/new"
```

The URL can be a GitLab/GitHub new-issue page. It is opened with `xdg-open`;
Tervo does not receive a token and does not submit an issue via an API.

## Non-goals

```text
automatic issue creation
MR/branch creation
git push
uploading terminal contents
uploading full logs or scrollback
```
