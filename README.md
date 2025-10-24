# ShareLatex Gitbridge (Read-only Git Smart HTTP Bridge)

Expose ShareLatex Community Edition projects as read-only Git repositories so downstream tooling—such as GitLab pull mirroring—can clone or fetch without touching the primary instance.

## Key Features

- Presents each project at `https://<TOKEN>:git@<HOST>:<PORT>/git/<projectId>.git` using Git Smart HTTP.
- Strictly read-only: push requests (`git-receive-pack`) return HTTP 403.
- Before every fetch the bridge syncs the ShareLatex workspace into a bare mirror (default branch `master`, configurable).
- Authentication options:
  - **Global tokens** managed via an Admin UI.
  - **Per-project token file**: place `.gitbridge` inside your ShareLatex project root folder; its contents are used as a token for `git` operations.

## Quick Start

1. Prepare directories (create or mount):
   - `./sharelatex-data/data/projects/<projectId>/` – ShareLatex project sources.
   - `./gitbridge-data/` – destination for bare mirrors and `tokens.json`.
2. Launch the service with the desired environment variables (see below). Startup logs print resolved paths and create an empty `tokens.json` if missing.
3. Populate `sharelatex-data/data/projects/<projectId>/` with the project files.
4. Create a token via the Admin UI or by editing `gitbridge-data/tokens.json`, then clone using `https://<TOKEN>@host:PORT/git/<projectId>.git`.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `PORT` | HTTP port (default `8022`). |
| `GIT_ROOT` | Location for bare mirrors and `tokens.json` (default `/data/git-bridge`). |
| `SHARELATEX_DATA_PATH` | Base path containing ShareLatex projects (default `/sharelatex-data`). |
| `PROJECTS_DIR` | Subdirectory under `SHARELATEX_DATA_PATH` with actual projects (default `data/projects`). |
| `READONLY_BRANCH` | Branch name used in the mirror repository (default `master`). |
| `ADMIN_PASSWORD` | Enables the Admin UI when set; leave empty to disable the UI. |
| `ADMIN_COOKIE_SECURE` | `true/1/on` marks the admin cookie as `Secure` (HTTPS only). |
| `ADMIN_SESSION_TTL_SECONDS` | Lifetime of an admin session in seconds (default `3600`). |

## Admin UI

- Served at `/admin` as a small single-page app that talks to JSON endpoints (`/admin/api/...`).
- Manage tokens (list/create/delete) and perform login/logout; errors appear inline.
- Sessions are stored as SHA-256 hashes, and throttling kicks in after five failed logins within 60 seconds.
- Tailwind CSS is bundled locally (`/assets/tailwind.js`); no external CDN access required.

## Operational Notes

- Removing a ShareLatex project directory automatically deletes its bare mirror.
- `.gitbridge` token files should remain private; they authorize a single project only.
- For UI tweaks, edit the templates in `templates/` (HTML, CSS) and rebuild.
