const webcrypto = (globalThis.crypto) || {};
export function randomUUID(){ return 'xxxxxxxxxxxx4xxxyxxxxxxxxxxxxxxx'.replace(/[xy]/g,c=>{const r=Math.random()*16|0;return (c==='x'?r:(r&3|8)).toString(16);}); }
export function randomBytes(n){ const a=new Uint8Array(n); for(let i=0;i<n;i++)a[i]=Math.random()*256|0; return a; }
export function getRandomValues(a){ for(let i=0;i<a.length;i++)a[i]=Math.random()*256|0; return a; }
export { webcrypto };
export default { webcrypto, randomUUID, randomBytes, getRandomValues };
