export class ReadableStream { constructor(){} getReader(){ return { read(){ return Promise.resolve({done:true}); }, releaseLock(){} }; } }
export class WritableStream { constructor(){} }
export class TransformStream { constructor(){} }
export default { ReadableStream, WritableStream, TransformStream };
