export const TYPES = ['shell','sidebar','brand','newChat','navigation','sessions','searchInput','sessionRow','profile','connectionStatus','workspace','toolbar','main','conversation','welcome','composer','inspector','status','card','text','button','sendButton','messageRow','messageContent','messageAvatar','messageMeta','messageBody','messageError','messageStatus','messageActions','settingField','settingToggle','settingTheme','settingNote','divider','slot','pluginReservoir'] as const;
export type NodeType = typeof TYPES[number];
export type Props = {text?:string;subtitle?:string;hint?:string;modeText?:string;field?:string;placeholder?:string;onlineText?:string;offlineText?:string;connectingText?:string;protocolText?:string;assistantText?:string;userText?:string;showTimestamp?:boolean;icon?:string;action?:string;prompt?:string;tone?:string;layout?:'row'|'column';hidden?:boolean};
export type UINode = {id:string;type:NodeType;props?:Props;children?:UINode[]};
export type Operation = {op:'add';parent:string;node:UINode;index?:number} | {op:'remove';target:string} |
  {op:'patch';target:string;type?:NodeType;props?:Props} | {op:'move';target:string;parent:string;index?:number};
export type Plugin = {id:string;name:string;version:string;description:string;enabled:boolean;operations:Operation[];theme?:Record<string,string>};
export const CONTAINERS:NodeType[]=['shell','sidebar','navigation','sessions','sessionRow','profile','workspace','toolbar','main','conversation','welcome','composer','messageRow','messageContent','messageActions','inspector','status','card','text','button','slot','pluginReservoir'];
export const baseTree:UINode = {
  id:'app',type:'shell',children:[
    {id:'sidebar',type:'sidebar',children:[
      {id:'brand',type:'brand',props:{text:'云栖',subtitle:'任务工作区'}},
      {id:'new-chat',type:'newChat',props:{text:'开启新对话'}},
      {id:'navigation',type:'navigation',children:[
        {id:'nav-chat',type:'button',props:{text:'对话空间',icon:'message',action:'chat',tone:'navigation'}},
        {id:'nav-memories',type:'button',props:{text:'长期记忆',icon:'brain',action:'memories',tone:'navigation'}},
        {id:'nav-plugins',type:'button',props:{text:'插件与界面',icon:'puzzle',action:'plugins',tone:'navigation'}}]},
      {id:'sidebar-extra',type:'slot'},
      {id:'sessions',type:'sessions',children:[
        {id:'sessions-heading',type:'text',props:{text:'最近对话',tone:'section-label'}},
        {id:'sessions-search-button',type:'button',props:{text:'搜索对话',icon:'search',action:'focus-session-search',tone:'toolbar'}},
        {id:'sessions-search-input',type:'searchInput',props:{text:'搜索对话',placeholder:'搜索对话…'}},
        {id:'sessions-items',type:'slot',children:[]},
        {id:'session-row-template',type:'sessionRow',children:[
          {id:'session-title',type:'button',props:{text:'会话名称',icon:'message',action:'select-session',tone:'session-name'}},
          {id:'session-delete',type:'button',props:{text:'删除对话',icon:'trash',action:'delete-session',tone:'session-delete'}}]},
        {id:'sessions-tip',type:'text',props:{text:'双击对话名称即可重命名',tone:'session-tip'}}]},
      {id:'profile',type:'profile',children:[
        {id:'profile-connection',type:'connectionStatus',props:{onlineText:'Agent 已连接',offlineText:'Agent 未连接',connectingText:'正在连接',protocolText:'SSH'}},
        {id:'profile-button',type:'button',props:{text:'我的工作空间',icon:'settings',action:'settings',tone:'profile'}}]}]},
    {id:'workspace',type:'workspace',children:[
      {id:'toolbar',type:'toolbar',children:[
        {id:'toolbar-breadcrumb',type:'text',props:{tone:'breadcrumb'}},
        {id:'toolbar-actions',type:'slot',props:{layout:'row'},children:[
          {id:'toolbar-model',type:'text',props:{tone:'model'}},
          {id:'toolbar-export',type:'button',props:{text:'导出当前对话',icon:'download',action:'export',tone:'toolbar'}},
          {id:'toolbar-inspector',type:'button',props:{text:'切换侧面板',icon:'panel',action:'toggle-inspector',tone:'toolbar'}}]}]},
      {id:'main',type:'main',children:[
        {id:'conversation-column',type:'slot',props:{layout:'column'},children:[
          {id:'before-chat',type:'slot'},
          {id:'conversation',type:'conversation',children:[
          {id:'welcome',type:'welcome',children:[
              {id:'welcome-eyebrow',type:'text',props:{text:'新对话',tone:'eyebrow'}},
              {id:'welcome-title',type:'text',props:{text:'新的任务',tone:'heading'}},
              {id:'welcome-description',type:'text',props:{text:'描述要完成的工作，或粘贴参考资料。',tone:'description'}},
            {id:'suggestions',type:'slot',props:{tone:'suggestions',hidden:true},children:[
                {id:'suggest-code',type:'button',props:{text:'一起写点代码',subtitle:'从一个想法，到可以运行',icon:'code',tone:'suggestion',action:'prompt',prompt:'我有一个项目想法，请先帮我梳理需求和实现步骤。'}},
                {id:'suggest-think',type:'button',props:{text:'整理我的思路',subtitle:'给零散的想法，一个方向',icon:'brain',tone:'suggestion',action:'prompt',prompt:'帮我梳理思路，请先问我三个关键问题。'}},
                {id:'suggest-solve',type:'button',props:{text:'解决一个问题',subtitle:'一步步找到问题的答案',icon:'terminal',tone:'suggestion',action:'prompt',prompt:'帮我排查一个技术问题，请告诉我需要提供哪些信息。'}}]},
            ]},
            {id:'message-row-template',type:'messageRow',children:[
          {id:'message-avatar',type:'messageAvatar',props:{icon:'message',text:'Y'}},
              {id:'message-content',type:'messageContent',children:[
                {id:'message-meta',type:'messageMeta',props:{assistantText:'云栖',userText:'你',showTimestamp:true}},
                {id:'message-body',type:'messageBody'},
                {id:'message-error',type:'messageError'},
                {id:'message-status',type:'messageStatus'},
                {id:'message-actions',type:'messageActions',children:[
                  {id:'message-copy',type:'button',props:{text:'复制消息',icon:'copy',action:'copy-message'}},
                  {id:'message-tool-count',type:'text',props:{text:'工具调用',icon:'terminal',tone:'tool-count'}},
                  {id:'message-edit-again',type:'button',props:{text:'重新编辑',icon:'edit',action:'retry-message'}}]}]}]},
          ]},
          {id:'before-composer',type:'slot'},
          {id:'composer',type:'composer',props:{text:'输入任务或粘贴资料…',subtitle:'Enter 发送 · Shift + Enter 换行',hint:'Shift + Enter 换行',modeText:'对话'},children:[
            {id:'composer-actions',type:'slot',props:{layout:'row'},children:[
              {id:'composer-send',type:'sendButton',props:{text:'发送消息',icon:'arrow'}}]}]}
        ]},
        {id:'inspector',type:'inspector',children:[{id:'inspector-extra',type:'slot'}]}
      ]},
      {id:'status',type:'status',children:[{id:'status-extra',type:'slot',props:{layout:'row'}}]}
    ]},
    {id:'plugin-surfaces',type:'pluginReservoir',children:[
      {id:'settings-connection-fields',type:'slot',props:{layout:'column'},children:[
        {id:'settings-host-port',type:'slot',props:{layout:'row'},children:[
          {id:'settings-host',type:'settingField',props:{field:'host',text:'服务器地址'}},
          {id:'settings-port',type:'settingField',props:{field:'port',text:'SSH 端口'}}]},
        {id:'settings-user-password',type:'slot',props:{layout:'row'},children:[
          {id:'settings-username',type:'settingField',props:{field:'username',text:'用户名'}},
          {id:'settings-password',type:'settingField',props:{field:'password',text:'密码'}}]},
        {id:'settings-model',type:'settingField',props:{field:'model',text:'模型'}}]},
      {id:'settings-connection-extra',type:'slot'},
      {id:'settings-advanced-fields',type:'slot',props:{layout:'column'},children:[
        {id:'settings-binary',type:'settingField',props:{field:'binary',text:'Agent 程序路径'}},
        {id:'settings-fingerprint',type:'settingField',props:{field:'fingerprint',text:'服务器主机指纹（SHA256）'}}]},
      {id:'settings-appearance-control',type:'settingTheme',props:{text:'界面主题',subtitle:'为你的工作空间选择浅色或深色主题。'}},
      {id:'settings-appearance-toggles',type:'slot',props:{layout:'column'},children:[
        {id:'settings-compact',type:'settingToggle',props:{field:'compact',text:'紧凑布局',subtitle:'减少间距，在屏幕里放下更多内容。'}},
        {id:'settings-reduced-motion',type:'settingToggle',props:{field:'reducedMotion',text:'减少动态效果',subtitle:'跟随系统偏好，关闭非必要动态。'}},
        {id:'settings-inspector',type:'settingToggle',props:{field:'inspector',text:'显示工作台侧栏',subtitle:'随时查看连接、插件和任务动态。'}}]},
      {id:'settings-appearance-extra',type:'slot'},
      {id:'settings-privacy-note',type:'settingNote',props:{text:'连接通过 SSH 加密。密码由 Windows 保护后保存在本机，模型密钥留在服务器。'}},
      {id:'memories-extra',type:'slot'}
    ]}
  ]
};
export const examplePlugin:Plugin={id:'daily-workbench',name:'每日工作台',version:'1.0.0',description:'在对话上方添加专注卡片，修改品牌文字，并切换雾蓝主题。',enabled:true,
  theme:{accent:'#3b6fd8','accent-dim':'#e7efff'},operations:[
    {op:'patch',target:'brand',props:{text:'云栖 · 专注'}},
    {op:'patch',target:'toolbar-inspector',props:{text:'工作台'}},
    {op:'add',parent:'composer-actions',node:{id:'daily-plan-shortcut',type:'button',props:{text:'先梳理需求',icon:'brain',action:'prompt',prompt:'请先帮我把需求梳理清楚，再给出实现步骤。'}}},
    {op:'add',parent:'before-chat',node:{id:'daily-focus',type:'card',props:{text:'今天，专注一件重要的事。',subtitle:'把想法交给云栖，留出时间给创造。',icon:'layers'},children:[
      {id:'daily-action',type:'button',props:{text:'规划今天的任务',action:'prompt',prompt:'帮我把今天的工作拆成一个清晰、可执行的计划。'}}]}}
  ]};
