const { test, expect } = require('@playwright/test');
const { AxeBuilder } = require('@axe-core/playwright');
const fs = require('node:fs');
const path = require('node:path');

function fixture() {
  const monitors = [
    { friendly_name: 'Laptop', stable_key: 'display#internal#1', role: 'internal', position: [0, 0], resolution: [1920, 1080] },
    { friendly_name: 'Desk Display', stable_key: 'display#external#2', role: 'external', position: [1920, 0], resolution: [2560, 1440] },
    { friendly_name: 'Saved USB Display', stable_key: 'display#usb#3', role: 'usb_indirect', position: [-1920, 0], resolution: [1920, 1080], active: false, available: false },
  ].map((m, i) => ({ active: true, available: true, is_primary: i === 0, refresh_hz: 60, edid_code: `TEST${i}`, gdi_name: `DISPLAY${i}`, ...m }));
  return {
    shortcuts: { start_menu: false, desktop: false, start_menu_path: '', desktop_path: '' },
    monitors, autostart: false, active_rule_indices: [],
    status: { state: 'active_on', active_monitors: 2, total_available_monitors: 2, snapshot_monitors: 2, snapshot_captured_unix_ms: 1700000000000, hotkey_warnings: [] },
    config: {
      theme: 'system',
      start_minimized_to_tray: true,
      lock_workstation_on_sleep: false,
      wake_on_session_unlock: false,
      wake_mode: 'hotkey_only',
      hotkeys: { toggle_all: 'Ctrl+Alt+M', toggle_externals: 'Ctrl+Alt+E', force_recover: 'Ctrl+Alt+Shift+M', monitor_hotkeys: [
        { label: 'Desk focus', hotkey: '', include: ['display#external#2'], exclude: [], action: 'toggle' },
      ] },
      recovery: { retrain_usb_displays_on_wake: true, ddc_assist: true, restore_windows: true, recover_on_system_wake: true, blackout_curtain_when_locked: true },
    },
  };
}

async function setup(page, options = {}) {
  const mock = { state: fixture(), writes: [], actions: [], offline: false, failSave: false };
  await page.route('http://127.0.0.1:33333/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname.startsWith('/api/')) {
      if (mock.offline) return route.abort();
      if (url.pathname === '/api/state') return route.fulfill({ json: mock.state });
      const body = request.postDataJSON();
      if (url.pathname === '/api/validate-hotkey') return route.fulfill({ json: { valid: true, canonical: body.hotkey } });
      if (url.pathname === '/api/config') {
        if (mock.failSave) return route.fulfill({ status: 400, json: { error: 'Settings changed since editing began.' } });
        mock.writes.push(body);
        mock.state.config = body.config;
        if ('autostart' in body) mock.state.autostart = body.autostart;
        return route.fulfill({ json: mock.state });
      }
      mock.actions.push(body);
      return route.fulfill({ json: { message: 'Action queued', state: mock.state } });
    }
    if (url.pathname === '/mascot.svg') {
      const body = fs.readFileSync(path.join(__dirname, '../../assets/source/mascot-master.svg'));
      return route.fulfill({ body, contentType: 'image/svg+xml' });
    }
    if (url.pathname === '/favicon.ico') {
      const body = fs.readFileSync(path.join(__dirname, '../../assets/icons/CtrlDark.ico'));
      return route.fulfill({ body, contentType: 'image/x-icon' });
    }
    const file = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
    const contentType = file.endsWith('.js') ? 'application/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html';
    const body = fs.readFileSync(path.join(__dirname, '../../src/ui', file), 'utf8').replace('__MT_SESSION_TOKEN__', 'fixture-token');
    return route.fulfill({ body, contentType });
  });
  if (options.theme) await page.addInitScript((theme) => localStorage.setItem('color-scheme', theme), options.theme);
  await page.goto('http://127.0.0.1:33333/');
  await expect(page.locator('#phase-badge')).toHaveText('Active');
  return mock;
}

test('refresh and display actions preserve draft settings and focus', async ({ page }) => {
  await setup(page);
  const input = page.locator('#hk-toggle-all');
  await input.fill('Ctrl+Shift+F8');
  await page.locator('#btn-refresh').click();
  await expect(input).toHaveValue('Ctrl+Shift+F8');
  await input.focus();
  await page.waitForTimeout(1800); // Exercise the automatic state refresh.
  await expect(input).toBeFocused();
  await expect(input).toHaveValue('Ctrl+Shift+F8');
  await page.locator('#btn-wake-all').click();
  await expect(input).toHaveValue('Ctrl+Shift+F8');
  await expect(page.locator('#settings-dirty-hint')).toHaveText('Unsaved changes');
});

