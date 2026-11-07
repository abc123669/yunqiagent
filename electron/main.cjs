const { app, BrowserWindow, ipcMain, dialog, safeStorage, Menu } = require('electron');
const { Client } = require('ssh2');
const fs = require('node:fs/promises');
const path = require('node:path');
const { StringDecoder } = require('node:string_decoder');
const crypto = require('node:crypto');
if (process.env.YUNQI_TEST_DATA && !app.isPackaged) app.setPath('userData', path.resolve(process.env.YUNQI_TEST_DATA));
const DEFAULTS = { host:'', port:22, username:'',
  binary:'~/yunqiagent/target/release/yunqiagent', model:'', fingerprint:'' };
let win, client, connecting, saved = {}, settings = DEFAULTS, password = '', writeQueue = Promise.resolve();
const channels = new Map();
const pendingCancels = new Set();
const emit = value => { if (win && !win.isDestroyed()) win.webContents.send('agent:event', value); };
const storageFile = () => path.join(app.getPath('userData'), 'workspace.json');
async function persist() {
  const snapshot = JSON.stringify({...saved, settings}, null, 2);
  writeQueue = writeQueue.catch(()=>{}).then(async () => {
    await fs.mkdir(app.getPath('userData'), {recursive:true});
    const temp = storageFile()+'.tmp';
    await fs.writeFile(temp, snapshot, {mode:0o600});
    await fs.rename(temp, storageFile());
  });
  await writeQueue;
}
function validateSettings(value) {
  if (!value || typeof value !== 'object') throw new Error('连接设置无效');
  const s = {...DEFAULTS, ...value};
  if (typeof s.host !== 'string' || !/^[a-zA-Z0-9.:-]{1,253}$/.test(s.host)) throw new Error('服务器地址无效');
  if (!Number.isInteger(s.port) || s.port < 1 || s.port > 65535) throw new Error('端口无效');
  if (typeof s.username !== 'string' || !/^[a-zA-Z0-9_.-]{1,64}$/.test(s.username)) throw new Error('用户名无效');
  if (typeof s.binary !== 'string' || !/^\/[a-zA-Z0-9_./-]+$/.test(s.binary)) throw new Error('程序路径必须是绝对路径，仅允许字母、数字、点、横线、下划线和斜杠');
  s.fingerprint = String(s.fingerprint || '').replace(/^SHA256:/, '').replace(/=+$/, '');
  s.model=String(s.model||'').trim();
  if(s.model&&!/^[\p{L}\p{N}_./:-]{1,128}$/u.test(s.model))throw new Error('模型名称包含不支持的字符');
  if (!/^[A-Za-z0-9+/]{43}$/.test(s.fingerprint)) throw new Error('请填写服务器 SHA256 主机指纹');
  return {host:s.host, port:s.port, username:s.username, binary:s.binary, model:s.model, fingerprint:s.fingerprint};
}
function disconnect() {
  for (const channel of channels.values()) channel.close();
  channels.clear();
  pendingCancels.clear();
  if (client) { const old=client;client=null;old.end(); }
}
async function ensureConnected() {
  if (client) return client;
  if (connecting) return connecting;
  if (!password) throw new Error('请先在连接设置中填写 SSH 密码');
  connecting = new Promise((resolve,reject) => {
    const connection = new Client();
    let ready = false;
    connection.once('ready',()=>{ready=true;client=connection;resolve(connection);});
    connection.on('error',err=>{ if(!ready)reject(new Error('SSH 连接失败：'+err.message));else emit({type:'connection-error',message:err.message}); });
    connection.on('close',()=>{if(client===connection) {client=null;emit({type:'disconnected'});} if(!ready)reject(new Error('SSH 连接已关闭'));});
    connection.connect({host:settings.host, port:settings.port, username:settings.username, password,
      readyTimeout:15000, keepaliveInterval:15000, keepaliveCountMax:3,
      hostVerifier:key=>crypto.createHash('sha256').update(key).digest('base64').replace(/=+$/,'')===settings.fingerprint});
  }).finally(()=>{connecting=null;});
  return connecting;
}
async function request(payload, id, streaming=false) {
  const connection = await ensureConnected();
  return new Promise((resolve,reject)=>{
    const command=(settings.model?`env YQ_LLM_MODEL='${settings.model}' `:'')+`'${settings.binary}' desktop-stdio`;
    connection.exec(command, (err, channel)=>{
      if(err) return reject(err);
      if(id) channels.set(id,channel);
      let buffer='', stderr='', terminal=false, answer, settled=false;
      const decoder=new StringDecoder('utf8');
      const finish=(error)=>{
        if(settled)return;settled=true;clearTimeout(timer);if(id){channels.delete(id);pendingCancels.delete(id);}
        if(error)reject(error);else resolve(answer);
      };
      const timer=setTimeout(()=>{channel.close();finish(new Error('Agent 响应超时，请重试'));},streaming?1800000:30000);
      const parse=chunk=>{
        buffer+=chunk;
        if(buffer.length>2*1024*1024){channel.close();return finish(new Error('服务器返回的数据过大'));}
        let newline;
        while((newline=buffer.indexOf('\n'))>=0) {
          const line=buffer.slice(0,newline);buffer=buffer.slice(newline+1);
          if(!line.trim())continue;
          let event;
          try {event=JSON.parse(line);}catch{channel.close();return finish(new Error('服务器协议无效，请检查 desktop-stdio 版本'));}
          if(streaming)emit({...event,id});
          if(event.type==='error'){terminal=true;channel.end();return finish(new Error(event.message));}
          if(['done','info','memories'].includes(event.type)){terminal=true;answer=event;channel.end();finish();}
        }
      };
      channel.on('data',chunk=>parse(decoder.write(chunk)));
      channel.stderr.on('data',chunk=>{stderr=(stderr+chunk.toString()).slice(-2000);});
      channel.on('error',error=>finish(error));
      channel.on('close',()=>{parse(decoder.end());if(!terminal)finish(new Error('Agent 连接中断'+(stderr.includes('No such file')?'：服务器程序尚未构建或路径不正确':'')));});
      // Keep stdin open: the second line (or EOF) is the cooperative cancellation signal.
      channel.write(JSON.stringify(payload)+'\n');
      if(id && pendingCancels.delete(id))channel.write('{"cancel":true}\n');
    });
  });
}
function handle(name, fn) {
  ipcMain.handle(name, async(event,...args)=>{
    if(!win || event.sender!==win.webContents || event.senderFrame!==win.webContents.mainFrame) throw new Error('非法调用来源');
    return fn(...args);
  });
}
async function initialize() {
  try {saved=JSON.parse(await fs.readFile(storageFile(),'utf8'));settings=validateSettings(saved.settings||DEFAULTS);}
  catch {saved={};settings=DEFAULTS;}
  if(saved.secret) {
    try { password=await safeStorage.decryptStringAsync(Buffer.from(saved.secret,'base64'));if(typeof password==='object') password=password.result || password.plaintext || ''; }
    catch {password='';}
  }
  handle('state:load',()=>({workspace:saved.workspace||null, settings:{...settings,hasPassword:!!password}}));
  handle('state:save',async workspace=>{
    const text=JSON.stringify(workspace);
    if(text.length>20*1024*1024)throw new Error('本地工作区超过 20 MB，请导出或清理旧对话');
    saved.workspace=JSON.parse(text);await persist();return true;
  });
  handle('ssh:connect',async input=>{
    const next=validateSettings(input);
    const nextPassword=typeof input.password==='string'&&input.password ? input.password : password;
    disconnect();settings=next;password=nextPassword;
    const info=await request({kind:'info'});
    if(await safeStorage.isAsyncEncryptionAvailable()) {
      const encrypted=await safeStorage.encryptStringAsync(password);
      saved.secret=(Buffer.isBuffer(encrypted)?encrypted:encrypted.result||encrypted.encryptedData).toString('base64');
    } else {delete saved.secret;}
    await persist();return info;
  });
  handle('ssh:disconnect',()=>{disconnect();return true;});
  handle('agent:info',()=>request({kind:'info'}));
  handle('agent:memories',()=>request({kind:'memories'}));
  handle('agent:chat',async data=>{
    if(!data || !/^[a-zA-Z0-9-]{1,80}$/.test(data.id) || !/^[0-9a-f-]{36}$/i.test(data.session))throw new Error('会话参数无效');
    if(typeof data.input!=='string'||!data.input.trim()||Buffer.byteLength(data.input)>65536)throw new Error('消息为空或超过 64 KiB');
    if(channels.size)throw new Error('请等待当前任务结束');
    try {await request({kind:'chat',session:data.session,input:data.input},data.id,true);}
    catch(e){emit({type:'error',id:data.id,message:e.message});throw e;}
    return true;
  });
  handle('agent:cancel',id=>{if(typeof id!=='string'||!/^[a-zA-Z0-9-]{1,80}$/.test(id))return false;const channel=channels.get(id);if(channel)channel.write('{"cancel":true}\n');else pendingCancels.add(id);return true;});
  handle('plugin:import',async()=>{
    const result=await dialog.showOpenDialog(win,{title:'导入界面插件',filters:[{name:'界面插件 JSON',extensions:['json']}],properties:['openFile']});
    if(result.canceled)return null;
    const file=result.filePaths[0];if((await fs.stat(file)).size>262144)throw new Error('插件文件不能超过 256 KiB');
    return JSON.parse(await fs.readFile(file,'utf8'));
  });
  handle('plugin:export',async plugin=>{
    const result=await dialog.showSaveDialog(win,{title:'导出插件',defaultPath:'yunqi-plugin.json',filters:[{name:'JSON',extensions:['json']}]});
    if(result.canceled)return false;
    await fs.writeFile(result.filePath,JSON.stringify(plugin,null,2),'utf8');return true;
  });
  handle('window:action',action=>{
    if(action==='minimize')win.minimize();
    if(action==='maximize')win.isMaximized()?win.unmaximize():win.maximize();
    if(action==='close')win.close();
  });
  win=new BrowserWindow({width:1440,height:940,minWidth:920,minHeight:650,title:'云栖 Agent',
    backgroundColor:'#111316',frame:false,show:false,
    webPreferences:{preload:path.join(__dirname,'preload.cjs'),nodeIntegration:false,contextIsolation:true,sandbox:true}});
  Menu.setApplicationMenu(null);
  win.webContents.setWindowOpenHandler(()=>({action:'deny'}));
  win.webContents.on('will-navigate',event=>event.preventDefault());
  win.webContents.session.setPermissionRequestHandler((_wc,_permission,callback)=>callback(false));
  win.once('ready-to-show',()=>win.show());
  await win.loadFile(path.join(__dirname,'../dist/index.html'));
}
if(!app.requestSingleInstanceLock())app.quit();
else {
  app.on('second-instance',()=>{if(win){if(win.isMinimized())win.restore();win.show();win.focus();}});
  app.whenReady().then(initialize).catch(error=>{dialog.showErrorBox('云栖 Agent 启动失败',error.message);app.quit();});
  app.on('window-all-closed',()=>{disconnect();app.quit();});
}
