import { spawn } from 'node:child_process';
import { access, readFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const CONTRACT_EVENTS = new Set([
  'started',
  'blocked',
  'finished',
  'failed',
  'retry-needed',
  'pr-created',
  'test-started',
  'test-finished',
  'test-failed',
  'handoff-needed',
]);

const DEFAULT_CLAWHIP_BIN = 'clawhip';
const DEFAULT_DAEMON_URL = 'http://127.0.0.1:25294';

function trimString(value) {
  return typeof value === 'string' ? value.trim() : '';
}

function normalizeUrl(url) {
  return trimString(url).replace(/\/$/, '');
}

function pickFirstString(...values) {
  for (const value of values) {
    const trimmed = trimString(value);
    if (trimmed) return trimmed;
  }
  return '';
}

export function isSupportedNormalizedEvent(value) {
  return CONTRACT_EVENTS.has(trimString(value));
}

export function canForwardHookEvent(event) {
  return isSupportedNormalizedEvent(event?.context?.normalized_event);
}

function assertSupportedNormalizedEvent(value) {
  const normalizedEvent = trimString(value);
  if (!isSupportedNormalizedEvent(normalizedEvent)) {
    throw new Error(
      `unsupported clawhip OMX normalized_event: ${normalizedEvent || '<empty>'}`,
    );
  }
  return normalizedEvent;
}

function buildContext(context = {}, normalizedEvent) {
  const merged = {
    agent_name: 'omx',
    ...context,
    normalized_event: normalizedEvent,
  };
  return Object.fromEntries(
    Object.entries(merged).filter(([, value]) => value !== undefined && value !== null && value !== ''),
  );
}

export function buildHookEnvelope({
  normalizedEvent,
  event = 'notify',
  source = 'native',
  timestamp = new Date().toISOString(),
  context = {},
  sessionId,
  threadId,
  turnId,
  mode,
  channel,
  mention,
} = {}) {
  const canonical = assertSupportedNormalizedEvent(normalizedEvent);
  const envelope = {
    schema_version: '1',
    event,
    timestamp,
    source,
    context: buildContext(context, canonical),
  };

  const topLevel = {
    session_id: sessionId,
    thread_id: threadId,
    turn_id: turnId,
    mode,
    channel,
    mention,
  };

  for (const [key, value] of Object.entries(topLevel)) {
    if (value !== undefined && value !== null && value !== '') {
      envelope[key] = value;
    }
  }

  return envelope;
}

async function commandAvailable(bin) {
  return await new Promise((resolve) => {
    const child = spawn(bin, ['--version'], { stdio: 'ignore' });
    child.on('error', () => resolve(false));
    child.on('close', (code) => resolve(code === 0));
  });
}

async function readClawhipConfig(env) {
  const configPath = pickFirstString(env.CLAWHIP_CONFIG) || join(homedir(), '.clawhip', 'config.toml');
  try {
    await access(configPath, constants.R_OK);
    return await readFile(configPath, 'utf8');
  } catch {
    return null;
  }
}

function daemonSection(toml) {
  const lines = String(toml).split(/\r?\n/);
  const collected = [];
  let inDaemon = false;

  for (const line of lines) {
    const trimmed = trimString(line);
    if (/^\[[^\]]+\]$/.test(trimmed)) {
      if (inDaemon) break;
      inDaemon = trimmed === '[daemon]';
      continue;
    }
    if (inDaemon) {
      collected.push(line);
    }
  }

  return collected.join('\n');
}

function quotedValue(section, key) {
  const match = section.match(new RegExp(`^\\s*${key}\\s*=\\s*"([^"]+)"\\s*$`, 'm'));
  return match ? trimString(match[1]) : '';
}

function numericValue(section, key) {
  const match = section.match(new RegExp(`^\\s*${key}\\s*=\\s*(\\d+)\\s*$`, 'm'));
  return match ? Number.parseInt(match[1], 10) : undefined;
}

async function discoverDaemonUrl(env) {
  const explicit = normalizeUrl(pickFirstString(env.CLAWHIP_OMX_DAEMON_URL, env.CLAWHIP_DAEMON_URL));
  if (explicit) {
    return { url: explicit, source: env.CLAWHIP_OMX_DAEMON_URL ? 'env:CLAWHIP_OMX_DAEMON_URL' : 'env:CLAWHIP_DAEMON_URL' };
  }

  const configToml = await readClawhipConfig(env);
  if (configToml) {
    const daemon = daemonSection(configToml);
    const baseUrl = normalizeUrl(quotedValue(daemon, 'base_url'));
    if (baseUrl) {
      return { url: baseUrl, source: 'config:daemon.base_url' };
    }
    const port = numericValue(daemon, 'port');
    if (port) {
      return { url: `http://127.0.0.1:${port}`, source: 'config:daemon.port' };
    }
  }

  return { url: DEFAULT_DAEMON_URL, source: 'default' };
}

export async function discoverClawhip(options = {}) {
  const env = options.env ?? process.env;
  const preferredTransport = trimString(env.CLAWHIP_OMX_TRANSPORT || env.CLAWHIP_TRANSPORT).toLowerCase();
  const explicitBin = trimString(env.CLAWHIP_BIN);

  if (preferredTransport !== 'http') {
    if (explicitBin) {
      return { transport: 'cli', bin: explicitBin, source: 'env:CLAWHIP_BIN' };
    }
    if (preferredTransport !== 'http' && (await commandAvailable(DEFAULT_CLAWHIP_BIN))) {
      return { transport: 'cli', bin: DEFAULT_CLAWHIP_BIN, source: 'path' };
    }
  }

  const daemon = await discoverDaemonUrl(env);
  return { transport: 'http', url: daemon.url, source: daemon.source };
}

