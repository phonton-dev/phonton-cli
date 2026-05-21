function text(value) {
  if (value === null || value === undefined) {
    return "";
  }
  return String(value);
}

function hasItems(value) {
  return Array.isArray(value) && value.length > 0;
}

function renderChangedFiles(files) {
  if (!hasItems(files)) {
    return "";
  }
  const lines = ["## Changed Files", ""];
  for (const file of files) {
    let line = `- ${text(file.path)}`;
    if (file.added || file.removed) {
      line += ` (+${file.added || 0} -${file.removed || 0})`;
    }
    if (hasItems(file.verifiedBy)) {
      line += ` verified by ${file.verifiedBy.map(text).filter(Boolean).join(", ")}`;
    }
    lines.push(line);
  }
  lines.push("");
  return lines.join("\n");
}

function renderChecks(checks) {
  if (!hasItems(checks)) {
    return "";
  }
  const lines = ["## Verification", ""];
  for (const check of checks) {
    let line = `- ${text(check.status)}: ${text(check.name)}`;
    if (check.command) {
      line += ` (${text(check.command)})`;
    }
    lines.push(line);
  }
  lines.push("");
  return lines.join("\n");
}

function commandPreview(command) {
  const failed = command.exitCode !== undefined && command.exitCode !== 0;
  const stderr = text(command.stderr).trim();
  const stdout = text(command.stdout).trim();
  const source = failed && stderr ? stderr : stdout || stderr;
  if (!source) {
    return "";
  }
  const lines = source.split(/\r?\n/);
  return lines.slice(0, 3).join("\n");
}

function renderCommands(commands) {
  if (!hasItems(commands)) {
    return "";
  }
  const lines = ["## Commands", ""];
  for (const command of commands) {
    const label = text(command.label) || "command";
    const commandText = text(command.command);
    lines.push(`- ${label}: ${commandText}`);
    if (command.exitCode !== undefined) {
      lines.push(`  exit ${command.exitCode}`);
    }
    if (command.durationMs !== undefined) {
      lines.push(`  ${command.durationMs}ms`);
    }
    const preview = commandPreview(command);
    if (preview) {
      lines.push("  output:");
      for (const line of preview.split(/\r?\n/)) {
        lines.push(`  ${line}`);
      }
    }
  }
  lines.push("");
  return lines.join("\n");
}

function gapRank(gap) {
  const ranks = { high: 0, medium: 1, low: 2 };
  return ranks[text(gap.severity).toLowerCase()] ?? 3;
}

function renderKnownGaps(gaps) {
  if (!hasItems(gaps)) {
    return "";
  }
  const lines = ["## Known Gaps", ""];
  for (const gap of [...gaps].sort((a, b) => gapRank(a) - gapRank(b))) {
    const severity = text(gap.severity) || "unknown";
    const body = text(gap.text || gap.description || gap.summary);
    lines.push(`- ${severity}: ${body}`);
  }
  lines.push("");
  return lines.join("\n");
}

export function buildReceipt(run = {}) {
  const sections = [
    `# ${text(run.title) || "Task Receipt"}`,
    "",
    `Goal: ${text(run.goal)}`,
    "",
    renderChangedFiles(run.changedFiles),
    renderChecks(run.checks),
    renderCommands(run.commands),
    renderKnownGaps(run.knownGaps),
  ];

  if (run.summary) {
    sections.push("## Summary", "", text(run.summary), "");
  }

  return `${sections.join("\n").replace(/\n{3,}/g, "\n\n").trim()}\n`;
}
