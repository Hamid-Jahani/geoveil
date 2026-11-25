import { landRows } from "./map-data.js";

const byId = (id) => document.getElementById(id);
const setText = (id, text) => { byId(id).textContent = text; };
const show = (id, visible) => { byId(id).hidden = !visible; };
const dash = "—";
const countries = new Intl.DisplayNames(["en"], { type: "region" });
const progress = {
  status: "Reading system status…",
  detect: "Resolving the public IP and exit location…",
  leaks: "Running leak checks…",
  apply: "Applying the system profile…",
  restore: "Restoring original system settings…",
};
const state = { session: null, status: null, vpnInterfaces: null, detection: null, leaks: null, busy: "", checkedAt: null, scanAt: null };
const views = {
  overview: ["Overview", "Network overview", "Inspect your VPN exit, system profile, and network leaks."],
  leaks: ["Leak checks", "Leak checks", "Review VPN, IPv6, WebRTC, and DNS diagnostics."],
  profile: ["System profile", "System profile", "Preview the exit profile and manage your saved system settings."],
};
const checkNames = {
  vpn: ["VPN connection", "interface"],
  ipv6: ["IPv6", "globe"],
  webrtc: ["WebRTC / STUN", "pulse"],
  dns: ["DNS resolvers", "lock"],
  dns_active: ["Active DNS", "target"],
  verify: ["System verification", "sliders"],
};
const verdicts = {
  pass: ["Pass", "good"], info: ["Info", "info"], not_applicable: ["N/A", "quiet"],
  warn: ["Warning", "caution"], leak: ["Leak", "bad"], error: ["Error", "bad"],
};
const outcomes = {
  clean: ["No leaks found", "good"], leaks_found: ["Leaks found", "bad"],
  incomplete: ["Incomplete", "caution"],
};

