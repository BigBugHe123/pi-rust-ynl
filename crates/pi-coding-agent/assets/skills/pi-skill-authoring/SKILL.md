---
name: pi-skill-authoring
description: Create, install, update, disable, or remove Pi Skills for reusable workflows.
---

# Manage Pi skills

Reply in the user's language. Infer paths and config from the session. Ask in plain language only when a missing detail or ambiguous name affects the result. Explain what changed and how to use it.

Default to the current session project. Use PI_CODING_AGENT_DIR for global installs only when explicitly requested. If the project is unavailable or installation fails, explain and ask where to proceed. Check existing names and scope before editing.

After a successful change, tell the user to wait for the reply to finish, then send /reload in the same chat. Verify after reload and report only what you verified.

For packages, run pi install <source> -l from the project. Omit -l for explicitly requested global installs.

Create .pi/skills/<name>/SKILL.md in the project, or skills/<name>/SKILL.md under the global agent directory. Match the folder and skill name using lowercase letters, digits, and hyphens. Describe when to use it, required inputs, steps, and checks. Bundle supporting files with relative paths.

```markdown
---
name: review-changes
description: Review the current Git diff and report actionable issues.
---

Review the current Git diff. Report actionable issues by severity with file locations.
```

After reload, test with /skill:<name>. To remove a local skill, confirm its source and remove its folder. Disable package-provided skills individually, or remove the whole package when requested with pi remove <source> (-l for project scope). Verify the skill list after reload.
