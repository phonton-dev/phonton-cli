function resolveString(raw, env, rawKey, envKey, fallback) {
  const hasRaw = Object.prototype.hasOwnProperty.call(raw, rawKey);
  const value = hasRaw ? raw[rawKey] : env[envKey] ?? fallback;
  if (typeof value !== "string") {
    throw new Error(`${rawKey} must be a string`);
  }
  const trimmed = value.trim();
  if (hasRaw && trimmed === "") {
    throw new Error(`${rawKey} cannot be blank`);
  }
  return trimmed || fallback;
}

function resolveMaxRetries(raw, env) {
  const hasRaw = Object.prototype.hasOwnProperty.call(raw, "maxRetries");
  const value = hasRaw ? raw.maxRetries : env.MAX_RETRIES ?? 2;
  const parsed =
    typeof value === "number" && Number.isInteger(value)
      ? value
      : typeof value === "string" && value.trim() !== ""
        ? Number(value)
        : value;

  if (!Number.isInteger(parsed) || parsed < 0 || parsed > 10) {
    throw new Error("maxRetries must be an integer from 0 through 10");
  }
  return parsed;
}

export function loadConfig(raw = {}, env = process.env) {
  return {
    provider: resolveString(raw, env, "provider", "PROVIDER", "openai"),
    model: resolveString(raw, env, "model", "MODEL", "gpt-4o-mini"),
    maxRetries: resolveMaxRetries(raw, env),
  };
}
