const { contextBridge, ipcRenderer } = require('electron');
contextBridge.exposeInMainWorld('desktop', Object.freeze({
  load: () => ipcRenderer.invoke('state:load'),
  save: state => ipcRenderer.invoke('state:save', state),
  connect: settings => ipcRenderer.invoke('ssh:connect', settings),
  disconnect: () => ipcRenderer.invoke('ssh:disconnect'),
  info: () => ipcRenderer.invoke('agent:info'),
  memories: () => ipcRenderer.invoke('agent:memories'),
  chat: request => ipcRenderer.invoke('agent:chat', request),
  cancel: id => ipcRenderer.invoke('agent:cancel', id),
  importPlugin: () => ipcRenderer.invoke('plugin:import'),
  exportPlugin: plugin => ipcRenderer.invoke('plugin:export', plugin),
  window: action => ipcRenderer.invoke('window:action', action),
  onEvent: callback => {
    const listener = (_event, payload) => callback(payload);
    ipcRenderer.on('agent:event', listener);
    return () => ipcRenderer.removeListener('agent:event', listener);
  }
}));