function normalizeEnvelope(envelope) {
  if (!envelope || typeof envelope !== 'object' || Array.isArray(envelope)) {
    throw new Error('clawhip OMX envelope must be an object');
  }

  const normalizedEvent = assertSupportedNormalizedEvent(
    envelope.context?.normalized_event ?? envelope.normalized_event,
  );

  return {
    schema_version: trimString(envelope.schema_version) || '1',
    event: trimString(envelope.event) || 'notify',
    timestamp: trimString(envelope.timestamp) || new Date().toISOString(),
    source: trimString(envelope.source) || 'native',
    ...Object.fromEntries(
      Object.entries(envelope).filter(([key]) => !['schema_version', 'event', 'timestamp', 'source', 'context'].includes(key)),
    ),
    context: buildContext(envelope.context ?? {}, normalizedEvent),
  };
}

async function emitViaCli(bin, envelope) {
  return await new Promise((resolve, reject) => {
    const child = spawn(bin, ['omx', 'hook'], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: process.env,
    });

    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => {
      stdout += chunk.toString();
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk.toString();
    });
    child.on('error', reject);
    child.on('close', (code) => {
      if (code === 0) {
        resolve({ ok: true, transport: 'cli', stdout: trimString(stdout) || null, stderr: trimString(stderr) || null });
      } else {
        reject(new Error(`clawhip CLI transport failed with exit ${code}: ${trimString(stderr) || trimString(stdout) || 'unknown error'}`));
      }
    });

    child.stdin.end(`${JSON.stringify(envelope)}\n`);
  });
}

async function emitViaHttp(url, envelope) {
  const response = await fetch(`${normalizeUrl(url)}/api/omx/hook`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
    },
    body: JSON.stringify(envelope),
  });

  const payload = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(
      `clawhip HTTP transport failed (${response.status}): ${payload.error || response.statusText}`,
    );
  }

  return { ok: true, transport: 'http', payload };
}

export async function createClawhipOmxClient(options = {}) {
  const discovery = await discoverClawhip(options);

  async function emitEnvelope(envelope) {
    const normalized = normalizeEnvelope(envelope);
    if (discovery.transport === 'cli') {
      return await emitViaCli(discovery.bin, normalized);
    }
    return await emitViaHttp(discovery.url, normalized);
  }

  async function emitNormalizedEvent({ normalizedEvent, context = {}, ...rest } = {}) {
    return await emitEnvelope(buildHookEnvelope({ normalizedEvent, context, ...rest }));
  }

  return {
    discovery,
    emitEnvelope,
    emitNormalizedEvent,
    emitSessionStarted: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'started', ...args }),
    emitSessionBlocked: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'blocked', ...args }),
    emitSessionFinished: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'finished', ...args }),
    emitSessionFailed: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'failed', ...args }),
    emitRetryNeeded: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'retry-needed', ...args }),
    emitPrCreated: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'pr-created', ...args }),
    emitTestStarted: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'test-started', ...args }),
    emitTestFinished: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'test-finished', ...args }),
    emitTestFailed: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'test-failed', ...args }),
    emitTestResult: ({ success = true, ok = true, ...args } = {}) =>
      emitNormalizedEvent({
        normalizedEvent: success === false || ok === false ? 'test-failed' : 'test-finished',
        ...args,
      }),
    emitHandoffNeeded: (args = {}) => emitNormalizedEvent({ normalizedEvent: 'handoff-needed', ...args }),
    emitToolUse: ({ normalizedEvent = '', context = {}, toolName, command, ...rest } = {}) => {
      const mapped = pickFirstString(normalizedEvent);
      if (!mapped) {
        throw new Error(
          'tool.use is not a canonical clawhip v1 event; provide a canonical normalizedEvent such as test-started, test-failed, pr-created, failed, or handoff-needed',
        );
      }
      return emitNormalizedEvent({
        normalizedEvent: mapped,
        context: {
          ...context,
          ...(toolName ? { tool_name: toolName } : {}),
          ...(command ? { command } : {}),
        },
        ...rest,
      });
    },
    emitError: ({ normalizedEvent = 'failed', context = {}, errorSummary, ...rest } = {}) =>
      emitNormalizedEvent({
        normalizedEvent,
        context: {
          ...context,
          ...(errorSummary ? { error_summary: errorSummary } : {}),
        },
        ...rest,
      }),
    emitFromHookEvent: async (event, overrides = {}) => {
      const normalizedEvent = pickFirstString(
        overrides.normalizedEvent,
        overrides.context?.normalized_event,
        event?.context?.normalized_event,
      );

      if (!normalizedEvent) {
        return { ok: true, skipped: true, reason: 'missing_normalized_event' };
      }
      if (!isSupportedNormalizedEvent(normalizedEvent)) {
        return {
          ok: true,
          skipped: true,
          reason: 'unsupported_normalized_event',
          normalized_event: normalizedEvent,
        };
      }

      return await emitEnvelope({
        ...event,
        ...overrides,
        schema_version: '1',
        context: {
          agent_name: 'omx',
          ...(event?.context ?? {}),
          ...(overrides.context ?? {}),
          normalized_event: normalizedEvent,
        },
      });
    },
  };
}