function icon(name, className) {
  const namespace = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(namespace, "svg");
  const use = document.createElementNS(namespace, "use");
  use.setAttribute("href", "#i-" + name);
  svg.setAttribute("aria-hidden", "true");
  if (className) svg.setAttribute("class", className);
  svg.append(use);
  return svg;
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function pill(node, label, tone, dot = false) {
  node.className = "pill " + tone;
  node.replaceChildren();
  if (dot) node.append(element("span", "indicator"));
  node.append(document.createTextNode(label));
}

function notice(message, error = false) {
  show("notice", Boolean(message));
  byId("notice").classList.toggle("error", error);
  setText("notice-copy", message);
}

function countryName(code) {
  return /^[A-Z]{2}$/.test(code || "") ? countries.of(code) : dash;
}

function coordinateLabel(profile) {
  return [profile.lat, profile.lon].every(Number.isFinite)
    ? formatCoordinate(profile.lat, "N", "S") + "  " + formatCoordinate(profile.lon, "E", "W")
    : dash;
}

function formatCoordinate(value, positive, negative) {
  return Math.abs(value).toFixed(2) + "° " + (value < 0 ? negative : positive);
}

function detectionIp(detection) {
  return detection?.public_ip ?? detection?.profile?.source?.ip ?? null;
}

function sameAdapters(previous, current) {
  return previous?.length === current.length && current.every((name) => previous.includes(name));
}

function renderLocation() {
  const detected = state.detection?.profile;
  const connectedAdapters = state.vpnInterfaces || [];
  const resolvedIp = detectionIp(state.detection) || state.leaks?.exit_ip;
  let mapStatus = "AWAITING EXIT LOOKUP";
  if (state.busy === "detect") mapStatus = "RESOLVING PUBLIC IP";
  else if (detected) mapStatus = state.detection.vpn_interfaces.length ? "VPN EXIT ESTIMATE" : "PUBLIC IP ESTIMATE";
  else if (state.detection) mapStatus = "LOCATION UNAVAILABLE";
  setText("map-caption", mapStatus);
  show("map-key", Boolean(detected));

  if (connectedAdapters.length) pill(byId("vpn-badge"), "VPN detected", "good", true);
  else if (state.vpnInterfaces) pill(byId("vpn-badge"), "No VPN adapter", "quiet", true);
  else pill(byId("vpn-badge"), "Unknown", "quiet", true);
  setText("ip", resolvedIp || dash);
  setText("region", countryName(detected?.country));
  const interfaces = state.detection?.interfaces || [];
  const adapterNames = new Map(interfaces.map(({ interface: entry }) =>
    [entry.name, entry.description?.split(" | ")[0] || entry.name]));
  setText("adapter", connectedAdapters.map((name) => adapterNames.get(name) || name).join(", ") || dash);
  show("interfaces", interfaces.length > 0);
  setText("interface-count", "(" + interfaces.length + ")");
  byId("interface-list").replaceChildren(...interfaces.map(({ interface: entry, vpn }) => {
    const label = entry.name + (entry.description ? " · " + entry.description : "");
    const connection = entry.is_up ? (vpn ? "VPN · up" : "up") : "down";
    const addresses = entry.addrs?.length ? " · " + entry.addrs.join(", ") : "";
    return element("li", null, label + " · " + connection + addresses);
  }));
  drawMap();
}

function setActionAvailable(id, available) {
  const button = byId(id);
  button.disabled = !available;
  // Keep the current button focused while its request runs.
  if (available && state.busy) button.setAttribute("aria-disabled", "true");
  else button.removeAttribute("aria-disabled");
}

function renderProfile() {
  const liveProfile = state.status?.profile;
  const preview = state.detection?.profile;
  const profile = liveProfile || preview;
  const engaged = Boolean(state.status?.engaged);
  const canApply = Boolean(preview && state.vpnInterfaces?.length && state.detection?.vpn_interfaces?.length);
  const platform = state.session?.platform;
  const verify = state.status?.verify || [];
  const verificationIssue = verify.some((result) => !result.ok);
  const statusState = engaged
    ? (verificationIssue ? ["Settings differ", "caution"] : ["Applied", "good"])
    : preview ? ["Preview", "info"] : state.status ? ["Original", "quiet"] : ["Unknown", "quiet"];
  pill(byId("profile-badge"), ...statusState);
  setText("profile-subtitle", engaged ? "Profile saved on this device" : "Settings for your VPN exit");
  setText("zone", profile?.timezone || dash);
  setText("locale", profile?.locale || dash);
  setText("location", profile ? coordinateLabel(profile) : dash);
  setActionAvailable("apply", canApply);
  setActionAvailable("restore", engaged);
  setText("profile-help", engaged
    ? "Restore the original settings from the saved snapshot."
    : preview && !canApply
      ? "An active VPN adapter is required to apply this profile."
      : canApply ? "Time zone is included. Other settings are optional."
        : "Run Detect exit to find a location for the profile.");
  show("apply-options", canApply);
  show("location-option", platform === "linux" || platform === "darwin");
  setText("location-option-label", platform === "darwin" ? "Turn off Location Services" : "Also change system location");
  setText("platform-note", platform === "win32" ? "Windows location settings are unavailable."
    : platform === "linux" ? "Changing system location requires geoclue and elevated permissions."
      : platform === "darwin" ? "Time zone and Location Services can require elevated permissions." : "");
  show("verification", verify.length > 0);
  byId("verification-list").replaceChildren(...verify.map((entry) =>
    element("li", null, entry.backend + " · " + (entry.ok ? "Matches" : "Differs") + " · " + entry.detail)));
}

function reportSummary(report, warning) {
  if (report.outcome === "clean" && warning) return ["Warnings found", "caution"];
  return outcomes[report.outcome] || ["Report ready", "info"];
}

function renderLeakGuidance(report, warning) {
  const guidance = byId("leak-guidance");
  const routeLeak = report?.checks.some((check) => check.id === "webrtc" && check.verdict === "leak");
  const confirmedLeak = report?.outcome === "leaks_found";
  show("leak-guidance", confirmedLeak || warning || report?.outcome === "incomplete");
  if (guidance.hidden) return;

  guidance.dataset.tone = confirmedLeak ? "bad" : "caution";
  if (confirmedLeak) {
    setText("leak-guidance-title", routeLeak ? "Traffic bypassed your VPN" : "A network leak was confirmed");
    setText("leak-guidance-body", routeLeak
      ? "A test on a physical adapter reached the internet outside the tunnel. Use your VPN’s traffic blocking controls, then rerun these checks. Applying an exit profile only changes system settings."
      : "Expand the result marked Leak for the evidence and next steps.");
  } else {
    setText("leak-guidance-title", report?.outcome === "incomplete" ? "Some checks could not finish" : "Warnings need review");
    setText("leak-guidance-body", report?.outcome === "incomplete"
      ? "Expand any errors to see what could not be verified."
      : "Expand the warnings to see which settings or network paths need attention.");
  }
}

function renderReport() {
  const report = state.leaks;
  const badge = byId("leak-badge");
  const checks = byId("checks");
  const expanded = new Set(Array.from(checks.querySelectorAll("details[open]"), (row) => row.dataset.check));
  const results = report?.checks || [];
  const warning = results.some((check) => check.verdict === "warn");
  const status = state.busy === "leaks" ? ["Checking", "info"]
    : report ? reportSummary(report, warning) : ["Not run", "quiet"];
  pill(badge, ...status, true);
  const scanTime = state.scanAt?.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  setText("report-caption", report
    ? (state.busy === "leaks" ? "Showing the previous results while checks run." : "Last checked at " + scanTime)
    : "Check for traffic that could escape your VPN tunnel.");
  renderLeakGuidance(report, warning);
  checks.replaceChildren();
  if (!results.length) {
    checks.append(element("div", "empty-checks", state.busy === "leaks"
      ? "Waiting for diagnostic results…" : "Run leak checks to see DNS, IPv6, and WebRTC results."));
  }
  for (const check of results) {
    const [name, glyph] = checkNames[check.id] || [check.id, "shield"];
    const row = element("details", "check");
    row.dataset.check = check.id;
    row.open = expanded.has(check.id);
    const label = element("span", "check-name");
    const glyphBox = element("span", "check-icon");
    glyphBox.append(icon(glyph));
    label.append(glyphBox, document.createTextNode(name));
    const summary = element("summary");
    const verdict = element("span");
    pill(verdict, ...(verdicts[check.verdict] || [check.verdict, "quiet"]));
    summary.append(label, element("span", "check-preview", check.summary), verdict, icon("chevron", "check-chevron"));
    const body = element("div", "check-body");
    if (check.details?.length) {
      const details = element("ul");
      details.append(...check.details.map((text) => element("li", null, text)));
      body.append(details);
    } else {
      body.append(element("p", null, "No additional observations."));
    }
    if (check.advice) body.append(element("p", "advice", check.advice));
    row.append(summary, body);
    checks.append(row);
  }
  const hosts = report?.inputs?.third_parties || [];
  show("hosts", hosts.length > 0);
  byId("host-list").replaceChildren(...hosts.map((host) => element("li", null, host)));
}

function render() {
  show("mode", state.session?.demo);
  pill(byId("mode"), "Demo mode", "caution");
  show("connection", !state.session?.demo);
  const status = state.status ? ["CLI ready", "good"] : state.session ? ["CLI unavailable", "quiet"] : ["Connecting", "quiet"];
  pill(byId("connection"), ...status, true);
  for (const button of ["detect", "scan", "refresh"]) setActionAvailable(button, Boolean(state.session));
  byId("detect").querySelector("span").textContent = state.busy === "detect" ? "Detecting…" : "Detect exit";
  byId("scan").querySelector("span").textContent = state.busy === "leaks" ? "Checking…" : "Run leak checks";
  byId("active-dns").disabled = state.busy === "leaks";
  setText("status-time", state.checkedAt
    ? "Status checked at " + state.checkedAt.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : "Local diagnostics");
  renderLocation();
  renderProfile();
  renderReport();
}

function route(focus = false) {
  const hash = location.hash.slice(1);
  const view = Object.hasOwn(views, hash) ? hash : "overview";
  const [name, title, description] = views[view];
  byId("dashboard").dataset.view = view;
  setText("breadcrumb", name);
  setText("page-title", title);
  setText("page-description", description);
  document.querySelectorAll(".nav a[data-view]").forEach((link) => {
    if (link.dataset.view === view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  show("map-panel", view === "overview");
  show("profile-panel", view !== "leaks");
  show("leaks-panel", view !== "profile");
  show("detect", view !== "leaks");
  show("scan", view !== "profile");
  if (focus) byId("page-title").focus({ preventScroll: true });
  drawMap();
}

async function request(command, options = {}) {
  const response = await fetch("/api/" + command, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Geoveil-Token": state.session.token },
    body: JSON.stringify(options),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || "Geoveil returned an error.");
  return body;
}

async function refreshStatus() {
  try {
    const { data } = await request("status");
    if (!sameAdapters(state.vpnInterfaces, data.vpn_interfaces)) {
      state.detection = null;
      state.leaks = null;
    }
    state.status = data;
    state.vpnInterfaces = data.vpn_interfaces;
    state.checkedAt = new Date();
  } catch (error) {
    state.status = null;
    state.vpnInterfaces = null;
    throw error;
  }
}

async function perform(command, options) {
  if (state.busy || !state.session) return;
  state.busy = command;
  setText("operation-status", progress[command]);
  notice("");
  render();
  try {
    if (command === "status") {
      await refreshStatus();
      setText("operation-status", "System status updated.");
      return;
    }
    const { data, exitCode } = await request(command, options);
    if (command === "detect") {
      if (state.leaks && (!sameAdapters(state.vpnInterfaces, data.vpn_interfaces)
        || state.leaks.exit_ip !== detectionIp(data))) state.leaks = null;
      state.detection = data;
      state.vpnInterfaces = data.vpn_interfaces;
      if (data.error) {
        const source = data.vpn_interfaces.length ? "VPN adapter detected." : "No VPN adapter detected.";
        const lookup = detectionIp(data) ? "Public IP found. Location lookup failed." : "The public IP and location could not be resolved.";
        notice(source + " " + lookup + " Try Detect exit again.", true);
        setText("operation-status", "");
      } else if (!data.vpn_interfaces.length) {
        notice("No VPN adapter detected. The estimate may reflect your direct internet connection.");
      } else {
        setText("operation-status", "Exit location updated.");
      }
    } else if (command === "leaks") {
      state.leaks = data;
      state.scanAt = new Date();
      setText("operation-status", reportSummary(data, data.checks.some((check) => check.verdict === "warn"))[0] + ".");
      const adapterChanged = !sameAdapters(state.vpnInterfaces, data.vpn_interfaces);
      const previousIp = detectionIp(state.detection);
      state.vpnInterfaces = data.vpn_interfaces;
      if (state.detection && (adapterChanged || (data.exit_ip && previousIp && data.exit_ip !== previousIp))) {
        state.detection = null;
        notice("The connection changed since the last location lookup. Run Detect exit again to update the map.");
      }
    } else {
      setText("operation-status", "");
      const failures = (data.report?.outcomes || []).filter((result) => !result.ok);
      const details = failures.map((result) => result.backend + " · " + result.error).join(" ");
      const message = exitCode !== 0
        ? "Some settings could not be updated. " + details
        : data.message || (command === "apply" ? "Exit profile applied." : "Original settings restored.");
      try {
        await refreshStatus();
        notice(message, exitCode !== 0);
      } catch (error) {
        notice(message + " Status could not be refreshed. " + error.message, true);
      }
    }
  } catch (error) {
    notice(error.message, true);
  } finally {
    state.busy = "";
    render();
  }
}

const world = byId("world");
const mapContext = world.getContext("2d");

function drawMap() {
  if (!mapContext || !world.clientWidth || !world.clientHeight) return;
  const ctx = mapContext;
  const { clientWidth: width, clientHeight: height } = world;
  const scale = window.devicePixelRatio || 1;
  world.width = Math.round(width * scale);
  world.height = Math.round(height * scale);
  ctx.scale(scale, scale);

  const unit = Math.min(width / 371, height / 158);
  const x = (lon) => width / 2 + lon * unit;
  const y = (lat) => height / 2 + (10 - lat) * unit;
  ctx.lineWidth = 1;
  ctx.strokeStyle = "#91bdd01d";
  ctx.setLineDash([2, 5]);
  ctx.beginPath();
  for (let lon = -150; lon <= 150; lon += 30) {
    ctx.moveTo(x(lon), Math.max(2, y(82)));
    ctx.lineTo(x(lon), Math.min(height - 2, y(-63)));
  }
  for (let lat = -60; lat <= 60; lat += 30) {
    ctx.moveTo(x(-180), y(lat));
    ctx.lineTo(x(180), y(lat));
  }
  ctx.stroke();
  ctx.setLineDash([]);

  ctx.fillStyle = "#527f8a";
  ctx.beginPath();
  landRows.forEach((row, index) => {
    for (let column = 0; column < row.length; column++) {
      if (row[column] !== "1") continue;
      const radius = Math.max(.65, unit * .43);
      const cx = x(-179 + 2 * column);
      const cy = y(83 - 2 * index);
      ctx.moveTo(cx + radius, cy);
      ctx.arc(cx, cy, radius, 0, Math.PI * 2);
    }
  });
  ctx.fill();

  const profile = state.detection?.profile;
  if (!profile || ![profile.lat, profile.lon].every(Number.isFinite)) return;
  const mx = x(profile.lon);
  const my = Math.min(height - 18, Math.max(21, y(profile.lat)));
  ctx.fillStyle = "#65e9c022";
  ctx.strokeStyle = "#79f5d091";
  ctx.beginPath();
  ctx.arc(mx, my, 17, 0, Math.PI * 2);
  ctx.fill();
  ctx.stroke();
  ctx.fillStyle = "#6debc4";
  ctx.beginPath();
  ctx.arc(mx, my, 4, 0, Math.PI * 2);
  ctx.fill();

  const label = countryName(profile.country);
  if (label === dash || width < 280) return;
  ctx.font = '600 11px "Segoe UI", sans-serif';
  const labelWidth = ctx.measureText(label).width + 17;
  const fitsRight = mx + 24 + labelWidth < width - 6;
  const lx = fitsRight ? mx + 23 : Math.max(6, mx - 23 - labelWidth);
  const ly = Math.max(6, Math.min(height - 31, my - 12));
  ctx.fillStyle = "#122a2c";
  ctx.strokeStyle = "#4ea78c";
  ctx.beginPath();
  ctx.roundRect(lx, ly, labelWidth, 24, 5);
  ctx.fill();
  ctx.stroke();
  ctx.fillStyle = "#bdf2de";
  ctx.textBaseline = "middle";
  ctx.fillText(label, lx + 8, ly + 12);
}

function confirm(command) {
  if (state.busy || !state.session) return;
  const dialog = byId("confirm");
  const applying = command === "apply";
  const options = applying ? { locale: byId("use-locale").checked, location: byId("use-location").checked } : {};
  const included = ["time zone"];
  if (options.locale) included.push("regional formats");
  if (options.location) included.push(state.session.platform === "darwin" ? "Location Services" : "system location");
  const settings = new Intl.ListFormat("en").format(included);
  setText("confirm-title", applying ? "Apply exit profile?" : "Restore original settings?");
  setText("confirm-copy", state.session.demo ? "This will update the demo profile shown in the dashboard." : applying
    ? "Geoveil will resolve the current VPN exit, save your original settings, and update your " + settings + "."
    : "Geoveil will restore the settings from its saved snapshot.");
  setText("confirm-action", applying ? "Apply profile" : "Restore settings");
  dialog.returnValue = "";
  dialog.addEventListener("close", () => {
    if (dialog.returnValue === "confirm") void perform(command, options);
  }, { once: true });
  dialog.showModal();
}

byId("refresh").addEventListener("click", () => void perform("status"));
byId("detect").addEventListener("click", () => void perform("detect"));
byId("scan").addEventListener("click", () => void perform("leaks", { activeDns: byId("active-dns").checked }));
byId("apply").addEventListener("click", () => confirm("apply"));
byId("restore").addEventListener("click", () => confirm("restore"));
window.addEventListener("hashchange", () => route(true));
new ResizeObserver(drawMap).observe(world);
route();
render();

try {
  const session = await fetch("/api/session");
  if (!session.ok) throw new Error("The local dashboard is unavailable.");
  state.session = await session.json();
  await perform("status");
  if (state.session.demo) {
    await perform("detect");
    await perform("leaks");
  }
} catch (error) {
  pill(byId("connection"), "Disconnected", "bad", true);
  notice(error.message, true);
}
