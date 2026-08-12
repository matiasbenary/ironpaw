# GitHub workflows

Use the `shell` tool with `tool: "github"`. Authentication is already supplied through `GH_TOKEN`; never run interactive authentication or request, print, or store the token.

Use `gh` for GitHub objects and `git` for repository state. Prefer JSON output from `gh` when inspecting data.

## Repository workspace

Clone once into `repo`:

```json
{"tool":"github","command":"gh","args":["repo","clone","OWNER/REPO","repo"]}
```

For later repository commands, pass `"cwd":"repo"`. The working directory must remain inside this tool's private workspace.

## Issues

- List: `gh issue list --repo OWNER/REPO --json number,title,state,url`
- View: `gh issue view NUMBER --repo OWNER/REPO --comments`
- Create non-interactively with `--title` and `--body`.
- Comment non-interactively with `gh issue comment NUMBER --repo OWNER/REPO --body TEXT`.

## Branches and commits

Before changing repository state, inspect `git status --short --branch` and `git diff` with `cwd: "repo"`.

- Create a branch: `git switch -c BRANCH`
- Configure commit identity only when required: `git config user.name NAME` and `git config user.email EMAIL`
- Stage explicit paths with `git add PATH`; inspect `git diff --staged` before committing.
- Commit non-interactively with `git commit -m MESSAGE`.
- Push with `git push -u origin BRANCH`. Use `--force-with-lease` only when explicitly required.

## Pull requests

- List: `gh pr list --repo OWNER/REPO --json number,title,state,headRefName,baseRefName,url`
- View: `gh pr view NUMBER --repo OWNER/REPO --comments`
- Create from the cloned repository using `cwd: "repo"`, `--title`, `--body`, `--base`, and `--head`.
- Comment with `gh pr comment NUMBER --repo OWNER/REPO --body TEXT`.

Do not use `--web`; Ironpaw commands are non-interactive. Do not invent unsupported flags: inspect `list_tools` for the installed command interface if a call is rejected.