const validId=(s:unknown):s is string=>typeof s==='string'&&/^[a-zA-Z][a-zA-Z0-9_.-]{0,79}$/.test(s);
const object=(v:unknown):v is Record<string,unknown>=>!!v&&typeof v==='object'&&!Array.isArray(v);
function checkProps(v:unknown) {
  if(v===undefined)return;
  if(!object(v))throw new Error('props 必须是对象');
  for(const [key,value] of Object.entries(v)){
    if(!['text','subtitle','hint','modeText','field','placeholder','onlineText','offlineText','connectingText','protocolText','assistantText','userText','showTimestamp','icon','action','prompt','tone','layout','hidden'].includes(key))throw new Error(`不支持的属性：${key}`);
    if(key==='hidden'||key==='showTimestamp'){if(typeof value!=='boolean')throw new Error(`${key} 必须是布尔值`);}
    else if(typeof value!=='string'||value.length>12000)throw new Error(`属性 ${key} 无效或过长`);
    if(key==='action'&&!['chat','new-chat','plugins','settings','memories','prompt','toggle-inspector','export','focus-session-search','select-session','delete-session','copy-message','retry-message'].includes(value as string))throw new Error('插件动作不在允许列表');
    if(key==='layout'&&!['row','column'].includes(value as string))throw new Error('layout 必须为 row 或 column');
    if(key==='field'&&!['host','port','username','password','model','binary','fingerprint','compact','reducedMotion','inspector'].includes(value as string))throw new Error('设置字段不在允许列表');
  }
}
function checkNode(v:unknown,depth=0,count={n:0}) {
  if(!object(v)||!validId(v.id)||!TYPES.includes(v.type as NodeType))throw new Error('组件 id 或类型无效');
  if(depth>12||++count.n>300)throw new Error('组件树最多 12 层、300 个组件');
  checkProps(v.props);
  if(v.children!==undefined){if(!Array.isArray(v.children))throw new Error('children 必须是数组');if(v.children.length&&!CONTAINERS.includes(v.type as NodeType))throw new Error(`${v.type} 不支持子组件`);for(const n of v.children)checkNode(n,depth+1,count);}
}
export function validatePlugin(v:unknown):Plugin {
  if(!object(v)||!validId(v.id)||typeof v.name!=='string'||!v.name.trim()||v.name.length>80||
    typeof v.version!=='string'||v.version.length>40||typeof v.description!=='string'||v.description.length>2000||
    typeof v.enabled!=='boolean'||!Array.isArray(v.operations)||v.operations.length>200)throw new Error('插件清单无效：需要 id、name、version、description、enabled 和 operations');
  for(const operation of v.operations){
    if(!object(operation)||!['add','remove','patch','move'].includes(operation.op as string))throw new Error('未知插件操作');
    if(operation.op==='add'||operation.op==='move'){
      if(!validId(operation.parent))throw new Error('parent 无效');
      if(operation.index!==undefined&&(!Number.isInteger(operation.index)||(operation.index as number)<0))throw new Error('index 无效');
    }
    if(operation.op==='add')checkNode(operation.node);
    else if(!validId(operation.target))throw new Error('target 无效');
    if(operation.op==='patch'){
      if(operation.type!==undefined&&!TYPES.includes(operation.type as NodeType))throw new Error('组件类型无效');
      checkProps(operation.props);
    }
  }
  if(v.theme!==undefined){
    if(!object(v.theme))throw new Error('theme 必须是对象');
    for(const [k,color] of Object.entries(v.theme)){
      if(!['accent','accent-dim','bg','panel','sidebar','text','muted','border','surface','soft','input','hover','scrollbar','focus','danger','success','warning'].includes(k)||typeof color!=='string'||!/^#[\da-f]{6}$/i.test(color))throw new Error('主题仅支持预设颜色变量与六位 HEX 色值');
    }
  }
  // Round-trip removes prototypes and ensures imported values stay plain data.
  return JSON.parse(JSON.stringify(v)) as Plugin;
}
export function findNode(root:UINode,id:string):UINode|undefined {
  if(root.id===id)return root;
  for(const child of root.children||[]){const found=findNode(child,id);if(found)return found;}
}
function detach(root:UINode,id:string):UINode|undefined {
  const at=root.children?.findIndex(n=>n.id===id)??-1;
  if(at>=0)return root.children!.splice(at,1)[0];
  for(const child of root.children||[]){const found=detach(child,id);if(found)return found;}
}
function ids(root:UINode):string[]{return [root.id,...(root.children||[]).flatMap(ids)];}
export function applyPlugin(tree:UINode,plugin:Plugin):UINode {
  const next=structuredClone(tree);
  for(const op of plugin.operations){
    if(op.op==='add'){
      const parent=findNode(next,op.parent);if(!parent)throw new Error(`找不到容器 ${op.parent}`);
      if(!CONTAINERS.includes(parent.type))throw new Error(`${parent.id} 不支持子组件`);
      const incoming=ids(op.node),existing=new Set(ids(next));
      if(new Set(incoming).size!==incoming.length||incoming.some(id=>existing.has(id)))throw new Error('新增组件 id 重复');
      (parent.children??=[]).splice(op.index??parent.children.length,0,structuredClone(op.node));
    } else {
      if(op.target==='app')throw new Error('根组件 app 保留；可修改其子元素');
      if(op.target==='plugin-surfaces')throw new Error('插件扩展槽根节点保留；可修改其子槽');
      const target=findNode(next,op.target);if(!target)throw new Error(`找不到组件 ${op.target}`);
      if(op.op==='remove')detach(next,op.target);
      if(op.op==='patch'){if(op.type)target.type=op.type;target.props={...target.props,...op.props};}
      if(op.op==='move'){
        const parent=findNode(next,op.parent);if(!parent)throw new Error(`找不到容器 ${op.parent}`);
        if(!CONTAINERS.includes(parent.type))throw new Error(`${parent.id} 不支持子组件`);
        if(findNode(target,parent.id))throw new Error('不能把组件移动到自身或其子组件');
        detach(next,op.target);(parent.children??=[]).splice(op.index??parent.children.length,0,target);
      }
    }
  }
  checkNode(next);
  return next;
}
export function compose(plugins:Plugin[]) {
  let tree=structuredClone(baseTree);const errors:Record<string,string>={};const theme:Record<string,string>={};
  for(const raw of plugins){
    if(!raw.enabled)continue;
    try{const plugin=validatePlugin(raw);tree=applyPlugin(tree,plugin);Object.assign(theme,plugin.theme);}
    catch(e){errors[raw.id]=(e as Error).message;}
  }
  return {tree,errors,theme};
}
