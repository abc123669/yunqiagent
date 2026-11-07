import {useCallback,useEffect,useLayoutEffect,useMemo,useRef,useState,useSyncExternalStore, type ReactNode, type CSSProperties} from 'react';
import {Sparkles,Plus,MessageSquare,BrainCircuit,Puzzle,Settings2,ArrowUp,Square,ChevronDown,ChevronRight,Search,SlidersHorizontal,PanelRightClose,PanelRightOpen,Check,Copy,Trash2,Download,Upload,X,Minus,Maximize2,RotateCcw,Code2,Command,Terminal,Activity,Wifi,WifiOff,MoreHorizontal,ArrowRight,Sun,Moon,GripVertical,Palette,Layers,ShieldCheck,LoaderCircle} from 'lucide-react';
import {CONTAINERS,compose,examplePlugin,findNode,validatePlugin,type Plugin,type UINode} from './plugins';
import type {AgentEvent,ChatMessage,Memory,Session,Settings,Workspace} from './types';

const api=window.desktop;
const defaults:Settings={host:'',port:22,username:'',binary:'~/yunqiagent/target/release/yunqiagent',model:'',fingerprint:''};
const newSession=():Session=>({id:crypto.randomUUID(),title:'新的对话',messages:[],updated:Date.now()});
const initialSession=newSession();
const initial:Workspace={sessions:[initialSession],active:initialSession.id,plugins:[],inspector:false,compact:false,reducedMotion:false,theme:'light',uiVersion:2};
const errorText=(e:unknown)=>(e as Error).message?.replace(/^Error invoking remote method '[^']+': Error: /,'')||String(e);
const time=(value:number)=>new Date(value).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit'});
const iconMap:Record<string,typeof Sparkles>={sparkles:Sparkles,terminal:Terminal,brain:BrainCircuit,puzzle:Puzzle,plus:Plus,arrow:ArrowRight,message:MessageSquare,code:Code2,copy:Copy,edit:Code2,download:Download,panel:PanelRightOpen,search:Search,trash:Trash2,settings:Settings2};
const liveText=new Map<string,string>();
const liveSubscriptions=new Map<string,Set<()=>void>>();
function subscribeLive(id:string,listener:()=>void){if(!id)return()=>{};let group=liveSubscriptions.get(id);if(!group){group=new Set();liveSubscriptions.set(id,group);}group.add(listener);return()=>{group!.delete(listener);if(!group!.size)liveSubscriptions.delete(id);};}
function readLive(id:string){return liveText.get(id)||'';}
function appendLive(id:string,delta:string){if(!id||!delta)return;liveText.set(id,(liveText.get(id)||'')+delta);for(const listener of liveSubscriptions.get(id)||[])listener();}
function clearLive(id:string){if(!liveText.has(id))return;liveText.delete(id);for(const listener of liveSubscriptions.get(id)||[])listener();}

function Composer({disabled,busy,onSend,draft,setDraft,placeholder,note,hint,modeText,children}:{disabled:boolean;busy:boolean;onSend:(s:string)=>void;draft:string;setDraft:(s:string)=>void;placeholder?:string;note?:string;hint?:string;modeText?:string;children?:ReactNode}) {
  const textarea=useRef<HTMLTextAreaElement>(null);
  useEffect(()=>{if(textarea.current){textarea.current.style.height='auto';textarea.current.style.height=Math.min(textarea.current.scrollHeight,180)+'px';}},[draft]);
  return <div className="composer-area"><div className={`composer ${disabled?'offline':''}`}>
    <textarea ref={textarea} aria-label="消息" value={draft} placeholder={disabled?'连接 Agent 后输入任务或粘贴资料…':placeholder||'输入任务或粘贴资料…'} rows={2}
      onChange={e=>setDraft(e.target.value)} onKeyDown={e=>{if(e.key==='Enter'&&!e.shiftKey&&!e.nativeEvent.isComposing){e.preventDefault();if(!disabled&&!busy&&draft.trim())onSend(draft);}}}/>
    <div className="composer-bottom"><span className="composer-mode"><MessageSquare size={14}/> {modeText||'对话'} <ChevronDown size={12}/></span><span className="keyboard-hint">{hint||'Shift + Enter 换行'}</span>{children}</div>
    </div><p className="composer-note">{note||'输入任务，或粘贴资料开始处理。'}<span>输出可能有误，请核实关键内容。</span></p></div>;
}

