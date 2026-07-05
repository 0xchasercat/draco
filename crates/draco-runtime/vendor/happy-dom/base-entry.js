import './set-env-first.js';
import 'fastestsmallesttextencoderdecoder';           // sets globalThis.TextEncoder/TextDecoder
import { URL, URLSearchParams } from 'whatwg-url';
import structuredCloneImpl from '@ungap/structured-clone';
const g = globalThis;
if (!g.global) g.global = g;
if (!g.self) g.self = g;
g.URL = g.URL || URL;
g.URLSearchParams = g.URLSearchParams || URLSearchParams;
g.structuredClone = g.structuredClone || ((v) => structuredCloneImpl(v, { lossy: false }));
g.performance = g.performance || { now: () => Date.now(), timeOrigin: Date.now() };
if (!g.crypto) g.crypto = {};
if (!g.crypto.getRandomValues) g.crypto.getRandomValues = (a) => { for (let i=0;i<a.length;i++) a[i]=Math.random()*256|0; return a; };
if (!g.crypto.randomUUID) g.crypto.randomUUID = () => 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g,c=>{const r=Math.random()*16|0;return (c==='x'?r:(r&3|8)).toString(16);});
// crypto.subtle stub: present for feature-detection; operations reject (no real WebCrypto in-isolate).
if (!g.crypto.subtle) { const rej = () => Promise.reject(new Error("crypto.subtle unavailable in isolate")); g.crypto.subtle = { digest: rej, importKey: rej, exportKey: rej, encrypt: rej, decrypt: rej, sign: rej, verify: rej, generateKey: rej, deriveBits: rej, deriveKey: rej, wrapKey: rej, unwrapKey: rej }; }
// base64 + Buffer (TextEncoder/Decoder now exist)
const CH="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
if (!g.btoa) g.btoa=(s)=>{let o="";for(let i=0;i<s.length;){const a=s.charCodeAt(i++),b=i<s.length?s.charCodeAt(i++):NaN,c=i<s.length?s.charCodeAt(i++):NaN;const n=(a<<16)|((isNaN(b)?0:b)<<8)|(isNaN(c)?0:c);o+=CH[(n>>18)&63]+CH[(n>>12)&63]+(isNaN(b)?"=":CH[(n>>6)&63])+(isNaN(c)?"=":CH[n&63]);}return o;};
if (!g.atob) g.atob=(s)=>{s=String(s).replace(/=+$/,"");let o="",bc=0,bs=0;for(let i=0;i<s.length;i++){const idx=CH.indexOf(s[i]);if(idx<0)continue;bs=bc%4?bs*64+idx:idx;if(bc++%4)o+=String.fromCharCode(255&(bs>>((-2*bc)&6)));}return o;};
if (!g.Buffer) {
  const te=new g.TextEncoder(), td=new g.TextDecoder();
  const wrap=(u)=>{u.__isBuffer=true;u.toString=function(enc){if(enc==="base64"){let s="";for(let i=0;i<this.length;i++)s+=String.fromCharCode(this[i]);return g.btoa(s);}return td.decode(this);};return u;};
  const Buffer=function(){};
  Buffer.from=(x,enc)=>{if(typeof x==="string"){if(enc==="base64"){const bin=g.atob(x);const u=new Uint8Array(bin.length);for(let i=0;i<bin.length;i++)u[i]=bin.charCodeAt(i);return wrap(u);}return wrap(te.encode(x));}return wrap(new Uint8Array(x));};
  Buffer.alloc=(n)=>wrap(new Uint8Array(n));
  Buffer.isBuffer=(x)=>!!(x&&x.__isBuffer);
  Buffer.concat=(list)=>{let len=0;for(const b of list)len+=b.length;const out=new Uint8Array(len);let o=0;for(const b of list){out.set(b,o);o+=b.length;}return wrap(out);};
  g.Buffer=Buffer;
}