test('recorder cancellation and successful capture preserve the correct values', async ({ page }) => {
  const mock = await setup(page);
  const first = page.locator('[data-record-target="hk-toggle-all"]');
  await first.click();
  await first.click();
  await expect(page.locator('#hk-toggle-all')).toHaveValue('Ctrl+Alt+M');
  await first.click();
  await page.locator('[data-record-target="hk-toggle-externals"]').click();
  await expect(page.locator('#hk-toggle-all')).toHaveValue('Ctrl+Alt+M');
  await page.keyboard.press('Escape');
  await first.click();
  await page.keyboard.press('Control+Shift+F8');
  await expect(page.locator('#settings-dirty-hint')).toHaveText('Unsaved changes');
  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].config.hotkeys.toggle_all).toBe('Ctrl+Shift+F8');
});

test('builder validates selection and excludes offline displays from outcome', async ({ page }) => {
  const mock = await setup(page);
  await page.locator('#btn-new-config').click();
  await page.locator('#cfg-label').fill('New desk');
  await page.locator('#btn-save-config').click();
  await expect(page.locator('#cfg-matrix-error')).toBeVisible();
  await page.locator('#monitor-matrix .matrix-row').nth(1).locator('[value="include"]').check();
  await expect(page.locator('#outcome-preview')).not.toContainText('Saved USB Display');
  await page.locator('#btn-save-config').click();
  await expect(page.locator('#config-dialog')).not.toBeVisible();
  expect(mock.writes[0].config.hotkeys.monitor_hotkeys.at(-1).include).toEqual(['display#external#2']);
  expect(mock.writes[0]).not.toHaveProperty('autostart');
});

test('modal errors are visible and disconnected controls cannot mutate state', async ({ page }) => {
  const mock = await setup(page);
  await page.getByRole('button', { name: 'Edit: Desk focus', exact: true }).click();
  mock.offline = true;
  await page.locator('#btn-test-config').click();
  await expect(page.locator('#config-alert')).toBeVisible();
  await page.locator('#btn-cancel-config').click();
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#phase-badge')).toHaveText('Disconnected');
  await expect(page.locator('#btn-new-config')).toBeDisabled();
  await expect(page.locator('#btn-toggle-all')).toBeDisabled();
});

test('stale editor saves retain their baseline and double submit sends one write', async ({ page }) => {
  const mock = await setup(page);
  await page.getByRole('button', { name: 'Edit: Desk focus', exact: true }).click();
  mock.state.config.hotkeys.toggle_all = 'Ctrl+F7';
  await page.waitForTimeout(1800);
  mock.failSave = true;
  await page.locator('#btn-save-config').click();
  await expect(page.locator('#config-alert')).toContainText('Settings changed');
  await expect(page.locator('#cfg-hotkey-error')).not.toBeVisible();
  await expect(page.locator('#config-dialog')).toBeVisible();
  mock.failSave = false;
  await page.locator('#config-form').evaluate((form) => {
    form.requestSubmit();
    form.requestSubmit();
  });
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].expected_config.hotkeys.toggle_all).toBe('Ctrl+Alt+M');
});