function MessageBody({text}:{text:string}) {
  const parts=text.split(/(```[\s\S]*?```|```[\s\S]*$)/g);
  return <>{parts.map((part,i)=>part.startsWith('```')?<pre key={i}><code>{part.replace(/^```[^\n]*\n?/,'').replace(/```$/,'')}</code></pre>:<div className="message-prose" key={i}>{part.split(/(`[^`\n]+`)/g).map((p,k)=>p.startsWith('`')?<code key={k}>{p.slice(1,-1)}</code>:p)}</div>)}</>;
}
function LiveMessageBody({id,text,streaming,onCommit}:{id:string;text:string;streaming:boolean;onCommit:()=>void}){const delta=useSyncExternalStore(listener=>subscribeLive(id,listener),()=>readLive(id),()=> '');const combined=text+delta;useLayoutEffect(()=>{if(streaming)onCommit();},[combined,streaming,onCommit]);return <><MessageBody text={combined}/>{streaming&&combined&&<span className="stream-caret" aria-hidden="true"/>}</>;}
function Messages({messages,busy,onDraft,progress,jobId,rowTemplate,renderExtension}:{messages:ChatMessage[];busy:boolean;onDraft:(s:string)=>void;progress:string[];jobId?:string;rowTemplate:UINode;renderExtension:(node:UINode)=>ReactNode}) {
  const end=useRef<HTMLDivElement>(null),scroll=useRef<HTMLDivElement>(null),following=useRef(true);
  const [limit,setLimit]=useState(60);
  const last=messages[messages.length-1];
  const followLatest=useCallback(()=>{if(following.current)end.current?.scrollIntoView({block:'end',behavior:'instant'});},[]);
  useEffect(()=>{if(following.current)end.current?.scrollIntoView({block:'end',behavior:'instant'});},[last?.text,last?.error,busy]);
  function renderPart(node:UINode,message:ChatMessage,streaming:boolean):ReactNode{
    if(node.props?.hidden)return null;
    const p=node.props||{},children=node.children?.map(child=><div className="node-wrapper" data-node={child.id} key={child.id}>{renderPart(child,message,streaming)}</div>);
    if(node.type==='messageRow')return children;
    if(node.type==='messageAvatar'){const AvatarIcon=iconMap[p.icon||'message']||MessageSquare;return <div className={`avatar ${message.role==='assistant'?'agent-avatar':''}`} data-node={node.id}>{message.role==='assistant'?<AvatarIcon size={16}/>:p.text||'Y'}</div>;}
    if(node.type==='messageContent')return <div className="message-content" data-node={node.id}>{children}</div>;
    if(node.type==='messageMeta')return <div className="message-meta" data-node={node.id}><b>{message.role==='assistant'?p.assistantText||'云栖':p.userText||'你'}</b>{p.showTimestamp!==false&&<time>{time(message.at)}</time>}{message.stopped&&<span>已停止</span>}</div>;
    if(node.type==='messageBody')return <div data-node={node.id}><LiveMessageBody id={streaming?jobId||'':''} text={message.text} streaming={streaming} onCommit={followLatest}/></div>;
    if(node.type==='messageStatus')return streaming?<div className="stream-status" role="status" data-node={node.id}><i className="stream-dot"/>{progress.at(-1)||p.text||'正在生成回答'}</div>:null;
    if(node.type==='messageError')return message.error?<div className="inline-error" data-node={node.id}>{message.error}{children}</div>:null;
    if(node.type==='messageActions')return <div className="message-actions" data-node={node.id}>{node.children?.map(item=>{
      if(item.props?.hidden)return null;const q=item.props||{};
      if(q.action==='copy-message'){if(!message.text)return null;const ActionIcon=iconMap[q.icon||'copy']||Copy;return <button key={item.id} data-node={item.id} type="button" aria-label={q.text||'复制消息'} title={q.text||'复制消息'} onClick={()=>navigator.clipboard.writeText(message.text)}><ActionIcon size={13}/></button>;}
      if(q.action==='retry-message'){if(!message.error)return null;const previous=messages[Math.max(0,messages.indexOf(message)-1)]?.text||'';return <button key={item.id} data-node={item.id} type="button" className="quiet" aria-label={q.text||'重新编辑'} onClick={()=>onDraft(previous)}>{q.text||'重新编辑'}</button>;}
      if(q.tone==='tool-count')return message.tools?<span key={item.id} data-node={item.id}><Terminal size={12}/> {message.tools} {q.text||'次工具调用'}</span>:null;
      return <div className="node-wrapper" data-node={item.id} key={item.id}>{renderExtension(item)}</div>;
    })}</div>;
    return renderExtension(node);
  }
  return <div className="messages" ref={scroll} onScroll={()=>{const e=scroll.current!;following.current=e.scrollHeight-e.scrollTop-e.clientHeight<100;}}>
    <div className="message-inner">{messages.length>limit&&<button className="quiet center" onClick={()=>setLimit(l=>l+60)}>加载更早的消息</button>}
    {!rowTemplate.props?.hidden&&messages.slice(-limit).map(m=>{const streaming=busy&&m===last&&m.role==='assistant';return <article key={m.id} data-node={rowTemplate.id} className={`message ${m.role} ${streaming?'streaming':''}`}>{rowTemplate.children?.map(part=><div className="node-wrapper" data-node={part.id} key={part.id}>{renderPart(part,m,streaming)}</div>)}</article>;})}<div ref={end}/></div></div>;
}

export default function App() {
  const [ws,setWs]=useState<Workspace>(initial),[ready,setReady]=useState(false),[settings,setSettings]=useState(defaults);
  const [connection,setConnection]=useState<'offline'|'connecting'|'online'>('offline'),[model,setModel]=useState('尚未连接');
  const [modal,setModal]=useState<'settings'|'plugins'|null>(null),[modalRevision,setModalRevision]=useState(0),[view,setView]=useState<'chat'|'memories'>('chat');
  const [drafts,setDrafts]=useState<Record<string,string>>({}),[search,setSearch]=useState(''),[toast,setToast]=useState('');
  const [busy,setBusy]=useState<{id:string;session:string;message:string}|null>(null),[stopping,setStopping]=useState(false),[progress,setProgress]=useState<string[]>([]);
  const [memories,setMemories]=useState<Memory[]>([]),[memoryLoading,setMemoryLoading]=useState(false),[memoryError,setMemoryError]=useState('');
  const [renaming,setRenaming]=useState<string|null>(null),[renameValue,setRenameValue]=useState('');
  const [selectedNode,setSelectedNode]=useState('brand'),[inspectMode,setInspectMode]=useState(false);
  const [inspectorClosing,setInspectorClosing]=useState(false),inspectorTimer=useRef<number|undefined>(undefined);
  const busyRef=useRef(busy);busyRef.current=busy;
  const deltaBuffer=useRef(''),frame=useRef(0);
  const registry=useMemo(()=>compose(ws.plugins),[ws.plugins]);
  const session=ws.sessions.find(s=>s.id===ws.active)||ws.sessions[0];
  const notify=useCallback((message:string)=>setToast(message),[]);
  const openModal=(name:'settings'|'plugins')=>{setModalRevision(r=>r+1);setModal(name);};
  useEffect(()=>{if(!toast)return;const t=setTimeout(()=>setToast(''),4500);return()=>clearTimeout(t);},[toast]);
  useEffect(()=>{
    let alive=true;
    (async()=>{
      try {
        if(api){const loaded=await api.load();if(!alive)return;setSettings(loaded.settings);
          if(loaded.workspace?.sessions?.length){const next=loaded.workspace;next.plugins=Array.isArray(next.plugins)?next.plugins.map(validatePlugin):[];const migrated={...initial,...next};if((next.uiVersion||0)<2){migrated.uiVersion=2;migrated.theme='light';migrated.inspector=false;}setWs(migrated);}
          if(loaded.settings.hasPassword){setConnection('connecting');try{const info=await api.info();if(alive){setModel(info.model||'Agent');setConnection('online');}}catch(e){if(alive){setConnection('offline');notify(errorText(e));}}}
        }else{const cached=localStorage.getItem('yunqi-preview');if(cached){const next=JSON.parse(cached);if(next.sessions?.length)setWs({...initial,...next});}}
      }catch(e){notify('工作区加载失败：'+errorText(e));}
      finally{if(alive)setReady(true);}
    })();
    return()=>{alive=false;};
  },[notify]);
  useEffect(()=>{
    if(!ready)return;
    const t=setTimeout(()=>{if(api)api.save(ws).catch(e=>notify('保存失败：'+errorText(e)));else localStorage.setItem('yunqi-preview',JSON.stringify(ws));},450);
    return()=>clearTimeout(t);
  },[ws,ready,notify]);
  useEffect(()=>()=>{if(inspectorTimer.current!==undefined)window.clearTimeout(inspectorTimer.current);},[]);
  function toggleInspector(){
    if(ws.inspector){setInspectorClosing(true);setWs(w=>({...w,inspector:false}));inspectorTimer.current=window.setTimeout(()=>{setInspectorClosing(false);inspectorTimer.current=undefined;},220);}
    else{if(inspectorTimer.current!==undefined)window.clearTimeout(inspectorTimer.current);inspectorTimer.current=undefined;setInspectorClosing(false);setWs(w=>({...w,inspector:true}));}
  }
  const updateMessage=useCallback((job:{session:string;message:string},update:(m:ChatMessage)=>ChatMessage)=>{
    setWs(w=>({...w,sessions:w.sessions.map(s=>s.id===job.session?{...s,updated:Date.now(),messages:s.messages.map(m=>m.id===job.message?update(m):m)}:s)}));
  },[]);
  useEffect(()=>api?.onEvent((event:AgentEvent)=>{
    if(event.type==='disconnected'||event.type==='connection-error'){setConnection('offline');if(event.message)notify(event.message);return;}
    const job=busyRef.current;if(!job||event.id!==job.id)return;
    if(event.type==='delta'){
      deltaBuffer.current+=event.text||'';
      if(!frame.current)frame.current=requestAnimationFrame(()=>{const delta=deltaBuffer.current;deltaBuffer.current='';frame.current=0;appendLive(job.id,delta);});
    }
    if(event.type==='progress')setProgress(p=>[...p.slice(-29),event.text||'']);
    if(event.type==='done'||event.type==='error'){
      if(frame.current)cancelAnimationFrame(frame.current);frame.current=0;
      const remainder=deltaBuffer.current;deltaBuffer.current='';
      const completeText=event.type==='done'?event.text||readLive(job.id)+remainder:readLive(job.id)+remainder;
      updateMessage(job,m=>({...m,text:completeText,error:event.type==='error'?event.message:undefined,tools:event.tools,stopped:event.stopped}));
      const completedJobId=job.id;requestAnimationFrame(()=>clearLive(completedJobId));
      busyRef.current=null;setBusy(null);setStopping(false);
    }
  }),[notify,updateMessage]);
  const createSession=()=>{const next=newSession();setWs(w=>({...w,active:next.id,sessions:[next,...w.sessions]}));setView('chat');};
  const setDraft=(text:string)=>setDrafts(d=>({...d,[ws.active]:text}));
  async function send(text:string) {
    if(!api||connection!=='online'){openModal('settings');return;}
    if(busyRef.current||!text.trim())return;
    const messageId=crypto.randomUUID(),job={id:crypto.randomUUID(),session:session.id,message:messageId};
    busyRef.current=job;setBusy(job);setProgress([]);setStopping(false);setDraft('');
    setWs(w=>({...w,sessions:w.sessions.map(s=>s.id===session.id?{...s,title:s.messages.length?s.title:text.trim().slice(0,24),updated:Date.now(),messages:[...s.messages,
      {id:crypto.randomUUID(),role:'user',text:text.trim(),at:Date.now()},{id:messageId,role:'assistant',text:'',at:Date.now()}]}:s)}));
    try{await api.chat({id:job.id,session:job.session,input:text.trim()});}
    catch(e){if(busyRef.current?.id===job.id){updateMessage(job,m=>({...m,error:errorText(e)}));busyRef.current=null;setBusy(null);setStopping(false);}}
  }
  async function stop(){if(!busy||!api)return;setStopping(true);try{await api.cancel(busy.id);}catch(e){notify(errorText(e));setStopping(false);}}
  async function loadMemories(){setView('memories');if(!api||connection!=='online'){setMemoryError('连接服务器后，即可查看真实记忆。');return;}
    setMemoryLoading(true);setMemoryError('');try{const event=await api.memories();setMemories(event.items||[]);}catch(e){setMemoryError(errorText(e));}finally{setMemoryLoading(false);}}
  function install(plugin:unknown){
    try{const valid=validatePlugin(plugin);setWs(w=>({...w,plugins:[...w.plugins.filter(p=>p.id!==valid.id),valid]}));notify('插件已保存');}
    catch(e){notify(errorText(e));}
  }
  function action(name?:string,prompt?:string){
    if(name==='chat')setView('chat');
    if(name==='new-chat')createSession();
    if(name==='plugins')openModal('plugins');
    if(name==='settings')openModal('settings');
    if(name==='memories')void loadMemories();
    if(name==='toggle-inspector')toggleInspector();
    if(name==='export')exportSession();
    if(name==='focus-session-search'){setView('chat');requestAnimationFrame(()=>document.getElementById('session-search')?.focus());}
    if(name==='prompt'){setView('chat');setDraft(prompt||'');}
  }
  function deleteSession(id:string){
    if(busy?.session===id){notify('请先停止当前任务');return;}
    setWs(w=>{let sessions=w.sessions.filter(s=>s.id!==id);if(!sessions.length)sessions=[newSession()];return {...w,sessions,active:w.active===id?sessions[0].id:w.active};});
  }
  function exportSession(){
    const blob=new Blob([JSON.stringify(session,null,2)],{type:'application/json'}),url=URL.createObjectURL(blob),a=document.createElement('a');
    a.href=url;a.download=`yunqi-${session.id}.json`;a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
  }
  useEffect(()=>{const handler=(e:KeyboardEvent)=>{
    if((e.ctrlKey||e.metaKey)&&e.key==='n'){e.preventDefault();createSession();}
    if((e.ctrlKey||e.metaKey)&&e.key===','){e.preventDefault();openModal('settings');}
    if(e.key==='Escape'){setModal(null);setInspectMode(false);}
    if(e.key==='F8'){e.preventDefault();openModal('plugins');}
  };window.addEventListener('keydown',handler);return()=>window.removeEventListener('keydown',handler);});
  const memoryView=<div className="memory-view"><div className="page-heading"><div><div className="eyebrow">THINGS WORTH REMEMBERING</div><h1>记忆，慢慢积累。</h1><p>来自服务器的长期记忆，让每一次对话更懂你。</p></div><button className="secondary" onClick={loadMemories} disabled={memoryLoading}><RotateCcw size={14}/> 刷新</button></div>
    <label className="search-field"><Search size={15}/><input aria-label="搜索记忆" value={search} onChange={e=>setSearch(e.target.value)} placeholder="搜索记忆内容…"/></label>
    {memoryLoading?<div className="empty"><LoaderCircle className="spin"/>正在读取记忆…</div>:memoryError?<div className="empty">{memoryError}</div>:!memories.length?<div className="empty"><BrainCircuit size={30}/>还没有记忆，新的理解会在这里积累。</div>:
    <div className="memory-grid">{memories.filter(m=>(m.content+' '+m.kind).toLowerCase().includes(search.toLowerCase())).map(m=><article className="memory-card" key={m.id}><div><span className="tag">{m.kind}</span><small>#{m.id}</small></div><p>{m.content}</p><footer>重要度 <b>{Math.round(m.importance*100)}%</b></footer></article>)}</div>}{renderWrapped(findNode(registry.tree,'memories-extra')!)}</div>;
  function renderNode(node:UINode):ReactNode {
    if(node.props?.hidden)return null;
    const p=node.props||{},children=node.children?.map(n=><div key={n.id} data-node={n.id} className="node-wrapper">{renderNodeWithChildren(n)}</div>);
    switch(node.type){
      case 'shell':return <div className="app-shell">{children}</div>;
      case 'sidebar':return <aside className="sidebar">{children}</aside>;
      case 'brand':return <div className="brand"><div className="brand-icon">Y</div><div><b>{p.text}</b><span>{p.subtitle}</span></div></div>;
      case 'newChat':return <button className="new-chat" onClick={createSession}><Plus size={17}/>{p.text}<kbd>Ctrl N</kbd></button>;
      case 'navigation':return <nav className="navigation">{children}</nav>;
      case 'sessions':{const child=(id:string)=>node.children?.find(item=>item.id===id);const template=child('session-row-template');const titleNode=template?.children?.find(item=>item.id==='session-title');const deleteNode=template?.children?.find(item=>item.id==='session-delete');const itemsNode=child('sessions-items');const TitleIcon=iconMap[titleNode?.props?.icon||'message']||MessageSquare;const DeleteIcon=iconMap[deleteNode?.props?.icon||'trash']||Trash2;return <div className="session-list"><div className="session-head">{child('sessions-heading')&&renderWrapped(child('sessions-heading')!)}{child('sessions-search-button')&&renderWrapped(child('sessions-search-button')!)}</div>{child('sessions-search-input')&&renderWrapped(child('sessions-search-input')!)}
        <div data-node="sessions-items" className="session-items">{!itemsNode?.props?.hidden&&!template?.props?.hidden&&ws.sessions.filter(s=>view!=='chat'||s.title.toLowerCase().includes(search.toLowerCase())).map(s=><div key={s.id} data-node={template?.id} className={`session-row ${ws.active===s.id&&view==='chat'?'selected':''}`}>
          {renaming===s.id?<input autoFocus aria-label="对话名称" value={renameValue} onChange={e=>setRenameValue(e.target.value)} onBlur={()=>{setWs(w=>({...w,sessions:w.sessions.map(i=>i.id===s.id?{...i,title:renameValue.trim()||'新的对话'}:i)}));setRenaming(null);}} onKeyDown={e=>{if(e.key==='Enter')e.currentTarget.blur();}}/>:titleNode&&!titleNode.props?.hidden&&<button data-node={titleNode.id} type="button" className="session-name" aria-label={s.title} onClick={()=>{setWs(w=>({...w,active:s.id}));setView('chat');}} onDoubleClick={()=>{setRenaming(s.id);setRenameValue(s.title);}}><TitleIcon size={14}/><span>{s.title}</span></button>}
          {deleteNode&&!deleteNode.props?.hidden&&<button data-node={deleteNode.id} type="button" className="delete-session" title={`${deleteNode.props?.text||'删除对话'} ${s.title}`} aria-label={`${deleteNode.props?.text||'删除对话'} ${s.title}`} onClick={()=>deleteSession(s.id)}><DeleteIcon size={12}/></button>}</div>)}{itemsNode?.children?.map(item=>renderWrapped(item))}</div>{child('sessions-tip')&&renderWrapped(child('sessions-tip')!)}</div>;}
      case 'profile':return <div className="sidebar-footer">{children}</div>;
      case 'connectionStatus':return <div className="connection-chip"><i className={connection}/><span>{connection==='online'?p.onlineText||'Agent 已连接':connection==='connecting'?p.connectingText||'正在连接':p.offlineText||'Agent 未连接'}</span><span className="ssh-label">{p.protocolText||'SSH'}</span>{children}</div>;
      case 'workspace':return <section className="workspace">{children}</section>;
      case 'toolbar':return <header className="toolbar">{children}</header>;
      case 'main':return <main className={`main-layout ${ws.inspector?'with-inspector':''}`}>{children}</main>;
      case 'slot':return <div className={`slot ${node.id==='conversation-column'?'conversation-column':''} ${node.id==='toolbar-actions'?'toolbar-actions':''} ${p.layout==='row'?'slot-row':''}`}>{children}</div>;
      case 'conversation':return <div className="conversation">{view==='memories'?memoryView:!session.messages.length?children:<Messages key={session.id} messages={session.messages} busy={busy?.session===session.id} jobId={busy?.session===session.id?busy.id:undefined} progress={busy?.session===session.id?progress:[]} onDraft={setDraft} rowTemplate={findNode(node,'message-row-template')||{id:'message-row-template',type:'messageRow'}} renderExtension={renderWrapped}/>}</div>;
      case 'welcome':return <div className="welcome">{children}{connection!=='online'&&<button className="connect-welcome quiet" onClick={()=>openModal('settings')}><Wifi size={14}/>{connection==='connecting'?'正在连接服务器…':'连接你的 Agent'}<ArrowRight size={13}/></button>}</div>;
      case 'composer':return view==='chat'?<Composer draft={drafts[ws.active]||''} setDraft={setDraft} disabled={connection!=='online'} busy={!!busy} onSend={send} placeholder={p.text} note={p.subtitle} hint={p.hint} modeText={p.modeText}>{children}</Composer>:null;
      case 'inspector':return ws.inspector||inspectorClosing?<aside className={`inspector ${inspectorClosing?'closing':''}`} aria-hidden={inspectorClosing}><div className="inspector-heading"><span><Layers size={15}/> 工作台</span></div>
        <div className="inspector-section"><div className="section-label">连接与运行</div><div className="server-card"><div className="server-icon"><Terminal size={18}/></div><div><b>{connection==='online'?'云栖 Agent':connection==='connecting'?'正在连接…':'等待连接'}</b><span>{settings.host}</span></div><i className={`dot ${connection}`}/></div><div className="detail-line"><span>传输方式</span><b><ShieldCheck size={12}/> 加密 SSH</b></div><div className="detail-line"><span>当前模型</span><b className="model-name" title={model}>{model}</b></div><button className="text-link" onClick={()=>openModal('settings')}>管理连接 <ArrowRight size={12}/></button></div>
        <div className="inspector-section"><div className="section-label">让工作台适合你 <SlidersHorizontal size={13}/></div><div className="customize-preview"><div className="mini-sidebar"/><div className="mini-main"><i/><i/><i/></div><div className="mini-right"/></div><p className="muted small">每个习惯，都值得一个顺手的位置。</p><button className="secondary full" onClick={()=>openModal('plugins')}><Puzzle size={14}/> 定制我的界面</button><div className="detail-line"><span>启用的插件</span><b>{ws.plugins.filter(p=>p.enabled).length} 个</b></div>{Object.keys(registry.errors).length>0&&<p className="error small">{Object.keys(registry.errors).length} 个插件需要检查</p>}</div>
        <div className="inspector-section activity-section"><div className="section-label">任务动态 <Activity size={13}/></div>{busy?<><div className="running"><LoaderCircle className="spin" size={13}/>{stopping?'正在停止任务':'Agent 正在工作'}</div>{progress.map((p,i)=><div className="progress-item" key={i}>{p}</div>)}</>:<div className="activity-empty"><div><Check size={16}/></div><p>一切就绪</p><span>任务的每一步，会在这里呈现。</span></div>}</div>
        {children}<div className="inspector-bottom"><RotateCcw size={14}/><span>布局可随时恢复默认</span></div></aside>:null;
      case 'status':return <footer className="status-bar"><span><i className={`dot ${connection}`}/>{connection==='online'?'已连接':connection==='connecting'?'连接中':'离线'}<span className="status-divider"/>{busy?'正在处理任务':'准备就绪'}</span><span><Puzzle size={12}/>{ws.plugins.filter(p=>p.enabled).length} 个插件 <span className="status-divider"/>v0.2.17</span>{children}</footer>;
      case 'text':return p.tone==='heading'?<h1>{p.text}{children}</h1>:p.tone==='eyebrow'?<div className="eyebrow">{p.text}{children}</div>:p.tone==='description'?<p className="welcome-copy">{p.text}{children}</p>:p.tone==='breadcrumb'?<div className="breadcrumbs"><span>工作空间</span><ChevronRight size={13}/><b>{view==='chat'?'对话空间':'长期记忆'}</b></div>:p.tone==='model'?<span className="model-chip"><i/>{model.length>28?model.slice(0,28)+'…':model}</span>:<div className="plugin-text"><b>{p.text}</b>{p.subtitle&&<p>{p.subtitle}</p>}{children}</div>;
      case 'card':{const CardIcon=iconMap[p.icon||'layers']||Layers;return <section className="plugin-card"><div><CardIcon size={17}/><div><b>{p.text}</b>{p.subtitle&&<p>{p.subtitle}</p>}</div></div>{children}</section>;}
      case 'button':{const Icon=p.action==='toggle-inspector'&&ws.inspector?PanelRightClose:iconMap[p.icon||'arrow']||ArrowRight;
        if(p.tone==='navigation')return <button type="button" className={view===p.action||(view==='memories'&&p.action==='memories')?'active':''} onClick={()=>action(p.action,p.prompt)}><Icon size={17}/>{p.text}{p.action==='chat'?<span className="nav-count">{ws.sessions.length}</span>:p.action==='plugins'?<span className="nav-dot"/>:null}{children}</button>;
        if(p.tone==='suggestion')return <button type="button" onClick={()=>action(p.action,p.prompt)}><Icon size={19}/><b>{p.text}</b><span>{p.subtitle}</span><ArrowRight className="suggest-arrow" size={15}/>{children}</button>;
        if(p.tone==='toolbar')return <button type="button" className={`toolbar-icon ${p.action==='toggle-inspector'&&ws.inspector?'selected':''}`} title={p.text} aria-label={p.text} onClick={()=>action(p.action,p.prompt)}><Icon size={16}/>{children}</button>;
        if(p.tone==='profile')return <button type="button" className="profile" onClick={()=>action(p.action,p.prompt)}><div className="profile-avatar">Y</div><span><b>{p.text}</b><small>{p.subtitle||`${settings.username} · ${settings.host}`}</small></span><Icon size={16}/>{children}</button>;
        return <button type="button" className={`secondary plugin-button ${p.action==='toggle-inspector'&&ws.inspector?'selected':''}`} aria-label={p.action==='toggle-inspector'?'切换侧面板':undefined} onClick={()=>action(p.action,p.prompt)}><Icon size={14}/>{p.text}{children}</button>;}
      case 'sendButton':{const SendIcon=iconMap[p.icon||'arrow']||ArrowUp;return busy?<button type="button" className="send stopping" title={stopping?'正在停止':'停止生成'} aria-label={stopping?'正在停止':'停止生成'} disabled={stopping} onClick={stop}>{stopping?<LoaderCircle className="spin" size={16}/>:<Square size={14} fill="currentColor"/>}</button>:<button type="button" className="send" title={p.text||'发送消息'} aria-label={p.text||'发送消息'} disabled={connection!=='online'||!(drafts[ws.active]||'').trim()} onClick={()=>send(drafts[ws.active]||'')}><SendIcon size={19}/>{children}</button>;}
      case 'searchInput':return <input id="session-search" className="session-search" aria-label={p.text||'搜索对话'} placeholder={p.placeholder||'搜索对话…'} value={view==='chat'?search:''} onChange={e=>setSearch(e.target.value)}/>;
      case 'divider':return <hr className="plugin-divider"/>;
      default:return null;
    }
  }
  const themeStyle=Object.fromEntries(Object.entries(registry.theme).map(([k,v])=>['--'+k,v])) as CSSProperties;
  return <div className={`desktop ${ws.theme==='light'?'light':''} ${ws.compact?'compact':''} ${ws.reducedMotion?'reduce-motion':''}`} style={themeStyle}>
    <div className="titlebar"><span className="titlebar-wordmark"><span className="titlebar-mark">Y</span>云栖</span><span className="titlebar-center">工作台</span><div className="window-actions"><button title="插件与布局恢复 (F8)" aria-label="插件与布局恢复" onClick={()=>openModal('plugins')}><Puzzle size={14}/></button><button title="设置" aria-label="设置" onClick={()=>openModal('settings')}><Settings2 size={14}/></button><span/><button aria-label="最小化" onClick={()=>api?.window('minimize')}><Minus size={14}/></button><button aria-label="最大化" onClick={()=>api?.window('maximize')}><Maximize2 size={12}/></button><button className="close-window" aria-label="关闭窗口" onClick={()=>api?.window('close')}><X size={16}/></button></div></div>
    <div className={inspectMode?'inspect-root':'registry-root'} onClickCapture={e=>{if(!inspectMode)return;const target=(e.target as HTMLElement).closest('[data-node]');if(target){e.preventDefault();e.stopPropagation();setSelectedNode(target.getAttribute('data-node')!);setInspectMode(false);openModal('plugins');}}}>
      {renderWrapped(registry.tree)}
    </div>
    {toast&&<div className="toast" role="status"><Check size={15}/>{toast}<button onClick={()=>setToast('')}><X size={13}/></button></div>}
    {!api&&<div className="preview-badge">浏览器预览 · 请使用桌面程序连接 Agent</div>}
    {modal==='settings'&&<SettingsDialog settings={settings} ws={ws} setWs={setWs} connection={connection} busy={!!busy} close={()=>setModal(null)} reopenKey={modalRevision} connectionFields={findNode(registry.tree,'settings-connection-fields')||{id:'settings-connection-fields',type:'slot'}} advancedFields={findNode(registry.tree,'settings-advanced-fields')||{id:'settings-advanced-fields',type:'slot'}} appearanceControl={findNode(registry.tree,'settings-appearance-control')||{id:'settings-appearance-control',type:'settingTheme'}} appearanceToggles={findNode(registry.tree,'settings-appearance-toggles')||{id:'settings-appearance-toggles',type:'slot'}} privacyNote={findNode(registry.tree,'settings-privacy-note')||{id:'settings-privacy-note',type:'settingNote',props:{text:''}}} connectionExtra={renderWrapped(findNode(registry.tree,'settings-connection-extra')||{id:'settings-connection-extra',type:'slot'})} appearanceExtra={renderWrapped(findNode(registry.tree,'settings-appearance-extra')||{id:'settings-appearance-extra',type:'slot'})} renderExtension={renderWrapped} onConnect={async next=>{
      if(!api)throw new Error('请在 Windows 桌面程序中连接服务器');
      setConnection('connecting');try{const info=await api.connect(next);setSettings({...next,password:undefined,hasPassword:true});setModel(info.model||'Agent');setConnection('online');notify('已连接真实 Agent');}catch(e){setConnection('offline');throw e;}
    }} onDisconnect={async()=>{await api?.disconnect();setConnection('offline');}}/>}
    {modal==='plugins'&&<PluginDialog plugins={ws.plugins} errors={registry.errors} tree={registry.tree} selected={selectedNode} setSelected={setSelectedNode} install={install} reopenKey={modalRevision}
      close={()=>setModal(null)} notify={notify} setPlugins={plugins=>setWs(w=>({...w,plugins}))} inspect={()=>{setModal(null);setInspectMode(true);notify('点击界面中的元素进行编辑');}}/>}
  </div>;
  function renderWrapped(node:UINode):ReactNode {
    // Attach stable registry IDs without changing the layout box model.
    return <div data-node={node.id} className="node-wrapper">{renderNodeWithChildren(node)}</div>;
  }
  function renderNodeWithChildren(node:UINode):ReactNode {
    // Container traversal is handled here; leaf views use the same renderer.
    if(node.props?.hidden)return null;
    const childNodes=node.children?.map(child=><div key={child.id} data-node={child.id} className="node-wrapper">{renderNodeWithChildren(child)}</div>);
    if(node.type==='shell')return <div className="app-shell">{childNodes}</div>;
    if(node.type==='sidebar')return <aside className="sidebar">{childNodes}</aside>;
    if(node.type==='workspace')return <section className="workspace">{childNodes}</section>;
    if(node.type==='main')return <main className={`main-layout ${ws.inspector?'with-inspector':''}`}>{childNodes}</main>;
    if(node.type==='slot')return <div className={`slot ${node.id==='conversation-column'?'conversation-column':''} ${node.props?.layout==='row'?'slot-row':''} ${node.props?.tone==='suggestions'?'suggestions':''}`}>{childNodes}</div>;
    if(node.type==='card'){const CardIcon=iconMap[node.props?.icon||'layers']||Layers;return <section className="plugin-card"><div><CardIcon size={17}/><div><b>{node.props?.text}</b>{node.props?.subtitle&&<p>{node.props.subtitle}</p>}</div></div>{childNodes}</section>;}
    return renderNode(node);
  }
}

function Modal({title,subtitle,close,wide,reopenKey,children}:{title:string;subtitle?:string;close:()=>void;wide?:boolean;reopenKey:number;children:ReactNode}) {
  const root=useRef<HTMLDivElement>(null);
  const [closing,setClosing]=useState(false),closeTimer=useRef<number|undefined>(undefined);
  useEffect(()=>{const previous=document.activeElement as HTMLElement;const first=root.current?.querySelector<HTMLElement>('button,input,textarea,select');first?.focus();return()=>previous?.focus();},[]);
  useEffect(()=>()=>{if(closeTimer.current)window.clearTimeout(closeTimer.current);},[]);
  useEffect(()=>{if(closeTimer.current!==undefined){window.clearTimeout(closeTimer.current);closeTimer.current=undefined;setClosing(false);}},[reopenKey]);
  const requestClose=()=>{if(closing)return;setClosing(true);closeTimer.current=window.setTimeout(close,150);};
  return <div className={`modal-overlay ${closing?'closing':''}`} onMouseDown={e=>{if(e.target===e.currentTarget)requestClose();}}><div className={`modal ${wide?'modal-wide':''}`} role="dialog" aria-modal="true" aria-label={title} ref={root}
    onKeyDown={e=>{if(e.key==='Escape'){e.preventDefault();requestClose();return;}if(e.key!=='Tab')return;const items=[...root.current!.querySelectorAll<HTMLElement>('button:not(:disabled),input:not(:disabled),textarea,select')];const first=items[0],last=items.at(-1);if(e.shiftKey&&document.activeElement===first){e.preventDefault();last?.focus();}if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first?.focus();}}}>
    <header className="modal-header"><div><h2>{title}</h2>{subtitle&&<p>{subtitle}</p>}</div><button className="icon-button" aria-label="关闭弹窗" onClick={requestClose}><X size={19}/></button></header>{children}</div></div>;
}
function SettingsDialog({settings,ws,setWs,connection,busy,close,onConnect,onDisconnect,connectionFields,advancedFields,appearanceControl,appearanceToggles,privacyNote,connectionExtra,appearanceExtra,renderExtension,reopenKey}:{settings:Settings;ws:Workspace;setWs:React.Dispatch<React.SetStateAction<Workspace>>;connection:string;busy:boolean;close:()=>void;onConnect:(s:Settings)=>Promise<void>;onDisconnect:()=>Promise<void>;connectionFields:UINode;advancedFields:UINode;appearanceControl:UINode;appearanceToggles:UINode;privacyNote:UINode;connectionExtra:ReactNode;appearanceExtra:ReactNode;renderExtension:(node:UINode)=>ReactNode;reopenKey:number}) {
  const [form,setForm]=useState({...settings,password:''}),[error,setError]=useState(''),[pending,setPending]=useState(false),[tab,setTab]=useState<'connection'|'appearance'>('connection');
  const update=(key:keyof Settings,value:string|number)=>setForm(f=>({...f,[key]:value}));
  function renderSetting(node:UINode):ReactNode{
    if(node.props?.hidden)return null;
    const p=node.props||{},children=node.children?.map(child=><div className="node-wrapper" data-node={child.id} key={child.id}>{renderSetting(child)}</div>);
    if(node.type==='slot')return <div className={`slot settings-slot ${p.layout==='row'?'form-row':''}`} data-node={node.id}>{children}</div>;
    if(node.type==='settingField'){
      const field=p.field as 'host'|'port'|'username'|'password'|'model'|'binary'|'fingerprint';const value=field==='password'?form.password:form[field]??'';const password=field==='password';
      return <label className={field==='port'?'port-field':''} data-node={node.id}>{p.text}<input aria-label={password?'SSH 密码':p.text} autoComplete={password?'current-password':field==='username'?'username':undefined} type={password?'password':field==='port'?'number':'text'} min={field==='port'?1:undefined} max={field==='port'?65535:undefined} value={value} placeholder={password?(settings.hasPassword?'已安全保存；留空继续使用':'输入服务器密码'):field==='model'?'留空使用服务器模型配置':undefined} required={field==='host'||field==='port'||field==='username'} onChange={e=>update(field,field==='port'?Number(e.target.value):e.target.value)}/></label>;
    }
    if(node.type==='settingToggle'){
      const field=p.field as 'compact'|'reducedMotion'|'inspector';const checked=ws[field];
      return <div className="setting-row" data-node={node.id}><div><b>{p.text}</b><p>{p.subtitle}</p></div><button type="button" role="switch" aria-checked={checked} aria-label={p.text} className={`switch ${checked?'on':''}`} onClick={()=>setWs(w=>({...w,[field]:!w[field]}))}><i/></button></div>;
    }
    if(node.type==='settingTheme')return <div className="setting-row" data-node={node.id}><div><b>{p.text}</b><p>{p.subtitle}</p></div><div className="segmented"><button type="button" className={ws.theme==='dark'?'active':''} onClick={()=>setWs(w=>({...w,theme:'dark'}))}><Moon size={14}/> 深色</button><button type="button" className={ws.theme==='light'?'active':''} onClick={()=>setWs(w=>({...w,theme:'light'}))}><Sun size={14}/> 浅色</button></div></div>;
    if(node.type==='settingNote')return <div className="privacy-note" data-node={node.id}><ShieldCheck size={17}/><span>{p.text}</span></div>;
    return renderExtension(node);
  }
  return <Modal title="设置" subtitle="连接你的 Agent，调整你的工作方式。" close={close} reopenKey={reopenKey}><div className="modal-tabs"><button className={tab==='connection'?'active':''} onClick={()=>setTab('connection')}><Wifi size={15}/> 服务器连接</button><button className={tab==='appearance'?'active':''} onClick={()=>setTab('appearance')}><Palette size={15}/> 外观与体验</button></div>
    {tab==='connection'?<form className="settings-form" onSubmit={async e=>{e.preventDefault();setError('');setPending(true);try{await onConnect(form);}catch(e){setError(errorText(e));}finally{setPending(false);}}}>
      {renderSetting(connectionFields)}
      {connectionExtra}
      <details><summary>高级连接选项</summary>{renderSetting(advancedFields)}</details>
      {renderSetting(privacyNote)}
      {error&&<p className="error" role="alert">{error}</p>}
      <div className="form-footer"><span className="muted small">{busy?'请先停止任务，再修改连接':connection==='online'?'连接正常 · Agent 可用':'连接后即可使用真实模型和工具'}</span>{connection==='online'&&<button type="button" className="quiet" disabled={busy||pending} onClick={onDisconnect}>断开</button>}<button type="submit" className="primary" disabled={pending||busy}>{pending?<LoaderCircle className="spin" size={15}/>:<Wifi size={15}/>} {pending?'正在连接…':connection==='online'?'重新连接':'连接 Agent'}</button></div>
    </form>:<div className="appearance-settings">{renderSetting(appearanceControl)}{renderSetting(appearanceToggles)}{appearanceExtra}</div>}
  </Modal>;
}
function PluginDialog({plugins,errors,tree,selected,setSelected,install,close,notify,setPlugins,inspect,reopenKey}:{plugins:Plugin[];errors:Record<string,string>;tree:UINode;selected:string;setSelected:(s:string)=>void;install:(p:unknown)=>void;close:()=>void;notify:(s:string)=>void;setPlugins:(p:Plugin[])=>void;inspect:()=>void;reopenKey:number}) {
  const [tab,setTab]=useState<'plugins'|'elements'|'editor'>('plugins'),[source,setSource]=useState(JSON.stringify(examplePlugin,null,2)),[error,setError]=useState('');
  useEffect(()=>{setTab('plugins');},[reopenKey]);
  const selectedComponent=findNode(tree,selected)||tree;
  const [editText,setEditText]=useState(''),[editSubtitle,setEditSubtitle]=useState(''),[parent,setParent]=useState('before-chat');
  useEffect(()=>{setEditText(selectedComponent.props?.text||'');setEditSubtitle(selectedComponent.props?.subtitle||'');},[selectedComponent.id]);
  function saveSource(){try{const p=validatePlugin(JSON.parse(source));install(p);setError('');setTab('plugins');}catch(e){setError(errorText(e));}}
  function personalOperation(operation:Plugin['operations'][number]) {
    const previous=plugins.find(p=>p.id==='my-layout');
    const plugin:Plugin=previous?structuredClone(previous):{id:'my-layout',name:'我的界面调整',version:'1.0.0',description:'在界面管理器中创建的个人调整。',enabled:true,operations:[]};
    plugin.enabled=true;plugin.operations.push(operation);install(plugin);
  }
  function treeItem(node:UINode,depth=0):ReactNode{return <div key={node.id}><button className={`tree-item ${selected===node.id?'active':''}`} style={{paddingLeft:12+depth*14}} onClick={()=>setSelected(node.id)}><Layers size={12}/><span>{node.id}</span><small>{node.type}</small>{node.props?.hidden&&<span>隐藏</span>}</button>{node.children?.map(c=>treeItem(c,depth+1))}</div>;}
  return <Modal title="把界面，变成你喜欢的样子。" subtitle="添加一点灵感，移走一点干扰。每个调整，都由你决定。" close={close} wide reopenKey={reopenKey}>
    <div className="modal-tabs"><button className={tab==='plugins'?'active':''} onClick={()=>setTab('plugins')}><Puzzle size={15}/> 我的插件 <span>{plugins.length}</span></button><button className={tab==='elements'?'active':''} onClick={()=>setTab('elements')}><Layers size={15}/> 界面元素</button><button className={tab==='editor'?'active':''} onClick={()=>setTab('editor')}><Code2 size={15}/> 插件编辑器</button></div>
    {tab==='plugins'?<div className="plugin-manager"><div className="plugin-manager-toolbar"><span>{plugins.filter(p=>p.enabled).length} 个插件已启用 · 按列表顺序应用</span><button className="secondary" onClick={async()=>{if(!api){setTab('editor');return;}try{const p=await api.importPlugin();if(p)install(p);}catch(e){notify(errorText(e));}}}><Upload size={14}/> 导入插件</button><button className="primary" onClick={()=>{setSource(JSON.stringify(examplePlugin,null,2));setTab('editor');}}><Plus size={14}/> 创建插件</button></div>
      {!plugins.length&&<div className="plugin-empty"><div className="plugin-empty-icon"><Puzzle size={28}/></div><h3>一个空白画布，无限种可能。</h3><p>用插件调整组件、添加快捷入口，或换一种颜色。</p><button className="secondary" onClick={()=>install(examplePlugin)}><Sparkles size={14}/> 试试「每日工作台」</button></div>}
      <div className="plugin-list">{plugins.map((p,i)=><article className="plugin-item" key={p.id}><div className="plugin-item-icon"><Puzzle size={22}/></div><div className="plugin-item-info"><h3>{p.name}<span>v{p.version}</span></h3><p>{p.description}</p>{errors[p.id]&&<p className="error">应用失败：{errors[p.id]}（其他插件仍正常工作）</p>}<span className="tiny-tag">{p.operations.length} 项界面操作</span></div><div className="plugin-item-actions"><button title="上移插件" aria-label={`上移 ${p.name}`} disabled={i===0} onClick={()=>{const next=[...plugins];[next[i-1],next[i]]=[next[i],next[i-1]];setPlugins(next);}}><ArrowUp size={14}/></button><button title="编辑插件" aria-label={`编辑 ${p.name}`} onClick={()=>{setSource(JSON.stringify(p,null,2));setTab('editor');}}><Code2 size={15}/></button><button title="导出插件" aria-label={`导出 ${p.name}`} onClick={()=>api?.exportPlugin(p).catch(e=>notify(errorText(e)))}><Download size={15}/></button><button title="删除插件" aria-label={`删除 ${p.name}`} onClick={()=>setPlugins(plugins.filter(i=>i.id!==p.id))}><Trash2 size={14}/></button><button role="switch" aria-label={`启用 ${p.name}`} aria-checked={p.enabled} className={`switch ${p.enabled?'on':''}`} onClick={()=>setPlugins(plugins.map(i=>i.id===p.id?{...i,enabled:!i.enabled}:i))}><i/></button></div></article>)}</div>
      <div className="plugin-manager-footer"><span><ShieldCheck size={14}/> 界面插件使用声明式配置，不执行脚本。</span><button className="quiet" onClick={()=>{setPlugins(plugins.map(p=>({...p,enabled:false})));notify('已恢复默认布局；插件已保留并停用');}}><RotateCcw size={13}/> 恢复默认布局</button></div>
    </div>:tab==='elements'?<div className="elements-editor"><div className="element-tree"><div className="tree-heading">组件树 <button className="quiet" onClick={inspect}><Search size={12}/> 点选界面</button></div>{treeItem(tree)}</div><div className="element-properties"><div className="section-label">当前组件</div><h3>{selectedComponent.id}</h3><span className="tag">{selectedComponent.type}</span><label>显示文字<input aria-label="元素文字" value={editText} onChange={e=>setEditText(e.target.value)}/></label><label>辅助文字<input aria-label="元素辅助文字" value={editSubtitle} onChange={e=>setEditSubtitle(e.target.value)}/></label><button className="primary full" disabled={selectedComponent.id==='app'} onClick={()=>personalOperation({op:'patch',target:selectedComponent.id,props:{text:editText,subtitle:editSubtitle}})}>保存属性</button><div className="element-action-row"><button className="secondary" disabled={selectedComponent.id==='app'} onClick={()=>personalOperation({op:'patch',target:selectedComponent.id,props:{hidden:!selectedComponent.props?.hidden}})}>{selectedComponent.props?.hidden?'显示组件':'隐藏组件'}</button><button className="secondary danger" disabled={selectedComponent.id==='app'} onClick={()=>{personalOperation({op:'remove',target:selectedComponent.id});setSelected('brand');}}><Trash2 size={13}/> 移除</button></div>
      <label>目标容器<select aria-label="目标容器" value={parent} onChange={e=>setParent(e.target.value)}>{flattenNodes(tree).filter(n=>CONTAINERS.includes(n.type)&&n.type!=='pluginReservoir').map(n=><option key={n.id} value={n.id}>{n.id}</option>)}</select></label><button className="secondary full" disabled={selectedComponent.id==='app'} onClick={()=>personalOperation({op:'move',target:selectedComponent.id,parent})}><GripVertical size={14}/> 移动到目标容器</button><button className="secondary full" onClick={()=>personalOperation({op:'add',parent,node:{id:'custom-'+crypto.randomUUID().slice(0,8),type:'card',props:{text:'我的新组件',subtitle:'在编辑器中自由修改内容。'}}})}><Plus size={14}/> 在目标容器添加卡片</button><p className="muted small">更改保存到「我的界面调整」插件，可随时关闭或导出。工具栏和 F8 始终保留恢复入口。</p></div></div>:
    <div className="source-editor"><div className="source-toolbar"><span><Code2 size={14}/> plugin.json</span><button className="quiet" onClick={()=>{try{setSource(JSON.stringify(JSON.parse(source),null,2));setError('');}catch(e){setError(errorText(e));}}}>格式化</button></div><textarea aria-label="插件 JSON" spellCheck={false} value={source} onChange={e=>setSource(e.target.value)}/>{error&&<p className="error" role="alert">{error}</p>}<div className="source-help"><p>操作：<code>add</code> 添加 · <code>remove</code> 删除 · <code>patch</code> 修改/替换 · <code>move</code> 移动。<br/>可用容器包括侧栏、工具栏、对话/输入、状态、设置和记忆扩展槽。完整组件 ID 可在「界面元素」中查看。</p><button className="primary" onClick={saveSource}><Check size={14}/> 保存并应用</button></div></div>}
  </Modal>;
}
function flattenNodes(node:UINode):UINode[]{return [node,...(node.children||[]).flatMap(flattenNodes)];}
