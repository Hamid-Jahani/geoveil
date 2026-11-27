const { app, BrowserWindow, dialog, screen, session, shell } = require('electron');
const { randomBytes } = require('node:crypto');
const { join } = require('node:path');

const documentation = 'https://github.com/Hamid-Jahani/geoveil#readme';

app.enableSandbox();
if (process.platform === 'win32') app.setAppUserModelId('io.geoveil.desktop');

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  let dashboard;
  let mainWindow;
  let closing = false;
  let readyToQuit = false;

  const launch = app.whenReady().then(async () => {
    if (closing) return;
    const capability = randomBytes(32).toString('hex');
    const { startDashboard } = await import('./server.mjs');
    if (closing) return;
    dashboard = await startDashboard({
      port: 0,
      binary: app.isPackaged ? join(process.resourcesPath, 'bin', 'geoveil.exe') : undefined,
      capability,
      vpnLookup: app.isPackaged && process.platform === 'win32',
    });
    if (closing) return;

    const dashboardSession = session.fromPartition('geoveil-dashboard');
    await dashboardSession.setProxy({ mode: 'direct' });
    if (closing) return;

    dashboardSession.setPermissionCheckHandler(() => false);
    dashboardSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
    dashboardSession.on('will-download', (event) => event.preventDefault());

    const workArea = screen.getPrimaryDisplay().workAreaSize;
    const window = new BrowserWindow({
      title: 'Geoveil',
      icon: join(__dirname, 'geoveil.ico'),
      width: Math.min(1440, workArea.width),
      height: Math.min(940, workArea.height),
      minWidth: 640,
      minHeight: 640,
      show: false,
      backgroundColor: '#101816',
      webPreferences: {
        session: dashboardSession,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
        webviewTag: false,
        devTools: !app.isPackaged,
      },
    });
    mainWindow = window;
    window.setMenu(null);

    const contents = window.webContents;
    const contentsId = contents.id;
    const ownedOrigin = (url) => URL.parse(url)?.origin === dashboard.origin;
    const ownedRequest = (details) => details.webContentsId === contentsId && ownedOrigin(details.url);
    dashboardSession.webRequest.onBeforeRequest((details, callback) => {
      callback({ cancel: !ownedRequest(details) });
    });
    dashboardSession.webRequest.onBeforeSendHeaders((details, callback) => {
      if (!ownedRequest(details)) {
        callback({ cancel: true });
        return;
      }
      const headers = details.requestHeaders;
      for (const name of Object.keys(headers)) {
        if (name.toLowerCase() === 'x-geoveil-desktop') delete headers[name];
      }
      headers['X-Geoveil-Desktop'] = capability;
      callback({ requestHeaders: headers });
    });

    const guardNavigation = (event) => {
      if (!ownedOrigin(event.url)) event.preventDefault();
    };
    contents.on('will-navigate', guardNavigation);
    contents.on('will-redirect', guardNavigation);
    contents.on('will-frame-navigate', (event) => {
      if (!event.isMainFrame || !ownedOrigin(event.url)) event.preventDefault();
    });
    contents.on('will-attach-webview', (event) => event.preventDefault());
    contents.setWindowOpenHandler(({ url }) => {
      if (url === documentation) {
        void shell.openExternal(documentation).catch((error) => console.error(error));
      }
      return { action: 'deny' };
    });

    window.once('ready-to-show', () => { if (!closing) window.show(); });
    window.on('close', (event) => {
      if (readyToQuit) return;
      event.preventDefault();
      app.quit();
    });
    window.on('closed', () => { mainWindow = null; });
    await window.loadURL(dashboard.origin + '/');
  }).catch((error) => {
    console.error(error);
    if (!closing) {
      dialog.showErrorBox('Geoveil could not start', error.message);
      app.quit();
    }
  });

  app.on('second-instance', () => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.focus();
    }
  });
  app.on('before-quit', (event) => {
    if (readyToQuit) return;
    event.preventDefault();
    if (closing) return;
    closing = true;
    if (dashboard?.isMutating && mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.setTitle('Geoveil — Finishing system changes…');
    }
    void launch.then(() => dashboard?.close()).catch((error) => console.error(error)).finally(() => {
      readyToQuit = true;
      app.quit();
    });
  });
  app.on('window-all-closed', () => app.quit());
}