test('hotkey save errors highlight the shortcut field and profile saves merge with dirty settings', async ({ page }) => {
  const mock = await setup(page);
  await page.locator('#hk-toggle-all').fill('Ctrl+Shift+F9');
  await expect(page.locator('#settings-dirty-hint')).toHaveText('Unsaved changes');

  await page.locator('#btn-new-config').click();
  await page.locator('#cfg-label').fill('External Work');
  await page.locator('#cfg-hotkey').fill('Ctrl+Alt+M');
  await page.locator('#monitor-matrix .matrix-row').nth(1).locator('[value="include"]').check();

  // Simulate backend duplicate-shortcut error first -> should highlight #cfg-hotkey-error.
  await page.route('http://127.0.0.1:33333/api/config', async (route) => {
    const body = route.request().postDataJSON();
    if (body.config.hotkeys.monitor_hotkeys.at(-1)?.hotkey === 'Ctrl+Alt+M') {
      return route.fulfill({ status: 400, json: { error: "Duplicate shortcut 'Ctrl+Alt+M'" } });
    }
    return route.fallback();
  });

  await page.locator('#btn-save-config').click();
  await expect(page.locator('#cfg-hotkey-error')).toContainText('Duplicate shortcut');
  await expect(page.locator('#config-alert')).not.toBeVisible();

  await page.locator('#cfg-hotkey').fill('Ctrl+Alt+8');
  await page.locator('#btn-save-config').click();
  await expect(page.locator('#config-dialog')).not.toBeVisible();
  await expect(page.locator('#hk-toggle-all')).toHaveValue('Ctrl+Shift+F9');

  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(2);
  expect(mock.writes[1].expected_config.hotkeys.monitor_hotkeys.at(-1).label).toBe('External Work');
  expect(mock.writes[1].config.hotkeys.monitor_hotkeys.at(-1).label).toBe('External Work');
  expect(mock.writes[1].config.hotkeys.toggle_all).toBe('Ctrl+Shift+F9');
});

test('discard resets validation and long display labels fit narrow screens', async ({ page }) => {
  const mock = await setup(page);
  await page.locator('#hk-toggle-all').fill('Ctrl+Shift+F8');
  await page.locator('#btn-revert-settings').click();
  await expect(page.locator('#hk-toggle-all')).toHaveValue('Ctrl+Alt+M');
  await expect(page.locator('#settings-dirty-hint')).toBeEmpty();
  mock.state.monitors[0].friendly_name = 'VeryLongDisplayName'.repeat(12);
  await page.setViewportSize({ width: 320, height: 640 });
  await page.locator('#btn-refresh').click();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.locator('#btn-new-config').click();
  expect(await page.locator('#config-dialog').evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
  await page.emulateMedia({ reducedMotion: 'reduce' });
  expect(await page.locator('#btn-save-config').evaluate((el) => getComputedStyle(el).transitionDuration)).toBe('0s');
});

test('validation locks the editor transaction and preserves error focus', async ({ page }) => {
  const mock = await setup(page);
  await page.getByRole('button', { name: 'Edit: Desk focus', exact: true }).click();
  await page.locator('#cfg-hotkey').fill('Ctrl+Shift+F8');
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('http://127.0.0.1:33333/api/validate-hotkey', async (route) => {
    await gate;
    await route.fulfill({ json: { valid: false, error: 'Shortcut is unavailable' } });
  });
  await page.locator('#btn-save-config').click();
  try {
    await expect(page.locator('#config-form')).toHaveAttribute('aria-busy', 'true');
    await expect(page.locator('#btn-save-config')).toHaveText('Saving…');
    await expect(page.locator('#btn-cancel-config')).toBeDisabled();
    await expect(page.locator('#btn-close-config-dialog')).toBeDisabled();
    await page.keyboard.press('Escape');
    await expect(page.locator('#config-dialog')).toBeVisible();
    expect(await page.locator('#config-form').evaluate((form) => form.inert)).toBe(true);
  } finally { release(); }
  await expect(page.locator('#cfg-hotkey-error')).toContainText('Shortcut is unavailable');
  await expect(page.locator('#cfg-hotkey')).toBeFocused();
  await expect(page.locator('#btn-cancel-config')).toBeEnabled();
  expect(mock.writes).toHaveLength(0);
  await page.locator('#btn-cancel-config').click();
  await expect(page.locator('#config-dialog')).not.toBeVisible();
});

test('pending settings validation cannot be overwritten by polling', async ({ page }) => {
  const mock = await setup(page);
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('http://127.0.0.1:33333/api/validate-hotkey', async (route) => {
    await gate;
    return route.fallback();
  });
  await page.locator('#btn-save-settings').click();
  try {
    await expect(page.locator('#settings-form')).toHaveAttribute('aria-busy', 'true');
    mock.state.config.hotkeys.toggle_all = 'Ctrl+Shift+F9';
    await page.waitForTimeout(1800);
    await page.locator('#btn-refresh').click();
    await expect(page.locator('#hk-toggle-all')).toHaveValue('Ctrl+Alt+M');
  } finally { release(); }
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].expected_config.hotkeys.toggle_all).toBe('Ctrl+Alt+M');
  expect(mock.writes[0].config.hotkeys.toggle_all).toBe('Ctrl+Alt+M');
});

