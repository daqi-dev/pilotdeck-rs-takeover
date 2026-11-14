import {renderHook,waitFor,cleanup,act} from '@testing-library/react';
import {afterEach,beforeEach,expect,it,vi} from 'vitest';
import {useProjectsState} from '@baseline/ui/src/hooks/useProjectsState';
import {api} from '@baseline/ui/src/utils/api';
vi.mock('@baseline/ui/src/utils/api',()=>({api:{projects:vi.fn(),sessions:vi.fn()}}));
const preview=Array.from({length:5},(_,i)=>({id:`s${i+1}`}));
const projects=[{name:'workspace',displayName:'Workspace',fullPath:'/workspace',sessions:preview,sessionMeta:{hasMore:true,total:12}}];
const response=(body:unknown)=>({ok:true,status:200,headers:{get:()=>null},json:async()=>body});
const activeSessions=new Set<string>();const navigate=vi.fn();
beforeEach(()=>{vi.mocked(api.projects).mockReset().mockResolvedValue(response(projects) as never);vi.mocked(api.sessions).mockReset();localStorage.clear();});
afterEach(()=>{cleanup();vi.restoreAllMocks()});
const hook=(id:string)=>renderHook(({id})=>useProjectsState({sessionId:id,navigate,activeSessions,isMobile:false,latestMessage:null}),{initialProps:{id}});
it('finds an older target without corrupting preview pagination',async()=>{
 vi.mocked(api.sessions).mockResolvedValue(response({sessions:Array.from({length:12},(_,i)=>({id:`s${i+1}`})),hasMore:false}) as never);
 const h=hook('s10');await waitFor(()=>expect(h.result.current.selectedSession?.id).toBe('s10'));
 expect(h.result.current.selectedProject?.name).toBe('workspace');expect(h.result.current.projects[0].sessions).toHaveLength(5);
});
it('clears the old selection and reports an unresolved target',async()=>{
 vi.mocked(api.sessions).mockResolvedValue(response({sessions:preview,hasMore:false}) as never);
 const h=hook('s1');await waitFor(()=>expect(h.result.current.selectedSession?.id).toBe('s1'));
 h.rerender({id:'missing'});await waitFor(()=>expect(h.result.current.routeLookupError).toContain('未找到'));
 expect(h.result.current.selectedSession).toBeNull();
});
it('does not select a delayed stale response after navigation',async()=>{
 let resolveOld:(v:unknown)=>void=()=>{};
 vi.mocked(api.sessions).mockImplementation(()=>new Promise(r=>{resolveOld=r}) as never);
 const h=hook('old');await waitFor(()=>expect(api.sessions).toHaveBeenCalled());h.rerender({id:'s1'});
 await waitFor(()=>expect(h.result.current.selectedSession?.id).toBe('s1'));
 await act(async()=>resolveOld(response({sessions:[{id:'old'}],hasMore:false})));
 expect(h.result.current.selectedSession?.id).toBe('s1');expect(h.result.current.selectedProject?.name).toBe('workspace');
});
it('reports failed lookup and leaves no selected session',async()=>{
 vi.mocked(api.sessions).mockResolvedValue({ok:false,status:503} as never);
 const h=hook('missing');await waitFor(()=>expect(h.result.current.routeLookupError).toContain('无法定位'));
 expect(h.result.current.selectedSession).toBeNull();
});
