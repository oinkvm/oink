import { readFileSync, readdirSync } from 'node:fs';
import { join, relative } from 'node:path';
import ts from 'typescript';
import { expect, it } from 'vitest';
import allowlist from './backendNoticeAllowlist.json';

// Frozen debt from the bottom-notice audit. The bottom-centred notice is gone and
// these legacy paths only log their code. Removing old uses is welcome; adding an
// allowance to make a new call pass is not. New user-facing failures belong in oink-ui toast.
function noticeUses(file:string,source:string):string[] {
  const ast=ts.createSourceFile(file,source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
  const uses:string[]=[];
  const record=(node:ts.Node)=>uses.push(node.getText(ast).replace(/\s+/g,' ').trim());
  const visit=(node:ts.Node)=>{
    const access=ts.isPropertyAccessExpression(node)||ts.isElementAccessExpression(node);
    if(access){
      const object=node.expression.getText(ast);
      const name=ts.isPropertyAccessExpression(node)?node.name.text:node.argumentExpression?.getText(ast).replace(/['"]/g,'');
      if(object==='agent'&&name==='fail')record(node.parent);
    }
    // App's local wrapper can be called or handed off as a callback.
    if(file==='App.tsx'&&ts.isIdentifier(node)&&node.text==='fail'
      &&!ts.isPropertyAccessExpression(node.parent)&&!ts.isPropertyAssignment(node.parent)
      &&!ts.isVariableDeclaration(node.parent))record(node.parent);
    if(ts.isCallExpression(node)){
      const callee=node.expression.getText(ast);
      const first=node.arguments[0];
      if(first&&ts.isStringLiteral(first)&&first.text==='error'&&/^(agent\.)?(adopt|query)$/.test(callee))record(node);
      if(file==='api/store.ts'&&callee==='set'&&first&&ts.isObjectLiteralExpression(first)
        &&first.properties.some(p=>p.name?.getText(ast)==='error'))record(node);
    }
    ts.forEachChild(node,visit);
  };
  visit(ast);return uses;
}
function productionFiles(dir:string):string[]{
  return readdirSync(dir,{withFileTypes:true}).flatMap(entry=>{
    const path=join(dir,entry.name);
    return entry.isDirectory()?productionFiles(path):/\.tsx?$/.test(path)&&!/(\.test\.|\.fixture\.|\.d\.ts$)/.test(path)?[path]:[];
  });
}
it('does not add legacy bottom-centred notice emitters or aliases',()=>{
  const added:string[]=[];
  for(const path of productionFiles('src')){
    const file=relative('src',path).replaceAll('\\','/');
    const remaining=[...((allowlist as Record<string,string[]>)[file]??[])];
    for(const use of noticeUses(file,readFileSync(path,'utf8'))){
      const index=remaining.indexOf(use);
      if(index<0)added.push(`${file}: ${use}`);else remaining.splice(index,1);
    }
  }
  expect(added,'Use an in-app oink-ui toast; do not extend the legacy allowlist.').toEqual([]);
});
it('detects direct calls, callbacks, aliases and direct store writes',()=>{
  expect(noticeUses('App.tsx',"agent.fail('x'); task.catch(fail); const alias=agent['fail']; agent.adopt('error','x');")).toHaveLength(4);
  expect(noticeUses('api/store.ts',"set({error:'x'}); query('error',fetchError);")).toHaveLength(2);
});