test('profile actions include the rendered rule to detect stale indices', async ({ page }) => {
  const mock = await setup(page);
  const rendered = structuredClone(mock.state.config.hotkeys.monitor_hotkeys[0]);
  mock.state.config.hotkeys.monitor_hotkeys[0].include = ['internal'];
  await page.getByRole('button', { name: 'Toggle Now: Desk focus', exact: true }).click();
  await expect.poll(() => mock.actions.length).toBe(1);
  expect(mock.actions[0]).toMatchObject({ action: 'toggle_rule', index: 0, expected_rule: rendered });
});

test('duplicate opens an unsaved draft and modal test layout shows in-dialog feedback', async ({ page }) => {
  const mock = await setup(page);
  await page.getByRole('button', { name: 'Duplicate: Desk focus', exact: true }).click();
  await expect(page.locator('#config-dialog')).toBeVisible();
  await expect(page.locator('#cfg-label')).toHaveValue('Desk focus (Copy)');
  expect(mock.writes).toHaveLength(0);

  await page.locator('#btn-test-config').click();
  await expect(page.locator('#config-preview-bar')).toBeVisible();
  await expect.poll(() => mock.actions.length).toBe(1);
  expect(mock.actions[0]).toMatchObject({ action: 'apply_preview' });

  await page.locator('#btn-cancel-config').click();
  await expect(page.locator('#config-dialog')).not.toBeVisible();
  await expect.poll(() => mock.actions.length).toBe(2);
  expect(mock.actions[1]).toEqual({ action: 'wake_all' });
  expect(mock.writes).toHaveLength(0);

  await page.getByRole('button', { name: 'Duplicate: Desk focus', exact: true }).click();
  await page.locator('#btn-save-config').click();
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].config.hotkeys.monitor_hotkeys).toHaveLength(2);
  expect(mock.writes[0].config.hotkeys.monitor_hotkeys[1].label).toBe('Desk focus (Copy)');
});

test('dismissed alerts stay hidden across polling and settings hotkey errors highlight fields', async ({ page }) => {
  const mock = await setup(page);
  await expect(page.locator('#btn-revert-settings')).toBeDisabled();

  mock.state.status.hotkey_warnings = ['Hotkey Ctrl+Alt+M already in use'];
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#alert-banner')).toBeVisible();
  await expect(page.locator('#alert-prefix')).toHaveText('Warning:');
  await page.locator('#btn-dismiss-alert').click();
  await expect(page.locator('#alert-banner')).not.toBeVisible();

  await page.waitForTimeout(1700);
  await expect(page.locator('#alert-banner')).not.toBeVisible();

  mock.state.status.hotkey_warnings = ['Hotkey Ctrl+Alt+E already in use'];
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#alert-banner')).toBeVisible();

  await page.locator('#hk-toggle-externals').fill('Ctrl+Alt+M');
  await expect(page.locator('#btn-revert-settings')).toBeEnabled();
  await page.route('http://127.0.0.1:33333/api/config', (route) =>
    route.fulfill({
      status: 400,
      json: { error: "Duplicate shortcut 'Ctrl+Alt+M': used by both 'Toggle All Monitors' and 'Toggle External Monitors'" },
    }),
  );
  await page.locator('#btn-save-settings').click();
  await expect(page.locator('#hk-toggle-externals-error')).toContainText('Duplicate shortcut');
  await expect(page.locator('#hk-toggle-externals')).toBeFocused();
});

test('semantic lists, distinct record labels, and Wake All label-in-name match accessibility rules', async ({ page }) => {
  const mock = await setup(page);
  await expect(page.locator('ul#monitors-list > li')).toHaveCount(3);
  await expect(page.locator('ul#configs-list > li')).toHaveCount(1);
  await expect(page.locator('#topology-stage')).toHaveAttribute('role', 'group');
  await expect(page.locator('#status-toast')).not.toHaveAttribute('aria-live', /.+/);

  await expect(page.getByRole('button', { name: 'Record Toggle All Monitors shortcut' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Record Toggle External Monitors shortcut' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Record Force Recover Displays shortcut' })).toBeVisible();
  await expect(page.locator('.app-title')).toHaveText('Ctrl+Dark');
  expect(await page.locator('.brand-icon img').evaluate((img) => img.complete && img.naturalWidth > 0)).toBe(true);

  mock.state.status.state = 'locked_off';
  mock.state.monitors.forEach((m) => { m.active = false; });
  await page.locator('#btn-refresh').click();
  const wakeButtons = page.locator('#monitors-list button');
  await expect(wakeButtons.first()).toHaveText('Wake All');
  await expect(wakeButtons.first()).toHaveAttribute('aria-label', /Wake All/);
});

