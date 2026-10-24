// Desktop UI Logic & Mock/Tauri IPC Integration

const state = {
  activeView: 'instances',
  instances: [],
  selectedInstanceId: null,
  worlds: [],
  backups: [],
  serverBackupsEnabled: true,
};

// Navigation
document.querySelectorAll('.nav-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.view-panel').forEach(p => p.classList.remove('active'));
    
    btn.classList.add('active');
    const viewId = `${btn.dataset.view}-view`;
    document.getElementById(viewId)?.classList.add('active');
    state.activeView = btn.dataset.view;

    if (state.activeView === 'saves') {
      loadInstanceWorlds();
    }
  });
});

async function invokeCommand(cmd, payload = {}) {
  if (window.__TAURI_INTERNALS__) {
    return window.__TAURI_INTERNALS__.invoke(cmd, payload);
  }
  // Mock implementations for previewing UI
  console.log(`[IPC Mock] ${cmd}`, payload);
  if (cmd === 'list_instances') {
    return [
      { id: 'inst-1', name: 'Survival 1.21', minecraft_version: '1.21.1', server_backups_enabled: true },
      { id: 'inst-2', name: 'Vanilla Hardcore', minecraft_version: '1.20.4', server_backups_enabled: false }
    ];
  }
  if (cmd === 'list_instance_worlds') {
    return [
      { folder_name: 'Overworld_One', display_name: 'Overworld One', size_bytes: 48920194, last_played_timestamp: Date.now() / 1000, is_locked: false },
      { folder_name: 'Creative_Builds', display_name: 'Creative Builds', size_bytes: 14209102, last_played_timestamp: Date.now() / 1000 - 86400, is_locked: false }
    ];
  }
  if (cmd === 'list_instance_backups') {
    return [
      { file_name: 'Overworld_One_20261024_120000.zip', world_name: 'Overworld_One', size_bytes: 28419200, timestamp: new Date().toISOString(), is_remote: false }
    ];
  }
  return {};
}

async function loadInstances() {
  const instances = await invokeCommand('list_instances');
  state.instances = instances;
  const listEl = document.getElementById('instances-list');
  const selectEl = document.getElementById('backup-instance-select');
  listEl.innerHTML = '';
  selectEl.innerHTML = '';

  instances.forEach((inst, idx) => {
    if (idx === 0) state.selectedInstanceId = inst.id;
    
    // Select option
    const opt = document.createElement('option');
    opt.value = inst.id;
    opt.textContent = `${inst.name} (${inst.minecraft_version})`;
    selectEl.appendChild(opt);

    // Card
    const card = document.createElement('div');
    card.className = 'card';
    card.innerHTML = `
      <div class="card-title">${inst.name}</div>
      <div class="card-meta">Minecraft ${inst.minecraft_version}</div>
      <div>
        <span class="badge ${inst.server_backups_enabled ? 'badge-success' : 'badge-disabled'}">
          ${inst.server_backups_enabled ? 'Server Backups Allowed' : 'Server Backups Disabled'}
        </span>
      </div>
      <button class="btn btn-primary" onclick="launchInstance('${inst.id}')">Play</button>
    `;
    listEl.appendChild(card);
  });
}

async function loadInstanceWorlds() {
  const select = document.getElementById('backup-instance-select');
  const instanceId = select.value || state.selectedInstanceId;
  if (!instanceId) return;

  const currentInstance = state.instances.find(i => i.id === instanceId);
  const serverEnabled = currentInstance ? currentInstance.server_backups_enabled : false;

  const worlds = await invokeCommand('list_instance_worlds', { instanceId });
  const backups = await invokeCommand('list_instance_backups', { instanceId });

  // Render worlds
  const worldsTbody = document.getElementById('worlds-tbody');
  worldsTbody.innerHTML = '';
  worlds.forEach(w => {
    const tr = document.createElement('tr');
    tr.innerHTML = `
      <td><strong>${w.display_name}</strong></td>
      <td>${(w.size_bytes / (1024 * 1024)).toFixed(1)} MB</td>
      <td>${new Date(w.last_played_timestamp * 1000).toLocaleString()}</td>
      <td>${w.is_locked ? '<span class="badge badge-disabled">Running</span>' : '<span class="badge badge-success">Ready</span>'}</td>
      <td>
        <div class="action-group">
          <button class="btn" onclick="backupWorld('${w.folder_name}')">Backup ZIP</button>
        </div>
      </td>
    `;
    worldsTbody.appendChild(tr);
  });

  // Render backups
  const backupsTbody = document.getElementById('backups-tbody');
  backupsTbody.innerHTML = '';
  backups.forEach(b => {
    const tr = document.createElement('tr');
    tr.innerHTML = `
      <td>${b.file_name}</td>
      <td>${b.world_name}</td>
      <td>${(b.size_bytes / (1024 * 1024)).toFixed(1)} MB</td>
      <td>
        <span class="badge ${serverEnabled ? 'badge-success' : 'badge-disabled'}">
          ${serverEnabled ? 'Permitted' : 'Disabled by Server'}
        </span>
      </td>
      <td>
        <div class="action-group">
          <button class="btn" onclick="restoreBackup('${b.file_name}')">Restore</button>
          <button class="btn btn-primary" ${!serverEnabled ? 'disabled' : ''} onclick="uploadBackup('${b.file_name}')">
            Upload to Server
          </button>
        </div>
      </td>
    `;
    backupsTbody.appendChild(tr);
  });
}

document.getElementById('backup-instance-select')?.addEventListener('change', () => {
  loadInstanceWorlds();
});

async function backupWorld(worldFolderName) {
  const instanceId = document.getElementById('backup-instance-select').value;
  alert(`Zipping world ${worldFolderName}...`);
  await invokeCommand('create_world_backup', { instanceId, worldFolderName });
  loadInstanceWorlds();
}

async function uploadBackup(archiveName) {
  const instanceId = document.getElementById('backup-instance-select').value;
  alert(`Uploading zipped backup ${archiveName} to remote server...`);
  await invokeCommand('upload_world_backup', { instanceId, archiveName });
  alert(`Upload completed!`);
}

async function restoreBackup(archiveName) {
  const instanceId = document.getElementById('backup-instance-select').value;
  if (confirm(`Restore ${archiveName}? This will unpack the archive into saves.`)) {
    await invokeCommand('restore_world_backup', { instanceId, archiveName });
    alert('Restored successfully!');
    loadInstanceWorlds();
  }
}

// Initial Boot
loadInstances();
