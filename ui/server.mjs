import { execFile } from 'node:child_process';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { accessSync, constants, statSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { delimiter, resolve } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath, pathToFileURL } from 'node:url';

const runFile = promisify(execFile);
const locationSupported = ['linux', 'darwin'].includes(process.platform);
const executable = process.platform === 'win32' ? 'geoveil.exe' : 'geoveil';
const cliOptions = {
  encoding: 'utf8',
  maxBuffer: 4 * 1024 * 1024,
  windowsHide: true,
};
const assets = new Map([
  ['/', ['index.html', 'text/html']],
  ['/index.html', ['index.html', 'text/html']],
  ['/app.js', ['app.js', 'text/javascript']],
  ['/styles.css', ['styles.css', 'text/css']],
  ['/map-data.js', ['map-data.js', 'text/javascript']],
]);

const routes = new Map([
  ['/api/status', { command: 'status', flags: [] }],
  ['/api/detect', { command: 'detect', flags: [] }],
  ['/api/leaks', { command: 'leaks', flags: ['activeDns'] }],
  ['/api/apply', { command: 'apply', flags: ['locale', 'location'], mutation: true }],
  ['/api/restore', { command: 'restore', flags: [], mutation: true }],
]);

class RequestError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

function findBinary(configuredBinary) {
  if (configuredBinary) return resolve(configuredBinary);
  if (process.env.GEOVEIL_BIN) return resolve(process.env.GEOVEIL_BIN);
  const candidates = [
    fileURLToPath(new URL('../target/release/' + executable, import.meta.url)),
    fileURLToPath(new URL('../target/debug/' + executable, import.meta.url)),
    ...(process.env.PATH ?? '').split(delimiter).filter(Boolean)
      .map((directory) => resolve(directory.replace(/^"|"$/g, ''), executable)),
  ];
  for (const candidate of candidates) {
    try {
      accessSync(candidate, constants.X_OK);
      if (statSync(candidate).isFile()) return candidate;
    } catch {
      // This location does not have an executable.
    }
  }
  throw missingBinary();
}

function missingBinary() {
  return new RequestError(503,
    'Geoveil CLI could not be found. Build it with cargo or set GEOVEIL_BIN to its executable.');
}

function hasResult(command, data) {
  switch (command) {
    case 'status': return typeof data?.engaged === 'boolean' && Array.isArray(data.vpn_interfaces);
    case 'detect': return Array.isArray(data?.interfaces) && Array.isArray(data.vpn_interfaces);
    case 'leaks': return Array.isArray(data?.checks);
    case 'apply': return Array.isArray(data?.report?.outcomes);
    case 'restore': return typeof data?.restored === 'boolean';
  }
}

async function runCli(configuredBinary, command, flags = []) {
  const binary = findBinary(configuredBinary);
  const mutation = command === 'apply' || command === 'restore';
  let stdout;
  let exitCode = 0;
  try {
    ({ stdout } = await runFile(binary, ['--json', command, ...flags], {
      ...cliOptions,
      timeout: mutation ? 15 * 60_000 : 4 * 60_000,
    }));
  } catch (error) {
    if (error.code === 'ENOENT') throw missingBinary();
    if (error.killed) throw new RequestError(504, 'Geoveil did not finish in time. Refresh system status before retrying.');
    if (typeof error.code !== 'number') {
      throw new RequestError(502, 'Could not run Geoveil (' + (error.code ?? error.signal ?? 'unknown exit') + '). Check the executable and its permissions.');
    }
    exitCode = error.code;
    stdout = error.stdout;
  }

  let data;
  try {
    data = JSON.parse(stdout);
  } catch {
    throw new RequestError(502, 'Geoveil returned unreadable output (exit ' + exitCode + '). Check that the executable supports --json.');
  }
  if (!hasResult(command, data)) {
    throw new RequestError(502, typeof data?.error === 'string' ? data.error : 'Geoveil returned an unexpected result.');
  }
  return { data, exitCode };
}

