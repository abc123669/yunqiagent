import type {Plugin} from './plugins';
export type ChatMessage={id:string;role:'user'|'assistant';text:string;at:number;error?:string;stopped?:boolean;tools?:number};
export type Session={id:string;title:string;messages:ChatMessage[];updated:number};
export type Workspace={sessions:Session[];active:string;plugins:Plugin[];inspector:boolean;compact:boolean;reducedMotion:boolean;theme:'dark'|'light';uiVersion?:number};
export type Settings={host:string;port:number;username:string;binary:string;fingerprint:string;model?:string;hasPassword?:boolean;password?:string};
export type AgentEvent={type:string;id?:string;text?:string;message?:string;tools?:number;stopped?:boolean;model?:string;items?:Memory[]};
export type Memory={id:number;kind:string;content:string;importance:number};
export type Desktop={
  load():Promise<{workspace:Workspace|null;settings:Settings}>;
  save(state:Workspace):Promise<boolean>;
  connect(settings:Settings):Promise<AgentEvent>;
  disconnect():Promise<boolean>;
  info():Promise<AgentEvent>;
  memories():Promise<AgentEvent>;
  chat(request:{id:string;session:string;input:string}):Promise<boolean>;
  cancel(id:string):Promise<boolean>;
  importPlugin():Promise<unknown>;
  exportPlugin(plugin:Plugin):Promise<boolean>;
  window(action:string):Promise<void>;
  onEvent(callback:(event:AgentEvent)=>void):()=>void;
};
declare global {interface Window {desktop?:Desktop}}
