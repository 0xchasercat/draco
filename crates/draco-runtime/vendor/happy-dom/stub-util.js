export const TextEncoder = globalThis.TextEncoder;
export const TextDecoder = globalThis.TextDecoder;
export function inherits(a,b){ if(b){a.super_=b;Object.setPrototypeOf(a.prototype,b.prototype);} }
export function promisify(fn){ return fn; }
export function inspect(x){ try { return JSON.stringify(x); } catch { return String(x); } }
export const types = { isAnyArrayBuffer:()=>false, isArrayBufferView:(x)=>ArrayBuffer.isView(x) };
export default { TextEncoder, TextDecoder, inherits, promisify, inspect, types };