async function runLive(binary, command, options) {
  switch (command) {
    case 'apply': {
      const { data } = await runCli(binary, 'status');
      if (!data.vpn_interfaces.length) {
        throw new RequestError(409, 'Connect a VPN that Geoveil recognizes before applying a profile.');
      }
      const flags = [];
      if (options.locale) flags.push('--locale');
      if (options.location) flags.push('--location');
      return runCli(binary, 'apply', flags);
    }
    case 'restore':
      // The CLI restores only entries in the saved snapshot.
      return runCli(binary, 'restore', locationSupported ? ['--locale', '--location'] : ['--locale']);
    case 'leaks':
      return runCli(binary, 'leaks', options.activeDns ? ['--active-dns'] : []);
    default:
      return runCli(binary, command);
  }
}

const demoIp = '203.0.113.42';
const demoVpn = ['WireGuard'];
const demoProfile = {
  lat: 52.52, lon: 13.405, accuracy_m: 5000, timezone: 'Europe/Berlin', locale: 'de-DE',
  country: 'DE', source: { kind: 'vpn_exit', ip: demoIp },
};
const demoInterfaces = [
  { interface: { name: 'WireGuard', description: 'WireGuard Tunnel', is_up: true, addrs: ['10.64.0.2'] }, vpn: true },
  { interface: { name: 'Wi-Fi', description: 'Wireless network adapter', is_up: true, addrs: ['192.168.1.24'] }, vpn: false },
];
const demoVerification = [{ backend: 'timezone', ok: true, detail: 'Time zone matches Europe/Berlin.' }];

function demoReport(backends) {
  return { outcomes: Array.from(backends, (backend) => ({ backend, ok: true, error: null })) };
}

function demoLeakReport(demoSnapshots, activeDns) {
  const checks = [];
  const addCheck = (id, verdict, summary, details = []) =>
    checks.push({ id, verdict, summary, details, advice: null });
  addCheck('vpn', 'pass', 'VPN adapter detected. The example exit is ' + demoIp + '.');
  addCheck('ipv6', 'not_applicable', 'No public IPv6 route found.');
  addCheck('webrtc', 'pass', 'Probes did not find a public address outside the tunnel.');
  addCheck('dns', 'pass', 'DNS resolvers are configured on the VPN adapter.', ['WireGuard: 10.64.0.1']);
  if (activeDns) addCheck('dns_active', 'pass', 'The example resolver belongs to the VPN provider.');
  if (demoSnapshots.size) addCheck('verify', 'pass', 'The applied time zone matches the exit profile.');

  const third_parties = [
    'api.ipify.org', 'icanhazip.com', 'ifconfig.me/ip', 'api6.ipify.org', 'ipv6.icanhazip.com',
    'stun.l.google.com:19302', 'stun.cloudflare.com:3478',
  ];
  const dnsHosts = ['am.i.mullvad.net', '<random>.dnsleak.am.i.mullvad.net'];
  if (activeDns) third_parties.push(...dnsHosts);

  return {
    outcome: 'clean',
    vpn_interfaces: demoVpn,
    exit_ip: demoIp,
    exit_ipv6: null,
    checks,
    inputs: {
      interfaces: demoInterfaces.map(({ interface: nic, vpn }, index) =>
        ({ ...nic, index: index + 1, is_loopback: false, has_gateway: !vpn })),
      exit: { status: 'found', ip: demoIp },
      exit_v6: { status: 'failed', error: 'No IPv6 route in the example.' },
      stun: [
        {
          server: 'stun.cloudflare.com:3478', server_addr: null, interface: null, local: null,
          outcome: { status: 'mapped', addr: demoIp + ':53001' },
        },
        {
          server: 'stun.cloudflare.com:3478', server_addr: null, interface: 'Wi-Fi', local: '192.168.1.24',
          outcome: { status: 'blocked', detail: 'The example VPN blocks traffic outside its tunnel.' },
        },
      ],
      dns: {
        source: 'Example interface settings',
        servers: [{ interface: 'WireGuard', if_index: 1, addr: '10.64.0.1' }],
        error: null,
        notes: [],
      },
      active_dns: activeDns ? {
        provider: 'Mullvad', contacted: dnsHosts, exit: { ip: demoIp, country: 'DE', organization: null },
        resolvers: [{ ip: '192.0.2.53', country: 'DE', organization: 'Example VPN', provider_dns: true }],
        error: null,
      } : null,
      verify: demoSnapshots.size ? demoVerification : null,
      third_parties,
    },
  };
}