test('start minimized setting persists and launcher shortcuts toggle cleanly', async ({ page }) => {
  const mock = await setup(page);
  const startMinBox = page.locator('#opt-start-minimized');
  await expect(startMinBox).toBeChecked();
  await startMinBox.uncheck();
  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].config.start_minimized_to_tray).toBe(false);

  await expect(page.locator('#shortcut-sm-status')).toHaveText('Not installed');
  await page.locator('#btn-shortcut-start-menu').click();
  await expect.poll(() => mock.actions.length).toBe(1);
  expect(mock.actions[0]).toMatchObject({ action: 'install_shortcuts', start_menu: true, desktop: false });

  mock.state.shortcuts.start_menu = true;
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#shortcut-sm-status')).toHaveText('Installed');
  await expect(page.locator('#btn-shortcut-start-menu')).toHaveText('Remove Start Menu Shortcut');
  await page.locator('#btn-shortcut-start-menu').click();
  await expect.poll(() => mock.actions.length).toBe(2);
  expect(mock.actions[1]).toMatchObject({ action: 'remove_shortcuts', start_menu: true, desktop: false });
});

test('lock workstation and wake on unlock options toggle independently', async ({ page }) => {
  const mock = await setup(page);
  const lockBox = page.locator('#opt-lock-on-sleep');
  const wakeBox = page.locator('#opt-wake-on-unlock');
  await expect(lockBox).not.toBeChecked();
  await expect(wakeBox).not.toBeChecked();

  await lockBox.check();
  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].config.lock_workstation_on_sleep).toBe(true);
  expect(mock.writes[0].config.wake_on_session_unlock).toBe(false);

  await lockBox.uncheck();
  await wakeBox.check();
  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(2);
  expect(mock.writes[1].config.lock_workstation_on_sleep).toBe(false);
  expect(mock.writes[1].config.wake_on_session_unlock).toBe(true);
});

test('selective off topology stage renders all monitors cleanly without overlap', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  const mock = await setup(page);
  mock.state.status.state = 'selective_off';
  mock.state.status.active_monitors = 2;
  mock.state.status.total_available_monitors = 4;
  mock.state.monitors = [
    {
      friendly_name: 'J560T09', stable_key: 'display#boe2775#1', gdi_name: 'DISPLAY2', edid_code: 'BOE2775',
      role: 'external', active: true, available: true, is_primary: true,
      position: [0, 0], resolution: [2560, 1600], refresh_hz: 120,
    },
    {
      friendly_name: 'P27h-20', stable_key: 'display#len61e9#2', gdi_name: 'DISPLAY3', edid_code: 'LEN61E9',
      role: 'external', active: true, available: true, is_primary: false,
      position: [6, 1600], resolution: [2560, 1440], refresh_hz: 60,
    },
    {
      friendly_name: 'ASUS MB16AC', stable_key: 'display#aus1641#3', gdi_name: '', edid_code: 'AUS1641',
      role: 'usb_indirect', active: false, available: true, is_primary: false,
      position: [2560, 362], resolution: [1920, 1080], refresh_hz: 60,
    },
    {
      friendly_name: 'Internal Display (LEN8AA5)', stable_key: 'display#len8aa5#4', gdi_name: '', edid_code: 'LEN8AA5',
      role: 'internal', active: false, available: true, is_primary: false,
      position: [2566, 1442], resolution: [2560, 1600], refresh_hz: 60,
    },
  ];
  await page.locator('#btn-refresh').click();

  const stageBoxes = page.locator('#topology-stage .stage-monitor');
  await expect(stageBoxes).toHaveCount(4);

  const stageRect = await page.locator('#topology-stage').boundingBox();
  const boxes = [];
  for (let i = 0; i < 4; i++) {
    const box = await stageBoxes.nth(i).boundingBox();
    expect(box).not.toBeNull();
    expect(box.x).toBeGreaterThanOrEqual(stageRect.x - 1);
    expect(box.y).toBeGreaterThanOrEqual(stageRect.y - 1);
    expect(box.x + box.width).toBeLessThanOrEqual(stageRect.x + stageRect.width + 1);
    expect(box.y + box.height).toBeLessThanOrEqual(stageRect.y + stageRect.height + 1);
    boxes.push(box);
  }

  for (let a = 0; a < boxes.length; a++) {
    for (let b = a + 1; b < boxes.length; b++) {
      const overlaps =
        boxes[a].x < boxes[b].x + boxes[b].width - 1 &&
        boxes[a].x + boxes[a].width > boxes[b].x + 1 &&
        boxes[a].y < boxes[b].y + boxes[b].height - 1 &&
        boxes[a].y + boxes[a].height > boxes[b].y + 1;
      expect(overlaps).toBe(false);
    }
  }

  // J560T09 above P27h-20; ASUS MB16AC above Internal; active column left of off column.
  expect(boxes[0].y).toBeLessThan(boxes[1].y);
  expect(boxes[2].y).toBeLessThan(boxes[3].y);
  expect(boxes[0].x).toBeLessThan(boxes[2].x);
  expect(boxes[1].x).toBeLessThan(boxes[3].x);

  const zActive = await stageBoxes.nth(0).evaluate((el) => Number(getComputedStyle(el).zIndex));
  const zOff = await stageBoxes.nth(2).evaluate((el) => Number(getComputedStyle(el).zIndex));
  expect(zActive).toBeGreaterThan(zOff);
});

