(() => {
  'use strict';

  const SESSION_TOKEN =
    document.querySelector('meta[name="mt-session-token"]')?.getAttribute('content') || '';

  let appState = null;
  let editingIndex = null; // null = create new, integer = edit existing
  let pendingDeleteIndex = null;
  let recordingInput = null;
  let recordingPrevValue = '';
  let toastTimer = null;
  let announceTimer = null;
  let settingsDirty = false;
  let connected = false;
  let actionBusy = false;
  let configBusy = false;
  let submissionBusy = false;
  let refreshing = false;
  let stateVersion = 0;
  let monitorSignature = '';
  let ruleSignature = '';
  let builderMonitors = [];
  let builderConfig = null;
  let settingsBase = null;
  let deleteBase = null;
  let currentAlertKey = '';
  let dismissedAlertKey = '';

  function markSettingsDirty() {
    if (!settingsDirty && appState) settingsBase = structuredClone(appState.config);
    settingsDirty = true;
    document.getElementById('settings-dirty-hint').textContent = 'Unsaved changes';
  }

  function advanceSettingsBaseProfiles(prevConfig, updatedConfig) {
    if (!settingsDirty || !settingsBase || !prevConfig || !updatedConfig) return;
    const sameGlobal =
      prevConfig.start_minimized_to_tray === updatedConfig.start_minimized_to_tray &&
      prevConfig.lock_workstation_on_sleep === updatedConfig.lock_workstation_on_sleep &&
      prevConfig.wake_on_session_unlock === updatedConfig.wake_on_session_unlock &&
      prevConfig.wake_mode === updatedConfig.wake_mode &&
      prevConfig.hotkeys.toggle_all === updatedConfig.hotkeys.toggle_all &&
      prevConfig.hotkeys.toggle_externals === updatedConfig.hotkeys.toggle_externals &&
      prevConfig.hotkeys.force_recover === updatedConfig.hotkeys.force_recover &&
      JSON.stringify(prevConfig.recovery) === JSON.stringify(updatedConfig.recovery);
    if (sameGlobal) {
      settingsBase.hotkeys.monitor_hotkeys = structuredClone(updatedConfig.hotkeys.monitor_hotkeys);
    }
  }

  function syncControls() {
    document.querySelectorAll('button').forEach((button) => {
      if (button.id === 'btn-refresh' || button.id === 'btn-dismiss-alert' ||
          button.closest('#delete-dialog')) return;
      if (button.id === 'btn-close-config-dialog' || button.id === 'btn-cancel-config') {
        button.disabled = submissionBusy;
        return;
      }
      button.disabled = !connected || configBusy || submissionBusy || actionBusy || button.dataset.offline === 'true';
    });
    const snapshot = document.getElementById('btn-snapshot');
    snapshot.disabled ||= appState?.status.state !== 'active_on';
    snapshot.title = snapshot.disabled ? 'Wake all displays before replacing the recovery snapshot' : '';
    document.getElementById('btn-revert-settings').disabled =
      !connected || !settingsDirty || configBusy || submissionBusy || actionBusy;
    document.getElementById('settings-form').inert = !connected || configBusy || submissionBusy;
    document.getElementById('config-form').inert = submissionBusy;
  }


  // Theme management (dark-mode & dark-mode-toggle)
  const colorSchemeMeta = document.querySelector('meta[name="color-scheme"]');
  const darkMedia = window.matchMedia('(prefers-color-scheme: dark)');

  function syncThemeUI() {
    let stored = null;
    try {
      stored = localStorage.getItem('color-scheme');
    } catch (_) {}
    const mode = stored === 'light' || stored === 'dark' ? stored : 'system';
    if (colorSchemeMeta) {
      colorSchemeMeta.content = mode === 'system' ? 'light dark' : mode;
    }
    const sysText = darkMedia.matches ? 'dark' : 'light';
    const sysLabel = document.getElementById('system-theme-label');
    if (sysLabel) {
      sysLabel.textContent = `System (Currently ${sysText})`;
    }
    document.querySelectorAll('input[name="color_scheme"]').forEach((radio) => {
      radio.checked = radio.value === mode;
    });
  }

  darkMedia.addEventListener('change', () => {
    // Only update the informative "(Currently light/dark)" label; never overwrite a pinned choice.
    syncThemeUI();
  });

  window.addEventListener('storage', (e) => {
    if (e.key === 'color-scheme') syncThemeUI();
  });

  document.querySelectorAll('input[name="color_scheme"]').forEach((radio) => {
    radio.addEventListener('change', () => {
      if (!radio.checked) return;
      try {
        if (radio.value === 'system') {
          localStorage.removeItem('color-scheme');
        } else {
          localStorage.setItem('color-scheme', radio.value);
        }
      } catch (_) {}
      syncThemeUI();
    });
  });

  // Centralized polite screen-reader announcer (skipped when background is inert behind open modal)
  function announce(msg) {
    const anyModalOpen = Boolean(document.querySelector('dialog[open]'));
    if (anyModalOpen) return;
    const node = document.getElementById('live-announcer');
    if (!node) return;
    clearTimeout(announceTimer);
    node.textContent = '';
    announceTimer = setTimeout(() => {
      node.textContent = msg;
    }, 120);
  }

  function showToast(msg) {
    const dialogStatus = document.getElementById('config-status');
    if (configDialog.open && dialogStatus) {
      dialogStatus.textContent = msg;
      dialogStatus.hidden = false;
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => {
        dialogStatus.hidden = true;
      }, 3500);
      return;
    }
    const el = document.getElementById('status-toast');
    if (!el) return;
    el.textContent = msg;
    el.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      el.hidden = true;
    }, 3200);
    announce(msg);
  }

  function showAlert(msg, isError = true) {
    const banner = document.getElementById('alert-banner');
    const prefix = document.getElementById('alert-prefix');
    const text = document.getElementById('alert-text');
    const modalAlert = document.querySelector('dialog[open] .dialog-alert');
    if (modalAlert) {
      modalAlert.textContent = msg;
      modalAlert.hidden = false;
      return;
    }
    if (!banner || !text) return;
    currentAlertKey = `${isError ? 'err' : 'warn'}:${msg}`;
    if (prefix) prefix.textContent = isError ? 'Error: ' : 'Warning: ';
    text.textContent = msg;
    banner.classList.toggle('is-error', isError);
    banner.hidden = false;
  }

  function hideAlert() {
    const banner = document.getElementById('alert-banner');
    if (banner) banner.hidden = true;
  }

  // :user-invalid fallback & ARIA error linkage (validate-input-after-interaction)
  const UserInvalidTracker = (() => {
    const dirtyState = new WeakMap();

    const linkError = (el, shown) => {
      const errorId = el.dataset.error;
      if (!errorId) return;
      const ids = (el.getAttribute('aria-describedby') || '')
        .split(/\s+/)
        .filter((id) => id && id !== errorId);
      if (shown) ids.push(errorId);
      if (ids.length) el.setAttribute('aria-describedby', ids.join(' '));
      else el.removeAttribute('aria-describedby');
    };

    const syncField = (el) => {
      if (!el.matches?.('input, select, textarea')) return;
      const isValid = el.validity.valid;
      const showFallback = el.classList.contains('user-invalid-fallback') || !CSS.supports('selector(:user-invalid)');
      if (!CSS.supports('selector(:user-invalid)')) {
        el.classList.toggle('user-invalid-fallback', !isValid);
      } else if (!isValid && el.matches(':user-invalid')) {
        el.classList.add('user-invalid-fallback');
      } else if (isValid) {
        el.classList.remove('user-invalid-fallback');
      }
      const isShownInvalid = !isValid && ((CSS.supports('selector(:user-invalid)') && el.matches(':user-invalid')) || showFallback);
      el.setAttribute('aria-invalid', isShownInvalid ? 'true' : 'false');
      linkError(el, isShownInvalid);
    };

    const setManualError = (el, message) => {
      el.setCustomValidity(message || '');
      const errorEl = el.dataset.error ? document.getElementById(el.dataset.error) : null;
      if (message) {
        if (errorEl) {
          errorEl.textContent = message;
          errorEl.classList.add('visible');
        }
        el.classList.add('user-invalid-fallback');
        el.setAttribute('aria-invalid', 'true');
        linkError(el, true);
      } else {
        if (errorEl) {
          errorEl.classList.remove('visible');
        }
        el.classList.remove('user-invalid-fallback');
        el.setAttribute('aria-invalid', 'false');
        linkError(el, false);
      }
    };

    const bindForm = (form) => {
      form.querySelectorAll('input, select, textarea').forEach((el) => {
        if (!el.hasAttribute('aria-invalid')) el.setAttribute('aria-invalid', 'false');
      });

      form.addEventListener('input', (e) => {
        const el = e.target;
        if (!el.matches?.('input, select, textarea')) return;
        const st = dirtyState.get(el) || { interacted: false, blurred: false };
        st.interacted = true;
        dirtyState.set(el, st);
        // Clear any custom error immediately while typing
        setManualError(el, '');
        if (st.blurred) syncField(el);
      });

      form.addEventListener(
        'blur',
        (e) => {
          const el = e.target;
          if (!el.matches?.('input, select, textarea')) return;
          const st = dirtyState.get(el) || { interacted: false, blurred: false };
          st.blurred = true;
          dirtyState.set(el, st);
          if (st.interacted) syncField(el);
        },
        true
      );

      form.addEventListener(
        'invalid',
        (e) => {
          const el = e.target;
          if (!el.matches?.('input, select, textarea')) return;
          dirtyState.set(el, { interacted: true, blurred: true });
          el.classList.add('user-invalid-fallback');
          syncField(el);
        },
        true
      );
    };

    const resetFormValidation = (form) => {
      form.querySelectorAll('input, select, textarea').forEach((el) => {
        dirtyState.delete(el);
        setManualError(el, '');
      });
    };

    return { bindForm, setManualError, resetFormValidation };
  })();

  // Native <dialog> progressive fallback for closedby="any" (light-dismiss-a-dialog)
  if (!('closedBy' in HTMLDialogElement.prototype)) {
    const isOutside = (e, dlg) => {
      const r = dlg.getBoundingClientRect();
      return e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom;
    };
    for (const dlg of document.querySelectorAll('dialog[closedby="any"]')) {
      let pressedOutside = false;
      dlg.addEventListener('pointerdown', (e) => {
        pressedOutside = isOutside(e, dlg);
      });
      dlg.addEventListener('pointerup', (e) => {
        if (pressedOutside && isOutside(e, dlg)) {
          dlg.close('');
        }
      });
    }
  }

  async function apiFetch(url, options = {}) {
    const headers = new Headers(options.headers || {});
    if (SESSION_TOKEN) headers.set('X-MT-Token', SESSION_TOKEN);
    if (options.body && !headers.has('Content-Type')) {
      headers.set('Content-Type', 'application/json');
    }
    const isConfigWrite = url === '/api/config' && options.method === 'POST';
    if (isConfigWrite && configBusy) throw new Error('A save is already in progress.');
    if (isConfigWrite) { configBusy = true; stateVersion++; syncControls(); }
    try {
      const resp = await fetch(url, { ...options, headers, signal: AbortSignal.timeout(10000) });
      const data = await resp.json();
      if (!resp.ok) throw new Error(data.error || `Request failed (${resp.status})`);
      return data;
    } finally {
      if (isConfigWrite) { configBusy = false; syncControls(); }
    }
  }

  async function refreshState() {
    if (refreshing || configBusy || submissionBusy || actionBusy) return;
    refreshing = true;
    const version = stateVersion;
    try {
      const data = await apiFetch('/api/state');
      if (version !== stateVersion) return;
      connected = true;
      appState = data;
      renderAll();
      if (data.status?.hotkey_warnings?.length) {
        const msg = `Hotkey warning: ${data.status.hotkey_warnings.join(' | ')}`;
        const key = `warn:${msg}`;
        if (dismissedAlertKey !== key) {
          dismissedAlertKey = '';
          showAlert(msg, false);
        }
      } else if (data.status?.last_recovery_summary?.startsWith('Error:')) {
        const msg = data.status.last_recovery_summary;
        const key = `err:${msg}`;
        if (dismissedAlertKey !== key) {
          dismissedAlertKey = '';
          showAlert(msg, true);
        }
      } else {
        dismissedAlertKey = '';
        currentAlertKey = '';
        hideAlert();
      }
    } catch (err) {
      if (version !== stateVersion) return;
      connected = false;
      document.getElementById('phase-badge').textContent = 'Disconnected';
      showAlert(`Cannot reach the daemon. ${err.message}`, true);
    } finally {
      refreshing = false;
      syncControls();
    }
  }

  function renderAll() {
    if (!appState) return;
    renderHeader();
    const focusKey = document.activeElement?.dataset.focusKey;
    const nextMonitors = JSON.stringify([appState.monitors, appState.status.state]);
    if (nextMonitors !== monitorSignature) {
      monitorSignature = nextMonitors;
      renderTopologyStage();
      renderMonitorCards();
    }
    const nextRules = JSON.stringify([appState.config.hotkeys.monitor_hotkeys, appState.active_rule_indices]);
    if (nextRules !== ruleSignature) {
      ruleSignature = nextRules;
      renderConfigsList();
    }
    if (!settingsDirty && !recordingInput) populateSettingsForm();
    renderShortcutStatus();
    syncControls();
    if (focusKey && document.activeElement === document.body) {
      Array.from(document.querySelectorAll('[data-focus-key]')).find((el) => el.dataset.focusKey === focusKey)?.focus();
    }
  }

  function renderHeader() {
    const { status } = appState;
    const badge = document.getElementById('phase-badge');
    const summary = document.getElementById('display-summary');

    const phaseMap = {
      active_on: { text: 'Active', cls: 'status-active' },
      selective_off: { text: 'Selective Off', cls: 'status-selective' },
      locked_off: { text: 'Locked Off (Esc × 3)', cls: 'status-off' },
      sleeping_normal: { text: 'Sleeping', cls: 'status-off' },
      recovering: { text: 'Recovering…', cls: 'status-recover' },
      recovery_failed: { text: 'Recovery failed', cls: 'status-recover' },
    };
    const p = phaseMap[status.state] || { text: status.state, cls: 'status-selective' };
    badge.textContent = p.text;
    badge.className = `status-pill ${p.cls}`;
    const asleep = ['locked_off', 'sleeping_normal'].includes(status.state);
    summary.textContent = asleep ? 'All displays sleeping' : `${status.active_monitors} / ${status.total_available_monitors} displays active`;
    document.getElementById('btn-toggle-all').textContent = asleep ? 'Wake All' : 'Sleep All';

    const snapMeta = document.getElementById('snapshot-meta');
    if (status.snapshot_monitors > 0) {
      const when = status.snapshot_captured_unix_ms
        ? new Date(status.snapshot_captured_unix_ms).toLocaleString()
        : 'saved';
      snapMeta.textContent = `Recovery snapshot: ${status.snapshot_monitors} display(s) captured (${when}).`;
    } else {
      snapMeta.textContent = 'No topology snapshot captured yet.';
    }
  }

  function roleLabel(role) {
    if (role === 'usb_indirect') return 'USB / DisplayLink';
    if (role === 'internal') return 'Internal';
    return 'External';
  }

  function allDisplaysSleeping() {
    return ['locked_off', 'sleeping_normal'].includes(appState.status.state);
  }

  function renderTopologyStage() {
    const stage = document.getElementById('topology-stage');
    stage.replaceChildren();
    const monitors = appState.monitors.filter((m) => m.resolution[0] > 0 && m.resolution[1] > 0);
    if (!monitors.length) {
      const empty = document.createElement('span');
      empty.className = 'meta-text';
      empty.textContent = 'No display geometry available';
      stage.append(empty);
      return;
    }

    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const m of monitors) {
      const [x, y] = m.position;
      const [w, h] = m.resolution;
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x + w);
      maxY = Math.max(maxY, y + h);
    }

    const totalW = Math.max(1, maxX - minX);
    const totalH = Math.max(1, maxY - minY);
    const stageW = stage.clientWidth || 360;
    const stageH = stage.clientHeight || 175;
    const pad = 10;
    const scale = Math.min((stageW - pad * 2) / totalW, (stageH - pad * 2) / totalH);
    const offsetX = (stageW - totalW * scale) / 2;
    const offsetY = (stageH - totalH * scale) / 2;

    monitors.forEach((m, idx) => {
      const [x, y] = m.position;
      const [w, h] = m.resolution;
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = `stage-monitor${m.active ? '' : ' is-off'}`;
      btn.style.left = `${offsetX + (x - minX) * scale}px`;
      btn.style.top = `${offsetY + (y - minY) * scale}px`;
      btn.style.width = `${Math.max(42, w * scale - 3)}px`;
      btn.style.height = `${Math.max(28, h * scale - 3)}px`;
      btn.dataset.offline = String(!m.available);
      btn.dataset.focusKey = `stage:${m.stable_key}`;
      btn.disabled = !m.available;
      const accessibleAction = !m.available
        ? `${m.friendly_name} (disconnected)`
        : allDisplaysSleeping()
          ? `Wake All displays (including ${m.friendly_name})`
          : m.active
            ? `Turn Off ${m.friendly_name}`
            : `Restore All displays (including ${m.friendly_name})`;
      btn.setAttribute('aria-label', accessibleAction);
      btn.title = m.available
        ? `Toggle ${m.friendly_name} (${m.active ? 'currently ON' : 'currently OFF'})`
        : `${m.friendly_name} (disconnected)`;

      const name = document.createElement('span');
      name.className = 'stage-monitor-title';
      name.textContent = `${idx + 1}. ${m.friendly_name}`;

      const sub = document.createElement('span');
      sub.className = 'stage-monitor-sub tabular-nums';
      sub.textContent = !m.available ? 'Unplugged' : allDisplaysSleeping() ? 'Sleeping' : m.active ? `${w}×${h}` : 'OFF';

      btn.append(name, sub);
      btn.addEventListener('click', () => runAction({ action: 'toggle_monitor', stable_key: m.stable_key }));
      stage.append(btn);
    });
  }

  function renderMonitorCards() {
    const grid = document.getElementById('monitors-list');
    grid.replaceChildren();
    if (!appState.monitors.length) {
      const empty = document.createElement('li');
      empty.className = 'empty-state';
      empty.textContent = 'No connected displays detected.';
      grid.append(empty);
      return;
    }

    for (const m of appState.monitors) {
      const card = document.createElement('li');
      card.className = `monitor-card${m.active ? '' : ' is-off'}`;

      const top = document.createElement('div');
      top.className = 'monitor-card-top';

      const left = document.createElement('div');
      const title = document.createElement('div');
      title.className = 'monitor-name';
      title.textContent = m.friendly_name || m.edid_code || 'Display';

      const specs = document.createElement('div');
      specs.className = 'monitor-specs tabular-nums';
      const hz = m.refresh_hz > 0 ? `@${Math.round(m.refresh_hz)}Hz` : '';
      const res = m.resolution[0] > 0 ? `${m.resolution[0]}×${m.resolution[1]}${hz}` : 'Inactive';
      specs.textContent = `${res} · ${roleLabel(m.role)}`;
      left.append(title, specs);

      const sleeping = allDisplaysSleeping();
      const toggleBtn = document.createElement('button');
      toggleBtn.type = 'button';
      toggleBtn.className = `btn btn-xs ${m.active ? 'btn-outlined' : 'btn-tonal'}`;
      toggleBtn.textContent = sleeping ? 'Wake All' : m.active ? 'Turn Off' : 'Restore All';
      toggleBtn.setAttribute(
        'aria-label',
        sleeping
          ? `Wake All displays (including ${m.friendly_name})`
          : m.active
            ? `Turn Off ${m.friendly_name}`
            : `Restore All displays (including ${m.friendly_name})`
      );
      toggleBtn.dataset.offline = String(!m.available);
      toggleBtn.dataset.focusKey = `monitor:${m.stable_key}`;
      toggleBtn.disabled = !m.available;
      toggleBtn.addEventListener('click', () =>
        runAction({ action: 'toggle_monitor', stable_key: m.stable_key })
      );

      top.append(left, toggleBtn);

      const badges = document.createElement('div');
      badges.className = 'monitor-badges';
      const stateTag = document.createElement('span');
      stateTag.className = `tag ${m.active ? 'tag-on' : 'tag-off'}`;
      stateTag.textContent = !m.available ? 'Offline' : allDisplaysSleeping() ? 'Sleeping' : m.active ? 'ON' : 'OFF';
      badges.append(stateTag);

      if (m.is_primary) {
        const pri = document.createElement('span');
        pri.className = 'tag tag-primary';
        pri.textContent = 'Primary';
        badges.append(pri);
      }

      if (m.edid_code) {
        const edid = document.createElement('span');
        edid.className = 'tag';
        edid.textContent = m.edid_code;
        badges.append(edid);
      }

      card.append(top, badges);
      grid.append(card);
    }
  }

  function ruleDisplayLabel(rule) {
    if (rule.label && rule.label.trim()) return rule.label.trim();
    const inc = rule.include || [];
    const exc = rule.exclude || [];
    if (inc.length && !exc.length) return `Only ${inc.join(' & ')}`;
    if (!inc.length && exc.length) return `Exclude ${exc.join(' & ')}`;
    if (inc.length && exc.length) return `Include ${inc.join(', ')} / Exclude ${exc.join(', ')}`;
    return 'Monitor Configuration';
  }

  function selectorLabel(selector) {
    return appState.monitors.find((mon) => mon.stable_key.toLowerCase() === selector.toLowerCase())?.friendly_name || selector;
  }

  function renderConfigsList() {
    const container = document.getElementById('configs-list');
    container.replaceChildren();
    const rules = appState.config.hotkeys.monitor_hotkeys || [];
    const activeSet = new Set(appState.active_rule_indices || []);

    if (!rules.length) {
      const empty = document.createElement('li');
      empty.className = 'empty-state';
      empty.textContent = 'No custom monitor configurations saved yet. Click "New Configuration" to add one.';
      container.append(empty);
      return;
    }

    rules.forEach((rule, index) => {
      const row = document.createElement('li');
      const isActive = activeSet.has(index);
      row.className = `config-row${isActive ? ' is-active-profile' : ''}`;

      const info = document.createElement('div');
      info.className = 'config-info';

      const titleLine = document.createElement('div');
      titleLine.className = 'config-title-line';

      const nameEl = document.createElement('span');
      nameEl.className = 'config-name';
      nameEl.textContent = ruleDisplayLabel(rule);
      titleLine.append(nameEl);

      if (rule.hotkey && rule.hotkey.trim()) {
        const kbd = document.createElement('kbd');
        kbd.textContent = rule.hotkey.trim();
        titleLine.append(kbd);
      } else {
        const trayTag = document.createElement('span');
        trayTag.className = 'tag';
        trayTag.textContent = 'Tray menu only';
        titleLine.append(trayTag);
      }

      const modeTag = document.createElement('span');
      modeTag.className = 'tag';
      modeTag.textContent = rule.action === 'off' ? 'Off only' : 'Toggle';
      titleLine.append(modeTag);

      if (isActive) {
        const activeTag = document.createElement('span');
        activeTag.className = 'tag tag-on';
        activeTag.textContent = 'Active Now';
        titleLine.append(activeTag);
      }

      const pills = document.createElement('div');
      pills.className = 'config-pills';
      for (const s of rule.include || []) {
        const p = document.createElement('span');
        p.className = 'pill-inc';
        p.textContent = `Keep ON: ${selectorLabel(s)}`;
        pills.append(p);
      }
      for (const s of rule.exclude || []) {
        const p = document.createElement('span');
        p.className = 'pill-exc';
        p.textContent = `Turn OFF: ${selectorLabel(s)}`;
        pills.append(p);
      }

      info.append(titleLine, pills);

      const actions = document.createElement('div');
      actions.className = 'config-actions';

      const btnRun = document.createElement('button');
      btnRun.type = 'button';
      btnRun.className = 'btn btn-tonal btn-sm';
      btnRun.textContent = isActive && rule.action !== 'off' ? 'Restore All' : rule.action === 'off' ? 'Turn Off' : 'Toggle Now';
      btnRun.addEventListener('click', () => runAction({ action: 'toggle_rule', index, expected_rule: rule }));

      const btnEdit = document.createElement('button');
      btnEdit.type = 'button';
      btnEdit.className = 'btn btn-outlined btn-sm';
      btnEdit.textContent = 'Edit';
      btnEdit.addEventListener('click', () => openConfigDialog(index));

      const btnDup = document.createElement('button');
      btnDup.type = 'button';
      btnDup.className = 'btn btn-outlined btn-sm';
      btnDup.textContent = 'Duplicate';
      btnDup.addEventListener('click', () => duplicateConfig(index));

      const btnDel = document.createElement('button');
      btnDel.type = 'button';
      btnDel.className = 'btn btn-danger-outline btn-sm';
      btnDel.textContent = 'Delete';
      btnDel.addEventListener('click', () => openDeleteDialog(index));

      [btnRun, btnEdit, btnDup, btnDel].forEach((button, action) => {
        button.dataset.focusKey = `rule:${index}:${action}`;
        button.setAttribute('aria-label', `${button.textContent}: ${ruleDisplayLabel(rule)}`);
      });
      actions.append(btnRun, btnEdit, btnDup, btnDel);
      row.append(info, actions);
      container.append(row);
    });
  }

  function populateSettingsForm() {
    const cfg = appState.config;
    document.getElementById('hk-toggle-all').value = cfg.hotkeys.toggle_all || '';
    document.getElementById('hk-toggle-externals').value = cfg.hotkeys.toggle_externals || '';
    document.getElementById('hk-force-recover').value = cfg.hotkeys.force_recover || '';

    document.querySelectorAll('input[name="wake_mode"]').forEach((r) => {
      r.checked = r.value === cfg.wake_mode;
    });

    const rec = cfg.recovery || {};
    document.getElementById('opt-autostart').checked = Boolean(appState.autostart);
    document.getElementById('opt-start-minimized').checked = cfg.start_minimized_to_tray ?? true;
    document.getElementById('opt-lock-on-sleep').checked = Boolean(cfg.lock_workstation_on_sleep);
    document.getElementById('opt-wake-on-unlock').checked = Boolean(cfg.wake_on_session_unlock);
    document.getElementById('opt-settle-timeout').value = String(rec.settle_timeout_ms ?? 4000);
    document.getElementById('opt-retrain-usb').checked = Boolean(rec.retrain_usb_displays_on_wake);
    document.getElementById('opt-ddc').checked = Boolean(rec.ddc_assist);
    document.getElementById('opt-restore-win').checked = Boolean(rec.restore_windows);
    document.getElementById('opt-system-wake').checked = Boolean(rec.recover_on_system_wake);
    document.getElementById('opt-curtain').checked = Boolean(rec.blackout_curtain_when_locked);
    document.getElementById('opt-audio-cue').checked = rec.audio_cue_on_force_recover ?? true;
    document.getElementById('settings-dirty-hint').textContent = '';
    syncControls();
  }

  function renderShortcutStatus() {
    const shortcuts = appState?.shortcuts || { start_menu: false, desktop: false };
    const smBadge = document.getElementById('shortcut-sm-status');
    const smBtn = document.getElementById('btn-shortcut-start-menu');
    const deskBadge = document.getElementById('shortcut-desktop-status');
    const deskBtn = document.getElementById('btn-shortcut-desktop');

    if (smBadge && smBtn) {
      smBadge.textContent = shortcuts.start_menu ? 'Installed' : 'Not installed';
      smBadge.className = `tag ${shortcuts.start_menu ? 'tag-on' : 'tag-off'}`;
      smBtn.textContent = shortcuts.start_menu ? 'Remove Start Menu Shortcut' : 'Install Start Menu Shortcut';
      smBtn.className = `btn btn-sm ${shortcuts.start_menu ? 'btn-danger-outline' : 'btn-outlined'}`;
    }
    if (deskBadge && deskBtn) {
      deskBadge.textContent = shortcuts.desktop ? 'Installed' : 'Not installed';
      deskBadge.className = `tag ${shortcuts.desktop ? 'tag-on' : 'tag-off'}`;
      deskBtn.textContent = shortcuts.desktop ? 'Remove Desktop Shortcut' : 'Install Desktop Shortcut';
      deskBtn.className = `btn btn-sm ${shortcuts.desktop ? 'btn-danger-outline' : 'btn-outlined'}`;
    }
  }

  async function runAction(payload) {
    if (!connected || actionBusy || configBusy || submissionBusy) return;
    actionBusy = true;
    stateVersion++;
    syncControls();
    try {
      const resp = await apiFetch('/api/action', {
        method: 'POST',
        body: JSON.stringify(payload),
      });
      if (resp.state) {
        appState = resp.state;
        renderAll();
      }
      showToast(resp.message || 'Done');
      setTimeout(refreshState, 450);
    } catch (err) {
      showAlert(err.message, true);
    } finally {
      actionBusy = false;
      syncControls();
    }
  }

  // Interactive Keybind Recorder (with Esc isolation inside modal dialog)
  function startHotkeyRecording(inputEl, buttonEl) {
    if (recordingInput && recordingInput !== inputEl) {
      stopHotkeyRecording(true);
    }
    if (recordingInput === inputEl) {
      stopHotkeyRecording(true);
      return;
    }
    recordingInput = inputEl;
    recordingPrevValue = inputEl.value;
    inputEl.value = 'Press shortcut keys… (Esc to cancel)';
    inputEl.readOnly = true;
    buttonEl.classList.add('is-recording');
    buttonEl.textContent = 'Listening…';
    inputEl.focus();
  }

  function stopHotkeyRecording(restorePrevious = true) {
    if (!recordingInput) return;
    const inputEl = recordingInput;
    const btn = document.querySelector(`.record-btn[data-record-target="${inputEl.id}"]`);
    if (restorePrevious) {
      inputEl.value = recordingPrevValue;
    }
    inputEl.readOnly = false;
    if (btn) {
      btn.classList.remove('is-recording');
      btn.textContent = 'Record';
    }
    recordingInput = null;
    recordingPrevValue = '';
  }

  function keyTokenFromEvent(e) {
    const code = e.code || '';
    if (code.startsWith('Key') && code.length === 4) return code.slice(3).toUpperCase();
    if (code.startsWith('Digit') && code.length === 6) return code.slice(5);
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
    const map = {
      Space: 'Space',
      Tab: 'Tab',
      Home: 'Home',
      End: 'End',
      PageUp: 'PageUp',
      PageDown: 'PageDown',
      Insert: 'Insert',
      Delete: 'Delete',
      Backspace: 'Backspace',
      Pause: 'Pause',
      ScrollLock: 'ScrollLock',
    };
    return map[code] || null;
  }

  window.addEventListener(
    'keydown',
    (e) => {
      if (!recordingInput) return;
      // MANDATORY light-dismiss-a-dialog safeguard: Esc cancels key-recording, never closes modal
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        stopHotkeyRecording(true);
        return;
      }
      if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) {
        e.preventDefault();
        return;
      }

      const token = keyTokenFromEvent(e);
      if (!token) {
        e.preventDefault();
        return;
      }
      const isFn = /^F([1-9]|1[0-9]|2[0-4])$/.test(token);
      if (!e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && !isFn) {
        // Require modifier unless F1..F24
        e.preventDefault();
        return;
      }

      e.preventDefault();
      e.stopPropagation();
      const parts = [];
      if (e.ctrlKey) parts.push('Ctrl');
      if (e.altKey) parts.push('Alt');
      if (e.shiftKey) parts.push('Shift');
      if (e.metaKey) parts.push('Win');
      parts.push(token);
      recordingInput.value = parts.join('+');
      recordingInput.dispatchEvent(new Event('input', { bubbles: true }));
      UserInvalidTracker.setManualError(recordingInput, '');
      stopHotkeyRecording(false);
    },
    true
  );

  document.querySelectorAll('.record-btn').forEach((btn) => {
    btn.addEventListener('click', () => {
      const target = document.getElementById(btn.dataset.recordTarget);
      if (target) startHotkeyRecording(target, btn);
    });
  });

  // Monitor Configuration Dialog Builder
  const configDialog = document.getElementById('config-dialog');
  const configForm = document.getElementById('config-form');
  const deleteDialog = document.getElementById('delete-dialog');

  // Guard modal Esc while recording shortcut & reset returnValue on close (dialog guidance)
  configDialog.addEventListener('cancel', (e) => {
    if (submissionBusy) { e.preventDefault(); return; }
    if (recordingInput) {
      e.preventDefault();
      stopHotkeyRecording(true);
    }
  });
  configDialog.addEventListener('close', () => {
    stopHotkeyRecording(true);
    const statusEl = document.getElementById('config-status');
    if (statusEl) statusEl.hidden = true;
    configDialog.returnValue = '';
  });
  deleteDialog.addEventListener('close', () => {
    const choice = deleteDialog.returnValue;
    deleteDialog.returnValue = '';
    if (choice === 'delete' && pendingDeleteIndex !== null) {
      performDeleteConfig(pendingDeleteIndex);
    }
    pendingDeleteIndex = null;
  });

  function monitorSelectorToken(m) {
    return m.stable_key;
  }

  function selectorMatchesMonitor(sel, mon, index1Based) {
    const s = (sel || '').trim().toLowerCase();
    if (!s) return false;
    if (s === 'all') return true;
    if (s === 'external' || s === 'externals' || s === 'ext') return mon.role !== 'internal';
    if (s === 'internal') return mon.role === 'internal';
    if (/^\d+$/.test(s)) return Number(s) === index1Based;
    return (
      (mon.stable_key || '').toLowerCase().includes(s) ||
      (mon.edid_code || '').toLowerCase().includes(s) ||
      (mon.friendly_name || '').toLowerCase().includes(s) ||
      (mon.gdi_name || '').toLowerCase() === s
    );
  }

  function isLiteralMonitorSelector(sel, mon) {
    const s = (sel || '').trim().toLowerCase();
    if (!s) return false;
    return (
      s === (mon.friendly_name || '').toLowerCase() ||
      s === (mon.stable_key || '').toLowerCase() ||
      (mon.edid_code && s === mon.edid_code.toLowerCase()) ||
      (mon.gdi_name && s === mon.gdi_name.toLowerCase())
    );
  }

  function openConfigDialog(idx, prefillRule = null) {
    if (!appState) return;
    stopHotkeyRecording(true);
    editingIndex = idx;
    builderMonitors = structuredClone(appState.monitors);
    builderConfig = structuredClone(appState.config);
    document.getElementById('config-alert').hidden = true;
    const statusEl = document.getElementById('config-status');
    if (statusEl) {
      statusEl.hidden = true;
      statusEl.textContent = '';
    }
    UserInvalidTracker.resetFormValidation(configForm);
    document.getElementById('cfg-matrix-error').hidden = true;

    const rule =
      prefillRule !== null
        ? structuredClone(prefillRule)
        : idx === null
          ? { hotkey: '', label: '', include: [], exclude: [], action: 'toggle' }
          : structuredClone(appState.config.hotkeys.monitor_hotkeys[idx]);

    document.getElementById('config-dialog-title').textContent =
      prefillRule !== null
        ? `Duplicate: ${ruleDisplayLabel(rule)}`
        : idx === null
          ? 'New Monitor Configuration'
          : `Edit: ${ruleDisplayLabel(rule)}`;
    document.getElementById('cfg-label').value = rule.label || '';
    document.getElementById('cfg-hotkey').value = rule.hotkey || '';

    document.querySelectorAll('input[name="cfg_action"]').forEach((r) => {
      r.checked = r.value === (rule.action || 'toggle');
    });

    const matrix = document.getElementById('monitor-matrix');
    matrix.replaceChildren();

    const matchedInc = new Set();
    const matchedExc = new Set();

    builderMonitors.forEach((mon, monIdx) => {
      const token = monitorSelectorToken(mon);
      const isInc = (rule.include || []).some((s) => {
        if (isLiteralMonitorSelector(s, mon)) {
          matchedInc.add(s);
          return true;
        }
        return false;
      });
      const isExc = (rule.exclude || []).some((s) => {
        if (isLiteralMonitorSelector(s, mon)) {
          matchedExc.add(s);
          return true;
        }
        return false;
      });
      // Precedence if somehow both matched: exclude wins visually
      const initialMode = isExc ? 'exclude' : isInc ? 'include' : 'auto';

      const row = document.createElement('div');
      row.className = 'matrix-row';
      row.dataset.token = token;
      row.dataset.monIndex = String(monIdx);

      const left = document.createElement('div');
      left.className = 'matrix-mon-info';
      const n = document.createElement('span');
      n.className = 'matrix-mon-name';
      n.textContent = `${monIdx + 1}. ${mon.friendly_name}`;
      const meta = document.createElement('span');
      meta.className = 'matrix-mon-meta tabular-nums';
      const res = mon.resolution[0] > 0 ? `${mon.resolution[0]}×${mon.resolution[1]}` : 'Offline';
      meta.textContent = `${roleLabel(mon.role)} · ${res} · currently ${mon.active ? 'ON' : 'OFF'}`;
      left.append(n, meta);

      const seg = document.createElement('div');
      seg.className = 'tri-radios';
      seg.setAttribute('role', 'radiogroup');
      seg.setAttribute('aria-label', `${mon.friendly_name} mode`);

      for (const [val, label] of [
        ['auto', 'Auto'],
        ['include', 'Include (ON)'],
        ['exclude', 'Exclude (OFF)'],
      ]) {
        const lbl = document.createElement('label');
        lbl.className = 'tri-option';
        lbl.dataset.mode = val;
        const radio = document.createElement('input');
        radio.type = 'radio';
        radio.name = `mon_mode_${monIdx}`;
        radio.value = val;
        radio.checked = initialMode === val;
        radio.addEventListener('change', updateOutcomePreview);
        lbl.append(radio, document.createTextNode(label));
        seg.append(lbl);
      }

      row.append(left, seg);
      matrix.append(row);
    });

    const extraInc = (rule.include || []).filter((s) => !matchedInc.has(s));
    const extraExc = (rule.exclude || []).filter((s) => !matchedExc.has(s));
    document.getElementById('cfg-extra-include').value = extraInc.join(', ');
    document.getElementById('cfg-extra-exclude').value = extraExc.join(', ');
    document.querySelector('.advanced-details').open = extraInc.length > 0 || extraExc.length > 0;

    updateOutcomePreview();
    configDialog.showModal();
  }

  function gatherBuilderSelection() {
    const include = [];
    const exclude = [];

    document.querySelectorAll('#monitor-matrix .matrix-row').forEach((row) => {
      const idx = row.dataset.monIndex;
      const token = row.dataset.token;
      const checked = row.querySelector(`input[name="mon_mode_${idx}"]:checked`);
      const mode = checked ? checked.value : 'auto';
      if (mode === 'include') include.push(token);
      else if (mode === 'exclude') exclude.push(token);
    });

    const extraInc = document
      .getElementById('cfg-extra-include')
      .value.split(',')
      .map((s) => s.trim())
      .filter(Boolean);
    const extraExc = document
      .getElementById('cfg-extra-exclude')
      .value.split(',')
      .map((s) => s.trim())
      .filter(Boolean);

    include.push(...extraInc);
    exclude.push(...extraExc);
    return { include, exclude };
  }

  function updateOutcomePreview() {
    const { include, exclude } = gatherBuilderSelection();
    const preview = document.getElementById('outcome-preview');
    preview.replaceChildren();

    if (!include.length && !exclude.length) {
      const msg = document.createElement('span');
      msg.className = 'meta-text';
      msg.textContent = 'Preview: mark one or more displays above as Include (keep ON) or Exclude (turn OFF).';
      preview.append(msg);
      return;
    }

    document.getElementById('cfg-matrix-error').hidden = true;
    const staysOn = [];
    const turnsOff = [];

    builderMonitors.filter((mon) => mon.available).forEach((mon, idx) => {
      const inInc = include.some((s) => selectorMatchesMonitor(s, mon, idx + 1));
      const inExc = exclude.some((s) => selectorMatchesMonitor(s, mon, idx + 1));
      const shouldTurnOff = include.length ? !inInc || inExc : inExc;
      if (shouldTurnOff) turnsOff.push(mon.friendly_name);
      else staysOn.push(mon.friendly_name);
    });

    const rowOn = document.createElement('div');
    rowOn.className = 'outcome-row';
    const lblOn = document.createElement('span');
    lblOn.className = 'outcome-label';
    lblOn.textContent = 'Stays ON:';
    rowOn.append(lblOn);
    if (staysOn.length) {
      for (const name of staysOn) {
        const p = document.createElement('span');
        p.className = 'pill-inc';
        p.textContent = name;
        rowOn.append(p);
      }
    } else {
      const none = document.createElement('span');
      none.className = 'meta-text';
      none.textContent = 'None (all displays enter power-off sleep)';
      rowOn.append(none);
    }

    const rowOff = document.createElement('div');
    rowOff.className = 'outcome-row';
    const lblOff = document.createElement('span');
    lblOff.className = 'outcome-label';
    lblOff.textContent = 'Turns OFF:';
    rowOff.append(lblOff);
    if (turnsOff.length) {
      for (const name of turnsOff) {
        const p = document.createElement('span');
        p.className = 'pill-exc';
        p.textContent = name;
        rowOff.append(p);
      }
    } else {
      const none = document.createElement('span');
      none.className = 'meta-text';
      none.textContent = 'None matched';
      rowOff.append(none);
    }

    preview.append(rowOn, rowOff);
  }

  document.getElementById('cfg-extra-include').addEventListener('input', updateOutcomePreview);
  document.getElementById('cfg-extra-exclude').addEventListener('input', updateOutcomePreview);

  document.getElementById('btn-preset-active').addEventListener('click', () => {
    document.querySelectorAll('#monitor-matrix .matrix-row').forEach((row) => {
      const idx = Number(row.dataset.monIndex);
      const mon = builderMonitors[idx];
      const val = mon && mon.active ? 'include' : 'auto';
      row.querySelector(`input[value="${val}"]`).checked = true;
    });
    updateOutcomePreview();
  });

  document.getElementById('btn-preset-externals').addEventListener('click', () => {
    document.querySelectorAll('#monitor-matrix .matrix-row').forEach((row) => {
      const idx = Number(row.dataset.monIndex);
      const mon = builderMonitors[idx];
      const val = mon && mon.role !== 'internal' ? 'include' : 'auto';
      row.querySelector(`input[value="${val}"]`).checked = true;
    });
    updateOutcomePreview();
  });

  document.getElementById('btn-preset-clear').addEventListener('click', () => {
    document.querySelectorAll('#monitor-matrix .matrix-row').forEach((row) => {
      row.querySelector('input[value="auto"]').checked = true;
    });
    document.getElementById('cfg-extra-include').value = '';
    document.getElementById('cfg-extra-exclude').value = '';
    updateOutcomePreview();
  });

  document.getElementById('btn-clear-cfg-hk').addEventListener('click', () => {
    stopHotkeyRecording(false);
    const hkInput = document.getElementById('cfg-hotkey');
    hkInput.value = '';
    UserInvalidTracker.setManualError(hkInput, '');
  });

  document.getElementById('btn-test-config').addEventListener('click', async () => {
    const { include, exclude } = gatherBuilderSelection();
    if (!include.length && !exclude.length) {
      const errEl = document.getElementById('cfg-matrix-error');
      errEl.hidden = false;
      errEl.scrollIntoView({ block: 'nearest' });
      const firstRadio = document.querySelector('#monitor-matrix input[type="radio"]');
      (firstRadio || document.getElementById('cfg-extra-include'))?.focus();
      return;
    }
    const action = document.querySelector('input[name="cfg_action"]:checked')?.value || 'toggle';
    await runAction({
      action: 'apply_preview',
      include,
      exclude,
      off_only: action === 'off',
    });
  });

  document.getElementById('btn-close-config-dialog').addEventListener('click', () => {
    configDialog.close('');
  });
  document.getElementById('btn-cancel-config').addEventListener('click', () => {
    configDialog.close('cancel');
  });

  function bindSubmission(form, handler) {
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (submissionBusy || configBusy || actionBusy || !connected) return;
      const previousFocus = document.activeElement;
      const submitButton = form.querySelector('button[type="submit"]');
      const buttonLabel = submitButton?.textContent;
      submissionBusy = true;
      stateVersion++; // Discard state requests started before validation.
      form.setAttribute('aria-busy', 'true');
      if (submitButton) {
        submitButton.style.minWidth = `${Math.ceil(submitButton.getBoundingClientRect().width)}px`;
        submitButton.textContent = 'Saving…';
      }
      syncControls();
      try { await handler(e); }
      catch (err) { showAlert(err.message, true); }
      finally {
        submissionBusy = false;
        form.removeAttribute('aria-busy');
        if (submitButton) {
          submitButton.textContent = buttonLabel;
          submitButton.style.minWidth = '';
        }
        syncControls();
        if (form.closest('dialog')?.open !== false) {
          const invalid = form.querySelector('[aria-invalid="true"]');
          const matrixErr = form.querySelector('#cfg-matrix-error:not([hidden])');
          if (invalid) {
            invalid.focus();
          } else if (matrixErr) {
            (form.querySelector('#monitor-matrix input[type="radio"]') || form.querySelector('#cfg-extra-include'))?.focus();
          } else if (form.contains(previousFocus)) {
            previousFocus.focus();
          }
        }
      }
    });
  }

  bindSubmission(configForm, async (e) => {
    e.preventDefault();
    stopHotkeyRecording(true);

    const labelEl = document.getElementById('cfg-label');
    const hkEl = document.getElementById('cfg-hotkey');
    const labelVal = labelEl.value.trim();
    if (!labelVal) {
      UserInvalidTracker.setManualError(labelEl, 'Enter a configuration name.');
      labelEl.focus();
      return;
    }

    const { include, exclude } = gatherBuilderSelection();
    if (!include.length && !exclude.length) {
      const errEl = document.getElementById('cfg-matrix-error');
      errEl.hidden = false;
      errEl.scrollIntoView({ block: 'nearest' });
      return;
    }

    const hkRaw = hkEl.value.trim();
    if (hkRaw) {
      const check = await apiFetch('/api/validate-hotkey', {
        method: 'POST',
        body: JSON.stringify({ hotkey: hkRaw }),
      }).catch((err) => ({ valid: false, error: err.message }));
      if (!check.valid) {
        UserInvalidTracker.setManualError(hkEl, check.error || 'Invalid keyboard shortcut.');
        hkEl.focus();
        return;
      }
      hkEl.value = check.canonical || hkRaw;
    }

    const nextCfg = structuredClone(builderConfig);
    const rule = {
      hotkey: hkEl.value.trim(),
      label: labelVal,
      include,
      exclude,
      action: document.querySelector('input[name="cfg_action"]:checked')?.value || 'toggle',
    };

    if (editingIndex === null) {
      nextCfg.hotkeys.monitor_hotkeys.push(rule);
    } else {
      nextCfg.hotkeys.monitor_hotkeys[editingIndex] = rule;
    }

    const saveBtn = document.getElementById('btn-save-config');
    saveBtn.disabled = true;
    try {
      const updated = await apiFetch('/api/config', {
        method: 'POST',
        body: JSON.stringify({ config: nextCfg, expected_config: builderConfig }),
      });
      advanceSettingsBaseProfiles(builderConfig, updated.config);
      appState = updated;
      configDialog.close('saved');
      renderAll();
      showToast(`Saved "${labelVal}"`);
    } catch (err) {
      if (/shortcut|hotkey/i.test(err.message)) {
        document.getElementById('config-alert').hidden = true;
        UserInvalidTracker.setManualError(hkEl, err.message);
        hkEl.focus();
      } else {
        UserInvalidTracker.setManualError(hkEl, '');
        showAlert(err.message, true);
      }
    } finally {
      saveBtn.disabled = false;
    }
  });

  function openDeleteDialog(index) {
    pendingDeleteIndex = index;
    deleteBase = structuredClone(appState.config);
    const rule = appState.config.hotkeys.monitor_hotkeys[index];
    document.getElementById('delete-dialog-body').textContent =
      `Delete "${ruleDisplayLabel(rule)}" (${rule.hotkey || 'tray-only'})?`;
    deleteDialog.showModal();
  }

  async function performDeleteConfig(index) {
    const nextCfg = structuredClone(deleteBase);
    const [removed] = nextCfg.hotkeys.monitor_hotkeys.splice(index, 1);
    try {
      const updated = await apiFetch('/api/config', {
        method: 'POST',
        body: JSON.stringify({ config: nextCfg, expected_config: deleteBase }),
      });
      advanceSettingsBaseProfiles(deleteBase, updated.config);
      appState = updated;
      renderAll();
      showToast(`Deleted "${ruleDisplayLabel(removed)}"`);
    } catch (err) {
      showAlert(err.message, true);
    }
  }

  function duplicateConfig(index) {
    if (!appState) return;
    const orig = appState.config.hotkeys.monitor_hotkeys[index];
    if (!orig) return;
    const copy = structuredClone(orig);
    copy.label = `${ruleDisplayLabel(copy)} (Copy)`;
    copy.hotkey = ''; // Avoid conflicting shortcut automatically
    openConfigDialog(null, copy);
  }

  // Global Settings Form Submission
  const settingsForm = document.getElementById('settings-form');
  for (const event of ['input', 'change']) {
    settingsForm.addEventListener(event, () => markSettingsDirty());
  }
  document.getElementById('btn-revert-settings').addEventListener('click', () => {
    if (!settingsDirty) return;
    stopHotkeyRecording(true);
    settingsDirty = false;
    settingsBase = null;
    UserInvalidTracker.resetFormValidation(settingsForm);
    populateSettingsForm();
    showToast('Unsaved settings discarded');
  });

  bindSubmission(settingsForm, async (e) => {
    e.preventDefault();
    stopHotkeyRecording(true);
    if (!appState) return;

    const settleEl = document.getElementById('opt-settle-timeout');
    const settleVal = Number(settleEl.value);
    if (!Number.isInteger(settleVal) || settleVal < 0 || settleVal > 30000) {
      UserInvalidTracker.setManualError(settleEl, 'Enter a whole number between 0 and 30000 ms.');
      settleEl.scrollIntoView({ block: 'nearest' });
      settleEl.focus();
      return;
    }

    const hkInputs = [
      document.getElementById('hk-toggle-all'),
      document.getElementById('hk-toggle-externals'),
      document.getElementById('hk-force-recover'),
    ];

    for (const input of hkInputs) {
      const val = input.value.trim();
      if (val) {
        const check = await apiFetch('/api/validate-hotkey', {
          method: 'POST',
          body: JSON.stringify({ hotkey: val }),
        }).catch((err) => ({ valid: false, error: err.message }));
        if (!check.valid) {
          UserInvalidTracker.setManualError(input, check.error || 'Invalid shortcut');
          input.focus();
          return;
        }
        input.value = check.canonical || val;
      }
    }

    const expectedConfig = settingsBase || appState.config;
    const nextCfg = structuredClone(expectedConfig);
    nextCfg.start_minimized_to_tray = document.getElementById('opt-start-minimized').checked;
    nextCfg.lock_workstation_on_sleep = document.getElementById('opt-lock-on-sleep').checked;
    nextCfg.wake_on_session_unlock = document.getElementById('opt-wake-on-unlock').checked;
    nextCfg.hotkeys.toggle_all = hkInputs[0].value.trim();
    nextCfg.hotkeys.toggle_externals = hkInputs[1].value.trim();
    nextCfg.hotkeys.force_recover = hkInputs[2].value.trim();
    nextCfg.wake_mode =
      document.querySelector('input[name="wake_mode"]:checked')?.value || 'hotkey_only';
    nextCfg.recovery.settle_timeout_ms = settleVal;
    nextCfg.recovery.retrain_usb_displays_on_wake =
      document.getElementById('opt-retrain-usb').checked;
    nextCfg.recovery.ddc_assist = document.getElementById('opt-ddc').checked;
    nextCfg.recovery.restore_windows = document.getElementById('opt-restore-win').checked;
    nextCfg.recovery.recover_on_system_wake =
      document.getElementById('opt-system-wake').checked;
    nextCfg.recovery.blackout_curtain_when_locked =
      document.getElementById('opt-curtain').checked;
    nextCfg.recovery.audio_cue_on_force_recover =
      document.getElementById('opt-audio-cue').checked;

    const wantAutostart = document.getElementById('opt-autostart').checked;
    const submitBtn = document.getElementById('btn-save-settings');
    submitBtn.disabled = true;
    try {
      const updated = await apiFetch('/api/config', {
        method: 'POST',
        body: JSON.stringify({ config: nextCfg, expected_config: expectedConfig, autostart: wantAutostart }),
      });
      appState = updated;
      renderAll();
      settingsDirty = false;
      settingsBase = null;
      populateSettingsForm();
      showToast('Settings saved and hotkeys reloaded');
    } catch (err) {
      if (/shortcut|hotkey/i.test(err.message)) {
        const lower = err.message.toLowerCase();
        const target = lower.includes('external')
          ? hkInputs[1]
          : lower.includes('force recover')
            ? hkInputs[2]
            : hkInputs[0];
        UserInvalidTracker.setManualError(target, err.message);
        target.scrollIntoView({ block: 'nearest' });
        target.focus();
      } else {
        showAlert(err.message, true);
      }
    } finally {
      submitBtn.disabled = false;
    }
  });

  // Top-level Buttons
  document.getElementById('btn-new-config').addEventListener('click', () => openConfigDialog(null));
  document.getElementById('btn-wake-all').addEventListener('click', () => runAction({ action: 'wake_all' }));
  document.getElementById('btn-force-recover').addEventListener('click', () =>
    runAction({ action: 'recover', hard: true })
  );
  document.getElementById('btn-refresh').addEventListener('click', refreshState);
  document.getElementById('btn-toggle-all').addEventListener('click', () => runAction({ action: 'toggle_all' }));
  document.getElementById('btn-toggle-externals').addEventListener('click', () =>
    runAction({ action: 'toggle_externals' })
  );
  document.getElementById('btn-snapshot').addEventListener('click', () => runAction({ action: 'snapshot' }));
  document.getElementById('btn-shortcut-start-menu').addEventListener('click', () => {
    const installed = Boolean(appState?.shortcuts?.start_menu);
    runAction({ action: installed ? 'remove_shortcuts' : 'install_shortcuts', start_menu: true, desktop: false });
  });
  document.getElementById('btn-shortcut-desktop').addEventListener('click', () => {
    const installed = Boolean(appState?.shortcuts?.desktop);
    runAction({ action: installed ? 'remove_shortcuts' : 'install_shortcuts', start_menu: false, desktop: true });
  });
  document.getElementById('btn-dismiss-alert').addEventListener('click', () => {
    dismissedAlertKey = currentAlertKey;
    hideAlert();
  });

  window.addEventListener('resize', () => {
    if (appState) { renderTopologyStage(); syncControls(); }
  });

  // Initialize
  UserInvalidTracker.bindForm(configForm);
  UserInvalidTracker.bindForm(settingsForm);
  syncThemeUI();
  syncControls();
  refreshState();
  setInterval(() => { if (!document.hidden) refreshState(); }, 1500);
  window.addEventListener('beforeunload', (e) => {
    if (settingsDirty) { e.preventDefault(); e.returnValue = ''; }
  });
})();