function runDemo(demoSnapshots, command, options) {
  let data;
  switch (command) {
    case 'status':
      data = {
        engaged: demoSnapshots.size > 0,
        profile: demoSnapshots.size ? demoProfile : null,
        verify: demoSnapshots.size ? demoVerification : [],
        vpn_interfaces: demoVpn,
        state_file: 'Demo snapshot',
      };
      break;
    case 'detect':
      data = { interfaces: demoInterfaces, vpn_interfaces: demoVpn, profile: demoProfile, error: null };
      break;
    case 'leaks':
      data = demoLeakReport(demoSnapshots, options.activeDns);
      break;
    case 'apply': {
      const backends = ['timezone'];
      if (options.locale) backends.push('locale');
      if (options.location) backends.push('location');
      for (const backend of backends) demoSnapshots.add(backend);
      data = { profile: demoProfile, report: demoReport(backends) };
      break;
    }
    case 'restore':
      data = demoSnapshots.size
        ? { restored: true, report: demoReport([...demoSnapshots].reverse()) }
        : { restored: false, report: null, message: 'No example snapshot to restore.' };
      demoSnapshots.clear();
      break;
  }
  return { data, exitCode: 0 };
}

function json(res, code, value) {
  res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(value));
}

async function readOptions(req, flags) {
  if (req.headers['content-type']?.split(';')[0].trim().toLowerCase() !== 'application/json') {
    throw new RequestError(415, 'Send options as application/json.');
  }
  const limit = 2048;
  if (Number(req.headers['content-length']) > limit) throw new RequestError(413, 'Too many options.');
  const chunks = [];
  let length = 0;
  for await (const chunk of req.iterator({ destroyOnReturn: false })) {
    length += chunk.length;
    if (length > limit) {
      req.resume();
      throw new RequestError(413, 'Too many options.');
    }
    chunks.push(chunk);
  }

  let options;
  try {
    options = JSON.parse(Buffer.concat(chunks));
  } catch {
    throw new RequestError(400, 'Options must be a valid JSON object.');
  }
  if (!options || typeof options !== 'object' || Array.isArray(options) ||
      Object.entries(options).some(([key, value]) => !flags.includes(key) || typeof value !== 'boolean')) {
    throw new RequestError(400, 'This action received unsupported options.');
  }
  if (options.location && !locationSupported) {
    throw new RequestError(400, 'Geoveil does not manage OS location on this platform.');
  }
  return options;
}

function matchesSecret(received, expected) {
  return typeof received === 'string' &&
    Buffer.byteLength(received) === expected.length &&
    timingSafeEqual(Buffer.from(received), expected);
}

async function handle(req, res, context) {
  const { host, origin, capability, token, demo, binary, running, demoSnapshots } = context;
  res.setHeader('Cache-Control', 'no-store');
  res.setHeader('X-Content-Type-Options', 'nosniff');
  res.setHeader('Referrer-Policy', 'no-referrer');
  res.setHeader('Content-Security-Policy',
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
    "font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'");

  // A hostile site must not be able to reach local data or actions by changing its DNS answer.
  const path = req.url?.split('?')[0];
  const asset = assets.get(path);
  const fetchSite = req.headers['sec-fetch-site'];
  const openingDashboard = req.method === 'GET' && asset?.[1] === 'text/html' &&
    req.headers['sec-fetch-mode'] === 'navigate' && req.headers['sec-fetch-dest'] === 'document';
  if (req.headers.host !== host || (req.headers.origin && req.headers.origin !== origin) ||
      (fetchSite && !['same-origin', 'none'].includes(fetchSite) && !openingDashboard)) {
    throw new RequestError(403, 'This dashboard only accepts requests from its loopback address.');
  }

  if (capability && !matchesSecret(req.headers['x-geoveil-desktop'], capability)) {
    throw new RequestError(403, 'This dashboard is available in the Geoveil window.');
  }
  if (context.closing) throw new RequestError(503, 'Geoveil is closing.');

  if (asset && (req.method === 'GET' || req.method === 'HEAD')) {
    const [filename, type] = asset;
    const bytes = await readFile(new URL(filename, import.meta.url));
    res.writeHead(200, { 'Content-Type': type + '; charset=utf-8', 'Content-Length': bytes.length });
    res.end(req.method === 'HEAD' ? undefined : bytes);
    return;
  }

  if (path === '/api/session' && req.method === 'GET') {
    json(res, 200, { demo, platform: process.platform, token: token.toString('ascii') });
    return;
  }
  const route = routes.get(path);
  if (!route) throw new RequestError(404, 'Page or action not found.');
  if (req.method !== 'POST') throw new RequestError(405, 'Use POST for this action.');

  if (!matchesSecret(req.headers['x-geoveil-token'], token)) {
    throw new RequestError(403, 'Dashboard session expired. Reload the page.');
  }
  const options = await readOptions(req, route.flags);
  if (context.closing) throw new RequestError(503, 'Geoveil is closing.');

  const key = route.mutation ? 'mutation' : route.command;
  if (running.has(key)) throw new RequestError(409, 'This action is already running.');
  running.add(key);
  const operation = (async () => {
    const result = demo
      ? runDemo(demoSnapshots, route.command, options)
      : await runLive(binary, route.command, options);
    json(res, 200, result);
  })();
  if (route.mutation) context.mutation = operation;
  try {
    await operation;
  } finally {
    running.delete(key);
    if (route.mutation) context.mutation = null;
  }
}