test('editing a profile preserves untouched friendly-name selectors', async ({ page }) => {
  const mock = await setup(page);
  mock.state.config.hotkeys.monitor_hotkeys[0].include = ['Desk Display'];
  await page.locator('#btn-refresh').click();

  await page.getByRole('button', { name: 'Edit: Desk focus', exact: true }).click();
  await page.locator('#cfg-label').fill('Desk focus renamed');
  await page.locator('#btn-save-config').click();

  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].config.hotkeys.monitor_hotkeys[0].label).toBe('Desk focus renamed');
  expect(mock.writes[0].config.hotkeys.monitor_hotkeys[0].include).toEqual(['Desk Display']);
});

test('saving unrelated settings omits autostart unless explicitly toggled', async ({ page }) => {
  const mock = await setup(page);
  await page.locator('#hk-toggle-all').fill('Ctrl+Shift+F7');
  await page.locator('#btn-save-settings').click();

  await expect.poll(() => mock.writes.length).toBe(1);
  expect(mock.writes[0].autostart).toBeUndefined();
  expect(mock.writes[0].expected_autostart).toBeUndefined();

  await page.locator('#opt-autostart').check();
  await page.locator('#btn-save-settings').click();
  await expect.poll(() => mock.writes.length).toBe(2);
  expect(mock.writes[1].autostart).toBe(true);
  expect(mock.writes[1].expected_autostart).toBe(false);
});

test('header primary action handles active sleep_all, recovery_failed retry, and recovering lockouts', async ({ page }) => {
  const mock = await setup(page);
  await expect(page.locator('#btn-toggle-all')).toHaveText('Sleep All');
  await page.locator('#btn-toggle-all').click();
  await expect.poll(() => mock.actions.length).toBe(1);
  expect(mock.actions[0]).toEqual({ action: 'sleep_all' });

  mock.state.status.state = 'recovery_failed';
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#btn-toggle-all')).toHaveText('Retry Recovery');
  await page.locator('#btn-toggle-all').click();
  await expect.poll(() => mock.actions.length).toBe(2);
  expect(mock.actions[1]).toEqual({ action: 'recover', hard: false });

  mock.state.status.state = 'recovering';
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#btn-toggle-all')).toBeDisabled();
  await expect(page.locator('#btn-wake-all')).toBeDisabled();
  await expect(page.locator('#btn-force-recover')).toBeDisabled();
  await expect(page.locator('#btn-toggle-externals')).toBeDisabled();
  await expect(page.locator('#btn-snapshot')).toBeDisabled();
});

