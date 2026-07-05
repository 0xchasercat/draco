export class Script { constructor(code){ this.code = code; } runInContext(){ return undefined; } runInNewContext(){ return undefined; } runInThisContext(){ return undefined; } }
export function createContext(o){ return o || {}; }
export function isContext(){ return false; }
export function runInNewContext(){ return undefined; }
export function runInThisContext(){ return undefined; }
export function compileFunction(){ return function(){}; }
export default { Script, createContext, isContext, runInNewContext, runInThisContext, compileFunction };