/** Start an isolated dashboard instance. Port zero selects an available loopback port. */
export async function startDashboard({ port = 4173, demo = false, binary, capability } = {}) {
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw new RangeError('Dashboard port must be an integer from 0 to 65535.');
  }
  if (capability !== undefined && !/^[0-9a-f]{64}$/.test(capability)) {
    throw new TypeError('Dashboard capability must be a 32-byte hex string.');
  }
  const context = {
    demo,
    binary,
    capability: capability === undefined ? null : Buffer.from(capability),
    token: Buffer.from(randomBytes(32).toString('hex')),
    running: new Set(),
    demoSnapshots: new Set(),
    closing: false,
    mutation: null,
  };
  const server = createServer({ requestTimeout: 15_000, headersTimeout: 10_000 }, (req, res) => {
    handle(req, res, context).catch((error) => {
      if (res.destroyed) return;
      if (error instanceof RequestError) {
        req.resume();
        json(res, error.status, { error: error.message });
      } else {
        console.error(error);
        json(res, 500, { error: 'The local dashboard could not complete this request.' });
      }
    });
  });

  await new Promise((resolveListening, rejectListening) => {
    server.once('error', rejectListening);
    server.listen(port, '127.0.0.1', () => {
      context.host = '127.0.0.1:' + server.address().port;
      context.origin = 'http://' + context.host;
      server.off('error', rejectListening);
      resolveListening();
    });
  });
  let closing;
  return {
    origin: context.origin,
    get isMutating() {
      return context.mutation !== null;
    },
    close() {
      if (closing) return closing;
      context.closing = true;
      const mutation = context.mutation;
      const closed = new Promise((resolveClosed) => server.close(resolveClosed));
      closing = (async () => {
        // The CLI may still be writing the restore snapshot or system settings.
        await mutation?.catch(() => {});
        server.closeAllConnections();
        await closed;
      })();
      return closing;
    },
  };
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  const args = process.argv.slice(2);
  const demo = args.includes('--demo');
  const port = Number(process.env.GEOVEIL_UI_PORT ?? 4173);
  if (args.length > Number(demo) || !Number.isInteger(port) || port < 1 || port > 65535) {
    console.error('Usage: node ui/server.mjs [--demo] (GEOVEIL_UI_PORT must be 1–65535)');
    process.exitCode = 1;
  } else {
    try {
      const dashboard = await startDashboard({ port, demo });
      console.log('Geoveil Dashboard: ' + dashboard.origin + (demo ? ' (demo mode)' : ''));
      process.once('SIGINT', () => { void dashboard.close(); });
      process.once('SIGTERM', () => { void dashboard.close(); });
    } catch (error) {
      console.error('Could not start Geoveil Dashboard on port ' + port + ': ' + error.message);
      process.exitCode = 1;
    }
  }
}