test('action errors survive refresh polls while lock errors take alert precedence', async ({ page }) => {
  const mock = await setup(page);
  await page.route('http://127.0.0.1:33333/api/action', (route) =>
    route.fulfill({ status: 400, json: { error: 'Action rejected' } }),
  );
  await page.locator('#btn-wake-all').click();
  await expect(page.locator('#alert-banner')).toContainText('Action rejected');

  await page.locator('#btn-refresh').click();
  await expect(page.locator('#alert-banner')).toContainText('Action rejected');

  mock.state.status.last_lock_error = 'Workstation lock rejected';
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#alert-banner')).toContainText('Workstation lock rejected');

  mock.state.status.last_lock_error = null;
  await page.locator('#btn-refresh').click();
  await expect(page.locator('#alert-banner')).toContainText('Action rejected');
});

test('mirrored displays sharing coordinates stay independently clickable', async ({ page }) => {
  const mock = await setup(page);
  mock.state.monitors = [
    {
      friendly_name: 'Laptop Mirror', stable_key: 'display#internal#1', gdi_name: 'DISPLAY1', edid_code: 'INT1',
      role: 'internal', active: true, available: true, is_primary: true,
      position: [0, 0], resolution: [1920, 1080], refresh_hz: 60,
    },
    {
      friendly_name: 'Projector Mirror', stable_key: 'display#ext#2', gdi_name: 'DISPLAY1', edid_code: 'EXT2',
      role: 'external', active: true, available: true, is_primary: false,
      position: [0, 0], resolution: [1920, 1080], refresh_hz: 60,
    },
  ];
  await page.locator('#btn-refresh').click();

  await expect(page.locator('#topology-stage .stage-monitor-group')).toHaveCount(1);
  const members = page.locator('#topology-stage .stage-mirror-member');
  await expect(members).toHaveCount(2);

  for (let i = 0; i < 2; i++) {
    const box = await members.nth(i).boundingBox();
    expect(box).not.toBeNull();
    const hitSelf = await members.nth(i).evaluate((el, pt) => {
      const hit = document.elementFromPoint(pt.x, pt.y);
      return hit === el || el.contains(hit);
    }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
    expect(hitSelf).toBe(true);
    await members.nth(i).click();
  }

  await expect.poll(() => mock.actions.length).toBe(2);
  expect(mock.actions).toEqual([
    { action: 'toggle_monitor', stable_key: 'display#internal#1' },
    { action: 'toggle_monitor', stable_key: 'display#ext#2' },
  ]);
});

test('timed layout preview supports Keep/Revert and theme toggle persists via set_theme', async ({ page }) => {
  const mock = await setup(page);
  await page.locator('input[name="color_scheme"][value="dark"]').check();
  await expect.poll(() => mock.actions.length).toBe(1);
  expect(mock.actions[0]).toEqual({ action: 'set_theme', theme: 'dark' });

  await page.locator('#btn-new-config').click();
  await page.locator('#cfg-label').fill('Preview Profile');
  await page.locator('#monitor-matrix .matrix-row').nth(1).locator('[value="include"]').check();

  await page.locator('#btn-test-config').click();
  await expect(page.locator('#config-preview-bar')).toBeVisible();
  await expect.poll(() => mock.actions.length).toBe(2);
  expect(mock.actions[1].action).toBe('apply_preview');

  await page.locator('#btn-keep-preview').click();
  await expect(page.locator('#config-preview-bar')).not.toBeVisible();
  expect(mock.actions).toHaveLength(2);

  await page.locator('#btn-test-config').click();
  await expect(page.locator('#config-preview-bar')).toBeVisible();
  await page.locator('#btn-revert-preview').click();
  await expect(page.locator('#config-preview-bar')).not.toBeVisible();
  await expect.poll(() => mock.actions.length).toBe(4);
  expect(mock.actions[3]).toEqual({ action: 'wake_all' });
});

for (const theme of ['light', 'dark']) {
  for (const width of [320, 768, 1024, 1920]) {
    test(`${theme} ${width}px layout and accessibility`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width, height: 900 });
      await setup(page, { theme });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      await page.screenshot({ path: testInfo.outputPath('overview.png'), fullPage: true });
      const overview = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
      expect(overview.violations).toEqual([]);
      await page.locator('#btn-new-config').click();
      expect(await page.locator('#config-dialog').evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
      await expect(page.locator('#cfg-label')).toBeFocused();
      await page.screenshot({ path: testInfo.outputPath('builder.png') });
      const dialog = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
      expect(dialog.violations).toEqual([]);
    });
  }
}
