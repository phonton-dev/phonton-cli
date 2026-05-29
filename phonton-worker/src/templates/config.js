function readTrimmedString(value) {
  if (value === undefined || value === null) {
    return undefined;
  }
  const trimmed = String(value).trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function readMaxRetries(value, envValue) {
  const raw = value ?? envValue ?? 2;
  if (raw === "" || raw === undefined || raw === null) {
    return 2;
  }
  const parsed = Number(raw);
  if (!Number.isInteger(parsed) || parsed < 0 || parsed > 10) {
    throw new Error("maxRetries must be an integer from 0 through 10");
  }
  return parsed;
}

export function loadConfig(raw = {}, env = process.env) {
  const explicitProvider = readTrimmedString(raw.provider);
  const explicitModel = readTrimmedString(raw.model);

  if (Object.prototype.hasOwnProperty.call(raw, "provider") && !explicitProvider) {
    throw new Error("provider must not be blank");
  }
  if (Object.prototype.hasOwnProperty.call(raw, "model") && !explicitModel) {
    throw new Error("model must not be blank");
  }

  const provider = explicitProvider ?? readTrimmedString(env.PROVIDER) ?? "openai";
  const model = explicitModel ?? readTrimmedString(env.MODEL) ?? "gpt-4o-mini";
  const maxRetries = readMaxRetries(raw.maxRetries, env.MAX_RETRIES);

  return {
    provider,
    model,
    maxRetries,
  };
}
