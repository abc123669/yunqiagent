import {test} from 'node:test';
import assert from 'node:assert/strict';
import {applyPlugin,baseTree,compose,examplePlugin,findNode,validatePlugin,CONTAINERS,TYPES,type Plugin} from '../src/plugins';
const plugin=(operations:Plugin['operations']):Plugin=>({id:'test',name:'测试',version:'1',description:'',enabled:true,operations});
test('add + patch + theme produce a new tree and leave the default unchanged',()=>{
  const before=JSON.stringify(baseTree),result=compose([examplePlugin]);
  assert.equal(result.errors['daily-workbench'],undefined);
  assert.equal(findNode(result.tree,'brand')?.props?.text,'云栖 · 专注');
  assert.equal(findNode(result.tree,'daily-action')?.props?.action,'prompt');
  assert.equal(findNode(result.tree,'daily-plan-shortcut')?.props?.action,'prompt');
  assert.equal(findNode(result.tree,'toolbar-inspector')?.props?.text,'工作台');
  assert.equal(result.theme.accent,'#3b6fd8');assert.equal(JSON.stringify(baseTree),before);
});
test('stable surface slots allow plugins to extend the composer, toolbar, settings and memories',()=>{
  for(const id of ['toolbar-actions','composer-actions','composer-send','status-extra','inspector-extra','sessions-heading','sessions-search-button','sessions-search-input','session-row-template','session-title','session-delete','profile-connection','profile-button','settings-connection-fields','settings-connection-extra','settings-advanced-fields','settings-appearance-control','settings-appearance-toggles','settings-appearance-extra','settings-privacy-note','memories-extra'])assert.ok(findNode(baseTree,id),id);
  for(const type of ['toolbar','composer','inspector','status','sessions','sessionRow','profile','pluginReservoir'] as const)assert.ok(CONTAINERS.includes(type),type);
  for(const type of ['sendButton','searchInput','settingField','settingToggle','settingTheme','settingNote','connectionStatus'] as const)assert.ok(TYPES.includes(type),type);
  const extended=compose([plugin([{op:'patch',target:'sessions-heading',props:{text:'历史会话'}},{op:'patch',target:'session-delete',props:{hidden:true}},{op:'add',parent:'settings-connection-extra',node:{id:'custom-connection-help',type:'text',props:{text:'连接说明'}}}])]);
  assert.deepEqual(extended.errors,{});assert.equal(findNode(extended.tree,'sessions-heading')?.props?.text,'历史会话');assert.equal(findNode(extended.tree,'session-delete')?.props?.hidden,true);assert.equal(findNode(extended.tree,'custom-connection-help')?.props?.text,'连接说明');
});
test('remove, move, replace, hide and disable are reversible by recomposition',()=>{
  const p=plugin([{op:'remove',target:'inspector'},{op:'move',target:'status',parent:'sidebar'},
    {op:'patch',target:'brand',type:'text',props:{text:'个人界面',hidden:true}}]);
  const result=compose([p]);assert.deepEqual(result.errors,{});
  assert.equal(findNode(result.tree,'inspector'),undefined);
  assert.ok(findNode(result.tree,'sidebar')?.children?.some(n=>n.id==='status'));
  assert.equal(findNode(result.tree,'brand')?.type,'text');
  assert.equal(findNode(result.tree,'brand')?.props?.hidden,true);
  assert.deepEqual(compose([{...p,enabled:false}]).tree,baseTree);
});
test('a failing plugin rolls back every operation and does not break later plugins',()=>{
  const bad=plugin([{op:'patch',target:'brand',props:{text:'不应出现'}},{op:'remove',target:'missing'}]);
  const result=compose([bad,examplePlugin]);assert.match(result.errors.test,/missing/);
  assert.equal(findNode(result.tree,'brand')?.props?.text,'云栖 · 专注');
});
test('cycles, duplicate IDs and root removal are rejected',()=>{
  assert.throws(()=>applyPlugin(baseTree,plugin([{op:'move',target:'sidebar',parent:'brand'}])),/子组件/);
  assert.throws(()=>applyPlugin(baseTree,plugin([{op:'add',parent:'before-chat',node:{id:'brand',type:'text'}}])),/重复/);
  assert.throws(()=>applyPlugin(baseTree,plugin([{op:'remove',target:'app'}])),/根组件/);
  assert.throws(()=>applyPlugin(baseTree,plugin([{op:'remove',target:'plugin-surfaces'}])),/扩展槽根节点/);
});
test('untrusted plugin properties cannot inject scripts, URLs, CSS or arbitrary actions',()=>{
  for(const props of [{html:'<script/>'},{action:'exec'},{layout:'absolute'}]){
    assert.throws(()=>validatePlugin(plugin([{op:'patch',target:'brand',props:props as never}])));
  }
  assert.throws(()=>validatePlugin({...examplePlugin,theme:{accent:'url(https://bad.example)'}}));
  assert.throws(()=>validatePlugin({...examplePlugin,theme:{position:'#ffffff'}}));
  assert.throws(()=>validatePlugin({...examplePlugin,operations:Array(201).fill({op:'remove',target:'brand'})}));
});
test('plugin ordering is deterministic and later patches win',()=>{
  const first=plugin([{op:'patch',target:'brand',props:{text:'first'}}]);
  const second={...plugin([{op:'patch',target:'brand',props:{text:'second'}}]),id:'second'};
  assert.equal(findNode(compose([first,second]).tree,'brand')?.props?.text,'second');
  assert.equal(findNode(compose([second,first]).tree,'brand')?.props?.text,'first');
});
